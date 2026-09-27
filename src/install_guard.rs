//! Own window preparation for a synchronous external installation. Installation
//! and rollback remain the shell installer's transaction; this layer only owns
//! its launch fence and reopening the exact slots that were saved.
use std::{
    ffi::OsString,
    fs::File,
    io,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::instance::UiUpdateGuard;

/// Installer assets and authority are scoped to the update transaction. A
/// long-lived process must not pass stale paths or an unbacked FD marker to
/// hosted terminals and their future installer invocations.
pub(crate) fn sanitize_child_environment(command: &mut Command) {
    for name in [
        crate::instance::INSTALL_GUARD_ENV,
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

pub(crate) fn run(prefix: &Path, installer: &Path, arguments: &[OsString]) -> io::Result<()> {
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
    if executable.is_file() {
        guard.set_fallback_executable(executable.clone());
    } else if guard.has_running_windows()? {
        return Err(io::Error::other(
            "close the running application before installing into a new prefix; no previous UI exists there to restore on failure",
        ));
    }
    let slots = guard.prepare_and_close_windows(Duration::from_secs(45))?;
    let fd = script.as_raw_fd();
    let mut command = Command::new("sh");
    command
        .arg(format!("/proc/self/fd/{fd}"))
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0);
    guard.inherit_for_install(&mut command)?;
    // SAFETY: script remains open until the subprocess is reaped.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let installed = execute(&mut command);
    let reopened = guard.relaunch_windows(&slots, &executable);
    match (installed, reopened) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(io::Error::other(format!(
            "{error}; saved windows reopened from the current installation"
        ))),
        (Ok(()), Err(error)) => Err(io::Error::other(format!(
            "installation finished, but reopening windows failed: {error}"
        ))),
        (Err(error), Err(reopen)) => Err(io::Error::other(format!(
            "{error}; reopening the current installation also failed: {reopen}"
        ))),
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
