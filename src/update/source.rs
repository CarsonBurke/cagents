//! Upgrade-only access to an authenticated source daemon. Ordinary application
//! clients continue to require a fully matched protocol.
mod v77;

use super::{UpdateError, impact, invalid};
use crate::{
    control::{ControlClient, ControlError},
    model::HostSessionProof,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::net::UnixStream,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) enum Client {
    Current { client: ControlClient, peer: Peer },
    V77(v77::Client),
    V119(v77::Client),
}

pub(crate) struct Peer {
    pid: u32,
    start: String,
    process: File,
    socket: PathBuf,
}

impl Peer {
    #[cfg(target_os = "linux")]
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
        // Pin the original socket peer before observing its process epoch.
        // A later reused numeric PID cannot authenticate this connection.
        // SAFETY: pidfd_open takes scalar arguments and returns a new owned FD.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, credentials.pid, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful pidfd_open returned a fresh descriptor owned here.
        let process = unsafe { File::from_raw_fd(descriptor as i32) };
        let peer = Self {
            pid: credentials.pid as u32,
            start: process_start(credentials.pid as u32)?,
            process,
            socket: socket.to_owned(),
        };
        peer.alive()?;
        Ok(peer)
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn observe(_stream: &UnixStream, _socket: &Path) -> Result<Self, UpdateError> {
        Err(unsupported_source_inspection().into())
    }

    pub(super) fn verify(&self, proof: &HostSessionProof) -> Result<(), UpdateError> {
        self.alive()?;
        let owner = crate::daemon::lease_owner(&self.socket.with_file_name("cagentsd.lock"), proof)
            .map_err(|_| {
                invalid("The upgrade source ownership changed; existing windows were kept open")
            })?;
        if owner != self.pid {
            return Err(invalid(
                "The upgrade socket does not belong to its exact daemon lease",
            ));
        }
        self.alive()
    }

    fn alive(&self) -> Result<(), UpdateError> {
        let mut poll = libc::pollfd {
            fd: self.process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized writable pollfd.
        if unsafe { libc::poll(&mut poll, 1, 0) } != 0 || process_start(self.pid)? != self.start {
            return Err(invalid("The original upgrade source has exited or changed"));
        }
        Ok(())
    }

    fn installed_identity(&self) -> Result<(HostSessionProof, PathBuf), UpdateError> {
        #[derive(serde::Deserialize)]
        struct Lease {
            pid: u32,
            proof: HostSessionProof,
        }
        self.alive()?;
        let path = self.socket.with_file_name("cagentsd.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let before = file.metadata()?;
        if !before.is_file()
            || before.uid() != crate::daemon::current_uid()
            || before.mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() > 4096
        {
            return Err(invalid(
                "The upgrade source lease cannot be safely inspected",
            ));
        }
        let identity = |m: &fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != before.len()
            || identity(&before) != identity(&file.metadata()?)
            || identity(&before) != identity(&fs::symlink_metadata(&path)?)
        {
            return Err(invalid(
                "The upgrade source lease changed during inspection",
            ));
        }
        let lease: Lease = serde_json::from_slice(&bytes)?;
        if lease.pid != self.pid {
            return Err(invalid(
                "The upgrade socket is not the original lease owner",
            ));
        }
        self.verify(&lease.proof)?;
        let executable = fs::read_link(format!("/proc/{}/exe", self.pid))?;
        self.verify(&lease.proof)?;
        Ok((lease.proof, executable))
    }
}

fn process_start(pid: u32) -> Result<String, UpdateError> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    raw.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or_else(|| invalid("The upgrade source process epoch is unavailable"))
}

impl Client {
    #[cfg(target_os = "linux")]
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
        Self::connect_selected(socket, deadline, v77::discover_protocol(socket, deadline)?)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn connect(_socket: &Path) -> Result<Self, ControlError> {
        Err(ControlError::Configure(unsupported_source_inspection()))
    }

    fn connect_selected(
        socket: &Path,
        deadline: Instant,
        protocol: u16,
    ) -> Result<Self, ControlError> {
        match protocol {
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
            119 => v77::Client::connect_at_protocol(socket, deadline, 119)
                .map(Self::V119)
                .map_err(|error| ControlError::Configure(io::Error::other(error.to_string()))),
            actual => Err(ControlError::UnsupportedVersion { actual }),
        }
    }

    pub(crate) fn protocol(&self) -> u16 {
        match self {
            Self::Current { .. } => crate::terminal_ipc::PROTOCOL_VERSION,
            Self::V77(_) => 77,
            Self::V119(_) => 119,
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        match self {
            Self::Current { client, .. } => client.proof(),
            Self::V77(client) | Self::V119(client) => client.proof(),
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
            Self::V77(client) | Self::V119(client) => client.upgrade_impact(policy),
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
            Self::V77(client) | Self::V119(client) => {
                client.prepare_confirmed_upgrade(impact, relaunch_agents)
            }
        }
    }
}

pub(super) struct InstalledIdentity {
    pub protocol: u16,
    pub proof: Option<HostSessionProof>,
    unqualified_executable: Option<PathBuf>,
    peer: Option<Peer>,
    deadline: Instant,
}

impl InstalledIdentity {
    /// Check continuity of an already authenticated owner across an inspection.
    /// This grants no new routing authority and does not renew either TTL: the
    /// caller must separately authenticate the fresh observation's executable.
    pub(super) fn verify_same_owner(&self, after: &Self) -> Result<(), UpdateError> {
        match (&self.peer, &after.peer, &self.proof, &after.proof) {
            (Some(before), Some(current), Some(proof), Some(after_proof))
                if self.protocol == after.protocol
                    && proof == after_proof
                    && before.pid == current.pid
                    && before.start == current.start
                    && before.socket == current.socket =>
            {
                before.verify(proof)?;
                current.verify(after_proof)?;
                before.verify(proof)
            }
            _ => Err(invalid(
                "The original installed source changed during update inspection",
            )),
        }
    }

    pub(super) fn requires_matched_owner_worker(&self) -> bool {
        self.unqualified_executable.is_some()
    }

    pub(super) fn verify_owner(&self, executable: Option<&Path>) -> Result<(), UpdateError> {
        if Instant::now() >= self.deadline {
            return Err(invalid("The installed source inspection timed out"));
        }
        match (&self.peer, &self.proof, executable) {
            (Some(peer), Some(proof), Some(expected)) => {
                peer.verify(proof)?;
                let current = fs::read_link(format!("/proc/{}/exe", peer.pid))?;
                if current != expected
                    || self
                        .unqualified_executable
                        .as_ref()
                        .is_some_and(|observed| observed != expected)
                {
                    return Err(invalid(
                        "The installed source owner changed before worker selection",
                    ));
                }
                peer.verify(proof)?;
            }
            (None, None, None) => absent_source(None)?,
            _ => {
                return Err(invalid(
                    "The source has no matching owned installed coordinator",
                ));
            }
        }
        if Instant::now() >= self.deadline {
            return Err(invalid("The installed source inspection timed out"));
        }
        Ok(())
    }
}

/// Installed entry routing, not permission to decode or mutate an unknown
/// source. Only its exact authenticated old executable may own that workflow.
#[cfg(target_os = "linux")]
pub(super) fn inspect_installed_identity() -> Result<InstalledIdentity, UpdateError> {
    if crate::device::is_remote() {
        return Err(invalid(
            "Cross-version application upgrades require the local source daemon",
        ));
    }
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    match v77::discover_installed_protocol(&socket, deadline) {
        Ok((protocol, peer)) => {
            if matches!(protocol, 77 | 119 | crate::terminal_ipc::PROTOCOL_VERSION) {
                let client = Client::connect_selected(&socket, deadline, protocol)
                    .map_err(|error| invalid(error.to_string()))?;
                peer.verify(client.proof())?;
                return Ok(InstalledIdentity {
                    protocol,
                    proof: Some(client.proof().clone()),
                    unqualified_executable: None,
                    peer: Some(peer),
                    deadline,
                });
            }
            let (proof, executable) = peer.installed_identity()?;
            if Instant::now() >= deadline {
                return Err(invalid("The installed source inspection timed out"));
            }
            Ok(InstalledIdentity {
                protocol,
                proof: Some(proof),
                unqualified_executable: Some(executable),
                peer: Some(peer),
                deadline,
            })
        }
        Err(ControlError::Connect(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            absent_source(None)?;
            Ok(InstalledIdentity {
                protocol: crate::terminal_ipc::PROTOCOL_VERSION,
                proof: None,
                unqualified_executable: None,
                peer: None,
                deadline,
            })
        }
        Err(error) => Err(invalid(error.to_string())),
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn inspect_installed_identity() -> Result<InstalledIdentity, UpdateError> {
    Err(unsupported_source_inspection().into())
}

/// The inspector must receive the source's actual protocol and ownership, not
/// infer either from the newly installed coordinator executable.
#[cfg(target_os = "linux")]
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
            absent_source(expected)?;
            Ok((crate::terminal_ipc::PROTOCOL_VERSION, None))
        }
        Err(error) => Err(invalid(error.to_string())),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn inspect_identity(
    _expected: Option<&HostSessionProof>,
) -> Result<(u16, Option<HostSessionProof>), UpdateError> {
    Err(unsupported_source_inspection().into())
}

#[cfg(target_os = "linux")]
fn absent_source(expected: Option<&HostSessionProof>) -> Result<(), UpdateError> {
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    if expected.is_some()
        || crate::daemon::HostLease::is_held(&paths).map_err(|error| invalid(error.to_string()))?
    {
        return Err(invalid(
            "The source service is starting or stopping; existing windows were kept open",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn absent_source(_expected: Option<&HostSessionProof>) -> Result<(), UpdateError> {
    Err(unsupported_source_inspection().into())
}

#[cfg(not(target_os = "linux"))]
fn unsupported_source_inspection() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Authenticating an upgrade source requires Linux peer credentials and process identity",
    )
}

#[cfg(all(test, not(target_os = "linux")))]
mod platform_tests {
    use super::*;

    #[test]
    fn unsupported_source_cannot_be_classified_as_an_absent_daemon() {
        assert!(matches!(inspect_identity(None), Err(UpdateError::Io(error))
            if error.kind() == io::ErrorKind::Unsupported));
        assert!(
            matches!(inspect_installed_identity(), Err(UpdateError::Io(error))
            if error.kind() == io::ErrorKind::Unsupported)
        );
        assert!(matches!(absent_source(None), Err(UpdateError::Io(error))
            if error.kind() == io::ErrorKind::Unsupported));
        assert!(matches!(Client::connect(Path::new("missing-daemon.sock")),
            Err(ControlError::Configure(error)) if error.kind() == io::ErrorKind::Unsupported));

        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(
            matches!(Peer::observe(&stream, Path::new("untrusted.sock")),
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::Unsupported)
        );
    }
}
