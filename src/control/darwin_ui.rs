//! Launch an installed window through its stable LaunchServices identity.

use super::{Command, Duration, Path, Stdio, io, read_ui_launch_handshake};
use crate::pinned_process::PinnedProcess;
use std::{
    ffi::CString,
    fs::OpenOptions,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt},
};

const OPEN_DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn launch(
    command: &Command,
    bundle: &Path,
    timeout: Duration,
) -> io::Result<Option<String>> {
    // LaunchServices opens the named stdout when it dispatches the app. The
    // read-only descriptor sees EOF if that app exits before acknowledging.
    let directory = tempfile::tempdir()?;
    let readiness = directory.path().join("startup");
    let name = CString::new(readiness.as_os_str().as_bytes())?;
    // SAFETY: name is a NUL-terminated path inside our fresh private directory.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut response = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&readiness)?;
    let mut open = Command::new("/usr/bin/open");
    open.args(["-n", "-a"])
        .arg(bundle)
        .arg("--stdout")
        .arg(&readiness)
        .arg("--args")
        .args(command.get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // open passes its environment to the application. Preserve the caller's
    // window and runtime configuration and the descriptor-authority removals.
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            open.env(key, value);
        } else {
            open.env_remove(key);
        }
    }
    // SAFETY: The hook seals inherited descriptors before exec.
    unsafe {
        open.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    dispatch(&mut open)?;
    read_ui_launch_handshake(&mut response, timeout)
}

fn dispatch(open: &mut Command) -> io::Result<()> {
    let mut child = open.spawn()?;
    let exited = match PinnedProcess::open(child.id()) {
        Ok(process) => process.wait_for_exit(OPEN_DISPATCH_TIMEOUT),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    };
    if !matches!(exited, Ok(true)) {
        let _ = child.kill();
        let _ = child.wait();
        exited?;
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "macOS did not open Cagents.app in time",
        ));
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "macOS could not open Cagents.app ({status})"
        )));
    }
    Ok(())
}
