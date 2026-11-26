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

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::descriptor_frame::send_json_with_fds_until;

    #[test]
    fn startup_admits_only_the_exact_peer_epoch() {
        use std::io::Read;

        const PATH: &str = "CAGENTS_TEST_WINDOW_STARTUP_PATH";
        const WRONG_EPOCH: &str = "CAGENTS_TEST_WINDOW_STARTUP_WRONG_EPOCH";
        if let Some(path) = std::env::var_os(PATH) {
            let mut stream = UnixStream::connect(path).unwrap();
            let wrong_epoch = std::env::var_os(WRONG_EPOCH).is_some();
            let started = crate::pinned_process::started_ticks_of(std::process::id()).unwrap();
            send_json_with_fds_until(
                &stream,
                &Identity {
                    started: started + u64::from(wrong_epoch),
                    address: startup_admits_only_the_exact_peer_epoch as fn() as usize as u64,
                },
                &[],
                Instant::now() + CONNECT_TIMEOUT,
                HANDSHAKE_LIMIT,
                0,
                FrameByteOrder::LittleEndian,
            )
            .unwrap();
            let mut decision = [0];
            if wrong_epoch {
                assert_eq!(stream.read(&mut decision).unwrap(), 0);
            } else {
                stream.read_exact(&mut decision).unwrap();
                assert_eq!(decision, [ADMITTED]);
                stream.write_all(b"ready\n").unwrap();
            }
            return;
        }

        let directory = tempfile::Builder::new()
            .prefix("cgl.")
            .tempdir_in("/private/tmp")
            .unwrap();
        let path = directory.path().join("startup");
        let listener = UnixListener::bind(&path).unwrap();
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        let metadata = fs::metadata(&executable).unwrap();
        for wrong_epoch in [false, true] {
            let mut command = Command::new(&executable);
            command
                .args([
                    "--exact",
                    "control::darwin_ui::tests::startup_admits_only_the_exact_peer_epoch",
                ])
                .env(PATH, &path)
                .env_remove(WRONG_EPOCH);
            if wrong_epoch {
                command.env(WRONG_EPOCH, "1");
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            assert!(
                crate::descriptor::wait_for_activity(
                    listener.as_fd(),
                    libc::POLLIN,
                    Some(deadline)
                )
                .unwrap()
            );
            let (mut response, _) = crate::descriptor::accept(&listener).unwrap();
            let result = verify_window(
                &response,
                &executable,
                (metadata.dev(), metadata.ino()),
                deadline,
            );
            if wrong_epoch {
                assert!(result.is_err());
            } else {
                result.unwrap();
                response.write_all(&[ADMITTED]).unwrap();
                assert_eq!(
                    read_ui_launch_handshake(&mut response, CONNECT_TIMEOUT).unwrap(),
                    None
                );
            }
            drop(response);
            assert!(child.wait().unwrap().success());
        }
    }
}
