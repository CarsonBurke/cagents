//! Upgrade-only access to an authenticated source daemon. Ordinary application
//! clients continue to require a fully matched protocol.
mod v77;

use super::{UpdateError, impact, invalid};
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

pub(crate) enum Client {
    Current { client: ControlClient, peer: Peer },
    V77(v77::Client),
}

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

impl Client {
    pub(crate) fn connect(socket: &Path) -> Result<Self, ControlError> {
        if crate::device::is_remote() {
            return Err(ControlError::Configure(io::Error::other(
                "Cross-version application upgrades require the local source daemon",
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        // The source may answer a newer Connect with an old error DTO. Read
        // only its bounded version header before choosing a decoder. Discovery
        // authorizes nothing: the selected client authenticates its own fresh
        // handshake, exact local peer and kernel-held lease below.
        match v77::discover_protocol(socket, deadline)? {
            crate::terminal_ipc::PROTOCOL_VERSION => {
                let stream = crate::control::connect_control_socket(
                    socket,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(ControlError::Connect)?;
                let peer = Peer::observe(&stream, socket).map_err(|error| {
                    ControlError::Configure(io::Error::other(error.to_string()))
                })?;
                let client = ControlClient::from_stream_with_deadline(stream, deadline)?;
                peer.verify(client.proof()).map_err(|error| {
                    ControlError::Configure(io::Error::other(error.to_string()))
                })?;
                Ok(Self::Current { client, peer })
            }
            77 => v77::Client::connect(socket, deadline)
                .map(Self::V77)
                .map_err(|error| ControlError::Configure(io::Error::other(error.to_string()))),
            actual => Err(ControlError::UnsupportedVersion { actual }),
        }
    }

    pub(crate) fn protocol(&self) -> u16 {
        match self {
            Self::Current { .. } => crate::terminal_ipc::PROTOCOL_VERSION,
            Self::V77(_) => 77,
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        match self {
            Self::Current { client, .. } => client.proof(),
            Self::V77(client) => client.proof(),
        }
    }

    pub(crate) fn upgrade_impact(
        &mut self,
        policy: impact::Policy,
    ) -> Result<impact::DaemonImpact, UpdateError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .upgrade_impact_with_deadline(policy, deadline)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::V77(client) => client.upgrade_impact(policy),
        }
    }

    pub(crate) fn prepare_confirmed_upgrade(
        &mut self,
        impact: impact::DaemonImpact,
        relaunch_agents: bool,
    ) -> Result<(u64, u64), UpdateError> {
        if impact.capability.proof != *self.proof() || !impact.ready() {
            return Err(invalid(
                "The source migration capability no longer matches its reviewed ownership",
            ));
        }
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .prepare_confirmed_upgrade(impact, relaunch_agents)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::V77(client) => client.prepare_confirmed_upgrade(impact, relaunch_agents),
        }
    }
}

/// The inspector must receive the source's actual protocol and ownership, not
/// infer either from the newly installed coordinator executable.
pub(crate) fn inspect_identity(
    expected: Option<&HostSessionProof>,
) -> Result<(u16, Option<HostSessionProof>), UpdateError> {
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    match Client::connect(&socket) {
        Ok(client) => {
            if expected.is_some_and(|proof| proof != client.proof()) {
                return Err(invalid("The source daemon changed before pane inspection"));
            }
            Ok((client.protocol(), Some(client.proof().clone())))
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
