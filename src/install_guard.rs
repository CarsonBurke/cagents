//! Own window preparation for a synchronous external installation. Installation
//! and rollback remain the shell installer's transaction; this layer only owns
//! its launch fence and reopening the exact slots that were saved.
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::instance::UiUpdateGuard;

mod companions;
pub(crate) use companions::{preflight_editor_companions, trusted_executable_owner};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceInspectionRequest {
    pub(crate) protocol: u16,
    pub(crate) proof: Option<crate::model::HostSessionProof>,
    pub(crate) require_resumable: bool,
}

pub(crate) fn preflight_source_panes(
    guard: &UiUpdateGuard,
    proof: Option<crate::model::HostSessionProof>,
    require_resumable: bool,
) -> io::Result<()> {
    crate::update::inspect_source_panes(guard, proof, require_resumable).map_err(io::Error::other)
}

const RESULT_FD_ENV: &str = "CAGENTS_INSTALL_RESULT_FD";
const OWNER_CLI_ENV: &str = "CAGENTS_INSTALL_OWNER_CLI";
const DEFERRED_RECEIPT: &[u8] = b"{\"version\":1,\"preserve_previous_ui\":true}\n";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    preserve_previous_ui: bool,
}

/// The inner installer publishes routing before switching `current`, while
/// the coordinator still holds the exact old UI executable path. This is not
/// a success receipt: shell status independently decides transaction success.
pub(crate) fn record_deferred(stage: Option<(&Path, &Path)>) -> io::Result<()> {
    if let Some((prefix, version_root)) = stage {
        crate::instance::validate_stage_guard()?;
        record_staged_owner(prefix, version_root)?;
    } else {
        crate::instance::validate_install_guard()?;
    }
    let fd = std::env::var(RESULT_FD_ENV).ok().and_then(|value| value.parse::<i32>().ok())
        .filter(|fd| *fd >= 3).ok_or_else(|| io::Error::other("this update requires an external installer coordinator to defer daemon replacement"))?;
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || flags & libc::O_ACCMODE != libc::O_WRONLY {
        return Err(io::Error::other("installer result pipe is unavailable"));
    }
    // SAFETY: F_GETFL verified the open descriptor; duplicate without owning
    // or closing the inherited descriptor used by the installer.
    let mut file = File::from(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?);
    let metadata = file.metadata()?;
    if !metadata.file_type().is_fifo()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("installer result pipe is not private"));
    }
    file.write_all(DEFERRED_RECEIPT)
}

struct ResultPipe {
    reader: File,
    writer: File,
}

impl ResultPipe {
    fn new() -> io::Result<Self> {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 initializes both owned descriptors atomically.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { File::from_raw_fd(fds[1]) };
        if matches!(writer.as_raw_fd(), 197 | 198) {
            // These slots belong to UI/worker leases. Keep our descriptor
            // outside them before the child installs its launch-fence lease.
            let fd = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            writer = unsafe { File::from_raw_fd(fd) };
        }
        Ok(Self { reader, writer })
    }

    fn deferred(&mut self) -> io::Result<bool> {
        let mut bytes = [0; 256];
        let length = match self.reader.read(&mut bytes) {
            Ok(length) => length,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        };
        if length == 0 {
            return Ok(false);
        }
        let receipt: Receipt =
            serde_json::from_slice(&bytes[..length]).map_err(io::Error::other)?;
        if receipt.version != 1 || !receipt.preserve_previous_ui {
            return Err(io::Error::other(
                "installer returned an unsupported routing receipt",
            ));
        }
        Ok(true)
    }
}

/// Installer assets and authority are scoped to the update transaction. A
/// long-lived process must not pass stale paths or an unbacked FD marker to
/// hosted terminals and their future installer invocations.
pub(crate) fn sanitize_child_environment(command: &mut Command) {
    for name in [
        crate::instance::INSTALL_GUARD_ENV,
        crate::instance::DAEMON_FENCE_ENV,
        RESULT_FD_ENV,
        OWNER_CLI_ENV,
        "CAGENTS_ARCHIVE",
        "CAGENTS_CHECKSUMS",
        "CAGENTS_REPOSITORY",
    ] {
        command.env_remove(name);
    }
}

static CANCELLED: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

struct Signals(Vec<(libc::c_int, libc::sigaction)>);

impl Signals {
    fn install() -> io::Result<Self> {
        CANCELLED.store(false, Ordering::Relaxed);
        let mut signals = Self(Vec::new());
        for (signal, handler) in [
            (libc::SIGINT, cancel as *const () as usize),
            (libc::SIGTERM, cancel as *const () as usize),
            // Ignored SIGCHLD survives exec. Keep the unreaped child as our
            // process-group identity until bounded cleanup has completed.
            (libc::SIGCHLD, libc::SIG_DFL),
        ] {
            // SAFETY: zero initialization plus an empty mask forms a valid
            // sigaction; handlers only store a lock-free atomic flag.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                let mut previous = std::mem::zeroed();
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_sigaction = handler;
                if libc::sigaction(signal, &action, &mut previous) == -1 {
                    return Err(io::Error::last_os_error());
                }
                signals.0.push((signal, previous));
            }
        }
        Ok(signals)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for (signal, previous) in self.0.iter().rev() {
            // SAFETY: restore the exact dispositions saved on installation.
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
    }
}

pub(crate) fn run(
    prefix: &Path,
    installer: &Path,
    arguments: &[OsString],
    stage_only: bool,
) -> io::Result<()> {
    if !prefix.is_absolute() {
        return Err(io::Error::other("installation prefix must be absolute"));
    }
    let script = File::open(installer)?;
    if !script.metadata()?.is_file() {
        return Err(io::Error::other(
            "installer must be a readable regular file",
        ));
    }
    let _signals = Signals::install()?;
    let executable = prefix.join("current/bin/cagents-ui");
    let mut guard = UiUpdateGuard::acquire()?;
    let owner_bin = crate::daemon::running_executable()
        .map_err(io::Error::other)?
        .map(|executable| running_owner_bin(prefix, &executable))
        .transpose()?;
    let previous_executable = owner_bin
        .as_ref()
        .map(|bin| bin.join("cagents-ui"))
        .or_else(|| executable.canonicalize().ok());
    if let Some(previous) = &previous_executable {
        guard.set_fallback_executable(previous.clone());
    } else if guard.has_running_windows()? {
        return Err(io::Error::other(
            "close the running application before installing into a new prefix; no previous UI exists there to restore on failure",
        ));
    }
    let slots = if stage_only {
        Vec::new()
    } else {
        if arguments
            .iter()
            .any(|argument| argument == "--restart-daemon")
        {
            preflight_editor_companions(None)?;
        }
        guard.prepare_and_close_windows_with_preflight(Duration::from_secs(45), |guard| {
            preflight_source_panes(
                guard,
                None,
                arguments
                    .iter()
                    .any(|argument| argument == "--restart-daemon"),
            )
        })?
    };
    let fd = script.as_raw_fd();
    let mut result = ResultPipe::new()?;
    let result_fd = result.writer.as_raw_fd();
    let mut command = Command::new("sh");
    command
        .arg(format!("/proc/self/fd/{fd}"))
        .args(arguments)
        .env(RESULT_FD_ENV, result_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    if let Some(bin) = &owner_bin {
        command.env(
            OWNER_CLI_ENV,
            bin.join(crate::companion::Product::Cli.file_name()),
        );
    } else {
        command.env_remove(OWNER_CLI_ENV);
    }
    guard.inherit_for_install(&mut command)?;
    // SAFETY: script remains open until the subprocess is reaped.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(result_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let installed = execute(&mut command);
    let deferred = result.deferred()?;
    if stage_only {
        // No window has been prepared: releasing this guard only releases the
        // launch fence, on success, failure and cancellation alike.
        drop(guard);
        return installed.and_then(|()| {
            if deferred {
                Ok(())
            } else {
                Err(io::Error::other(
                    "staging installer did not record deferred ownership",
                ))
            }
        });
    }
    let reopen = if deferred && !slots.is_empty() {
        previous_executable.as_ref().ok_or_else(|| {
            io::Error::other("deferred installation has no previous UI executable")
        })?
    } else {
        &executable
    };
    let reopened = guard.relaunch_windows(&slots, reopen);
    match (installed, reopened) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(io::Error::other(format!("{error}; saved windows reopened"))),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "installation finished, but reopening windows failed: {error}"
        ))),
        (Err(error), Err(reopen)) => Err(io::Error::other(format!(
            "{error}; reopening the current installation also failed: {reopen}"
        ))),
    }
}

const STAGED_OWNER: &str = ".deferred-owner.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedOwner {
    version: u32,
    owner_version: Option<String>,
    #[serde(default)]
    daemon_compatible: bool,
    #[serde(default)]
    candidate_protocol: u16,
}

fn compatible_owner() -> io::Result<bool> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let Ok(client) = crate::control::ControlClient::connect(&socket) else {
        return Ok(false);
    };
    crate::daemon::replacement_owner(&socket, client.proof()).map_err(io::Error::other)?;
    Ok(true)
}

fn record_staged_owner(prefix: &Path, version_root: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let layout =
        crate::update::locate(&version_root.join("bin/cagents")).map_err(io::Error::other)?;
    if layout.prefix != prefix.canonicalize()?
        || layout.version_root != version_root.canonicalize()?
    {
        return Err(io::Error::other(
            "staged candidate belongs to another installation",
        ));
    }
    let owner = crate::daemon::running_executable()
        .map_err(io::Error::other)?
        .map(|exe| running_owner_bin(prefix, &exe))
        .transpose()?;
    let supplied = std::env::var_os(OWNER_CLI_ENV).map(std::path::PathBuf::from);
    let expected = owner.as_ref().map(|bin| bin.join("cagents"));
    if supplied != expected {
        return Err(io::Error::other(
            "daemon ownership changed while staging; installation was not changed",
        ));
    }
    // A compatible candidate can attach to the preserved daemon, including
    // after an older installed updater completes its reviewed UI handoff.
    // Authenticate both the handshake proof and the kernel socket owner.
    let daemon_compatible = owner.is_some() && compatible_owner()?;
    let after = crate::daemon::running_executable()
        .map_err(io::Error::other)?
        .map(|exe| running_owner_bin(prefix, &exe))
        .transpose()?;
    if after != owner {
        return Err(io::Error::other(
            "daemon ownership changed during compatibility verification; installation was not changed",
        ));
    }
    let windows = crate::instance::verified_staged_windows()?;
    let window_owner = window_owner_bin(prefix, &windows)?;
    if owner
        .as_ref()
        .zip(window_owner.as_ref())
        .is_some_and(|(daemon, ui)| daemon != ui)
    {
        return Err(io::Error::other(
            "daemon does not match the live window while staging",
        ));
    }
    let after_windows = crate::daemon::running_executable()
        .map_err(io::Error::other)?
        .map(|exe| running_owner_bin(prefix, &exe))
        .transpose()?;
    if after_windows != owner {
        return Err(io::Error::other(
            "daemon ownership changed while verifying live windows",
        ));
    }
    let owner = owner.or(window_owner);
    let owner_version = owner
        .as_ref()
        .map(|bin| {
            bin.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .ok_or_else(|| io::Error::other("daemon version path is not UTF-8"))
        })
        .transpose()?
        .map(str::to_owned);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(version_root.join(STAGED_OWNER))?;
    serde_json::to_writer(
        &mut file,
        &StagedOwner {
            version: 1,
            owner_version,
            daemon_compatible,
            candidate_protocol: crate::terminal_ipc::PROTOCOL_VERSION,
        },
    )
    .map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// A marker only selects a complete same-prefix tree while the kernel still
/// identifies that exact daemon or matching live windows as the runtime owner.
/// Recovery can restart only their matched daemon under the launch fence; the
/// marker conveys no authority to stop a daemon, close windows or replace files.
pub(crate) fn staged_owner_bin(
    executable: &Path,
    recover: bool,
) -> io::Result<Option<std::path::PathBuf>> {
    use std::os::unix::fs::OpenOptionsExt;
    // Sealed update probes execute from anonymous FDs. Their kernel exe path
    // has no filesystem entry to canonicalize and carries no install routing.
    // Only an installed version shape can have our private ownership marker;
    // locate() authenticates that tree after a marker has actually been found.
    let Some(bin) = executable
        .parent()
        .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
    else {
        return Ok(None);
    };
    let Some(version_root) = bin.parent().filter(|root| {
        root.parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "versions")
    }) else {
        return Ok(None);
    };
    let marker = version_root.join(STAGED_OWNER);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 1024
    {
        return Err(io::Error::other(
            "invalid staged installation ownership marker",
        ));
    }
    let layout = crate::update::locate(executable).map_err(io::Error::other)?;
    let owner: StagedOwner = serde_json::from_reader(&mut file).map_err(io::Error::other)?;
    if owner.version != 1 {
        return Err(io::Error::other(
            "unsupported staged installation ownership marker",
        ));
    }
    if owner.owner_version.as_ref().is_some_and(|version| {
        version.is_empty()
            || !matches!(
                (
                    Path::new(version).components().next(),
                    Path::new(version).components().nth(1)
                ),
                (Some(std::path::Component::Normal(_)), None)
            )
    }) {
        return Err(io::Error::other("invalid staged daemon version"));
    }
    if owner.daemon_compatible && owner.candidate_protocol != crate::terminal_ipc::PROTOCOL_VERSION
    {
        return Err(io::Error::other(
            "staged compatibility belongs to another candidate protocol",
        ));
    }
    if owner.daemon_compatible {
        return Ok(None);
    }
    // Shared with every UI launch, exclusive against reviewed update/close.
    // Keep it through synchronous daemon recovery and ownership revalidation.
    let launch = crate::instance::LaunchFence::recovery()?;
    let running = crate::daemon::running_executable().map_err(io::Error::other)?;
    let (windows, _legacy) = if std::env::var_os(crate::instance::INSTALL_GUARD_ENV).is_some() {
        // The reviewed updater also holds the shared legacy fence after all
        // windows close. Authenticate its exact exclusive inherited guard;
        // probing legacy exclusivity here would mistake our coordinator for UI.
        (crate::instance::verified_staged_windows()?, None)
    } else if running.as_deref() == Some(layout.version_root.join("bin/cagentsd").as_path()) {
        let (windows, legacy) = crate::instance::verified_windows_for_daemon(
            &layout.version_root.join("bin/cagentsd"),
        )?;
        (windows, Some(legacy))
    } else {
        (crate::instance::verified_running_windows()?, None)
    };
    let window_bin = window_owner_bin(&layout.prefix, &windows)?;
    if running.as_ref() == Some(&layout.version_root.join("bin/cagentsd")) && windows.is_empty() {
        return Ok(None);
    }
    // A marker from an earlier installation is obsolete once its candidate is
    // itself the live UI owner. This also prevents recursive recovery through
    // an older CLI that once had its own deferred update.
    if window_bin.as_ref() == Some(&layout.version_root.join("bin")) {
        if running
            .as_ref()
            .is_some_and(|exe| exe != &layout.version_root.join("bin/cagentsd"))
        {
            return Err(io::Error::other("daemon does not match the live window"));
        }
        return Ok(None);
    }
    if windows.is_empty() && running.is_none() {
        return Ok(None);
    }
    let Some(version) = owner.owner_version else {
        if !windows.is_empty() || running.is_some() {
            return Err(io::Error::other(
                "staged installation has no authenticated previous version; close it through a reviewed handoff",
            ));
        }
        return Ok(None);
    };
    let expected = layout.prefix.join("versions").join(version).join("bin");
    crate::update::locate(&expected.join("cagents")).map_err(io::Error::other)?;
    for window in &windows {
        if window.executable != expected.join("cagents-ui") {
            return Err(io::Error::other(
                "live window belongs to another installation version; finish its reviewed handoff",
            ));
        }
    }
    if let Some(executable) = running {
        let bin = running_owner_bin(&layout.prefix, &executable)?;
        if bin == layout.version_root.join("bin") && windows.is_empty() {
            return Ok(None);
        }
        if bin != expected {
            return Err(io::Error::other(
                "daemon does not match the deferred runtime owner",
            ));
        }
        return Ok((bin != layout.version_root.join("bin")).then_some(bin));
    }
    if windows.is_empty() {
        return Ok(None);
    }
    // Starting through the old public CLI uses its own protocol and recovery
    // contract. Never ask the incompatible candidate to restore an old host.
    if !recover {
        return Ok(Some(expected));
    }
    restore_window_daemon(&expected, &launch)?;
    if crate::daemon::running_executable().map_err(io::Error::other)?
        != Some(expected.join("cagentsd"))
        || windows
            .iter()
            .any(|window| !window.still_owns_lease().unwrap_or(false))
    {
        return Err(io::Error::other(
            "window or daemon ownership changed during recovery",
        ));
    }
    Ok(Some(expected))
}

fn window_owner_bin(
    prefix: &Path,
    windows: &[crate::instance::VerifiedWindow],
) -> io::Result<Option<std::path::PathBuf>> {
    let prefix = prefix.canonicalize()?;
    let mut owner = None;
    for window in windows {
        if window
            .executable
            .file_name()
            .is_none_or(|name| name != "cagents-ui")
            || !window.still_owns_lease()?
        {
            return Err(io::Error::other(
                "live window identity is not an authenticated UI",
            ));
        }
        let layout = crate::update::locate(&window.executable).map_err(io::Error::other)?;
        let bin = layout.version_root.join("bin");
        if layout.prefix != prefix || owner.as_ref().is_some_and(|owner| owner != &bin) {
            return Err(io::Error::other(
                "live windows belong to different installation versions",
            ));
        }
        owner = Some(bin);
    }
    Ok(owner)
}

fn restore_window_daemon(bin: &Path, launch: &crate::instance::LaunchFence) -> io::Result<()> {
    let mut command = Command::new(bin.join(crate::companion::Product::Cli.file_name()));
    command
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    sanitize_child_environment(&mut command);
    launch.inherit(&mut command)?;
    wait_daemon_start(&mut command, daemon_start_timeout()?).map_err(|failure| failure.error)
}

/// Capture before spawning: the successor may consume its checkpoint while
/// restoring, but this outer caller must retain the same startup allowance.
pub(crate) fn daemon_start_timeout() -> io::Result<Duration> {
    crate::upgrade::handoff::startup_timeout()
        .map(|timeout| timeout + Duration::from_secs(30))
        .map_err(io::Error::other)
}

/// A startup error distinguishes a proven spawn failure from a command that
/// actually acquired custody. Readiness is never a substitute for its wait.
#[derive(Debug)]
pub(crate) struct DaemonStartError {
    pub(crate) error: io::Error,
    pub(crate) spawned: bool,
}

/// Startup owns restoration, not a disposable download. Crossing its budget
/// remains an error, but this coordinator must stay alive until the ORIGINAL
/// child has actually been waited. A pathological hang can delay it indefinitely;
/// there is no durable background owner after this process exits.
pub(crate) fn wait_daemon_start(
    command: &mut Command,
    timeout: Duration,
) -> Result<(), DaemonStartError> {
    wait_daemon_start_with_progress(command, timeout, || Ok(()))
}

/// Reports once when the startup budget is crossed with the original child
/// still running. Reporting failure cannot unwind child or caller-fence custody.
pub(crate) fn wait_daemon_start_with_progress(
    command: &mut Command,
    timeout: Duration,
    waiting: impl FnOnce() -> io::Result<()>,
) -> Result<(), DaemonStartError> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| DaemonStartError {
            error,
            spawned: false,
        })?;
    let budget_error = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "The matched terminal service exceeded its startup budget; its original command was retained until its actual wait completed. Check cagents daemon status before retrying",
        )
    };
    loop {
        let (error, overdue) = match child.try_wait() {
            Ok(Some(status)) => {
                return if Instant::now() >= deadline {
                    Err(DaemonStartError {
                        error: budget_error(),
                        spawned: true,
                    })
                } else if status.success() {
                    Ok(())
                } else {
                    Err(DaemonStartError {
                        error: io::Error::other("The matched daemon startup command failed"),
                        spawned: true,
                    })
                };
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Ok(None) => (budget_error(), true),
            Err(error) => (error, false),
        };
        tracing::warn!(
            pid = child.id(),
            "Daemon startup result was not confirmed within its budget; retaining the original wait owner"
        );
        // A failed diagnostic (including an unwinding reporter) must not drop
        // the original child or return through the caller's held fences.
        let progress_result = if overdue {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                waiting().map_err(|failure| failure.to_string())
            }))
        } else {
            // A failed try_wait is unconfirmed ownership, not evidence that
            // this allowance elapsed. Preserve its original error and wait.
            Ok(Ok(()))
        };
        let progress_failure = match &progress_result {
            Ok(Ok(())) => None,
            Ok(Err(failure)) => Some(failure.clone()),
            Err(_) => Some("the startup progress reporter panicked".to_owned()),
        };
        let error = match progress_failure {
            Some(failure) => io::Error::new(
                error.kind(),
                format!("{error}; Could not record the waiting status: {failure}"),
            ),
            None => error,
        };
        // Neither a timeout nor an unconfirmed wait permits dropping the
        // original Child. Retry actual wait errors without fabricating success.
        loop {
            match child.wait() {
                Ok(status) => {
                    tracing::debug!(
                        pid = child.id(),
                        success = status.success(),
                        "Original daemon startup command was naturally waited"
                    );
                    if let Err(payload) = progress_result {
                        // An arbitrary panic payload may itself panic on Drop.
                        // The short-lived coordinator must still return through
                        // its candidate-recovery routing after the exact reap.
                        std::mem::forget(payload);
                    }
                    return Err(DaemonStartError {
                        error,
                        spawned: true,
                    });
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

fn running_owner_bin(prefix: &Path, executable: &Path) -> io::Result<std::path::PathBuf> {
    let prefix = prefix.canonicalize()?;
    let executable = executable.canonicalize()?;
    let bin = executable
        .parent()
        .ok_or_else(|| io::Error::other("running daemon has no installation directory"))?;
    let version = bin
        .parent()
        .ok_or_else(|| io::Error::other("running daemon has no version directory"))?;
    if executable.file_name().is_none_or(|name| name != "cagentsd")
        || bin.file_name().is_none_or(|name| name != "bin")
        || version.parent() != Some(prefix.join("versions").as_path())
        || !bin.join("cagents").is_file()
        || !bin.join("cagents-ui").is_file()
    {
        return Err(io::Error::other(
            "the running daemon belongs to another installation; update from its own installation",
        ));
    }
    Ok(bin.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_timeout_retains_the_original_waiter_and_does_not_reap_another_child() {
        use std::{fs, path::PathBuf, process::Child};
        const CHILD_ROOT: &str = "CAGENTS_STARTUP_WAIT_FIXTURE_ROOT";
        fn command(root: &Path, label: &str) -> Command {
            let mut command = Command::new("/usr/bin/python3");
            command.args(["-u", "-c", "import os,pathlib,sys,time\nroot=pathlib.Path(sys.argv[1]); label=sys.argv[2]\nready=root/(label+'-ready.tmp'); ready.write_text(str(os.getpid())); ready.replace(root/(label+'-ready'))\ndeadline=time.monotonic()+10\nwhile not (root/(label+'-release')).exists():\n if time.monotonic()>=deadline: sys.exit(7)\n time.sleep(.005)\n(root/(label+'-finished')).write_text('natural exit')"]);
            command.arg(root).arg(label).current_dir(root);
            command
        }
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            // Exercise the production helper in a short-lived coordinator. Its
            // final error would normally return directly from process main.
            let failure =
                wait_daemon_start(&mut command(&root, "startup"), Duration::from_millis(10))
                    .unwrap_err();
            assert!(failure.spawned);
            assert_eq!(failure.error.kind(), io::ErrorKind::TimedOut);
            let pid: i32 = fs::read_to_string(root.join("startup-ready"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "helper returned before exact child reap"
            );
            fs::write(
                root.join("caller-final-error"),
                b"TimedOut after actual reap",
            )
            .unwrap();
            std::process::exit(1);
        }
        struct OwnedChild {
            child: Child,
            release: PathBuf,
        }
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = fs::write(&self.release, b"release");
                let _ = self.child.wait();
            }
        }
        let selector = thread::current()
            .name()
            .expect("libtest selector")
            .to_owned();
        let root = tempfile::tempdir().unwrap();
        let mut neighbor = OwnedChild {
            child: command(root.path(), "neighbor").spawn().unwrap(),
            release: root.path().join("neighbor-release"),
        };
        let mut caller = OwnedChild {
            child: Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(CHILD_ROOT, root.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
            release: root.path().join("startup-release"),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.path().join("startup-ready").exists()
            || !root.path().join("neighbor-ready").exists()
        {
            assert!(
                Instant::now() < deadline,
                "actual dummy children never became ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let pid: i32 = fs::read_to_string(root.path().join("startup-ready"))
            .unwrap()
            .parse()
            .unwrap();
        let process = crate::daemon::holder_registry::Process::read(pid).unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(raw >= 0);
        let pinned = unsafe { File::from_raw_fd(raw) };
        // The real caller must remain alive beyond the budget instead of
        // returning its error and abandoning the original startup process.
        thread::sleep(Duration::from_millis(75));
        assert!(caller.child.try_wait().unwrap().is_none());
        assert!(process.is_current());
        let mut readiness = libc::pollfd {
            fd: pinned.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 0);
        assert!(neighbor.child.try_wait().unwrap().is_none());
        assert!(!root.path().join("caller-final-error").exists());
        fs::write(&caller.release, b"natural exit").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = caller.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "coordinator did not settle its original child"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.code(), Some(1));
        assert_eq!(
            fs::read(root.path().join("caller-final-error")).unwrap(),
            b"TimedOut after actual reap"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("startup-finished")).unwrap(),
            "natural exit"
        );
        assert!(!process.is_current());
        assert_eq!(unsafe { libc::poll(&mut readiness, 1, 0) }, 1);
        assert_ne!(readiness.revents & libc::POLLIN, 0);
        assert!(
            neighbor.child.try_wait().unwrap().is_none(),
            "startup wait stole or stopped another child"
        );
        fs::write(&neighbor.release, b"natural exit").unwrap();
        assert!(neighbor.child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(root.path().join("neighbor-finished")).unwrap(),
            "natural exit"
        );
    }

    #[test]
    fn daemon_start_reports_actual_failure_and_spawn_failure_without_success() {
        wait_daemon_start(&mut Command::new("/bin/true"), Duration::from_secs(5)).unwrap();
        let elapsed =
            wait_daemon_start(&mut Command::new("/bin/true"), Duration::ZERO).unwrap_err();
        assert!(elapsed.spawned);
        assert_eq!(elapsed.error.kind(), io::ErrorKind::TimedOut);
        let error =
            wait_daemon_start(&mut Command::new("/bin/false"), Duration::from_secs(5)).unwrap_err();
        assert!(error.spawned);
        assert_eq!(error.error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.error.to_string(),
            "The matched daemon startup command failed"
        );
        let root = tempfile::tempdir().unwrap();
        let error = wait_daemon_start(
            &mut Command::new(root.path().join("missing-start-command")),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(!error.spawned);
        assert_eq!(error.error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn repeated_staging_keeps_the_running_version_even_when_current_is_newer() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("versions/old/bin");
        let staged = root.path().join("versions/staged/bin");
        for bin in [&old, &staged] {
            std::fs::create_dir_all(bin).unwrap();
            for executable in ["cagents", "cagentsd", "cagents-ui"] {
                std::fs::write(bin.join(executable), "fixture").unwrap();
            }
        }
        std::os::unix::fs::symlink("versions/staged", root.path().join("current")).unwrap();
        assert_eq!(
            running_owner_bin(root.path(), &old.join("cagentsd")).unwrap(),
            old
        );
        let unrelated = tempfile::tempdir().unwrap();
        assert!(running_owner_bin(unrelated.path(), &staged.join("cagentsd")).is_err());
    }
}

fn execute(command: &mut Command) -> io::Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "installation cancelled before it started",
        ));
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!("installer failed ({status})")))
            };
        }
        let cancelled = CANCELLED.load(Ordering::Relaxed);
        if cancelled || Instant::now() >= deadline {
            // The unreaped child owns this process group. Give the installer's
            // EXIT/TERM trap time to restore current before the hard bound.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            let cleanup = Instant::now() + Duration::from_secs(30);
            while child.try_wait()?.is_none() {
                if Instant::now() >= cleanup {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            return Err(io::Error::new(
                if cancelled {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                },
                if cancelled {
                    "installation cancelled; inspect its current installation before retrying"
                } else {
                    "installer exceeded its deadline; inspect its current installation before retrying"
                },
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}
