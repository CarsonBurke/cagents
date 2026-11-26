//! Launch an installed window through its stable LaunchServices identity.

use super::{Command, Duration, Instant, Path, Stdio, io, read_ui_launch_handshake};
use crate::{
    descriptor_frame::{FrameByteOrder, receive_json_with_fds_until},
    instance::ui_startup::{ADMITTED, CONNECT_TIMEOUT, HANDSHAKE_LIMIT, Identity, SOCKET_ENV},
    pinned_process::{PinnedProcess, peer_credentials},
};
use std::{
    fs,
    io::Write,
    os::{
        fd::AsFd,
        unix::{
            fs::MetadataExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
};

const OPEN_DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn launch(
    command: &Command,
    bundle: &Path,
    timeout: Duration,
) -> io::Result<Option<String>> {
    // Keep the rendezvous private and its pathname within sockaddr_un even
    // when the installation or the user's preferred temporary path is long.
    let directory = tempfile::Builder::new()
        .prefix("cgl.")
        .tempdir_in("/private/tmp")?;
    let socket = directory.path().join("startup");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let executable = Path::new(command.get_program()).canonicalize()?;
    let expected = fs::metadata(&executable)?;
    let mut open = Command::new("/usr/bin/open");
    open.args(["-n", "-a"])
        .arg(bundle)
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
    open.env(SOCKET_ENV, socket);
    // SAFETY: The hook seals inherited descriptors before exec.
    unsafe {
        open.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    dispatch(&mut open)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    if !crate::descriptor::wait_for_activity(listener.as_fd(), libc::POLLIN, Some(deadline))? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Cagents.app did not connect to its launcher",
        ));
    }
    let (mut response, _) = crate::descriptor::accept(&listener)?;
    verify_window(
        &response,
        &executable,
        (expected.dev(), expected.ino()),
        deadline,
    )?;
    response.write_all(&[ADMITTED])?;
    read_ui_launch_handshake(&mut response, timeout)
}

fn verify_window(
    stream: &UnixStream,
    executable: &Path,
    image: (u64, u64),
    deadline: Instant,
) -> io::Result<()> {
    let peer = peer_credentials(stream)?;
    if peer.uid != crate::daemon::current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The opening window belongs to another account",
        ));
    }
    let (identity, _): (Identity, _) = receive_json_with_fds_until(
        stream,
        deadline,
        HANDSHAKE_LIMIT,
        0,
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    let process = PinnedProcess::open(peer.pid)?;
    if process.started_ticks() != identity.started
        || process.executable()? != executable
        || crate::process::mapped_region_identity(peer.pid, identity.address)? != image
        || process.exited()?
    {
        return Err(io::Error::other(
            "The opening window runs a different installed build",
        ));
    }
    Ok(())
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
