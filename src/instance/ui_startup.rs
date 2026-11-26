//! A direct readiness connection from a window launched by LaunchServices.

use std::time::Duration;
#[cfg(feature = "ui")]
use std::{io, os::unix::net::UnixStream, time::Instant};

use serde::{Deserialize, Serialize};

pub(crate) const SOCKET_ENV: &str = "CAGENTS_UI_STARTUP_SOCKET";
pub(crate) const HANDSHAKE_LIMIT: usize = 128;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const ADMITTED: u8 = 1;

#[derive(Deserialize, Serialize)]
pub(crate) struct Identity {
    pub(crate) started: u64,
    pub(crate) address: u64,
}

#[cfg(feature = "ui")]
static REPLY: std::sync::Mutex<Option<UnixStream>> = std::sync::Mutex::new(None);

/// Called by normal UI startup before any service, lock or saved state work.
#[cfg(feature = "ui")]
pub fn connect_ui_launcher() -> io::Result<()> {
    use crate::descriptor_frame::{FrameByteOrder, send_json_with_fds_until};
    use std::{io::Read, os::fd::AsFd};
    let Some(path) =
        std::env::var_os(SOCKET_ENV).filter(|_| super::ui_launch_handshake_requested())
    else {
        return Ok(());
    };
    let mut stream = UnixStream::connect(path)?;
    if crate::pinned_process::peer_uid(&stream)? != crate::daemon::current_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The UI launcher belongs to another account",
        ));
    }
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    send_json_with_fds_until(
        &stream,
        &Identity {
            started: crate::pinned_process::started_ticks_of(std::process::id())?,
            address: crate::process::own_code_address(),
        },
        &[],
        deadline,
        HANDSHAKE_LIMIT,
        0,
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    if !crate::descriptor::wait_for_activity(stream.as_fd(), libc::POLLIN, Some(deadline))? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "The UI launcher did not admit this window",
        ));
    }
    let mut admitted = [0];
    stream.read_exact(&mut admitted)?;
    if admitted != [ADMITTED] {
        return Err(io::Error::other(
            "The UI launcher did not admit this window",
        ));
    }
    *REPLY.lock().unwrap_or_else(|error| error.into_inner()) = Some(stream);
    Ok(())
}

#[cfg(feature = "ui")]
pub(super) fn write_reply(message: &str, final_response: bool) -> Option<io::Result<()>> {
    use std::io::Write;
    let mut reply = REPLY.lock().unwrap_or_else(|error| error.into_inner());
    let stream = reply.as_mut()?;
    let result = writeln!(stream, "{message}").and_then(|()| stream.flush());
    // Admission already transferred window ownership. If the launcher exits
    // during restore, the window can still finish opening its saved work.
    let result = result.or_else(|error| match error.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::NotConnected => Ok(()),
        _ => Err(error),
    });
    if final_response {
        reply.take();
    }
    Some(result)
}
