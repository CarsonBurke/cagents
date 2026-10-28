//! Read-only ownership sampling for current-protocol source pane inspection.
use super::{UpdateError, invalid};
use crate::{
    control::{ControlClient, ControlError},
    model::HostSessionProof,
};
use std::{
    io,
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) struct Peer {
    pid: u32,
    socket: PathBuf,
}

impl Peer {
    pub(super) fn observe(stream: &UnixStream, socket: &Path) -> Result<Self, UpdateError> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: both buffers have the exact initialized kernel ABI layout.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if length as usize != std::mem::size_of::<libc::ucred>()
            || credentials.uid != crate::daemon::current_uid()
            || credentials.pid <= 1
        {
            return Err(invalid("The upgrade source is not an owned local daemon"));
        }
        Ok(Self {
            pid: credentials.pid as u32,
            socket: socket.to_owned(),
        })
    }

    pub(super) fn verify(&self, proof: &HostSessionProof) -> Result<(), UpdateError> {
        let owner = crate::daemon::lease_owner(&self.socket.with_file_name("cagentsd.lock"), proof)
            .map_err(|_| {
                invalid("The upgrade source ownership changed; existing windows were kept open")
            })?;
        if owner != self.pid {
            return Err(invalid(
                "The upgrade socket does not belong to its exact daemon lease",
            ));
        }
        Ok(())
    }
}

/// The inspector must receive the source's actual protocol and ownership, not
/// infer either from the newly installed coordinator executable.
pub(crate) fn inspect_identity(
    expected: Option<&HostSessionProof>,
) -> Result<(u16, Option<HostSessionProof>), UpdateError> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let connected = (|| {
        if crate::device::is_remote() {
            return Err(ControlError::Configure(io::Error::other(
                "Source pane inspection requires the local daemon",
            )));
        }
        let stream = crate::control::connect_control_socket(
            &socket,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(ControlError::Connect)?;
        let peer = Peer::observe(&stream, &socket)
            .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))?;
        let client = ControlClient::from_stream_with_deadline(stream, deadline)?;
        peer.verify(client.proof())
            .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))?;
        Ok(client)
    })();
    match connected {
        Ok(client) => {
            if expected.is_some_and(|proof| proof != client.proof()) {
                return Err(invalid("The source daemon changed before pane inspection"));
            }
            Ok((
                crate::terminal_ipc::PROTOCOL_VERSION,
                Some(client.proof().clone()),
            ))
        }
        Err(ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let paths = crate::daemon::RuntimePaths::discover()
                .map_err(|error| invalid(error.to_string()))?;
            if expected.is_some()
                || crate::daemon::HostLease::is_held(&paths)
                    .map_err(|error| invalid(error.to_string()))?
            {
                return Err(invalid(
                    "The source service is starting or stopping; existing windows were kept open",
                ));
            }
            Ok((crate::terminal_ipc::PROTOCOL_VERSION, None))
        }
        Err(error) => Err(invalid(error.to_string())),
    }
}
