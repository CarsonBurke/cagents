use std::{
    cell::Cell,
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
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
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

fn destination_socket_name(destination: &crate::notifications::Destination) -> String {
    match destination.application {
        crate::notifications::Application::Cagents => slot_socket_name(&destination.window),
        crate::notifications::Application::Ceditor => {
            format!("{ACTIVATION_SOCKET_FILE}.ceditor.{}", destination.window)
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

// --- diagnostics ----------------------------------------------------------
//
// Every shipped process runs with its standard streams on /dev/null:
// `control::spawn_ui_process` nulls the UI's stderr, and the daemon is spawned
// with both stdout and stderr nulled. A subscriber bound to those descriptors
// throws `RUST_LOG` output away in production, and a panic message goes with
// it. Diagnostics therefore go to a bounded file under the data directory
// that the user can find and attach to a report, plus stderr when a terminal
// is actually attached.

/// Size cap for the rolling diagnostic log. One previous generation is kept as
/// `<name>.1`, so a product costs at most twice this on disk. Only the lanes
/// that link `tracing-subscriber` have a log to bound.
#[cfg(any(feature = "host", feature = "ui"))]
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Crash records are a few kilobytes each. A tighter cap keeps a panic loop
/// from filling the data directory while still retaining the first failures.
const MAX_CRASH_LOG_BYTES: u64 = 512 * 1024;

static DIAGNOSTIC_PRODUCT: OnceLock<&'static str> = OnceLock::new();
/// Opened when the hook is installed: the hook itself runs on a panicking
/// thread, which is the worst place to create directories or take new locks.
static CRASH_LOG: OnceLock<Option<Mutex<RotatingFile>>> = OnceLock::new();
#[cfg(any(feature = "host", feature = "ui"))]
static TRACE_LOG: OnceLock<Option<Mutex<RotatingFile>>> = OnceLock::new();

thread_local! {
    /// Number of `catch_unwind` recovery scopes active on this thread.
    static RECOVERED_PANIC_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Latch that stops a panic raised inside the reporting path from
    /// re-entering the hook and aborting the process with no record at all.
    static REPORTING_PANIC: Cell<bool> = const { Cell::new(false) };
}

/// Append-only diagnostic file that keeps exactly one previous generation.
struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
    limit: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, limit: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = open_private_append(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
            limit,
        })
    }

    /// Several processes share one product log (the UI spawns standalone
    /// dialogs), so a concurrent rotation can rename the file out from under
    /// another writer. That writer keeps its own descriptor and its records
    /// land in the retained generation, which is why nothing here locks.
    fn rotate(&mut self) -> io::Result<()> {
        let mut previous = self.path.clone().into_os_string();
        previous.push(".1");
        fs::rename(&self.path, PathBuf::from(previous))?;
        self.file = open_private_append(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if self.written >= self.limit {
            // Losing the rotation must not lose the diagnostics; an
            // over-long file is strictly better than a silent process.
            let _ = self.rotate();
        }
        self.file.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens a diagnostic file with the crate's private-file discipline: 0600, no
/// symlink traversal, and a refusal to append to anything but a regular file
/// this user owns.
fn open_private_append(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "diagnostic log path is not an owned regular file",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(file)
}

fn open_diagnostic(product: &str, suffix: &str, limit: u64) -> Option<Mutex<RotatingFile>> {
    let dirs = project_dirs()?;
    let path = dirs.data_dir().join(format!("{product}{suffix}"));
    RotatingFile::open(path, limit).map(Mutex::new).ok()
}

fn crash_log() -> Option<&'static Mutex<RotatingFile>> {
    let product = *DIAGNOSTIC_PRODUCT.get()?;
    CRASH_LOG
        .get_or_init(|| open_diagnostic(product, "-crash.log", MAX_CRASH_LOG_BYTES))
        .as_ref()
}

fn append_crash_record(record: &str) {
    let Some(file) = crash_log() else {
        return;
    };
    let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = file.write_all(record.as_bytes());
    let _ = file.flush();
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default()
}

/// Marks the current thread as running inside a `catch_unwind` recovery guard.
/// Rust runs the panic hook *before* unwinding, so without this a deliberately
/// recovered fault is filed as a process crash: the Servo delegate dispatcher
/// alone would write one backtrace per callback into the crash log.
#[must_use = "the scope lasts until this guard is dropped"]
pub struct RecoveredPanicScope(());

impl Drop for RecoveredPanicScope {
    fn drop(&mut self) {
        let _ = RECOVERED_PANIC_DEPTH.try_with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Opens a scope whose panics are recorded as recovered rather than fatal.
/// Create the guard *outside* the `catch_unwind` closure so it survives the
/// unwind and is dropped once the caller has handled the failure.
pub fn recovered_panic_scope() -> RecoveredPanicScope {
    let _ = RECOVERED_PANIC_DEPTH.try_with(|depth| depth.set(depth.get().saturating_add(1)));
    RecoveredPanicScope(())
}

/// Installs a process-wide panic hook that persists a diagnosable record and
/// then chains to the hook it replaced. Idempotent: later calls do nothing.
pub fn install_panic_hook(product: &'static str) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = DIAGNOSTIC_PRODUCT.set(product);
    // Open the file now so the hook never creates a directory while unwinding.
    let _ = crash_log();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        report_panic(product, panic);
        previous(panic);
    }));
}

fn report_panic(product: &str, panic: &std::panic::PanicHookInfo<'_>) {
    // Thread-local state is gone during thread teardown; report anyway rather
    // than lose the record.
    if REPORTING_PANIC
        .try_with(|reporting| reporting.replace(true))
        .unwrap_or(false)
    {
        return;
    }
    let _reset = ReportingReset;
    let recovered = RECOVERED_PANIC_DEPTH
        .try_with(|depth| depth.get())
        .unwrap_or(0)
        > 0;
    let thread = std::thread::current();
    let message = panic_message(panic.payload());
    let location = panic.location();
    let mut record = format!(
        "--- {product} {} pid={} thread={} at_unix_ms={} ---\n",
        if recovered {
            "recovered panic"
        } else {
            "panic"
        },
        process::id(),
        thread.name().unwrap_or("<unnamed>"),
        unix_millis(),
    );
    if let Some(location) = location {
        record.push_str(&format!(
            "location: {}:{}:{}\n",
            location.file(),
            location.line(),
            location.column()
        ));
    }
    record.push_str(&format!("message: {message}\n"));
    if !recovered {
        // A fatal panic is the one report nobody can reproduce on demand, and
        // `capture` honours RUST_BACKTRACE, so a desktop launch - which never
        // sets it - left every crash record with nothing but a location. The
        // process is already dying here, so symbolization costs nothing that
        // matters. A recovered fault is a routine event on some paths and
        // still pays nothing.
        record.push_str(&format!(
            "backtrace:\n{}\n",
            std::backtrace::Backtrace::force_capture()
        ));
    }
    append_crash_record(&record);
}

struct ReportingReset;

impl Drop for ReportingReset {
    fn drop(&mut self) {
        let _ = REPORTING_PANIC.try_with(|reporting| reporting.set(false));
    }
}

fn panic_message<'a>(payload: &'a (dyn std::any::Any + Send)) -> &'a str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic>")
}

/// Records a startup failure next to the crash reports. Panics were already
/// durable; an ordinary `Err` from the startup sequence was not, even though
/// it is the failure a desktop launch hits most often.
pub fn report_startup_failure(error: &str) {
    append_crash_record(&format!(
        "--- {} startup failure pid={} at_unix_ms={} ---\nmessage: {}\n",
        DIAGNOSTIC_PRODUCT.get().copied().unwrap_or("cagents"),
        process::id(),
        unix_millis(),
        error.replace('\n', " "),
    ));
}

/// Installs the process-wide tracing subscriber. Output goes to a bounded
/// rolling file under the data directory, and additionally to stderr when a
/// terminal is attached. `RUST_LOG` wins; `default_filter` is the fallback.
/// Idempotent: later calls do nothing.
pub fn install_tracing(product: &'static str, default_filter: &str) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = DIAGNOSTIC_PRODUCT.set(product);
    install_global_subscriber(product, default_filter);
}

#[cfg(any(feature = "host", feature = "ui"))]
fn install_global_subscriber(product: &'static str, default_filter: &str) {
    use std::io::IsTerminal;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let sink = DiagnosticSink {
        file: TRACE_LOG
            .get_or_init(|| open_diagnostic(product, ".log", MAX_LOG_BYTES))
            .as_ref(),
        stderr: io::stderr().is_terminal(),
    };
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        // Upstream network/keyring debug events contain cookie values and raw
        // headers. This mandatory filter intersects RUST_LOG; no user-provided
        // target or span directive can enable those credential-bearing events.
        .with(tracing_subscriber::filter::filter_fn(|metadata| {
            !cfg!(feature = "integrated-browser")
                || !sensitive_browser_diagnostic(metadata.target(), metadata.level())
        }))
        // The same formatted bytes reach the file and the terminal, so escape
        // sequences would end up in the artifact the user attaches.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(sink),
        )
        .try_init();
}

#[cfg(any(feature = "host", feature = "ui"))]
fn sensitive_browser_diagnostic(target: &str, level: &tracing::Level) -> bool {
    *level > tracing::Level::INFO
        && ["net", "hyper", "hyper_util", "h2", "zbus", "secret_service"]
            .iter()
            .any(|prefix| {
                target == *prefix
                    || target
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with("::"))
            })
}

// `tracing-subscriber` is linked only into the UI and daemon builds. The entry
// point stays callable in every lane so no caller needs feature-dependent code.
#[cfg(not(any(feature = "host", feature = "ui")))]
fn install_global_subscriber(_product: &'static str, _default_filter: &str) {}

#[cfg(any(feature = "host", feature = "ui"))]
struct DiagnosticSink {
    file: Option<&'static Mutex<RotatingFile>>,
    stderr: bool,
}

#[cfg(any(feature = "host", feature = "ui"))]
struct DiagnosticWriter<'a>(&'a DiagnosticSink);

#[cfg(any(feature = "host", feature = "ui"))]
impl Write for DiagnosticWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        // The fmt layer hands over one fully formatted event per call, so
        // taking the lock once here is what keeps events from interleaving.
        if let Some(file) = self.0.file {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = file.write_all(buf);
        }
        if self.0.stderr {
            let _ = io::stderr().write_all(buf);
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.0.file {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = file.flush();
        }
        Ok(())
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DiagnosticSink {
    type Writer = DiagnosticWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        DiagnosticWriter(self)
    }
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
    lock: Option<OwnedLock>,
    runtime: PathBuf,
    _legacy_fence: OwnedLock,
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

pub fn activate_destination(
    destination: &crate::notifications::Destination,
    token: Option<&str>,
) -> io::Result<bool> {
    destination.validate().map_err(io::Error::other)?;
    let runtime = runtime_directory()?;
    activate_socket_with_token(&runtime, destination_socket_name(destination), token)
}

// The editor's daemon is a private matched companion. The other shell may be
// installed separately, so resolve only these fixed application names, never a
// path carried by an alert. Relative PATH entries cannot redirect activation.
fn notification_executable(name: &str) -> io::Result<PathBuf> {
    let current = env::current_exe()?;
    let sibling = current.with_file_name(name);
    let search = env::var_os("PATH").unwrap_or_default();
    resolve_notification_executable(&sibling, env::split_paths(&search).map(|p| p.join(name)))
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
    let mut command = process::Command::new(notification_executable("ceditor")?);
    command.args(["--notification-window", &destination.window]);
    command.env_remove("XDG_ACTIVATION_TOKEN");
    if let Some(token) = token.filter(|token| valid_activation_token(token)) {
        command.env("XDG_ACTIVATION_TOKEN", token);
    }
    crate::control::spawn_ui_process(&mut command)
}

pub fn launch_notification_window(slot: &str, token: Option<&str>) -> io::Result<()> {
    validate_window_slot(slot)?;
    let mut command = process::Command::new(notification_executable("cagents-ui")?);
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

/// This instance's settings file, which the UI owns and the CLI only reads.
pub fn settings_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn browser_credentials_are_excluded_even_with_explicit_debug_targets() {
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
            tracing::debug!(target: "net::http_loader", "fixture cookie headers must be filtered");
            tracing::trace!(target: "zbus::message", "fixture secret-store reply must be filtered");
            tracing::warn!(target: "net::http_loader", "ordinary connection failure");
            tracing::debug!(target: "cagents::browser", "ordinary app diagnostic");
        });
        assert_eq!(seen.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn diagnostic_logs_stay_bounded_and_private() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested").join("product-crash.log");
        let mut log = RotatingFile::open(path.clone(), 64).unwrap();
        for index in 0..64 {
            writeln!(log, "record {index} ------------------------------").unwrap();
        }
        log.flush().unwrap();

        let mut retained = path.clone().into_os_string();
        retained.push(".1");
        // A panic loop must cost two bounded generations, not one file that
        // grows until the data directory is full.
        assert!(fs::metadata(&path).unwrap().len() <= 128);
        assert!(fs::metadata(PathBuf::from(retained)).unwrap().len() <= 128);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );

        let target = directory.path().join("target");
        File::create(&target).unwrap();
        let link = directory.path().join("link.log");
        symlink(&target, &link).unwrap();
        assert!(RotatingFile::open(link, 64).is_err());
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
    fn activation_is_not_acknowledged_once_the_window_stops_listening() {
        let runtime = tempdir().unwrap();
        let (sender, receiver) = async_channel::unbounded::<UiActivation>();
        let _listener = UiActivationListener::bind_in(runtime.path(), sender).unwrap();
        drop(receiver);

        // "ok" makes `cagents ui` skip spawning a window, so it must never be
        // written for an activation no window can act on.
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
