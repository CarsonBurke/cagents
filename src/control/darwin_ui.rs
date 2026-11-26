//! Launch an installed window through its stable LaunchServices identity.

use super::{
    BorrowedFd, Command, Duration, Instant, Path, Stdio, io, read_ui_launch_handshake_with,
};
use crate::pinned_process::PinnedProcess;
use std::{
    ffi::CString,
    fs::OpenOptions,
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt},
    },
};

pub(super) fn launch(
    command: &Command,
    bundle: &Path,
    timeout: Duration,
) -> io::Result<Option<String>> {
    // Keep the pipe open until dispatch connects. LaunchServices can itself
    // retain a writer after the app exits, so use open -W for the exit event.
    let directory = tempfile::tempdir()?;
    let readiness = directory.path().join("startup");
    let name = CString::new(readiness.as_os_str().as_bytes())?;
    // SAFETY: name is a NUL-terminated path inside our fresh private directory.
    if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut response = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&readiness)?;
    let mut open = Command::new("/usr/bin/open");
    open.args(["-W", "-n", "-a"])
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
    let mut helper = open.spawn()?;
    let result = PinnedProcess::open_unreaped(helper.id()).and_then(|helper| {
        read_ui_launch_handshake_with(&mut response, timeout, |output, deadline| {
            wait_for_app(output, helper.exit_readiness(), deadline)
        })
    });
    // Reap the system wait helper. The application was launched separately by
    // LaunchServices and continues to own its own window and claims.
    let _ = helper.kill();
    let _ = helper.wait();
    result
}

fn wait_for_app(
    output: BorrowedFd<'_>,
    exited: BorrowedFd<'_>,
    deadline: Instant,
) -> io::Result<bool> {
    let mut watched = [output, exited].map(|descriptor| libc::pollfd {
        fd: descriptor.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    if !crate::descriptor::wait_for_events(&mut watched, Some(deadline))? {
        return Ok(false);
    }
    // Consume buffered startup bytes before acting on a simultaneous exit.
    if watched[0].revents != 0 {
        return Ok(true);
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "cagents-ui exited before acknowledging startup",
    ))
}
