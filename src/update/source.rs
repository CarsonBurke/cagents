//! Upgrade-only access to an authenticated source daemon. Ordinary application
//! clients continue to require a fully matched protocol.
mod discovery;
mod interrupts;
mod turns;

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

/// A source daemon authenticated at the protocol it serves: its exact local
/// peer is pinned and rechecked against the kernel-held lease per request.
pub(crate) struct Client {
    pub(super) client: ControlClient,
    pub(super) peer: Peer,
    /// How long requests the source refuses as busy are asked again.
    busy: BusyPatience,
    busy_pause: Duration,
    attempt_allowance: Duration,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrepareError {
    #[error("{0}")]
    Refused(UpdateError),
    #[error(
        "The source preparation was not confirmed: {0}. Its checkpoint and owners require inspection; preparation was not retried and automatic recovery was not authorized"
    )]
    Unconfirmed(UpdateError),
    /// The source stayed busy with another operation past every retry. It
    /// admitted nothing, so the update ends without stopping anything.
    #[error("{0}")]
    Busy(UpdateError),
    #[error("{0}")]
    Invalid(UpdateError),
}

/// How long a confirmed preparation keeps retrying a source that is busy with
/// another operation. Each attempt keeps the full reply allowance, so a slow
/// acceptance is never misread as an unconfirmed one.
const PREPARE_BUSY_PATIENCE: Duration = Duration::from_secs(60);
/// How long any source request refused as busy waits before it is asked again.
/// Each attempt first queues for exclusive admission, holding back other
/// clients' ordinary requests, so the pause gives them room in between.
const SOURCE_BUSY_PAUSE: Duration = Duration::from_secs(1);

/// How long one exclusive source request can take even when it is admitted:
/// the daemon queues a stop preflight or confirmation for exclusive admission
/// for up to 2.5 s (`host::client::exclusive_patience`), and then answers.
/// A request sharing a deadline is not sent with less than this left, so a run
/// of such requests ends near its deadline however the source is loaded; only
/// an admitted reply slower than the remaining half second can run past it.
const SOURCE_ATTEMPT_ALLOWANCE: Duration = Duration::from_secs(3);

/// How long one source request that the source refused while finishing another
/// operation keeps being asked again, unless its caller shares one deadline
/// across several requests. Such a refusal changed nothing, so asking again is
/// safe; the caller is already waiting on the answer.
const SOURCE_BUSY_PATIENCE: Duration = Duration::from_secs(30);

/// How long source requests refused as busy keep being asked again.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BusyPatience {
    /// Each request may be asked again for this long.
    PerRequest(Duration),
    /// Every request shares this deadline, so a run of several requests ends
    /// within a known time however busy the source stays: no attempt starts
    /// unless its admission wait and a prompt reply fit before the deadline.
    Until(Instant),
}

impl BusyPatience {
    /// Each request is asked again for [`SOURCE_BUSY_PATIENCE`].
    pub(crate) const PER_REQUEST: Self = Self::PerRequest(SOURCE_BUSY_PATIENCE);
    /// Each request is asked once; a busy refusal is final.
    #[cfg(any(feature = "host", test))]
    pub(crate) const ONCE: Self = Self::PerRequest(Duration::ZERO);

    /// One deadline, `patience` from now, for every request that uses it.
    pub(crate) fn shared_for(patience: Duration) -> Self {
        Self::Until(Instant::now() + patience)
    }

    /// One deadline, [`SOURCE_BUSY_PATIENCE`] from now, for a run of requests
    /// that answers one question, such as a recovery check.
    pub(crate) fn for_one_run() -> Self {
        Self::shared_for(SOURCE_BUSY_PATIENCE)
    }

    fn give_up(self) -> Instant {
        match self {
            Self::PerRequest(patience) => Instant::now() + patience,
            Self::Until(deadline) => deadline,
        }
    }
}

/// Asks `attempt` again, after `pause`, while its error is `busy` and the
/// next attempt would start before `give_up`. Any other reply is final at once.
fn retrying_busy<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    busy: impl Fn(&E) -> bool,
    give_up: Instant,
    pause: Duration,
) -> Result<T, E> {
    loop {
        match attempt() {
            Err(error) if busy(&error) && Instant::now() + pause < give_up => {
                std::thread::sleep(pause);
            }
            reply => return reply,
        }
    }
}

fn prepare_retrying_busy(
    attempt: impl FnMut() -> Result<(u64, u64), PrepareError>,
    patience: Duration,
    pause: Duration,
) -> Result<(u64, u64), PrepareError> {
    retrying_busy(
        attempt,
        |error| matches!(error, PrepareError::Busy(_)),
        Instant::now() + patience,
        pause,
    )
}

/// What an inventory fence still refused as busy when its patience ended says.
/// The fence only issues an expiring capability, so the check changed nothing;
/// each caller says what was kept and what to retry.
pub(crate) const STILL_BUSY_TO_CHECK: &str = "The daemon was busy finishing other operations, so the inventory check could not finish in time; the check itself changed nothing";
/// What a service stop's preflight or confirmation still refused as busy when
/// its patience ended says. Neither changed anything.
pub(crate) const STILL_BUSY_TO_STOP: &str = "The terminal service was busy finishing other operations and refused the stop, so nothing was stopped. Retry the update once it finishes.";

/// One attempt of a request asked again while the source is busy.
enum Asked {
    /// The source's ownership no longer verifies; nothing was sent.
    Unverified(UpdateError),
    /// A shared deadline left too little time for another attempt, which
    /// was not sent.
    OutOfTime,
    Replied(ControlError),
}

impl Asked {
    fn is_busy(&self) -> bool {
        matches!(self, Self::Replied(error) if error.is_daemon_busy())
    }
}

impl From<PrepareError> for UpdateError {
    fn from(error: PrepareError) -> Self {
        invalid(error.to_string())
    }
}

fn prepare_reply(reply: Result<(u64, u64), ControlError>) -> Result<(u64, u64), PrepareError> {
    reply.map_err(|error| {
        if error.is_daemon_busy() {
            PrepareError::Busy(invalid(
                "The source daemon stayed busy finishing another operation, so it did not checkpoint and nothing was stopped. Retry the update.",
            ))
        } else if matches!(error, ControlError::Remote { .. }) {
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
    /// Authenticate an already verified serving proof without protocol discovery.
    pub(super) fn confirm_serving(
        socket: &Path,
        executable: &Path,
        proof: &HostSessionProof,
    ) -> Result<(), UpdateError> {
        let stream = crate::control::connect_control_socket(socket, Duration::from_secs(5))?;
        let peer = Self::observe(&stream, socket)?;
        peer.verify(proof)?;
        if peer.executable()? != executable {
            return Err(invalid(
                "The terminal service does not belong to the confirmed installation",
            ));
        }
        peer.verify(proof)
    }

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
        Self::connect_selected(
            socket,
            deadline,
            discovery::discover_qualified_protocol(socket, deadline)?,
        )
    }

    fn connect_selected(
        socket: &Path,
        deadline: Instant,
        protocol: u16,
    ) -> Result<Self, ControlError> {
        if !is_qualified_source(protocol) {
            return Err(ControlError::Configure(io::Error::other(
                unsupported_source(protocol),
            )));
        }
        Self::connect_authenticated(socket, deadline, |stream| {
            ControlClient::from_stream_at_source_protocol(stream, deadline, protocol)
        })
    }

    /// Read from the daemon at this executable's own protocol, without source admission.
    pub(super) fn connect_matched(socket: &Path) -> Result<Self, ControlError> {
        Self::connect_matched_until(socket, Instant::now() + Duration::from_secs(5))
    }

    fn connect_matched_until(socket: &Path, deadline: Instant) -> Result<Self, ControlError> {
        Self::connect_authenticated(socket, deadline, |stream| {
            ControlClient::from_stream_with_deadline(stream, deadline)
        })
    }

    /// Verification can also read a compatible older daemon during staging or
    /// rollback. The matched daemon never goes through source admission.
    pub(super) fn connect_inventory(socket: &Path) -> Result<Self, ControlError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (protocol, peer) = discovery::discover_protocol_and_peer(socket, deadline)?;
        let client = if protocol == crate::terminal_ipc::PROTOCOL_VERSION {
            Self::connect_matched_until(socket, deadline)?
        } else if protocol < crate::terminal_ipc::PROTOCOL_VERSION && is_qualified_source(protocol)
        {
            Self::connect_selected(socket, deadline, protocol)?
        } else {
            return Err(ControlError::UnsupportedVersion { actual: protocol });
        };
        peer.verify(client.proof())
            .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))?;
        Ok(client)
    }

    fn connect_authenticated(
        socket: &Path,
        deadline: Instant,
        handshake: impl FnOnce(UnixStream) -> Result<ControlClient, ControlError>,
    ) -> Result<Self, ControlError> {
        let stream = crate::control::connect_control_socket(
            socket,
            deadline.saturating_duration_since(Instant::now()),
        )
        .map_err(ControlError::Connect)?;
        let peer = Peer::observe(&stream, socket)
            .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))?;
        let client = handshake(stream)?;
        peer.verify(client.proof())
            .map_err(|error| ControlError::Configure(io::Error::other(error.to_string())))?;
        Ok(Self {
            client,
            peer,
            busy: BusyPatience::PER_REQUEST,
            busy_pause: SOURCE_BUSY_PAUSE,
            attempt_allowance: SOURCE_ATTEMPT_ALLOWANCE,
        })
    }

    /// Recheck the authenticated peer before each source request.
    fn verified(&mut self) -> Result<&mut ControlClient, UpdateError> {
        self.peer.verify(self.client.proof())?;
        Ok(&mut self.client)
    }

    /// Sets how long this client asks again a request the source refuses as
    /// busy, so a caller can share one deadline across several requests.
    pub(crate) fn with_busy_patience(mut self, busy: BusyPatience) -> Self {
        self.busy = busy;
        self
    }

    /// Sends `request`, which the source refuses before admitting anything
    /// while it finishes another operation, asking it again on the same
    /// connection within this client's busy patience. Ownership is rechecked
    /// before every attempt. Under a shared deadline, no attempt is sent
    /// unless its admission wait and a prompt reply fit before that deadline. A source still busy when
    /// patience ends is reported as [`UpdateError::SourceBusy`] with
    /// `still_busy`, naming the authenticated daemon; any other reply is final
    /// at once.
    fn asked_while_busy<T>(
        &mut self,
        mut request: impl FnMut(&mut ControlClient) -> Result<T, ControlError>,
        still_busy: &str,
    ) -> Result<T, UpdateError> {
        let (give_up, pause) = (self.busy.give_up(), self.busy_pause);
        let shared = matches!(self.busy, BusyPatience::Until(_));
        let allowance = self.attempt_allowance;
        let reply = retrying_busy(
            || {
                // Rechecked even when no time is left, so a busy result only
                // ever names a daemon that still owns the source.
                let client = self.verified().map_err(Asked::Unverified)?;
                if shared && Instant::now() + allowance > give_up {
                    return Err(Asked::OutOfTime);
                }
                request(client).map_err(Asked::Replied)
            },
            Asked::is_busy,
            give_up,
            pause,
        );
        match reply {
            Ok(reply) => Ok(reply),
            Err(Asked::Unverified(error)) => Err(error),
            Err(Asked::OutOfTime) => Err(UpdateError::SourceBusy {
                serving: Box::new(self.proof().clone()),
                message: still_busy.into(),
            }),
            Err(Asked::Replied(error)) if error.is_daemon_busy() => Err(UpdateError::SourceBusy {
                serving: Box::new(self.proof().clone()),
                message: still_busy.into(),
            }),
            Err(Asked::Replied(error)) => Err(invalid(error.to_string())),
        }
    }

    /// The authenticated local socket peer, for a read-only shutdown survivor
    /// census. This is a daemon PID, never a terminal-child PID from List.
    pub(crate) fn peer_pid(&self) -> u32 {
        self.peer.pid
    }

    pub(crate) fn protocol(&self) -> u16 {
        self.client.protocol()
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        self.client.proof()
    }

    /// The pidfd-pinned, lease-authenticated peer's executable. Installation
    /// pointers cannot establish which binary is actually serving this roster.
    pub(crate) fn serving_executable(&self) -> Result<PathBuf, UpdateError> {
        self.peer.verify(self.client.proof())?;
        let executable = self.peer.executable()?;
        self.peer.verify(self.client.proof())?;
        Ok(executable)
    }

    pub(crate) fn upgrade_impact(
        &mut self,
        policy: impact::Policy,
    ) -> Result<impact::DaemonImpact, UpdateError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        self.verified()?
            .upgrade_impact_with_deadline(policy, deadline)
            .map_err(|error| invalid(error.to_string()))
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
        let pause = self.busy_pause;
        prepare_retrying_busy(
            || {
                let client = self.verified().map_err(PrepareError::Invalid)?;
                prepare_reply(client.prepare_confirmed_upgrade(impact.clone(), relaunch_agents))
            },
            PREPARE_BUSY_PATIENCE,
            pause,
        )
    }
}

impl Client {
    pub(crate) fn list(
        &mut self,
    ) -> Result<Vec<crate::terminal_ipc::TerminalDescriptor>, UpdateError> {
        self.verified()?
            .list()
            .map_err(|error| invalid(error.to_string()))
    }

    /// The capability a service stop confirms. Preflight only issues an
    /// expiring capability, and a busy source refuses it before issuing
    /// anything, so it is asked again while busy.
    pub(crate) fn shutdown_preflight(
        &mut self,
    ) -> Result<crate::terminal_ipc::ShutdownCapability, UpdateError> {
        let preflight =
            self.asked_while_busy(ControlClient::shutdown_preflight, STILL_BUSY_TO_STOP)?;
        self.peer.verify(self.client.proof())?;
        Ok(preflight.capability)
    }

    pub(crate) fn confirm_shutdown(
        &mut self,
        capability: crate::terminal_ipc::ShutdownCapability,
    ) -> Result<crate::terminal_ipc::ShutdownOutcome, UpdateError> {
        if &capability.proof != self.proof() {
            return Err(invalid("The source changed before its confirmed stop"));
        }
        // The gate refuses a busy stop before reading its capability, so the
        // same capability is offered again; one that expires meanwhile is
        // answered as Expired, and the review starts over. Ownership is not
        // rechecked after the reply: an accepted stop lets the source exit.
        self.asked_while_busy(
            |client| client.confirm_shutdown(capability.clone()),
            STILL_BUSY_TO_STOP,
        )
    }

    /// The conversations the source lists, as it issued them.
    pub(crate) fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError> {
        self.verified()?
            .list_chats()
            .map_err(|error| invalid(error.to_string()))
    }

    /// A daemon-bound monotonic fence for the split terminal/chat inventory
    /// RPCs. Preflight only issues an ephemeral capability; it never stops,
    /// adopts or replays a session. No capability is confirmed by this reader.
    pub(crate) fn inventory_generation(&mut self) -> Result<u64, UpdateError> {
        let preflight =
            self.asked_while_busy(ControlClient::shutdown_preflight, STILL_BUSY_TO_CHECK)?;
        self.peer.verify(self.client.proof())?;
        if preflight.capability.proof != *self.client.proof() {
            return Err(invalid("The inventory fence belongs to another daemon"));
        }
        Ok(preflight.capability.inventory_generation)
    }

    /// Every admitted source has the exact recorded StartupState request,
    /// response and nested phase DTO in LIFECYCLE_VARIANTS.
    pub(crate) fn startup_state(
        &mut self,
    ) -> Result<crate::terminal_ipc::StartupState, UpdateError> {
        self.verified()?
            .startup_state()
            .map_err(|e| invalid(e.to_string()))
    }

    /// Capture history once, immediately before a confirmed service stop. The
    /// returned snapshot is not evidence that shutdown was accepted.
    pub(crate) fn capture_chat_snapshot(
        &mut self,
        session: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        self.verified()?
            .capture_chat_snapshot(session)
            .map_err(|error| invalid(error.to_string()))
    }

    /// Ends one listed conversation through the source's own authenticated
    /// close: attached, as any client the source did not open must be, and
    /// answered only once the source confirms its provider's teardown.
    pub(crate) fn close_chat(
        &mut self,
        session: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        self.verified()?
            .close_chat_with_snapshot(session)
            .map_err(|error| invalid(error.to_string()))
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
/// include status reads. StartupState and every nested phase are frozen for
/// readiness checks on every qualified source. A source sends the chat
/// events in [`UNSOLICITED_SINCE`] unasked once its protocol has them.
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
            "ResolveUpgradeChat",
            "Shutdown",
            "ShutdownV1",
            "PrepareUpgrade",
            "UpgradeImpact",
            "PrepareConfirmedUpgrade",
            "BuildInfo",
            "StartupState",
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
            "detached",
            "accepted",
            "upgrade_chat_resolved",
            "shutdown_accepted",
            "shutdown_preflight",
            "shutdown_outcome",
            "upgrade_impact",
            "upgrade_prepared",
            "build_info",
            "startup_state",
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

/// Chat events a source sends attached clients unasked, from the protocol
/// that introduced each. The source clients accept them, so a source serving
/// one must send it as this build reads it.
#[cfg(test)]
pub(crate) const UNSOLICITED_SINCE: &[(&str, &str, u16)] = &[
    ("ServerMessage", "chat_limit", 160),
    ("ServerMessage", "chat_agent_addresses", 173),
    ("ServerMessage", "chat_active_model", 182),
    ("ServerMessage", "terminal_worker", 183),
];

/// Lifecycle requests and their replies that the source clients use only with
/// a source whose protocol has them, from the protocol that introduced each.
/// From there they are frozen like the rest of [`LIFECYCLE_VARIANTS`].
#[cfg(test)]
pub(crate) const LIFECYCLE_SINCE: &[(&str, &str, u16)] = &[
    ("ClientRequest", "ListPage", 190),
    ("ServerResponse", "listed_page", 190),
];

/// An installed build a live update may start from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReleasedSource {
    pub(crate) protocol: u16,
    /// The build users run it as, which the refusal names.
    pub(crate) release: &'static str,
}

/// The installed builds live updates start from, besides the current matched
/// release, by the protocol their daemons serve. Each is pinned by its own
/// number: bumping `PROTOCOL_VERSION` must not drop the build users are
/// running, as 174 once dropped 173. Retire one only on purpose, by moving it
/// to [`RETIRED_SOURCES`]; `scripts/check-update-sources` requires every
/// recorded protocol from the oldest pinned one up to the current one to be
/// in one of the two lists.
///
/// Historical DTO recordings do not grant admission: every source also needs
/// the complete readiness, census and recovery flow, and a rehearsal.
pub(crate) const RELEASED_SOURCES: &[ReleasedSource] = &[
    ReleasedSource {
        protocol: 156,
        release: "desktop 0.1.3+local.991daa43e",
    },
    ReleasedSource {
        protocol: 173,
        release: "0.1.3+local.43678f2fb",
    },
    ReleasedSource {
        protocol: 175,
        release: "0.1.3+local.c73002744",
    },
    ReleasedSource {
        protocol: 177,
        release: "0.1.3+local.374370965",
    },
    ReleasedSource {
        protocol: 178,
        // The published 3b1e81a4d and main's f016f047b share this protocol.
        release: "0.1.3+local.3b1e81a4d",
    },
    ReleasedSource {
        protocol: 179,
        release: "0.1.3+local.43f253ef8",
    },
    ReleasedSource {
        protocol: 187,
        release: "0.1.3+local.ccd4134c6",
    },
    ReleasedSource {
        protocol: 191,
        // The maintainer's install before the first signed release, 0.2.0.
        release: "0.1.3+local.e8bbb84f2",
    },
    ReleasedSource {
        protocol: 198,
        // The signed releases 0.2.0 to 0.2.3 all serve it.
        release: "0.2.0–0.2.3",
    },
];

/// Protocols main served since the oldest pinned source that live updates
/// deliberately do not start from, and why.
#[cfg(test)]
pub(crate) const RETIRED_SOURCES: &[(u16, &str)] = &[
    (160, "superseded before update sources were pinned"),
    (165, "superseded before update sources were pinned"),
    (166, "superseded before update sources were pinned"),
    (172, "superseded before update sources were pinned"),
    (174, "main served it under a day; never installed"),
    (176, "main served it under a day; never installed"),
    (
        180,
        "intermediate #2783 protocol; superseded by 181 without a qualified installed update source",
    ),
    (
        181,
        "intermediate #2815 protocol; superseded by 182 without a qualified installed update source",
    ),
    (
        182,
        "intermediate #2801 protocol; superseded by 183 without a qualified installed update source",
    ),
    (
        183,
        "intermediate #2798 protocol; superseded by 184 without a qualified installed update source",
    ),
    (
        184,
        "intermediate #2621 protocol; superseded by 185 without a qualified installed update source",
    ),
    (
        185,
        "intermediate #2475 protocol; superseded by 186 without a qualified installed update source",
    ),
    (
        186,
        "intermediate #2491 protocol; superseded by 187 without a qualified installed update source",
    ),
    (
        188,
        "intermediate #3075 protocol; superseded by 189 without a qualified installed update source",
    ),
    (
        189,
        "intermediate #3149 protocol; superseded by 190 without a qualified installed update source",
    ),
    (
        190,
        "intermediate #3209 protocol; superseded by 191 without a qualified installed update source",
    ),
    (
        193,
        "intermediate #3547 protocol; superseded by 194 without a qualified installed update source",
    ),
    (
        194,
        "intermediate #3459 protocol; superseded by 195 without a qualified installed update source",
    ),
    (
        195,
        "intermediate #3510 protocol; superseded by 196 without a qualified installed update source",
    ),
    (
        196,
        "intermediate #3670 protocol; superseded by 197 without a qualified installed update source",
    ),
    (
        197,
        "intermediate #3708 protocol; superseded by 198 without a qualified installed update source",
    ),
    (
        199,
        "intermediate #3777 protocol; superseded by 200 without a qualified installed update source",
    ),
    (
        200,
        "intermediate #3774 protocol; superseded by 201 without a qualified installed update source",
    ),
    (
        201,
        "intermediate #3672 protocol; superseded by 202 without a qualified installed update source",
    ),
    (
        202,
        "intermediate #3840 protocol; no release qualified as a live-update source",
    ),
];

#[cfg(test)]
pub(crate) fn protocols() -> impl Iterator<Item = u16> {
    RELEASED_SOURCES
        .iter()
        .map(|source| source.protocol)
        .filter(|&protocol| protocol != crate::terminal_ipc::PROTOCOL_VERSION)
        .chain([crate::terminal_ipc::PROTOCOL_VERSION])
}

/// Whether a live update may start from a daemon serving this protocol. Every
/// qualified source speaks this release's wire for the frozen lifecycle and
/// inspector subset, so one client serves them all.
pub(crate) fn is_qualified_source(protocol: u16) -> bool {
    protocol == crate::terminal_ipc::PROTOCOL_VERSION
        || RELEASED_SOURCES
            .iter()
            .any(|source| source.protocol == protocol)
}

pub(super) fn unsupported_source(protocol: u16) -> UpdateError {
    let release = if protocol > crate::terminal_ipc::PROTOCOL_VERSION {
        " Choose a newer release for the cold install."
    } else {
        ""
    };
    let released = RELEASED_SOURCES
        .iter()
        .map(|source| format!("protocol {} ({})", source.protocol, source.release))
        .collect::<Vec<_>>()
        .join(", ");
    invalid(format!(
        "The running cagents service uses protocol {protocol}; live updates support only {released} and protocol {}. This source check did not request preparation or shutdown, or create an update checkpoint.{release} To cold-install safely: save your work, finish or close every hosted session, and quit every cagents window. Using the running build's matching CLI, run `cagents --no-start daemon stop --require-empty` (it refuses while sessions remain), then verify `cagents --no-start daemon status` reports stopped. Run the downloaded release's `sh /path/to/install.sh` without --restart-daemon, then reopen cagents.",
        crate::terminal_ipc::PROTOCOL_VERSION,
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
    match discovery::discover_protocol_and_peer(&socket, deadline) {
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
            "busy",
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
                } else if outcome == "busy" {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::Internal,
                        crate::terminal_ipc::DAEMON_BUSY,
                        true,
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
                "busy" => assert!(matches!(reply, Err(PrepareError::Busy(_)))),
                _ => assert!(matches!(reply, Err(PrepareError::Unconfirmed(_)))),
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn a_busy_source_is_asked_again_on_the_same_connection_and_then_prepares() {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("busy-source"),
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
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let server = std::thread::spawn(move || {
            let reply = |peer: &mut UnixStream, request_id, message| {
                write_frame(
                    peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            reply(
                &mut peer,
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            let mut prepares = Vec::new();
            for attempt in 0..3 {
                let prepare: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(
                    prepare.request,
                    ClientRequest::PrepareConfirmedUpgrade { .. }
                ));
                prepares.push(prepare.request_id);
                let message = if attempt < 2 {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::Internal,
                        crate::terminal_ipc::DAEMON_BUSY,
                        true,
                    ))
                } else {
                    ServerMessage::Response {
                        response: ServerResponse::UpgradePrepared {
                            terminals: 2,
                            chats: 1,
                        },
                    }
                };
                reply(&mut peer, prepare.request_id, message);
            }
            prepares
        });
        let mut client = ControlClient::from_stream(stream).unwrap();
        let reply = prepare_retrying_busy(
            || prepare_reply(client.prepare_confirmed_upgrade(daemon.clone(), false)),
            Duration::from_secs(30),
            Duration::from_millis(1),
        );
        assert_eq!(reply.unwrap(), (2, 1));
        let prepares = server.join().unwrap();
        assert_eq!(prepares.len(), 3);
        assert!(prepares.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_busy_source_is_asked_again_until_it_answers_or_patience_ends() {
        let busy = || PrepareError::Busy(invalid("busy"));
        let mut attempts = 0;
        let reply = prepare_retrying_busy(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(busy())
                } else {
                    Ok((1, 2))
                }
            },
            Duration::from_secs(30),
            Duration::from_millis(1),
        );
        assert_eq!((reply.unwrap(), attempts), ((1, 2), 3));

        // Other outcomes are final at once: retrying a refusal or an
        // unconfirmed preparation would not be safe.
        for outcome in [
            PrepareError::Refused(invalid("refused")),
            PrepareError::Unconfirmed(invalid("lost")),
        ] {
            let mut outcome = Some(outcome);
            let mut attempts = 0;
            let reply = prepare_retrying_busy(
                || {
                    attempts += 1;
                    Err(outcome.take().expect("asked again after a final outcome"))
                },
                Duration::from_secs(30),
                Duration::from_millis(1),
            );
            assert!(reply.is_err() && attempts == 1);
        }

        let mut attempts = 0;
        let reply = prepare_retrying_busy(
            || {
                attempts += 1;
                Err(busy())
            },
            Duration::from_millis(50),
            Duration::from_millis(10),
        );
        assert!(matches!(reply, Err(PrepareError::Busy(_))));
        assert!((1..=5).contains(&attempts), "{attempts} attempts");
    }

    /// A source client over a socket pair whose peer is this test process,
    /// which holds a private daemon lease recorded for the client's proof.
    struct LeasedSource {
        client: Client,
        _lease: File,
        /// Each request the source received, in order.
        asked: std::sync::mpsc::Receiver<ClientRequest>,
        server: std::thread::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }

    fn lease_proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("busy-source"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("daemon"),
        }
    }

    fn write_lease(lease: &File, proof: &HostSessionProof) {
        use std::io::{Seek, SeekFrom};
        lease.set_len(0).unwrap();
        (&*lease).seek(SeekFrom::Start(0)).unwrap();
        serde_json::to_writer(
            lease,
            &serde_json::json!({"pid": std::process::id(), "proof": proof}),
        )
        .unwrap();
    }

    /// Answers each request with the next of `replies` on one connection.
    /// `before_reply` runs on the lease just before the reply with its index
    /// is written.
    fn leased_source(
        replies: Vec<ServerMessage>,
        before_reply: Option<(usize, fn(&File))>,
    ) -> LeasedSource {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let lease = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(directory.path().join("cagentsd.lock"))
            .unwrap();
        // SAFETY: flock receives a live descriptor and no pointers.
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let proof = lease_proof();
        write_lease(&lease, &proof);
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (sender, asked) = std::sync::mpsc::channel();
        let served = proof.clone();
        let held = lease.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let reply = |peer: &mut UnixStream, request_id, message| {
                write_frame(
                    peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request_id),
                        proof: served.clone(),
                        message,
                    },
                )
                .unwrap();
            };
            let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            reply(
                &mut peer,
                connect.request_id,
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: served.clone(),
                        resumed: Vec::new(),
                    },
                },
            );
            for (index, message) in replies.into_iter().enumerate() {
                let Ok(Some(request)) = read_frame::<ClientEnvelope>(&mut peer) else {
                    return;
                };
                let _ = sender.send(request.request);
                if let Some((at, change)) = before_reply
                    && at == index
                {
                    change(&held);
                }
                reply(&mut peer, request.request_id, message);
            }
        });
        let socket = directory.path().join("cagentsd.sock");
        let observed = Peer::observe(&stream, &socket).unwrap();
        let client = Client {
            client: ControlClient::from_stream(stream).unwrap(),
            peer: observed,
            busy: BusyPatience::PER_REQUEST,
            busy_pause: Duration::from_millis(1),
            attempt_allowance: Duration::ZERO,
        };
        assert_eq!(client.proof(), &proof);
        LeasedSource {
            client,
            _lease: lease,
            asked,
            server,
            _directory: directory,
        }
    }

    impl LeasedSource {
        /// Ends the connection and returns every request the source received.
        fn finish(self) -> Vec<ClientRequest> {
            drop(self.client);
            self.server.join().unwrap();
            self.asked.try_iter().collect()
        }
    }

    fn busy() -> ServerMessage {
        ServerMessage::Error(ProtocolError::new(
            ProtocolErrorCode::Internal,
            crate::terminal_ipc::DAEMON_BUSY,
            true,
        ))
    }

    fn capability(generation: u64) -> ShutdownCapability {
        ShutdownCapability {
            proof: lease_proof(),
            token: "fence".into(),
            inventory_generation: generation,
            live_terminals: 0,
            background_subagents: 0,
            background_identity: String::new(),
            expires_at_unix_ms: i64::MAX,
        }
    }

    fn preflight(generation: u64) -> ServerMessage {
        ServerMessage::Response {
            response: ServerResponse::ShutdownPreflight(crate::terminal_ipc::ShutdownPreflight {
                capability: capability(generation),
            }),
        }
    }

    /// The inventory fence is refused before anything is issued while the
    /// daemon finishes another operation, so a recovery check asks again on
    /// the same connection instead of reporting the daemon's terminals missing.
    #[test]
    fn a_busy_source_is_asked_again_for_its_inventory_fence() {
        let mut source = leased_source(vec![busy(), busy(), preflight(7)], None);
        assert_eq!(source.client.inventory_generation().unwrap(), 7);
        assert_eq!(source.finish().len(), 3);
    }

    /// A confirmed stop refused as busy was refused before its capability was
    /// read, so the same capability is offered again and can be accepted.
    #[test]
    fn a_busy_stop_offers_the_same_capability_again() {
        let accepted = ServerMessage::Response {
            response: ServerResponse::ShutdownOutcome(
                crate::terminal_ipc::ShutdownOutcome::Accepted {
                    live_terminals_before: 1,
                    termination_requested: true,
                },
            ),
        };
        let mut source = leased_source(vec![busy(), accepted], None);
        let outcome = source.client.confirm_shutdown(capability(3)).unwrap();
        assert!(matches!(
            outcome,
            crate::terminal_ipc::ShutdownOutcome::Accepted { .. }
        ));
        let asked = source.finish();
        assert_eq!(asked.len(), 2);
        for request in asked {
            assert!(matches!(
                request,
                ClientRequest::ShutdownV1 {
                    operation: crate::terminal_ipc::ShutdownOperation::Confirm { capability: offered },
                    ..
                } if offered == capability(3)
            ));
        }
    }

    /// Ownership is rechecked before every attempt: a source whose lease
    /// changed while it was busy is never asked again.
    #[test]
    fn a_busy_source_is_reauthenticated_before_it_is_asked_again() {
        let mut source = leased_source(
            vec![busy(), preflight(7)],
            Some((0, |lease| {
                let taken = HostSessionProof {
                    host_incarnation: HostIncarnation::new("another-daemon"),
                    ..lease_proof()
                };
                write_lease(lease, &taken);
            })),
        );
        let error = source.client.inventory_generation().unwrap_err();
        assert!(error.to_string().contains("ownership changed"), "{error}");
        assert_eq!(source.finish().len(), 1);
    }

    /// A source still busy when patience ends names the daemon that refused
    /// and says that nothing changed. A shared deadline bounds a whole run of
    /// requests: once it has passed, the next request is not sent at all.
    #[test]
    fn a_source_still_busy_past_a_shared_deadline_says_nothing_changed() {
        let mut source = leased_source(vec![busy(); 200], None);
        source.client = source
            .client
            .with_busy_patience(BusyPatience::shared_for(Duration::from_millis(50)));
        source.client.busy_pause = Duration::from_millis(10);
        let started = Instant::now();
        let error = source.client.inventory_generation().unwrap_err();
        assert!(
            matches!(&error, UpdateError::SourceBusy { serving, message }
                if **serving == lease_proof() && message == STILL_BUSY_TO_CHECK),
            "{error:?}"
        );
        let fenced = source.asked.try_iter().count();
        assert!((2..=6).contains(&fenced), "{fenced} fences");
        // The retry stops once a pause would cross the deadline, which may
        // still be a few milliseconds ahead; be past it.
        std::thread::sleep(Duration::from_millis(60));
        let error = source.client.shutdown_preflight().unwrap_err();
        assert!(
            matches!(&error, UpdateError::SourceBusy { message, .. } if message == STILL_BUSY_TO_STOP)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(source.finish().is_empty());
    }

    /// Under a shared deadline an attempt is sent only if it can finish,
    /// admission wait included, before the deadline; a lone request with its
    /// own patience is always asked at least once.
    #[test]
    fn a_shared_deadline_sends_no_attempt_it_cannot_finish() {
        let mut source = leased_source(vec![preflight(7)], None);
        source.client.attempt_allowance = SOURCE_ATTEMPT_ALLOWANCE;
        source.client = source
            .client
            .with_busy_patience(BusyPatience::shared_for(Duration::from_secs(1)));
        let error = source.client.inventory_generation().unwrap_err();
        assert!(matches!(error, UpdateError::SourceBusy { .. }), "{error:?}");
        assert!(source.finish().is_empty());

        let mut source = leased_source(vec![preflight(7)], None);
        source.client.attempt_allowance = SOURCE_ATTEMPT_ALLOWANCE;
        source.client = source.client.with_busy_patience(BusyPatience::ONCE);
        assert_eq!(source.client.inventory_generation().unwrap(), 7);
        assert_eq!(source.finish().len(), 1);
    }

    /// Any refusal other than busy is final at once and keeps its own words.
    #[test]
    fn another_refusal_is_not_asked_again() {
        let mut source = leased_source(
            vec![ServerMessage::Error(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Not now",
                false,
            ))],
            None,
        );
        let error = source.client.shutdown_preflight().unwrap_err();
        assert!(matches!(&error, UpdateError::Invalid(message) if message.contains("Not now")));
        assert_eq!(source.finish().len(), 1);
    }
}

#[cfg(test)]
mod admission_message_tests {
    use super::*;

    #[test]
    fn live_sources_are_the_pinned_releases_and_the_current_release() {
        assert_eq!(
            protocols().collect::<Vec<_>>(),
            [
                156,
                173,
                175,
                177,
                178,
                179,
                187,
                191,
                198,
                crate::terminal_ipc::PROTOCOL_VERSION
            ]
        );
        for source in RELEASED_SOURCES {
            assert!(source.protocol < crate::terminal_ipc::PROTOCOL_VERSION);
            assert!(is_qualified_source(source.protocol));
        }
        for &(protocol, _) in RETIRED_SOURCES {
            assert!(!is_qualified_source(protocol), "protocol {protocol}");
        }
        for protocol in [0, 56, 68, 77, 119, 128, 160, 165, 174, 176, 180, 181, 182] {
            assert!(!is_qualified_source(protocol), "protocol {protocol}");
            let message = unsupported_source(protocol).to_string();
            assert!(message.contains(&format!("uses protocol {protocol}")));
            assert!(message.contains(&format!(
                "live updates support only protocol 156 (desktop 0.1.3+local.991daa43e), \
                 protocol 173 (0.1.3+local.43678f2fb), protocol 175 (0.1.3+local.c73002744), \
                 protocol 177 (0.1.3+local.374370965), protocol 178 (0.1.3+local.3b1e81a4d), \
                 protocol 179 (0.1.3+local.43f253ef8), protocol 187 (0.1.3+local.ccd4134c6), \
                 protocol 191 (0.1.3+local.e8bbb84f2), protocol 198 (0.2.0–0.2.3) and protocol {}.",
                crate::terminal_ipc::PROTOCOL_VERSION
            )));
            assert!(message.contains("did not request preparation or shutdown"));
            assert!(message.contains("--no-start daemon stop --require-empty"));
            assert!(message.contains("sh /path/to/install.sh"));
            assert!(!message.contains("Update it to a build serving protocol 156"));
        }
    }

    #[test]
    fn newer_service_names_a_newer_cold_install_without_stopping_it() {
        let message = unsupported_source(crate::terminal_ipc::PROTOCOL_VERSION + 1).to_string();
        assert!(message.contains("Choose a newer release for the cold install"));
        assert!(message.contains("did not request preparation or shutdown"));
    }
}
