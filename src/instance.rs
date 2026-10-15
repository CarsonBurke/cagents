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
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[cfg(feature = "ui")]
use std::{
    os::unix::{fs::FileTypeExt, net::UnixListener},
    thread::{self, JoinHandle},
};

mod daemon_admission;
pub(crate) use daemon_admission::{
    DAEMON_FENCE_ENV, DaemonAdmissionGuard, LaunchFence, has_inherited_daemon_fence,
    inherit_daemon_fence,
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

pub(crate) fn runtime_directory() -> io::Result<PathBuf> {
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

fn destination_socket_name(destination: &crate::notifications::Destination) -> String {
    match destination.application {
        crate::notifications::Application::Cagents => slot_socket_name(&destination.window),
        crate::notifications::Application::Ceditor => {
            // Keep the editor's namespace independent and compact. Appending
            // it to the Cagents name exceeds sockaddr_un for ordinary isolated
            // development runtime directories and UUID window identities.
            let prefix = if IS_DEVELOPMENT {
                "ceditor-development.activate"
            } else {
                "ceditor.activate"
            };
            format!("{prefix}.{}", destination.window)
        }
    }
}

/// A lock ends with its owning scope, even if a concurrent fork inherited the
/// file description and has not reached exec yet. Closing a File alone does
/// not release flock until every inherited descriptor has also closed.
pub(crate) struct OwnedLock(File);

impl std::ops::Deref for OwnedLock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for OwnedLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub(crate) fn owned_lock(path: &Path, operation: i32) -> io::Result<OwnedLock> {
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
    Ok(OwnedLock(lock))
}

/// A slot owns its full workspace snapshot; terminal identifiers still refer to
/// the one shared daemon. The old global lock remains held shared, so older
/// exclusive-lease binaries cannot open any of these stores concurrently.
pub struct WindowLease {
    _legacy_fence: OwnedLock,
    _slot: OwnedLock,
}

impl WindowLease {
    pub fn acquire(new_window: bool, requested: Option<&str>) -> io::Result<Self> {
        let (lease, slot) = Self::acquire_in(&runtime_directory()?, new_window, requested)?;
        let mut identity = owned_lock(
            &runtime_directory()?.join(format!("{ACTIVATION_SOCKET_FILE}.{slot}.device")),
            libc::LOCK_EX,
        )?;
        identity.0.set_len(0)?;
        if let Some(device) = crate::device::selected() {
            identity.0.write_all(device.name.as_bytes())?;
        }
        identity.0.sync_all()?;
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
                Ok(mut lock) => {
                    daemon_admission::publish_capability(&mut lock.0)?;
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
    if result.is_ok() {
        crate::diagnostics::record(crate::diagnostics::Event::Ready);
    }
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

pub use crate::runtime_state::{RecoveredPanicScope, recovered_panic_scope};

pub fn install_panic_hook(product: &'static str) {
    install_panic_hook_with_version(product, crate::VERSION);
}

pub fn install_panic_hook_with_version(product: &'static str, version: &'static str) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(product) = crate::diagnostics::Product::from_name(product) {
        crate::diagnostics::initialize(product, version);
    }
    crate::diagnostics::record(crate::diagnostics::Event::Startup);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let Some(recovered) = crate::runtime_state::begin_panic_report() else {
            return;
        };
        let subsystem = std::thread::current().name() == Some("accesskit-unix");
        crate::diagnostics::record_panic(recovered || subsystem, panic.location());
        // Payloads/backtraces can contain secrets. Only a user-requested local
        // debug capture invokes the raw default hook.
        if crate::diagnostics::local_debug() {
            previous(panic);
            // Local capture is explicit opt-in. Force a backtrace even when
            // RUST_BACKTRACE was absent before this thread's first panic.
            eprintln!("cagents panic backtrace:\n{}", std::backtrace::Backtrace::force_capture());
        }
        crate::runtime_state::finish_panic_report();
    }));
}

pub fn report_startup_failure() {
    crate::diagnostics::record(crate::diagnostics::Event::StartupFailed);
}

pub fn install_tracing(_product: &'static str, default_filter: &str) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    install_global_subscriber(default_filter);
}

#[cfg(any(feature = "host", feature = "ui"))]
fn install_global_subscriber(default_filter: &str) {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    let enabled = crate::diagnostics::local_debug();
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with(tracing_subscriber::filter::filter_fn(move |metadata| {
            enabled
                && (!cfg!(feature = "integrated-browser")
                    || !sensitive_browser_diagnostic(metadata.target(), metadata.level()))
        }))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(io::stderr),
        )
        .try_init();
}

#[cfg(any(feature = "host", feature = "ui"))]
fn sensitive_browser_diagnostic(target: &str, level: &tracing::Level) -> bool {
    *level >= tracing::Level::INFO
        && ["net", "hyper", "hyper_util", "h2", "zbus", "secret_service"]
            .iter()
            .any(|prefix| {
                target == *prefix
                    || target
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with("::"))
            })
}

#[cfg(not(any(feature = "host", feature = "ui")))]
fn install_global_subscriber(_default_filter: &str) {}

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

/// A shared launch fence prevents a reviewed handoff from committing while a
/// public launcher authenticates and restores the surviving window's daemon.
pub(crate) fn recovery_launch_fence() -> io::Result<OwnedLock> {
    owned_lock(
        &runtime_directory()?.join(format!("{APPLICATION_LOCK_FILE}.update")),
        libc::LOCK_SH | libc::LOCK_NB,
    )
    .map_err(|error| io::Error::new(error.kind(), format!("an update is in progress: {error}")))
}

static EDITOR_PROTECTION_READY: OnceLock<OwnedLock> = OnceLock::new();

fn editor_ready_path(runtime: &Path, pid: u32) -> PathBuf {
    let product = if IS_DEVELOPMENT {
        "ceditor-development"
    } else {
        "ceditor"
    };
    runtime.join(format!("{product}.worktrees-ready.{pid}"))
}

pub(crate) fn standalone_editor_protection_ready() -> io::Result<()> {
    let _launch = recovery_launch_fence()?;
    if EDITOR_PROTECTION_READY.get().is_none() {
        let lock = owned_lock(
            &editor_ready_path(&runtime_directory()?, process::id()),
            libc::LOCK_EX | libc::LOCK_NB,
        )?;
        EDITOR_PROTECTION_READY
            .set(lock)
            .map_err(|_| io::Error::other("editor protection already initialized"))?;
    }
    Ok(())
}

/// An older independent editor has no interpretable directory inventory.
/// Preserve all candidates while any local editor cannot prove readiness.
/// Never inspect its buffers, environment, cwd or command arguments.
pub(crate) fn verified_running_editors() -> io::Result<Vec<VerifiedWindow>> {
    verified_editors_in(&runtime_directory()?, Path::new("/proc"))
}

fn editor_executable(
    process: &Path,
    executable: io::Result<PathBuf>,
) -> io::Result<Option<PathBuf>> {
    match executable {
        Ok(executable) => {
            let name = executable.file_name().and_then(|name| name.to_str());
            Ok(matches!(
                name,
                Some(
                    "ceditor"
                        | "ceditor (deleted)"
                        | "ceditor-development"
                        | "ceditor-development (deleted)"
                )
            )
            .then_some(executable))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // Shipped ceditor keeps its executable-derived main-thread comm;
            // unlike cagents-ui, it never marks itself with a different name.
            // Linux truncates this name to 15 bytes. This fallback identifies
            // an opaque old editor without treating every hardened desktop
            // process as editor ownership. Custom-renamed binaries are outside
            // the same installed-product identity policy as the exe census.
            let name = match fs::read_to_string(process.join("comm")) {
                Ok(name) => name,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return match process.metadata() {
                        Err(gone) if gone.kind() == io::ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error),
                        Ok(_) => Err(error),
                    };
                }
                Err(error) => return Err(error),
            };
            if matches!(name.trim_end(), "ceditor" | "ceditor-develop") {
                Err(io::Error::other(
                    "opaque independent editor ownership cannot be authenticated",
                ))
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(error),
    }
}

fn verified_editors_in(runtime: &Path, proc_root: &Path) -> io::Result<Vec<VerifiedWindow>> {
    use std::os::fd::FromRawFd;
    let mut editors = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let process = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.uid() != current_uid() {
            continue;
        }
        let Some(executable) = editor_executable(&process, fs::read_link(process.join("exe")))?
        else {
            continue;
        };
        let lease_path = editor_ready_path(runtime, pid);
        let lease = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lease_path)
            .map_err(|_| {
                io::Error::other(
                    "close older or unready independent editors before automatic cleanup",
                )
            })?;
        let expected = lease.metadata()?;
        if !expected.is_file()
            || expected.uid() != current_uid()
            || expected.nlink() != 1
            || !process_owns_slot(&process, pid, &expected)?
        {
            return Err(io::Error::other(
                "independent editor protection cannot be authenticated",
            ));
        }
        // SAFETY: pidfd_open returns a fresh descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ownership of the successful descriptor is transferred.
        let pidfd = unsafe { File::from_raw_fd(raw as i32) };
        let editor = VerifiedWindow {
            executable,
            pidfd,
            pid,
            lease,
            lease_path,
        };
        if !editor.still_owns_lease()? {
            return Err(io::Error::other("independent editor ownership changed"));
        }
        editors.push(editor);
    }
    Ok(editors)
}

pub(crate) fn window_is_initialized() -> bool {
    WINDOW_SLOT.get().is_some()
}

pub(crate) fn window_protection_ready() -> io::Result<OwnedLock> {
    owned_lock(
        &runtime_directory()?.join(format!("{}.worktrees-ready", slot_lock_name(window_slot()))),
        libc::LOCK_EX | libc::LOCK_NB,
    )
}

/// Blocks new windows while automatic cleanup authenticates existing owners.
pub(crate) fn worktree_mutation_launch_fence() -> io::Result<OwnedLock> {
    owned_lock(
        &runtime_directory()?.join(format!("{APPLICATION_LOCK_FILE}.update")),
        libc::LOCK_EX | libc::LOCK_NB,
    )
}

pub(crate) struct VerifiedWindow {
    pub executable: PathBuf,
    pidfd: File,
    pid: u32,
    lease: File,
    lease_path: PathBuf,
}

impl VerifiedWindow {
    pub(crate) fn protects_worktrees(&self) -> io::Result<bool> {
        let name = self
            .lease_path
            .file_name()
            .ok_or_else(|| io::Error::other("window lease has no name"))?
            .to_string_lossy();
        let marker = self
            .lease_path
            .with_file_name(format!("{name}.worktrees-ready"));
        let metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && metadata.uid() == current_uid() => metadata,
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(self.still_owns_lease()?
            && process_owns_slot(
                &Path::new("/proc").join(self.pid.to_string()),
                self.pid,
                &metadata,
            )?)
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
    pub fn still_owns_lease(&self) -> io::Result<bool> {
        let expected = self.lease.metadata()?;
        let actual = fs::symlink_metadata(&self.lease_path)?;
        Ok(actual.is_file()
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino()
            && self.is_alive()
            && process_owns_slot(
                &Path::new("/proc").join(self.pid.to_string()),
                self.pid,
                &expected,
            )?
            && fs::read_link(Path::new("/proc").join(self.pid.to_string()).join("exe"))?
                == self.executable)
    }

    fn is_alive(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized descriptor and does not block.
        unsafe { libc::poll(&mut poll, 1, 0) == 0 }
    }
}

/// Contention alone is not ownership: require the exact exclusive flock in the
/// process's fdinfo, then pin its lifetime before trusting its executable.
/// The caller must hold the update launch fence throughout use of this proof.
pub(crate) fn verified_running_windows() -> io::Result<Vec<VerifiedWindow>> {
    verified_windows_in(&runtime_directory()?, Path::new("/proc"))
}

/// A staged installer itself holds the shared legacy fence. Its validated
/// exclusive launch guard already rules out a legacy exclusive UI owner.
pub(crate) fn verified_staged_windows() -> io::Result<Vec<VerifiedWindow>> {
    validate_stage_guard()?;
    verified_window_lease_owners_in(&runtime_directory()?, Path::new("/proc"))
}

fn flock_owner(line: &str, metadata: &fs::Metadata) -> Option<u32> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != 8
        || fields[1..4] != ["FLOCK", "ADVISORY", "WRITE"]
        || fields[6..8] != ["0", "EOF"]
    {
        return None;
    }
    let identity: Vec<_> = fields[5].split(':').collect();
    if identity.len() != 3
        || u64::from_str_radix(identity[0], 16).is_err()
        || u64::from_str_radix(identity[1], 16).is_err()
        || identity[2].parse::<u64>().ok() != Some(metadata.ino())
    {
        return None;
    }
    fields[4].parse::<u32>().ok().filter(|pid| *pid > 1)
}

fn fdinfo_owns_slot(info: &str, pid: u32, metadata: &fs::Metadata) -> bool {
    info.lines()
        .filter_map(|line| line.strip_prefix("lock:"))
        .any(|line| flock_owner(line, metadata) == Some(pid))
}

fn verified_windows_in(runtime: &Path, proc_root: &Path) -> io::Result<Vec<VerifiedWindow>> {
    let owners = verified_window_lease_owners_in(runtime, proc_root)?;
    // A legacy exclusive owner cannot be interpreted as absence. Stage-only
    // callers use their authenticated inherited guard instead of this probe.
    if owners.is_empty() {
        owned_lock(
            &runtime.join(APPLICATION_LOCK_FILE),
            libc::LOCK_EX | libc::LOCK_NB,
        )
        .map_err(|error| io::Error::new(error.kind(), "cannot authenticate legacy UI ownership"))?;
    }
    Ok(owners)
}

fn verified_window_lease_owners_in(
    runtime: &Path,
    proc_root: &Path,
) -> io::Result<Vec<VerifiedWindow>> {
    use std::os::fd::FromRawFd;
    let slots = running_window_slots(runtime)?;
    let mut owners = Vec::new();
    let kernel_locks = fs::read_to_string(proc_root.join("locks"))?;
    for slot in slots {
        let lease_path = runtime.join(slot_lock_name(&slot));
        let lock = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lease_path)?;
        let expected = lock.metadata()?;
        if !expected.is_file() || expected.uid() != current_uid() || expected.nlink() != 1 {
            return Err(io::Error::other("invalid live window lease"));
        }
        // /proc/locks is only an index into processes; fdinfo below proves
        // ownership. Avoid scanning every descriptor of every agent per window.
        // Btrfs subvolume st_dev can differ from the superblock device printed
        // in /proc/locks. Index by inode, then disambiguate using the actual
        // locked descriptor's stat device and inode, not a mere open descriptor.
        let pids: std::collections::BTreeSet<_> = kernel_locks
            .lines()
            .filter_map(|line| flock_owner(line, &expected))
            .collect();
        let mut matches = Vec::new();
        for pid in pids {
            let process = proc_root.join(pid.to_string());
            if !fs::metadata(&process).is_ok_and(|metadata| metadata.uid() == current_uid())
                || !process_owns_slot(&process, pid, &expected)?
            {
                continue;
            }
            // SAFETY: pidfd_open returns a fresh owned descriptor or -1.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if raw == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful pidfd_open transferred ownership to us.
            let pidfd = unsafe { File::from_raw_fd(raw as i32) };
            let executable = fs::read_link(process.join("exe"))?;
            let verified = VerifiedWindow {
                executable,
                pidfd,
                pid,
                lease: lock.try_clone()?,
                lease_path: lease_path.clone(),
            };
            if !verified.still_owns_lease()? {
                return Err(io::Error::other(
                    "window ownership changed during verification",
                ));
            }
            matches.push(verified);
        }
        if matches.len() != 1 {
            return Err(io::Error::other(format!(
                "cannot authenticate live window {slot}"
            )));
        }
        owners.push(matches.pop().unwrap());
    }
    Ok(owners)
}

fn process_owns_slot(process: &Path, pid: u32, expected: &fs::Metadata) -> io::Result<bool> {
    Ok(fs::read_dir(process.join("fdinfo"))?
        .filter_map(Result::ok)
        .any(|entry| {
            let descriptor = process.join("fd").join(entry.file_name());
            let matches = |actual: fs::Metadata| {
                actual.dev() == expected.dev() && actual.ino() == expected.ino()
            };
            fs::metadata(&descriptor).is_ok_and(&matches)
                && fs::read_to_string(entry.path())
                    .is_ok_and(|info| fdinfo_owns_slot(&info, pid, expected))
                && fs::metadata(&descriptor).is_ok_and(matches)
        }))
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
    lock: Option<OwnedLock>,
    runtime: PathBuf,
    _legacy_fence: OwnedLock,
    prepared: Vec<String>,
    closed: Vec<String>,
    fallback_executable: Option<PathBuf>,
}

impl UiUpdateGuard {
    pub fn set_fallback_executable(&mut self, executable: PathBuf) {
        self.fallback_executable = Some(executable);
    }

    pub fn has_running_windows(&self) -> io::Result<bool> {
        running_window_slots(&self.runtime).map(|slots| !slots.is_empty())
    }
    /// Pass the held launch fence to an installer that would otherwise try to
    /// acquire it again. The child validates both this exact lock description
    /// and the completed window shutdown; an environment flag is not authority.
    pub fn inherit_for_install(&self, command: &mut process::Command) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        let lock = self
            .lock
            .as_ref()
            .ok_or_else(|| io::Error::other("update launch fence was released"))?;
        let fd = lock.as_raw_fd();
        command.env(INSTALL_GUARD_ENV, INSTALL_GUARD_FD.to_string());
        // SAFETY: the guard remains alive through spawn. Only descriptor
        // syscalls execute after fork, and the reserved slot cannot overwrite
        // another inherited release asset.
        unsafe {
            command.pre_exec(move || {
                if fd != INSTALL_GUARD_FD {
                    if libc::fcntl(INSTALL_GUARD_FD, libc::F_GETFD) != -1 {
                        return Err(io::Error::from_raw_os_error(libc::EBUSY));
                    }
                    if libc::dup2(fd, INSTALL_GUARD_FD) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::fcntl(INSTALL_GUARD_FD, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

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
            fallback_executable: None,
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

pub(crate) const INSTALL_GUARD_ENV: &str = "CAGENTS_UI_UPDATE_GUARD_FD";
const INSTALL_GUARD_FD: i32 = 197;

/// Verify the inherited open-file description owns the active exclusive fence.
/// Do not wrap it in OwnedLock: dropping a validator must not unlock its parent.
pub(crate) fn validate_install_guard() -> io::Result<()> {
    validate_install_fence(true)
}

/// Staging owns the same authenticated launch fence without closing modern windows.
pub(crate) fn validate_stage_guard() -> io::Result<()> {
    validate_install_fence(false)
}

fn validate_install_fence(require_closed: bool) -> io::Result<()> {
    use std::os::fd::BorrowedFd;
    if env::var(INSTALL_GUARD_ENV).ok().as_deref() != Some("197") {
        return Err(io::Error::other("installer has no inherited update fence"));
    }
    if unsafe { libc::fcntl(INSTALL_GUARD_FD, libc::F_GETFD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_GETFD established that the descriptor exists; clone it rather
    // than taking ownership of the descriptor shared with our parent.
    let inherited = unsafe { BorrowedFd::borrow_raw(INSTALL_GUARD_FD) }.try_clone_to_owned()?;
    let inherited = File::from(inherited);
    let runtime = runtime_directory()?;
    let path = runtime.join(format!("{APPLICATION_LOCK_FILE}.update"));
    let expected = fs::symlink_metadata(&path)?;
    let actual = inherited.metadata()?;
    if !actual.is_file()
        || actual.uid() != current_uid()
        || actual.mode() & 0o077 != 0
        || actual.nlink() != 1
        || !expected.is_file()
        || actual.dev() != expected.dev()
        || actual.ino() != expected.ino()
    {
        return Err(io::Error::other(
            "installer inherited a different update fence",
        ));
    }
    match owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
        Ok(_) => return Err(io::Error::other("installer update fence is not held")),
        Err(error) => return Err(error),
    }
    if unsafe { libc::flock(INSTALL_GUARD_FD, libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return Err(io::Error::other("installer does not own the update fence"));
    }
    if require_closed && !running_window_slots(&runtime)?.is_empty() {
        return Err(io::Error::other(
            "application windows have not finished closing",
        ));
    }
    let _legacy = owned_lock(
        &runtime.join(APPLICATION_LOCK_FILE),
        libc::LOCK_SH | libc::LOCK_NB,
    )?;
    Ok(())
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
            if let Some(executable) = &self.fallback_executable {
                let _ = launch_window_at(executable, false, Some(slot), None);
            } else {
                let _ = launch_window(false, Some(slot), None);
            }
        }
        let _ = self._legacy_fence.unlock();
    }
}

/// Starts another independent process using the same installed UI executable.
pub fn launch_window(new_window: bool, slot: Option<&str>, source: Option<&str>) -> io::Result<()> {
    let executable = crate::companion::Product::Ui.beside_current()?;
    launch_window_at(&executable, new_window, slot, source)
}

/// Session ids belong to a daemon/device namespace. Never resolve a local
/// coordination link against a different device's saved window.
pub(crate) fn window_matches_selected_device(slot: &str) -> io::Result<bool> {
    match (crate::device::selected(), slot.starts_with("device-")) {
        (None, device_slot) => Ok(!device_slot),
        (Some(_), false) => Ok(false),
        (Some(selected), true) => {
            let mut identity = match owned_lock(
                &runtime_directory()?.join(format!("{ACTIVATION_SOCKET_FILE}.{slot}.device")),
                libc::LOCK_SH,
            ) {
                Ok(identity) => identity,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error),
            };
            let mut name = String::new();
            (&mut identity.0).take(64).read_to_string(&mut name)?;
            Ok(name == selected.name)
        }
    }
}

fn window_device_identity(
    runtime: &Path,
    slot: &str,
    devices: &[crate::device::Device],
) -> io::Result<Option<String>> {
    if !slot.starts_with("device-") {
        return Ok(None);
    }
    let mut identity = owned_lock(
        &runtime.join(format!("{ACTIVATION_SOCKET_FILE}.{slot}.device")),
        libc::LOCK_SH,
    )?;
    let mut name = String::new();
    (&mut identity.0).take(64).read_to_string(&mut name)?;
    if !devices.iter().any(|device| device.name == name) {
        return Err(io::Error::other(format!(
            "device profile for window {slot} is unavailable"
        )));
    }
    Ok(Some(name))
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
    let device_name = match slot {
        Some(slot) => window_device_identity(&runtime_directory()?, slot, &crate::device::list()?)?,
        None => crate::device::selected().map(|device| device.name.clone()),
    };
    if let Some(name) = device_name {
        command.args(["--device", &name, "--device-window-scoped"]);
    }
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
    pub session: Option<crate::model::TerminalId>,
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

    pub fn bind_destination(
        destination: &crate::notifications::Destination,
        events: async_channel::Sender<UiActivation>,
    ) -> io::Result<Self> {
        destination.validate().map_err(io::Error::other)?;
        Self::bind_named(
            &runtime_directory()?,
            destination_socket_name(destination),
            events,
        )
    }

    fn bind_in(
        runtime_directory: &Path,
        events: async_channel::Sender<UiActivation>,
    ) -> io::Result<Self> {
        Self::bind_named(runtime_directory, slot_socket_name(window_slot()), events)
    }

    fn bind_named(
        runtime_directory: &Path,
        socket: String,
        events: async_channel::Sender<UiActivation>,
    ) -> io::Result<Self> {
        require_owned_runtime_directory(runtime_directory)?;
        let socket_path = runtime_directory.join(socket);
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

/// Use a desktop portal's activation token without mutating process-wide env.
pub fn activate_window_with_token(slot: &str, token: Option<&str>) -> io::Result<bool> {
    validate_window_slot(slot)?;
    activate_window_at_with_token(&runtime_directory()?, slot, token)
}

/// Ask the owning window to navigate to this exact session. A missing window
/// is reported to the caller, which may reopen its saved slot before retrying.
pub fn activate_window_session(
    slot: &str,
    session: crate::model::TerminalId,
    token: Option<&str>,
) -> io::Result<bool> {
    activate_window_session_in(&runtime_directory()?, slot, session, token)
}

fn activate_window_session_in(
    runtime: &Path,
    slot: &str,
    session: crate::model::TerminalId,
    token: Option<&str>,
) -> io::Result<bool> {
    validate_window_slot(slot)?;
    require_owned_runtime_directory(runtime)?;
    let mut stream = match UnixStream::connect(runtime.join(slot_socket_name(slot))) {
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
    match token.filter(|token| valid_activation_token(token)) {
        Some(token) => writeln!(stream, "session {session} {token}")?,
        None => writeln!(stream, "session {session}")?,
    }
    Ok(read_capped_line(&stream)? == "ok")
}

pub fn activate_destination(
    destination: &crate::notifications::Destination,
    token: Option<&str>,
) -> io::Result<bool> {
    destination.validate().map_err(io::Error::other)?;
    let runtime = runtime_directory()?;
    activate_socket_with_token(&runtime, destination_socket_name(destination), token)
}

pub const CEDITOR_NOTIFICATION_ARGUMENT: &str = "--ceditor-notifications";
const CEDITOR_COMPANIONS_MARKER: &str = ".ceditor-companions";
const CEDITOR_COMPANIONS_CONTENT: &[u8] = b"ceditor companions v1\n";

/// A fixed local install marker preserves desktop identity when the private
/// CLI restarts its daemon. Development launches use the explicit fixed flag.
/// Neither path permits a caller-supplied desktop ID or executable path.
pub fn notification_daemon_application(
    executable: &Path,
    editor_requested: bool,
) -> io::Result<crate::notifications::Application> {
    let marker = executable.with_file_name(CEDITOR_COMPANIONS_MARKER);
    let installed = match fs::symlink_metadata(&marker) {
        Ok(metadata) => {
            if !metadata.is_file()
                || metadata.mode() & 0o022 != 0
                || !(metadata.uid() == 0 || metadata.uid() == current_uid())
                || metadata.len() != CEDITOR_COMPANIONS_CONTENT.len() as u64
                || fs::read(&marker)? != CEDITOR_COMPANIONS_CONTENT
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "invalid private Ceditor companion marker",
                ));
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if installed || editor_requested {
        // A flag is accepted only with an owned co-installed editor, never a
        // PATH lookup which could silently select another installation.
        resolve_notification_executable(
            &crate::companion::Product::Editor.beside(executable)?,
            std::iter::empty(),
        )?;
        Ok(crate::notifications::Application::Ceditor)
    } else {
        Ok(crate::notifications::Application::Cagents)
    }
}

pub fn notification_desktop_id(application: crate::notifications::Application) -> &'static str {
    match application {
        crate::notifications::Application::Cagents => APPLICATION_ID,
        crate::notifications::Application::Ceditor if IS_DEVELOPMENT => {
            "io.github.cagents.ceditor.Development"
        }
        crate::notifications::Application::Ceditor => "io.github.cagents.ceditor",
    }
}

// The editor's daemon is a private matched companion. The other shell may be
// installed separately, so resolve only these fixed application names, never a
// path carried by an alert. Relative PATH entries cannot redirect activation.
fn notification_executable(product: crate::companion::Product) -> io::Result<PathBuf> {
    let current = env::current_exe()?;
    let sibling = product.beside(&current)?;
    let search = env::var_os("PATH").unwrap_or_default();
    resolve_notification_executable(
        &sibling,
        env::split_paths(&search).map(|p| p.join(product.file_name())),
    )
}

fn resolve_notification_executable(
    sibling: &Path,
    alternatives: impl IntoIterator<Item = PathBuf>,
) -> io::Result<PathBuf> {
    for candidate in std::iter::once(sibling.to_owned()).chain(alternatives) {
        if !candidate.is_absolute() {
            continue;
        }
        let Ok(path) = candidate.canonicalize() else {
            continue;
        };
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_file()
            && meta.mode() & 0o111 != 0
            && meta.mode() & 0o022 == 0
            && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() })
        {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} is not installed beside the notification service or in an absolute PATH directory; install that application to open this alert",
            sibling.file_name().unwrap_or_default().to_string_lossy()
        ),
    ))
}

pub fn launch_notification_destination(
    destination: &crate::notifications::Destination,
    token: Option<&str>,
) -> io::Result<()> {
    destination.validate().map_err(io::Error::other)?;
    if destination.application == crate::notifications::Application::Cagents {
        return launch_notification_window(&destination.window, token);
    }
    let mut command =
        process::Command::new(notification_executable(crate::companion::Product::Editor)?);
    command.args(["--notification-window", &destination.window]);
    command.env_remove("XDG_ACTIVATION_TOKEN");
    if let Some(token) = token.filter(|token| valid_activation_token(token)) {
        command.env("XDG_ACTIVATION_TOKEN", token);
    }
    crate::control::spawn_ui_process(&mut command)
}

pub fn launch_notification_window(slot: &str, token: Option<&str>) -> io::Result<()> {
    validate_window_slot(slot)?;
    let mut command =
        process::Command::new(notification_executable(crate::companion::Product::Ui)?);
    command.args(["--window", slot]);
    command.env_remove("XDG_ACTIVATION_TOKEN");
    if let Some(token) = token.filter(|token| valid_activation_token(token)) {
        command.env("XDG_ACTIVATION_TOKEN", token);
    }
    crate::control::spawn_ui_process(&mut command)
}

fn activate_window_in(runtime_directory: &Path, slot: &str) -> io::Result<bool> {
    let token = env::var("XDG_ACTIVATION_TOKEN").ok();
    activate_window_at_with_token(runtime_directory, slot, token.as_deref())
}

fn valid_activation_token(token: &str) -> bool {
    !token.is_empty()
        && !token.contains('\n')
        && !token.contains('\r')
        && token.len() < MAX_ACTIVATION_LINE_BYTES
}

fn activate_window_at_with_token(
    runtime_directory: &Path,
    slot: &str,
    token: Option<&str>,
) -> io::Result<bool> {
    activate_socket_with_token(runtime_directory, slot_socket_name(slot), token)
}

fn activate_socket_with_token(
    runtime_directory: &Path,
    socket: String,
    token: Option<&str>,
) -> io::Result<bool> {
    require_owned_runtime_directory(runtime_directory)?;
    let socket_path = runtime_directory.join(socket);
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
    let token = token.filter(|token| valid_activation_token(token));
    writeln!(
        stream,
        "{}",
        match token {
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
                // The acknowledgement is the launcher's proof that a window
                // will be raised: `cagents ui` reports "already running" and
                // spawns nothing once it reads "ok". Hand the event over
                // first, exactly as the update branch below does, so a closed
                // receiver reports a failure instead of a silent no-op.
                if events
                    .send_blocking(UiActivation {
                        token,
                        session: None,
                        update: None,
                    })
                    .is_err()
                {
                    let _ = writeln!(&stream, "error: window is no longer accepting activations");
                    break;
                }
                let _ = writeln!(&stream, "ok");
            }
            Ok(ActivationRequest::Session(session, token)) => {
                if events
                    .send_blocking(UiActivation {
                        token,
                        session: Some(session),
                        update: None,
                    })
                    .is_err()
                {
                    let _ = writeln!(&stream, "error: window is no longer accepting activations");
                    break;
                }
                let _ = writeln!(&stream, "ok");
            }
            Ok(ActivationRequest::Update(action)) => {
                let (response, receiver) = std::sync::mpsc::sync_channel(1);
                if events
                    .send_blocking(UiActivation {
                        token: None,
                        session: None,
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
    Session(crate::model::TerminalId, Option<String>),
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
        Some(("session", identity)) => {
            let (id, token) = identity
                .split_once(' ')
                .map_or((identity, None), |(id, token)| (id, Some(token)));
            if token.is_some_and(|token| !valid_activation_token(token)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid activation token",
                ));
            }
            id.parse::<crate::model::TerminalId>()
                .map(|id| ActivationRequest::Session(id, token.map(str::to_owned)))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
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
            let stat = match fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let fields = stat
                .rsplit_once(')')
                .map(|(_, fields)| fields.split_whitespace().collect::<Vec<_>>())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid legacy UI process state",
                    )
                })?;
            let state = fields.first().copied().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing legacy UI process state",
                )
            })?;
            // An exited single-thread UI has released its descriptors and
            // cannot write the store, even if its parent never collects it.
            // A zombie leader may still have live worker threads.
            if matches!(state, "Z" | "X") && fields.get(17) == Some(&"1") {
                continue;
            }
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
pub(crate) const IS_DEVELOPMENT: bool = cfg!(any(debug_assertions, feature = "development"));

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

/// This instance's settings file, which the UI owns and the CLI only reads.
pub fn settings_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn verified_window_requires_its_exact_kernel_flock_not_an_open_descriptor() {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join(slot_lock_name(PRIMARY_WINDOW));
        let held = owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let merely_open = File::open(&path).unwrap();
        let expected = held.metadata().unwrap();
        let info =
            fs::read_to_string(format!("/proc/self/fdinfo/{}", merely_open.as_raw_fd())).unwrap();
        assert!(!fdinfo_owns_slot(&info, std::process::id(), &expected));
        let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", held.as_raw_fd())).unwrap();
        assert!(
            fdinfo_owns_slot(&info, std::process::id(), &expected),
            "{info:?}, pid={}, dev={}, ino={}",
            std::process::id(),
            expected.dev(),
            expected.ino()
        );
        assert!(!fdinfo_owns_slot(&info, std::process::id() + 1, &expected));
        let owners = verified_windows_in(runtime.path(), Path::new("/proc")).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].executable, env::current_exe().unwrap());
        assert!(owners[0].still_owns_lease().unwrap());
        drop(held);
        assert!(!owners[0].still_owns_lease().unwrap());
        assert!(
            verified_windows_in(runtime.path(), Path::new("/proc"))
                .unwrap()
                .is_empty()
        );
        let held = owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let owners = verified_windows_in(runtime.path(), Path::new("/proc")).unwrap();
        fs::rename(&path, runtime.path().join("replaced-lease")).unwrap();
        let replacement = owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        assert!(
            !owners[0].still_owns_lease().unwrap(),
            "an old lock cannot authenticate a replacement slot inode"
        );
        drop(replacement);
        drop(held);
    }

    #[test]
    fn window_ownership_cannot_combine_different_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let held = owned_lock(&root.path().join("lease"), libc::LOCK_EX | libc::LOCK_NB).unwrap();
        let expected = held.metadata().unwrap();
        let locked_info =
            fs::read_to_string(format!("/proc/self/fdinfo/{}", held.as_raw_fd())).unwrap();
        let process = root.path().join("process");
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::create_dir(process.join("fdinfo")).unwrap();
        // One descriptor names the right file but has no flock. Another has
        // the expected lock record but names a different file: combining the
        // two observations would falsely authenticate this process.
        fs::write(root.path().join("other"), "unrelated").unwrap();
        symlink(root.path().join("lease"), process.join("fd/1")).unwrap();
        symlink(root.path().join("other"), process.join("fd/2")).unwrap();
        fs::write(process.join("fdinfo/1"), "pos: 0\n").unwrap();
        fs::write(process.join("fdinfo/2"), &locked_info).unwrap();
        assert!(!process_owns_slot(&process, std::process::id(), &expected).unwrap());
        fs::write(process.join("fdinfo/1"), locked_info).unwrap();
        assert!(process_owns_slot(&process, std::process::id(), &expected).unwrap());
    }

    #[test]
    fn opaque_editor_identity_preserves_editors_without_blocking_unrelated_hardened_processes() {
        let process = tempfile::tempdir().unwrap();
        for (name, editor) in [
            ("unrelated-daemo", false),
            ("ceditor", true),
            ("ceditor-develop", true),
        ] {
            fs::write(process.path().join("comm"), format!("{name}\n")).unwrap();
            let result = editor_executable(
                process.path(),
                Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            );
            assert_eq!(result.is_err(), editor, "{name}");
            if !editor {
                assert!(result.unwrap().is_none());
            }
        }
        fs::remove_file(process.path().join("comm")).unwrap();
        assert!(
            editor_executable(
                process.path(),
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            )
            .is_err()
        );
        assert!(
            editor_executable(
                process.path(),
                Err(io::Error::from(io::ErrorKind::NotFound))
            )
            .unwrap()
            .is_none()
        );
        for name in [
            "ceditor",
            "ceditor (deleted)",
            "ceditor-development",
            "ceditor-development (deleted)",
        ] {
            assert!(
                editor_executable(process.path(), Ok(PathBuf::from(name)))
                    .unwrap()
                    .is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn automatic_cleanup_preserves_older_and_unregistered_independent_editors() {
        use std::os::unix::fs::symlink;
        let runtime = tempfile::tempdir().unwrap();
        let processes = tempfile::tempdir().unwrap();
        let editor = processes.path().join("245");
        fs::create_dir(&editor).unwrap();
        fs::create_dir(editor.join("fdinfo")).unwrap();
        symlink("/fixture/ceditor", editor.join("exe")).unwrap();
        assert!(verified_editors_in(runtime.path(), processes.path()).is_err());
        // A stale named marker is not evidence of a process-owned kernel lock.
        fs::write(editor_ready_path(runtime.path(), 245), "").unwrap();
        assert!(verified_editors_in(runtime.path(), processes.path()).is_err());
        fs::remove_file(editor.join("exe")).unwrap();
        symlink("/fixture/unrelated-editor", editor.join("exe")).unwrap();
        assert!(
            verified_editors_in(runtime.path(), processes.path())
                .unwrap()
                .is_empty()
        );
        fs::remove_file(editor.join("exe")).unwrap();
        symlink("/fixture/ceditor (deleted)", editor.join("exe")).unwrap();
        assert!(verified_editors_in(runtime.path(), processes.path()).is_err());
    }

    #[test]
    fn recovery_rejects_unknown_legacy_ownership_and_pending_handoff() {
        let runtime = tempfile::tempdir().unwrap();
        let legacy = owned_lock(
            &runtime.path().join(APPLICATION_LOCK_FILE),
            libc::LOCK_SH | libc::LOCK_NB,
        )
        .unwrap();
        assert!(verified_windows_in(runtime.path(), Path::new("/proc")).is_err());
        drop(legacy);
        let update = runtime
            .path()
            .join(format!("{APPLICATION_LOCK_FILE}.update"));
        let handoff = owned_lock(&update, libc::LOCK_EX | libc::LOCK_NB).unwrap();
        assert!(owned_lock(&update, libc::LOCK_SH | libc::LOCK_NB).is_err());
        drop(handoff);
        assert!(owned_lock(&update, libc::LOCK_SH | libc::LOCK_NB).is_ok());
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn browser_credentials_are_excluded_even_with_explicit_trace_targets() {
        use tracing_subscriber::{Layer, layer::SubscriberExt};
        struct Observe(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl<S: tracing::Subscriber> Layer<S> for Observe {
            fn on_event(
                &self,
                _: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "trace,net::cookie=trace,net::http_loader=trace",
            ))
            .with(tracing_subscriber::filter::filter_fn(|metadata| {
                !sensitive_browser_diagnostic(metadata.target(), metadata.level())
            }))
            .with(Observe(seen.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "net::cookie", "fixture bearer cookie must be filtered");
            // Pinned Servo emits the complete Cookie header at INFO, including
            // when a site reads document.cookie, not just during extraction.
            tracing::info!(target: "net::cookie_storage", " === COOKIES SENT: session=fixture-bearer");
            tracing::debug!(target: "net::http_loader", "fixture cookie headers must be filtered");
            tracing::trace!(target: "zbus::message", "fixture secret-store reply must be filtered");
            // Servo uses `log`, whose events have different callsites from
            // native tracing events. Exercise the production bridge too.
            for level in [
                tracing_log::log::Level::Info,
                tracing_log::log::Level::Debug,
            ] {
                tracing_log::format_trace(
                    &tracing_log::log::Record::builder()
                        .target("net::cookie_storage")
                        .level(level)
                        .args(format_args!("session=fixture-bearer"))
                        .build(),
                )
                .unwrap();
            }
            tracing::warn!(target: "net::http_loader", "ordinary connection failure");
            tracing::debug!(target: "cagents::browser", "ordinary app diagnostic");
            for (target, level) in [
                ("net::http_loader", tracing_log::log::Level::Warn),
                ("cagents::browser", tracing_log::log::Level::Info),
            ] {
                tracing_log::format_trace(
                    &tracing_log::log::Record::builder()
                        .target(target)
                        .level(level)
                        .args(format_args!("ordinary diagnostic"))
                        .build(),
                )
                .unwrap();
            }
        });
        assert_eq!(seen.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn scoped_locks_release_inherited_descriptions_on_early_error() {
        let runtime = tempdir().unwrap();
        let path = runtime.path().join("early-error.lock");
        let mut inherited = None;
        let mut acquire_then_fail = || -> io::Result<()> {
            let lock = owned_lock(&path, libc::LOCK_SH | libc::LOCK_NB)?;
            inherited = Some(lock.try_clone()?);
            Err(io::Error::other("a later acquisition failed"))
        };
        assert!(acquire_then_fail().is_err());
        // The duplicated description models a child between fork and exec.
        // It remains open, but must no longer hold the abandoned shared lock.
        assert!(inherited.is_some());
        assert!(owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB).is_ok());
    }

    #[test]
    fn device_window_relaunch_retains_target_and_missing_identity_fails_closed() {
        let runtime = tempdir().unwrap();
        let device = crate::device::Device {
            name: "fw16".into(),
            host: "fw16".into(),
            executable: "cagents".into(),
        };
        let slot = crate::device::scoped_window_slot("fw16", "primary");
        assert!(
            window_device_identity(runtime.path(), &slot, std::slice::from_ref(&device)).is_err()
        );
        fs::write(
            runtime
                .path()
                .join(format!("{ACTIVATION_SOCKET_FILE}.{slot}.device")),
            "fw16",
        )
        .unwrap();
        assert_eq!(
            window_device_identity(runtime.path(), &slot, std::slice::from_ref(&device)).unwrap(),
            Some("fw16".into())
        );
        assert!(window_device_identity(runtime.path(), &slot, &[]).is_err());
        assert_eq!(
            window_device_identity(runtime.path(), "fw16-primary", &[device]).unwrap(),
            None
        );
    }

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
            fallback_executable: None,
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
            fallback_executable: None,
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
        let mut fields = ["0"; 18];
        fields[0] = "S";
        fields[1] = "1";
        fields[17] = "1";
        let stat = |fields: &[&str]| format!("4294967294 (cagents-ui) {}", fields.join(" "));
        fs::write(legacy.join("stat"), stat(&fields)).unwrap();
        assert!(legacy_ui_process_running(proc_root.path()).unwrap());
        for state in ["Z", "X"] {
            fields[0] = state;
            fs::write(legacy.join("stat"), stat(&fields)).unwrap();
            assert!(!legacy_ui_process_running(proc_root.path()).unwrap());
        }
        fields[17] = "2";
        fs::write(legacy.join("stat"), stat(&fields)).unwrap();
        assert!(legacy_ui_process_running(proc_root.path()).unwrap());
        fs::write(legacy.join("stat"), "malformed").unwrap();
        assert!(legacy_ui_process_running(proc_root.path()).is_err());
        fs::remove_file(legacy.join("stat")).unwrap();
        assert!(!legacy_ui_process_running(proc_root.path()).unwrap());
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

    #[test]
    fn private_editor_companion_uses_only_fixed_owned_desktop_context() {
        let root = tempfile::tempdir().unwrap();
        let daemon = root.path().join("cagentsd");
        assert_eq!(
            notification_daemon_application(&daemon, false).unwrap(),
            crate::notifications::Application::Cagents
        );
        assert!(notification_daemon_application(&daemon, true).is_err());
        let editor = root.path().join("ceditor");
        fs::write(&editor, "fixture executable").unwrap();
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            notification_daemon_application(&daemon, true).unwrap(),
            crate::notifications::Application::Ceditor
        );
        let marker = root.path().join(CEDITOR_COMPANIONS_MARKER);
        fs::write(&marker, CEDITOR_COMPANIONS_CONTENT).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            notification_daemon_application(&daemon, false).unwrap(),
            crate::notifications::Application::Ceditor
        );
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(notification_daemon_application(&daemon, false).is_err());
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&marker, "unrecognized context").unwrap();
        assert!(notification_daemon_application(&daemon, false).is_err());
    }

    #[test]
    fn notification_launch_resolves_private_siblings_then_safe_installed_shells() {
        let root = tempfile::tempdir().unwrap();
        let sibling = root.path().join("private/ceditor");
        let installed = root.path().join("bin/ceditor");
        fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        fs::create_dir_all(installed.parent().unwrap()).unwrap();
        fs::write(&installed, "installed").unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_notification_executable(&sibling, [installed.clone()]).unwrap(),
            installed
        );
        fs::write(&sibling, "matched").unwrap();
        fs::set_permissions(&sibling, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_notification_executable(&sibling, [installed.clone()]).unwrap(),
            sibling
        );
        fs::set_permissions(&sibling, fs::Permissions::from_mode(0o777)).unwrap();
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            resolve_notification_executable(&sibling, [installed, PathBuf::from("ceditor")])
                .is_err()
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn editor_activation_supports_isolated_runtime_and_full_window_name() {
        use crate::notifications::{Application, Destination};
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let padding = 37 - root.path().as_os_str().len() - 1;
        let runtime = root.path().join("r".repeat(padding));
        fs::create_dir(&runtime).unwrap();
        let destination = Destination {
            application: Application::Ceditor,
            window: "w".repeat(40),
        };
        destination.validate().unwrap();
        let (sender, receiver) = async_channel::bounded(1);
        let listener = UiActivationListener::bind_named(
            &runtime,
            destination_socket_name(&destination),
            sender,
        )
        .unwrap();
        assert!(
            activate_socket_with_token(
                &runtime,
                destination_socket_name(&destination),
                Some("editor-token"),
            )
            .unwrap()
        );
        assert_eq!(
            receiver.recv_blocking().unwrap().token.as_deref(),
            Some("editor-token")
        );
        drop(listener);
        assert!(
            !activate_socket_with_token(&runtime, destination_socket_name(&destination), None,)
                .unwrap()
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn notification_application_destinations_use_distinct_real_sockets() {
        use crate::notifications::{Application, Destination};
        let runtime = tempdir().unwrap();
        let agents = Destination {
            application: Application::Cagents,
            window: "review".into(),
        };
        let editor = Destination {
            application: Application::Ceditor,
            window: "review".into(),
        };
        let (agents_sender, agents_receiver) = async_channel::bounded(1);
        let (editor_sender, editor_receiver) = async_channel::bounded(1);
        let _agents = UiActivationListener::bind_named(
            runtime.path(),
            destination_socket_name(&agents),
            agents_sender,
        )
        .unwrap();
        let _editor = UiActivationListener::bind_named(
            runtime.path(),
            destination_socket_name(&editor),
            editor_sender,
        )
        .unwrap();
        assert!(
            activate_socket_with_token(
                runtime.path(),
                destination_socket_name(&editor),
                Some("editor-token")
            )
            .unwrap()
        );
        assert_eq!(
            editor_receiver.recv_blocking().unwrap().token.as_deref(),
            Some("editor-token")
        );
        assert!(agents_receiver.try_recv().is_err());
        assert!(
            activate_socket_with_token(
                runtime.path(),
                destination_socket_name(&agents),
                Some("agents-token")
            )
            .unwrap()
        );
        assert_eq!(
            agents_receiver.recv_blocking().unwrap().token.as_deref(),
            Some("agents-token")
        );
        assert!(editor_receiver.try_recv().is_err());
    }

    #[cfg(feature = "ui")]
    #[test]
    fn session_activation_delivers_the_originating_windows_token() {
        let runtime = tempdir().unwrap();
        let (sender, receiver) = async_channel::unbounded();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();
        let session = crate::model::TerminalId::new();
        assert!(
            activate_window_session_in(
                runtime.path(),
                PRIMARY_WINDOW,
                session,
                Some("click-token")
            )
            .unwrap()
        );
        let activation = receiver.recv_blocking().unwrap();
        assert_eq!(activation.session, Some(session));
        assert_eq!(activation.token.as_deref(), Some("click-token"));
        assert!(activation.update.is_none());
        assert!(activate_window_session_in(runtime.path(), PRIMARY_WINDOW, session, None).unwrap());
        let activation = receiver.recv_blocking().unwrap();
        assert_eq!(activation.session, Some(session));
        assert_eq!(activation.token, None);
    }

    #[test]
    fn local_session_links_exclude_device_window_namespaces() {
        assert!(window_matches_selected_device(PRIMARY_WINDOW).unwrap());
        assert!(window_matches_selected_device("window-2").unwrap());
        assert!(!window_matches_selected_device("device-0123456789abcdef").unwrap());
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
        let session = crate::model::TerminalId::new();
        assert!(
            matches!(parse_activation_request(&format!("session {session}")).unwrap(), ActivationRequest::Session(id, None) if id == session)
        );
        assert!(parse_activation_request("session invalid").is_err());
        assert!(
            matches!(parse_activation_request(&format!("session {session} wayland-token")).unwrap(), ActivationRequest::Session(id, Some(token)) if id == session && token == "wayland-token")
        );
        assert!(parse_activation_request(&format!("session {session} ")).is_err());
        assert!(parse_activation_request(&format!("session {session} token\nnoise")).is_err());
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
    fn activation_is_not_acknowledged_once_the_window_stops_listening() {
        let runtime = tempdir().unwrap();
        let (sender, receiver) = async_channel::unbounded::<UiActivation>();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();
        drop(receiver);

        // "ok" makes `cagents ui` skip spawning a window, so it must never be
        // written for an activation no window can act on.
        assert!(!activate_running_ui_in(runtime.path()).unwrap());
    }

    /// An exiting window closes its receiver while it still holds a clone;
    /// its activations must then be refused, never acknowledged.
    #[cfg(feature = "ui")]
    #[test]
    fn activation_is_refused_once_an_exiting_window_closes_its_receiver() {
        let runtime = tempdir().unwrap();
        let (sender, receiver) = async_channel::unbounded::<UiActivation>();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();
        assert!(activate_running_ui_in(runtime.path()).unwrap());
        assert!(receiver.recv_blocking().is_ok());

        receiver.close();

        assert!(!activate_running_ui_in(runtime.path()).unwrap());
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
