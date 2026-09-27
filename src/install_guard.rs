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
    thread,
    time::{Duration, Instant},
};

use crate::instance::UiUpdateGuard;

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
        if Instant::now() >= deadline {
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
                io::ErrorKind::TimedOut,
                "installer exceeded its deadline; inspect its current installation before retrying",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}
