//! Upgrade-only access to an authenticated source daemon. Ordinary application
//! clients continue to require a fully matched protocol.
mod interrupts;
mod turns;
mod v77;

pub(super) use interrupts::Interrupts;
pub(super) use turns::TurnReview;

use super::{UpdateError, impact, invalid};
use crate::{
    control::{ControlClient, ControlError},
    model::HostSessionProof,
};
use std::{
    fs::{self, File},
    io,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) enum Client {
    Current { client: ControlClient, peer: Peer },
    Legacy(v77::Client),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareError {
    #[error("{0}")]
    Refused(UpdateError),
    #[error(
        "The source preparation was not confirmed: {0}. Its checkpoint and owners require inspection; preparation was not retried and automatic recovery was not authorized"
    )]
    Unconfirmed(UpdateError),
    #[error("{0}")]
    Invalid(UpdateError),
}

impl From<PrepareError> for UpdateError {
    fn from(error: PrepareError) -> Self {
        invalid(error.to_string())
    }
}

fn prepare_reply(reply: Result<(u64, u64), ControlError>) -> Result<(u64, u64), PrepareError> {
    reply.map_err(|error| {
        if matches!(error, ControlError::Remote { .. }) {
            PrepareError::Refused(invalid(error.to_string()))
        } else {
            PrepareError::Unconfirmed(invalid(error.to_string()))
        }
    })
}

pub(crate) struct Peer {
    pid: u32,
    start: String,
    process: File,
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

    pub(super) fn executable(&self) -> Result<PathBuf, UpdateError> {
        self.alive()?;
        let executable = fs::read_link(format!("/proc/{}/exe", self.pid))?;
        self.alive()?;
        Ok(executable)
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
}

fn process_start(pid: u32) -> Result<String, UpdateError> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    raw.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .map(str::to_owned)
        .ok_or_else(|| invalid("The upgrade source process epoch is unavailable"))
}

impl Client {
    pub(crate) fn connect(socket: &Path) -> Result<Self, ControlError> {
        if crate::device::is_remote() {
            return Err(ControlError::Configure(io::Error::other(
                "Cross-version application upgrades require the local source daemon",
            )));
        }
        Self::connect_until(socket, Instant::now() + Duration::from_secs(5))
    }

    fn connect_until(socket: &Path, deadline: Instant) -> Result<Self, ControlError> {
        // The source may answer a newer Connect with an old error DTO. Read
        // only its bounded version header before choosing a decoder. Discovery
        // authorizes nothing: the selected client authenticates its own fresh
        // handshake, exact local peer and kernel-held lease below.
        Self::connect_selected(socket, deadline, v77::discover_protocol(socket, deadline)?)
    }

    fn connect_selected(
        socket: &Path,
        deadline: Instant,
        protocol: u16,
    ) -> Result<Self, ControlError> {
        match client_family(protocol) {
            Some(ClientFamily::Current) => {
                let stream = crate::control::connect_control_socket(
                    socket,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(ControlError::Connect)?;
                let peer = Peer::observe(&stream, socket).map_err(|error| {
                    ControlError::Configure(io::Error::other(error.to_string()))
                })?;
                // The lifecycle and inspector subset is wire-proven for each source.
                let client =
                    ControlClient::from_stream_at_source_protocol(stream, deadline, protocol)?;
                peer.verify(client.proof()).map_err(|error| {
                    ControlError::Configure(io::Error::other(error.to_string()))
                })?;
                Ok(Self::Current { client, peer })
            }
            Some(ClientFamily::V68 | ClientFamily::V77) => {
                v77::Client::connect_at_protocol(socket, deadline, protocol)
                    .map(Self::Legacy)
                    .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))
            }
            None => Err(ControlError::Configure(io::Error::other(
                unsupported_source(protocol),
            ))),
        }
    }

    /// The authenticated local socket peer, for a read-only shutdown survivor
    /// census. This is a daemon PID, never a terminal-child PID from List.
    pub(crate) fn peer_pid(&self) -> Option<u32> {
        match self {
            Self::Current { peer, .. } => Some(peer.pid),
            Self::Legacy(_) => None,
        }
    }

    pub(crate) fn protocol(&self) -> u16 {
        match self {
            Self::Current { client, .. } => client.protocol(),
            Self::Legacy(client) => client.protocol(),
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        match self {
            Self::Current { client, .. } => client.proof(),
            Self::Legacy(client) => client.proof(),
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
            Self::Legacy(client) => client.upgrade_impact(policy),
        }
    }

    pub(crate) fn prepare_confirmed_upgrade(
        &mut self,
        impact: impact::DaemonImpact,
        relaunch_agents: bool,
    ) -> Result<(u64, u64), PrepareError> {
        if impact.capability.proof != *self.proof() || !impact.ready() {
            return Err(PrepareError::Invalid(invalid(
                "The source migration capability no longer matches its reviewed ownership",
            )));
        }
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof()).map_err(PrepareError::Invalid)?;
                prepare_reply(client.prepare_confirmed_upgrade(impact, relaunch_agents))
            }
            Self::Legacy(client) => client
                .prepare_confirmed_upgrade(impact, relaunch_agents)
                .map_err(PrepareError::Unconfirmed),
        }
    }
}

impl Client {
    pub(crate) fn list(
        &mut self,
    ) -> Result<Vec<crate::terminal_ipc::TerminalDescriptor>, UpdateError> {
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client.list().map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "This source cannot list terminals for a confirmed stop",
            )),
        }
    }

    pub(crate) fn shutdown_preflight(
        &mut self,
    ) -> Result<crate::terminal_ipc::ShutdownCapability, UpdateError> {
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .shutdown_preflight()
                    .map(|preflight| preflight.capability)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "This source cannot confirm a terminal service stop",
            )),
        }
    }

    pub(crate) fn confirm_shutdown(
        &mut self,
        capability: crate::terminal_ipc::ShutdownCapability,
    ) -> Result<crate::terminal_ipc::ShutdownOutcome, UpdateError> {
        if &capability.proof != self.proof() {
            return Err(invalid("The source changed before its confirmed stop"));
        }
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .confirm_shutdown(capability)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "This source cannot confirm a terminal service stop",
            )),
        }
    }

    /// The conversations the source lists, as it issued them.
    pub(crate) fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError> {
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .list_chats()
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "This source cannot end conversations for the update",
            )),
        }
    }

    /// Capture history once, immediately before a confirmed service stop. The
    /// returned snapshot is not evidence that shutdown was accepted.
    pub(crate) fn capture_chat_snapshot(
        &mut self,
        session: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .capture_chat_snapshot(session)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "The source cannot capture this conversation's history",
            )),
        }
    }

    /// Ends one listed conversation through the source's own authenticated
    /// close: attached, as any client the source did not open must be, and
    /// answered only once the source confirms its provider's teardown.
    pub(crate) fn close_chat(
        &mut self,
        session: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        match self {
            Self::Current { client, peer } => {
                peer.verify(client.proof())?;
                client
                    .close_chat_with_snapshot(session)
                    .map_err(|error| invalid(error.to_string()))
            }
            Self::Legacy(_) => Err(invalid(
                "This source cannot end conversations for the update",
            )),
        }
    }
}

pub(super) struct InstalledIdentity {
    pub protocol: u16,
    pub proof: Option<HostSessionProof>,
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

    pub(super) fn verify_owner(&self, executable: Option<&Path>) -> Result<(), UpdateError> {
        if Instant::now() >= self.deadline {
            return Err(invalid("The installed source inspection timed out"));
        }
        match (&self.peer, &self.proof, executable) {
            (Some(peer), Some(proof), Some(expected)) => {
                peer.verify(proof)?;
                let current = fs::read_link(format!("/proc/{}/exe", peer.pid))?;
                if current != expected {
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

/// Frozen lifecycle and inspector contract, using the wire variant names.
/// Named variants define each qualified operation; unselected commands grant
/// no source authority. Remaining reachable payloads, including errors and
/// historical unsolicited chat traffic, must match in full.
/// ShutdownV1 includes preflight, stop and confirmed shutdown; List/BuildInfo
/// include status reads. Current-only ChatLimit cannot arrive from historical
/// sources; the full current-wire recording test covers that message.
/// Keep this beside source admission so adding support always runs the proof.
#[cfg(test)]
pub(crate) const LIFECYCLE_VARIANTS: &[(&str, &[&str])] = &[
    (
        "ClientRequest",
        &[
            "Connect",
            "List",
            "ChatList",
            "ChatCommand",
            "Agent",
            "ChatStart",
            "ChatAttach",
            "ChatVisibility",
            "ChatRead",
            "ChatClose",
            "ChatDetach",
            "ChatVisibility",
            "ChatRead",
            "ResolveUpgradeChat",
            "Shutdown",
            "ShutdownV1",
            "PrepareUpgrade",
            "UpgradeImpact",
            "PrepareConfirmedUpgrade",
            "BuildInfo",
        ],
    ),
    (
        "ServerResponse",
        &[
            "connected",
            "listed",
            "chat_listed",
            "chat_attached",
            "chat_snapshot",
            "chat_update",
            "chat_closed",
            "chat_snapshot",
            "detached",
            "accepted",
            "upgrade_chat_resolved",
            "shutdown_accepted",
            "shutdown_preflight",
            "shutdown_outcome",
            "upgrade_impact",
            "upgrade_prepared",
            "build_info",
            "agent",
        ],
    ),
    ("cagents::chat::Command", &["Interrupt", "StopSubagent"]),
    (
        "cagents::agent_execution::Operation",
        &["Directory", "Control"],
    ),
    ("cagents::agent_execution::Control", &["Interrupt"]),
    (
        "cagents::agent_execution::Reply",
        &["Directory", "Requests"],
    ),
    (
        "ServerMessage",
        &[
            "response",
            "error",
            "host_shutting_down",
            "chat_changed",
            "chat_observed",
            "chat_activity",
            "chat_account_usage",
        ],
    ),
];

/// One source qualification for lifecycle control and migration inspection.
/// The bespoke families share the frozen lifecycle subset but need separate
/// inventory decoders. Their byte proofs live beside those clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientFamily {
    Current,
    V77,
    V68,
}

/// Current-wire entries must pass the recorded lifecycle/inspector proof.
/// Unrecorded mainline protocols 99–103 and 119 are deliberately not admitted.
const SOURCE_CLIENTS: &[(u16, ClientFamily)] = &[
    (68, ClientFamily::V68),
    (77, ClientFamily::V77),
    (128, ClientFamily::Current),
    (156, ClientFamily::Current),
    (crate::terminal_ipc::PROTOCOL_VERSION, ClientFamily::Current),
];

pub(crate) fn client_family(protocol: u16) -> Option<ClientFamily> {
    SOURCE_CLIENTS
        .iter()
        .find_map(|&(version, family)| (version == protocol).then_some(family))
}

#[cfg(test)]
pub(crate) fn protocols() -> impl Iterator<Item = u16> {
    SOURCE_CLIENTS.iter().map(|&(protocol, _)| protocol)
}

#[cfg(test)]
pub(crate) fn current_client_protocols() -> impl Iterator<Item = u16> {
    protocols().filter(|&protocol| current_client_supports(protocol))
}

pub(crate) fn current_client_supports(protocol: u16) -> bool {
    client_family(protocol) == Some(ClientFamily::Current)
}

pub(crate) fn is_qualified_source(protocol: u16) -> bool {
    client_family(protocol).is_some()
}

pub(super) fn unsupported_source(protocol: u16) -> UpdateError {
    if protocol > crate::terminal_ipc::PROTOCOL_VERSION {
        return invalid(format!(
            "The running cagents service is newer than this build (protocol {protocol}). Install the newer build instead."
        ));
    }
    invalid(format!(
        "The running cagents service uses protocol {protocol}. Update it to a build serving protocol 156 (such as 0.1.3+local.991daa43e) first."
    ))
}

/// Authenticate an admitted source and retain its original peer through the
/// installed executable recheck. Discovery alone grants no update authority.
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
            if !is_qualified_source(protocol) {
                return Err(unsupported_source(protocol));
            }
            let client = Client::connect_selected(&socket, deadline, protocol)
                .map_err(|error| invalid(error.to_string()))?;
            peer.verify(client.proof())?;
            Ok(InstalledIdentity {
                protocol,
                proof: Some(client.proof().clone()),
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
                peer: None,
                deadline,
            })
        }
        Err(error) => Err(invalid(error.to_string())),
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
            absent_source(expected)?;
            Ok((crate::terminal_ipc::PROTOCOL_VERSION, None))
        }
        Err(error) => Err(invalid(error.to_string())),
    }
}

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

#[cfg(test)]
mod prepare_receipt_tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, ShutdownCapability, read_frame,
            write_frame,
        },
    };

    #[test]
    fn only_an_explicit_prepare_refusal_allows_destructive_fallback() {
        for outcome in [
            "accepted",
            "refused",
            "accepted-with-lost-ack",
            "foreign-ack",
        ] {
            let proof = HostSessionProof {
                host_session_id: HostSessionId::new("prepare-source"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("daemon"),
            };
            let daemon = impact::DaemonImpact {
                capability: ShutdownCapability {
                    proof: proof.clone(),
                    token: "confirmed-prepare".into(),
                    inventory_generation: 1,
                    live_terminals: 0,
                    background_subagents: 0,
                    background_identity: String::new(),
                    expires_at_unix_ms: i64::MAX,
                },
                policy: impact::Policy::WhenIdle,
                sessions: Vec::new(),
                blockers: Vec::new(),
            };
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let server = std::thread::spawn(move || {
                let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(connect.request, ClientRequest::Connect { .. }));
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connect.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: proof.clone(),
                                resumed: Vec::new(),
                            },
                        },
                    },
                )
                .unwrap();
                let prepare: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(
                    prepare.request,
                    ClientRequest::PrepareConfirmedUpgrade {
                        relaunch_agents: false,
                        ..
                    }
                ));
                if outcome == "accepted-with-lost-ack" {
                    // The old daemon accepted preparation and could have
                    // written Prepared, but its acknowledgement never arrives.
                    return;
                }
                let message = if outcome == "refused" {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::InvalidRequest,
                        "Cannot checkpoint this conversation",
                        false,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::UpgradePrepared {
                            terminals: 0,
                            chats: 0,
                        },
                    }
                };
                let mut response = ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(prepare.request_id),
                    proof,
                    message,
                };
                if outcome == "foreign-ack" {
                    response.proof.host_incarnation = HostIncarnation::new("another-daemon");
                }
                write_frame(&mut peer, &response).unwrap();
            });
            let mut client = ControlClient::from_stream(stream).unwrap();
            let reply = prepare_reply(client.prepare_confirmed_upgrade(daemon, false));
            match outcome {
                "accepted" => assert_eq!(reply.unwrap(), (0, 0)),
                "refused" => assert!(matches!(reply, Err(PrepareError::Refused(_)))),
                _ => assert!(matches!(reply, Err(PrepareError::Unconfirmed(_)))),
            }
            server.join().unwrap();
        }
    }
}

#[cfg(test)]
mod admission_message_tests {
    use super::*;

    #[test]
    fn newer_service_requires_the_newer_build() {
        let protocol = crate::terminal_ipc::PROTOCOL_VERSION + 1;
        assert_eq!(
            unsupported_source(protocol).to_string(),
            format!(
                "The running cagents service is newer than this build (protocol {protocol}). Install the newer build instead."
            )
        );
    }

    #[test]
    fn older_and_between_supported_protocols_name_the_exact_intermediate_build() {
        for protocol in [119, 56] {
            assert!(!is_qualified_source(protocol));
            assert_eq!(
                unsupported_source(protocol).to_string(),
                format!(
                    "The running cagents service uses protocol {protocol}. Update it to a build serving protocol 156 (such as 0.1.3+local.991daa43e) first."
                )
            );
        }
    }
}
