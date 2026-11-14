//! Frozen protocol68/77 update subset. This module deliberately does not decode
//! current ServerEnvelope/Request layouts or permit ordinary session controls.
use super::{Peer, UpdateError, impact, invalid};
use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

/// Version discovery deliberately does not decode an error or session DTO.
/// Only the subsequent version-specific handshake can authenticate ownership.
pub(super) fn discover_protocol(
    socket: &Path,
    deadline: Instant,
) -> Result<u16, crate::control::ControlError> {
    let (bytes, _) = discover_reply(socket, deadline)?;
    discovered_version(&bytes).map_err(discovery_error)
}

/// Read the source protocol without decoding its DTOs. The caller must admit
/// it and authenticate a fresh handshake before inspecting the installation.
pub(super) fn discover_installed_protocol(
    socket: &Path,
    deadline: Instant,
) -> Result<(u16, Peer), crate::control::ControlError> {
    let (bytes, peer) = discover_reply(socket, deadline)?;
    let (version, _) = postcard::take_from_bytes::<u16>(&bytes)
        .map_err(|_| discovery_error(invalid("Malformed source version header")))?;
    Ok((version, peer))
}

fn discover_reply(
    socket: &Path,
    deadline: Instant,
) -> Result<(Vec<u8>, Peer), crate::control::ControlError> {
    let mut stream = crate::control::connect_control_socket(
        socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .map_err(crate::control::ControlError::Connect)?;
    let peer = Peer::observe(&stream, socket).map_err(discovery_error)?;
    let bytes = postcard::to_stdvec(&(
        crate::terminal_ipc::PROTOCOL_VERSION,
        1u64,
        None::<Proof>,
        0u32,
        None::<Proof>,
        Vec::<()>::new(),
    ))
    .map_err(|_| discovery_error(invalid("Cannot encode source version discovery")))?;
    write(&mut stream, &bytes, deadline).map_err(discovery_error)?;
    let bytes = read(&mut stream, deadline).map_err(discovery_error)?;
    Ok((bytes, peer))
}

fn discovery_error(error: UpdateError) -> crate::control::ControlError {
    crate::control::ControlError::Configure(io::Error::other(error.to_string()))
}

fn discovered_version(bytes: &[u8]) -> Result<u16, UpdateError> {
    let (version, _) = postcard::take_from_bytes::<u16>(bytes)
        .map_err(|_| invalid("Malformed source version header"))?;
    if !super::is_qualified_source(version) {
        return Err(super::unsupported_source(version));
    }
    Ok(version)
}

#[derive(Clone, Serialize, Deserialize)]
struct Proof {
    host_session_id: String,
    boot_id: String,
    host_incarnation: String,
}
impl From<&HostSessionProof> for Proof {
    fn from(proof: &HostSessionProof) -> Self {
        Self {
            host_session_id: proof.host_session_id.as_str().into(),
            boot_id: proof.boot_id.as_str().into(),
            host_incarnation: proof.host_incarnation.as_str().into(),
        }
    }
}
impl From<Proof> for HostSessionProof {
    fn from(proof: Proof) -> Self {
        Self {
            host_session_id: HostSessionId::new(proof.host_session_id),
            boot_id: BootId::new(proof.boot_id),
            host_incarnation: HostIncarnation::new(proof.host_incarnation),
        }
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum Policy {
    WhenIdle,
    Interrupt,
}
impl From<impact::Policy> for Policy {
    fn from(policy: impact::Policy) -> Self {
        match policy {
            impact::Policy::WhenIdle => Self::WhenIdle,
            impact::Policy::Interrupt => Self::Interrupt,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Capability {
    proof: Proof,
    token: String,
    inventory_generation: u64,
    live_terminals: u64,
    background_subagents: u64,
    background_identity: String,
    expires_at_unix_ms: i64,
}
#[derive(Serialize, Deserialize)]
enum Kind {
    Terminal,
    NativeChat,
}
#[derive(Serialize, Deserialize)]
enum Disposition {
    Continues,
    Resumes,
    WaitsForIdle,
    Interrupted,
    ResumePicker,
    Blocked,
}
#[derive(Serialize, Deserialize)]
struct Session {
    id: uuid::Uuid,
    kind: Kind,
    title: String,
    identity: String,
    disposition: Disposition,
    reason: String,
    interrupted_subagents: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Impact {
    capability: Capability,
    policy: Policy,
    sessions: Vec<Session>,
    blockers: Vec<String>,
}

impl From<impact::DaemonImpact> for Impact {
    fn from(value: impact::DaemonImpact) -> Self {
        let c = value.capability;
        Self {
            capability: Capability {
                proof: Proof::from(&c.proof),
                token: c.token,
                inventory_generation: c.inventory_generation,
                live_terminals: c.live_terminals,
                background_subagents: c.background_subagents,
                background_identity: c.background_identity,
                expires_at_unix_ms: c.expires_at_unix_ms,
            },
            policy: value.policy.into(),
            sessions: value
                .sessions
                .into_iter()
                .map(|s| Session {
                    id: s.id.into(),
                    kind: match s.kind {
                        impact::SessionKind::Terminal => Kind::Terminal,
                        impact::SessionKind::NativeChat => Kind::NativeChat,
                    },
                    title: s.title,
                    identity: s.identity,
                    disposition: match s.disposition {
                        impact::Disposition::Continues => Disposition::Continues,
                        impact::Disposition::Resumes => Disposition::Resumes,
                        impact::Disposition::WaitsForIdle => Disposition::WaitsForIdle,
                        impact::Disposition::Interrupted => Disposition::Interrupted,
                        impact::Disposition::ResumePicker => Disposition::ResumePicker,
                        impact::Disposition::Blocked => Disposition::Blocked,
                    },
                    reason: s.reason,
                    interrupted_subagents: s.interrupted_subagents,
                })
                .collect(),
            blockers: value.blockers,
        }
    }
}
impl From<Impact> for impact::DaemonImpact {
    fn from(value: Impact) -> Self {
        let c = value.capability;
        Self {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: c.proof.into(),
                token: c.token,
                inventory_generation: c.inventory_generation,
                live_terminals: c.live_terminals,
                background_subagents: c.background_subagents,
                background_identity: c.background_identity,
                expires_at_unix_ms: c.expires_at_unix_ms,
            },
            policy: match value.policy {
                Policy::WhenIdle => impact::Policy::WhenIdle,
                Policy::Interrupt => impact::Policy::Interrupt,
            },
            sessions: value
                .sessions
                .into_iter()
                .map(|s| impact::SessionImpact {
                    id: s.id.into(),
                    kind: match s.kind {
                        Kind::Terminal => impact::SessionKind::Terminal,
                        Kind::NativeChat => impact::SessionKind::NativeChat,
                    },
                    title: s.title,
                    identity: s.identity,
                    disposition: match s.disposition {
                        Disposition::Continues => impact::Disposition::Continues,
                        Disposition::Resumes => impact::Disposition::Resumes,
                        Disposition::WaitsForIdle => impact::Disposition::WaitsForIdle,
                        Disposition::Interrupted => impact::Disposition::Interrupted,
                        Disposition::ResumePicker => impact::Disposition::ResumePicker,
                        Disposition::Blocked => impact::Disposition::Blocked,
                    },
                    reason: s.reason,
                    interrupted_subagents: s.interrupted_subagents,
                })
                .collect(),
            blockers: value.blockers,
        }
    }
}

#[derive(Deserialize)]
struct ReplyHeader {
    version: u16,
    request_id: Option<u64>,
    proof: Proof,
    message: u32,
    response: u32,
}

pub(crate) struct Client {
    stream: UnixStream,
    peer: Peer,
    proof: HostSessionProof,
    next: u64,
    protocol: u16,
}
impl Client {
    // Protocols 68 and 77 share this frozen lifecycle subset. Their inspector
    // DTOs differ and keep separate decoders; lifecycle exchanges no snapshots.
    // No ordinary requests, spawn budgets or notification DTOs are admitted.
    pub(super) fn connect_at_protocol(
        socket: &Path,
        deadline: Instant,
        protocol: u16,
    ) -> Result<Self, UpdateError> {
        if !matches!(
            super::client_family(protocol),
            Some(super::ClientFamily::V68 | super::ClientFamily::V77)
        ) {
            return Err(invalid("Unsupported source update protocol"));
        }
        let mut stream = crate::control::connect_control_socket(
            socket,
            deadline.saturating_duration_since(Instant::now()),
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let peer = Peer::observe(&stream, socket)?;
        // Postcard structs/tuples concatenate fields; enum tags are u32 varints.
        // Connect0 has exactly expected_host and an empty resume cursor vector.
        let bytes = postcard::to_stdvec(&(
            protocol,
            1u64,
            None::<Proof>,
            0u32,
            None::<Proof>,
            Vec::<()>::new(),
        ))
        .map_err(|_| invalid("Cannot encode the protocol77 source handshake"))?;
        write(&mut stream, &bytes, deadline)?;
        let bytes = read(&mut stream, deadline)?;
        let (header, body) = header_at_protocol(&bytes, 1, None, protocol)?;
        if header.response != 0 {
            return Err(invalid(
                "The source did not complete its protocol77 upgrade handshake",
            ));
        }
        let (proof, rest) = postcard::take_from_bytes::<Proof>(body)
            .map_err(|_| invalid("Malformed protocol77 source proof"))?;
        let (resumed, rest) = postcard::take_from_bytes::<u32>(rest)
            .map_err(|_| invalid("Malformed protocol77 source resume list"))?;
        let proof = HostSessionProof::from(proof);
        if resumed != 0
            || !rest.is_empty()
            || proof != HostSessionProof::from(header.proof)
            || !proof.is_complete()
        {
            return Err(invalid(
                "The protocol77 source handshake returned inconsistent ownership",
            ));
        }
        peer.verify(&proof)?;
        Ok(Self {
            stream,
            peer,
            proof,
            next: 2,
            protocol,
        })
    }

    pub(super) fn protocol(&self) -> u16 {
        self.protocol
    }

    pub(super) fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    fn request<T: Serialize>(
        &mut self,
        tag: u32,
        fields: T,
        response: u32,
        timeout: Duration,
    ) -> Result<Vec<u8>, UpdateError> {
        let deadline = Instant::now() + timeout;
        self.peer.verify(&self.proof)?;
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or_else(|| invalid("Source request identity exhausted"))?;
        let bytes = postcard::to_stdvec(&(
            self.protocol,
            id,
            Some(Proof::from(&self.proof)),
            tag,
            fields,
        ))
        .map_err(|_| invalid("Cannot encode the protocol77 source upgrade request"))?;
        write(&mut self.stream, &bytes, deadline)?;
        // Never repeat a partially written or unacknowledged preparation.
        let bytes = read(&mut self.stream, deadline).map_err(|error| {
            if tag == 36 {
                invalid(format!("The read-only update preview was not confirmed: {error}. This request does not checkpoint or stop sessions, and was not retried"))
            } else {
                invalid("The source upgrade response was not confirmed. Its checkpoint and ownership require inspection; the request was not retried")
            }
        })?;
        let (header, body) = header_at_protocol(&bytes, id, Some(&self.proof), self.protocol)?;
        if header.response != response {
            return Err(invalid(
                "The protocol77 source returned an unexpected upgrade response",
            ));
        }
        Ok(body.to_vec())
    }

    pub(super) fn upgrade_impact(
        &mut self,
        policy: impact::Policy,
    ) -> Result<impact::DaemonImpact, UpdateError> {
        let bytes = self.request(36, Policy::from(policy), 9, Duration::from_secs(30))?;
        let observed: Impact = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol77 upgrade impact"))?;
        let observed = impact::DaemonImpact::from(observed);
        if observed.capability.proof != self.proof
            || observed.policy != policy
            || observed.sessions.len() > 1152
        {
            return Err(invalid(
                "The protocol77 source impact does not match its authenticated inventory",
            ));
        }
        Ok(observed)
    }

    pub(super) fn prepare_confirmed_upgrade(
        &mut self,
        impact: impact::DaemonImpact,
        relaunch_agents: bool,
    ) -> Result<(u64, u64), UpdateError> {
        let bytes = self.request(
            37,
            (Impact::from(impact), relaunch_agents),
            17,
            Duration::from_secs(60),
        )?;
        let counts: (u64, u64) = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol77 checkpoint receipt"))?;
        if counts.0 > 1024 || counts.1 > 128 {
            return Err(invalid(
                "The source checkpoint receipt exceeds its declared inventory limits",
            ));
        }
        Ok(counts)
    }
}

fn header_at_protocol<'a>(
    bytes: &'a [u8],
    id: u64,
    proof: Option<&HostSessionProof>,
    protocol: u16,
) -> Result<(ReplyHeader, &'a [u8]), UpdateError> {
    let (header, body) = postcard::take_from_bytes::<ReplyHeader>(bytes)
        .map_err(|_| invalid("Malformed protocol77 source upgrade response"))?;
    if header.version != protocol || header.request_id != Some(id) || header.message != 0 {
        return Err(invalid(
            "The source upgrade response changed its protocol or request identity",
        ));
    }
    let actual = HostSessionProof::from(header.proof.clone());
    if !actual.is_complete() || proof.is_some_and(|proof| *proof != actual) {
        return Err(invalid(
            "The protocol77 source ownership changed during upgrade",
        ));
    }
    Ok((header, body))
}

#[cfg(test)]
fn header<'a>(
    bytes: &'a [u8],
    id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<(ReplyHeader, &'a [u8]), UpdateError> {
    header_at_protocol(bytes, id, proof, 77)
}

struct DeadlineStream<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl DeadlineStream<'_> {
    fn remaining(&self) -> io::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "source operation deadline elapsed",
            ))
        } else {
            Ok(remaining)
        }
    }
}
impl Read for DeadlineStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn write(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<(), UpdateError> {
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(invalid(
            "The protocol77 source request exceeds its bounded frame limit",
        ));
    }
    let mut stream = DeadlineStream { stream, deadline };
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(bytes)?;
    Ok(())
}
fn read(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>, UpdateError> {
    let mut stream = DeadlineStream { stream, deadline };
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 || length > 8 * 1024 * 1024 {
        return Err(invalid(
            "The protocol77 source exceeded its bounded frame limit",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ipc::{
        ClientEnvelope, ClientRequest, ServerEnvelope, ServerMessage, ServerResponse,
    };

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("source-host"),
            boot_id: BootId::new("source-boot"),
            host_incarnation: HostIncarnation::new("source-incarnation"),
        }
    }

    fn impact() -> impact::DaemonImpact {
        impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: proof(),
                token: "exact-reviewed-capability".into(),
                inventory_generation: 7,
                live_terminals: 1,
                background_subagents: 1,
                background_identity: "exact-background".into(),
                expires_at_unix_ms: 123,
            },
            policy: impact::Policy::Interrupt,
            sessions: vec![impact::SessionImpact {
                id: crate::model::TerminalId::new(),
                kind: impact::SessionKind::NativeChat,
                title: "Source conversation".into(),
                identity: "exact-source-hash".into(),
                disposition: impact::Disposition::Interrupted,
                reason: "User reviewed interruption".into(),
                interrupted_subagents: vec!["background-id".into()],
            }],
            blockers: Vec::new(),
        }
    }

    // Independent postcard oracle for the selected declarations at protocol68
    // (503a3319573c) and protocol77 (28ae28639). Client/ServerEnvelope,
    // ShutdownCapability and all of update/impact.rs are identical there.
    // Do not serialize today's wire DTOs to generate these expected bytes.
    fn frozen_uint(mut value: u64, bytes: &mut Vec<u8>) {
        while value >= 128 {
            bytes.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        bytes.push(value as u8);
    }

    fn frozen_text(value: &str, bytes: &mut Vec<u8>) {
        frozen_uint(value.len() as u64, bytes);
        bytes.extend_from_slice(value.as_bytes());
    }

    fn frozen_proof(value: &HostSessionProof, bytes: &mut Vec<u8>) {
        for field in [
            value.host_session_id.as_str(),
            value.boot_id.as_str(),
            value.host_incarnation.as_str(),
        ] {
            frozen_text(field, bytes);
        }
    }

    fn frozen_impact(value: &impact::DaemonImpact) -> Vec<u8> {
        let mut bytes = Vec::new();
        let capability = &value.capability;
        frozen_proof(&capability.proof, &mut bytes);
        frozen_text(&capability.token, &mut bytes);
        for field in [
            capability.inventory_generation,
            capability.live_terminals,
            capability.background_subagents,
        ] {
            frozen_uint(field, &mut bytes);
        }
        frozen_text(&capability.background_identity, &mut bytes);
        let expiry = capability.expires_at_unix_ms;
        frozen_uint(((expiry << 1) ^ (expiry >> 63)) as u64, &mut bytes);
        bytes.push(match value.policy {
            impact::Policy::WhenIdle => 0,
            impact::Policy::Interrupt => 1,
        });
        frozen_uint(value.sessions.len() as u64, &mut bytes);
        for session in &value.sessions {
            bytes.push(16); // UUID's non-human-readable byte string length.
            bytes.extend_from_slice(session.id.as_uuid().as_bytes());
            bytes.push(match session.kind {
                impact::SessionKind::Terminal => 0,
                impact::SessionKind::NativeChat => 1,
            });
            frozen_text(&session.title, &mut bytes);
            frozen_text(&session.identity, &mut bytes);
            bytes.push(match session.disposition {
                impact::Disposition::Continues => 0,
                impact::Disposition::Resumes => 1,
                impact::Disposition::WaitsForIdle => 2,
                impact::Disposition::Interrupted => 3,
                impact::Disposition::ResumePicker => 4,
                impact::Disposition::Blocked => 5,
            });
            frozen_text(&session.reason, &mut bytes);
            frozen_uint(session.interrupted_subagents.len() as u64, &mut bytes);
            for id in &session.interrupted_subagents {
                frozen_text(id, &mut bytes);
            }
        }
        frozen_uint(value.blockers.len() as u64, &mut bytes);
        for blocker in &value.blockers {
            frozen_text(blocker, &mut bytes);
        }
        bytes
    }

    #[cfg(feature = "host")]
    fn frozen_request(
        protocol: u16,
        id: u64,
        proof: Option<&HostSessionProof>,
        tag: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        frozen_uint(protocol.into(), &mut bytes);
        frozen_uint(id, &mut bytes);
        bytes.push(u8::from(proof.is_some()));
        if let Some(proof) = proof {
            frozen_proof(proof, &mut bytes);
        }
        bytes.push(tag);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn frozen_response(
        protocol: u16,
        id: u64,
        proof: &HostSessionProof,
        tag: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        frozen_uint(protocol.into(), &mut bytes);
        bytes.push(1); // Some(request_id).
        frozen_uint(id, &mut bytes);
        frozen_proof(proof, &mut bytes);
        bytes.extend([0, tag]); // ServerMessage::Response, response index.
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn frozen_68_and_77_lifecycle_payloads_match_historical_bytes() {
        for (index, disposition) in [
            impact::Disposition::Continues,
            impact::Disposition::Resumes,
            impact::Disposition::WaitsForIdle,
            impact::Disposition::Interrupted,
            impact::Disposition::ResumePicker,
            impact::Disposition::Blocked,
        ]
        .into_iter()
        .enumerate()
        {
            let mut expected = impact();
            expected.policy = if index % 2 == 0 {
                impact::Policy::WhenIdle
            } else {
                impact::Policy::Interrupt
            };
            expected.sessions[0].disposition = disposition;
            if index % 2 == 0 {
                expected.sessions[0].kind = impact::SessionKind::Terminal;
                expected.capability.expires_at_unix_ms = -123;
            }
            expected.capability.inventory_generation = 128;
            expected.blockers.push("Exact source blocker".into());
            let body = frozen_impact(&expected);
            assert_eq!(
                postcard::to_stdvec(&Impact::from(expected.clone())).unwrap(),
                body
            );
            assert_eq!(
                impact::DaemonImpact::from(postcard::from_bytes::<Impact>(&body).unwrap()),
                expected
            );
            for protocol in [68, 77] {
                let bytes = frozen_response(protocol, 2, &proof(), 9, &body);
                let (header, decoded) =
                    header_at_protocol(&bytes, 2, Some(&proof()), protocol).unwrap();
                assert_eq!(header.response, 9);
                assert_eq!(decoded, body);
                assert_eq!(
                    impact::DaemonImpact::from(postcard::from_bytes::<Impact>(decoded).unwrap()),
                    expected
                );
            }
        }
    }

    #[test]
    fn source_version_header_precedes_old_error_dto_and_rejects_unknown_versions() {
        // A version-mismatch reply from the immutable source may contain an
        // error variant/layout the new normal client cannot decode at all.
        for version in super::super::protocols() {
            let mut bytes = postcard::to_stdvec(&version).unwrap();
            bytes.extend_from_slice(&[255, 255, 255, 255, 255]);
            assert_eq!(discovered_version(&bytes).unwrap(), version);
            assert!(postcard::from_bytes::<ServerEnvelope>(&bytes).is_err());
        }
        for version in [0u16, 68, 77, 78, 119, 127, 128, 160, 165, u16::MAX] {
            if version != crate::terminal_ipc::PROTOCOL_VERSION {
                assert!(discovered_version(&postcard::to_stdvec(&version).unwrap()).is_err());
            }
        }
        assert!(discovered_version(&[]).is_err());
        assert!(discovered_version(&[0x80]).is_err());
    }

    #[cfg(feature = "host")]
    #[test]
    fn unqualified_165_refuses_after_discovery_without_preparing_or_writing_state() {
        use std::{fs, os::unix::net::UnixListener, process::Command, process::Stdio, thread};
        const ROOT: &str = "CAGENTS_UNQUALIFIED_SOURCE_TEST_ROOT";
        const TEST: &str = "update::source::v77::tests::unqualified_165_refuses_after_discovery_without_preparing_or_writing_state";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = std::path::PathBuf::from(root);
            let paths = crate::daemon::RuntimePaths {
                directory: root.clone(),
                socket: root.join("cagentsd.sock"),
                lock: root.join("cagentsd.lock"),
            };
            let mut source = proof();
            source.boot_id = crate::daemon::read_boot_id().unwrap();
            let _lease = crate::daemon::HostLease::acquire(&paths, &source).unwrap();
            let listener = UnixListener::bind(&paths.socket).unwrap();
            fs::write(root.join("ready"), []).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let request: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline).unwrap()).unwrap();
            assert!(
                matches!(request.request, ClientRequest::Connect { expected_host: None, ref resume } if resume.is_empty())
            );
            let mut reply = postcard::to_stdvec(&165u16).unwrap();
            reply.extend_from_slice(&[255, 255, 255]);
            write(&mut stream, &reply, deadline).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(
                stream.read(&mut [0]).unwrap(),
                0,
                "discovery must not send preparation"
            );
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock,
                "unqualified source must not receive an authenticated control connection"
            );
            // The parent polls for this file and then reads it: publish the
            // contents whole, so it can never observe a created, empty file.
            fs::write(root.join("observed.partial"), b"Connect only").unwrap();
            fs::rename(root.join("observed.partial"), root.join("observed")).unwrap();
            // Stay alive with the original lease until the parent finishes its
            // read-only assertions and closes this owned child's input pipe.
            assert_eq!(io::stdin().read(&mut [0]).unwrap(), 0);
            return;
        }
        struct OwnedMock(std::process::Child);
        impl Drop for OwnedMock {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        let retained = state.join("retained-checkpoint.json");
        let registry = state.join("workspace.sqlite3");
        fs::write(&retained, b"immutable existing checkpoint evidence").unwrap();
        fs::write(&registry, b"immutable registry evidence").unwrap();
        let snapshot = || {
            let mut files = fs::read_dir(&state)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<Vec<_>>();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        };
        let before = snapshot();
        let mut child = OwnedMock(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(ROOT, root)
                .env("XDG_DATA_HOME", &state)
                .env("XDG_CONFIG_HOME", &state)
                .env("XDG_RUNTIME_DIR", root)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let wait_for = |path: &Path| {
            while !path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "mock source did not reach {}",
                    path.display()
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for(&root.join("ready"));
        let error = super::super::Client::connect(&root.join("cagentsd.sock"))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("uses protocol 165"), "{error}");
        assert!(
            error.contains("--no-start daemon stop --require-empty"),
            "{error}"
        );
        wait_for(&root.join("observed"));
        assert_eq!(fs::read(root.join("observed")).unwrap(), b"Connect only");
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "refusal must keep the source alive"
        );
        assert_eq!(snapshot(), before);
        assert!(!state.join("daemon-upgrade.json").exists());
        drop(child.0.stdin.take());
        assert!(child.0.wait().unwrap().success());
    }

    /// Preserve the historical candidate line's frozen byte oracle. A
    /// recorded layout alone does not qualify it as a live update source.
    #[test]
    fn update_messages_keep_the_candidate_lines_variant_positions() {
        let tag = |bytes: Vec<u8>| bytes[0];
        let requests = [
            (
                ClientRequest::Connect {
                    expected_host: None,
                    resume: Vec::new(),
                },
                0,
            ),
            (ClientRequest::List, 1),
            (ClientRequest::ChatList, 29),
            (
                ClientRequest::UpgradeImpact {
                    policy: impact::Policy::WhenIdle,
                },
                36,
            ),
            (
                ClientRequest::PrepareConfirmedUpgrade {
                    impact: impact(),
                    relaunch_agents: false,
                },
                37,
            ),
        ];
        for (request, expected) in requests {
            assert_eq!(tag(postcard::to_stdvec(&request).unwrap()), expected);
        }
        let responses = [
            (
                ServerResponse::Connected {
                    proof: proof(),
                    resumed: Vec::new(),
                },
                0,
            ),
            (
                ServerResponse::Listed {
                    terminals: Vec::new(),
                },
                1,
            ),
            (
                ServerResponse::ChatListed {
                    sessions: Vec::new(),
                },
                13,
            ),
            (ServerResponse::UpgradeImpact(impact()), 9),
            (
                ServerResponse::UpgradePrepared {
                    terminals: 1,
                    chats: 1,
                },
                17,
            ),
        ];
        for (response, expected) in responses {
            assert_eq!(tag(postcard::to_stdvec(&response).unwrap()), expected);
        }
    }

    /// Every admitted current-wire source is reached through discovery and
    /// authenticated at its own protocol through preview and confirmed prepare.
    #[cfg(feature = "host")]
    #[test]
    fn authenticated_current_wire_sources_preview_and_prepare_at_their_own_protocol() {
        for protocol in super::super::current_client_protocols() {
            authenticated_current_wire_source(protocol);
        }
    }

    #[cfg(feature = "host")]
    fn authenticated_current_wire_source(protocol: u16) {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("cagentsd.sock");
        let paths = crate::daemon::RuntimePaths {
            directory: directory.path().to_owned(),
            socket: socket.clone(),
            lock: directory.path().join("cagentsd.lock"),
        };
        let mut pinned = proof();
        pinned.boot_id = crate::daemon::read_boot_id().unwrap();
        let expected_proof = pinned.clone();
        let _lease = crate::daemon::HostLease::acquire(&paths, &pinned).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let mut expected = impact();
        expected.capability.proof = pinned.clone();
        expected.policy = impact::Policy::WhenIdle;
        expected.sessions[0].disposition = impact::Disposition::Resumes;
        expected.sessions[0].interrupted_subagents.clear();
        let answer = expected.clone();
        let reviewed = expected.clone();
        let worker = std::thread::spawn(move || {
            let deadline = || Instant::now() + Duration::from_secs(2);
            let (mut discovery, _) = listener.accept().unwrap();
            let request: ClientEnvelope =
                postcard::from_bytes(&read(&mut discovery, deadline()).unwrap()).unwrap();
            assert_eq!(request.version, crate::terminal_ipc::PROTOCOL_VERSION);
            // The source refuses the newer number; only its header is read.
            let mut refusal = postcard::to_stdvec(&protocol).unwrap();
            refusal.extend_from_slice(&[255, 255, 255]);
            write(&mut discovery, &refusal, deadline()).unwrap();
            drop(discovery);
            let (mut stream, _) = listener.accept().unwrap();
            let reply = |stream: &mut UnixStream, request_id: u64, response: ServerResponse| {
                let envelope = ServerEnvelope {
                    version: protocol,
                    request_id: Some(request_id),
                    proof: pinned.clone(),
                    message: ServerMessage::Response { response },
                };
                write(stream, &postcard::to_stdvec(&envelope).unwrap(), deadline()).unwrap();
            };
            let connect: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(connect.version, protocol);
            assert!(matches!(connect.request, ClientRequest::Connect { .. }));
            reply(
                &mut stream,
                connect.request_id,
                ServerResponse::Connected {
                    proof: pinned.clone(),
                    resumed: Vec::new(),
                },
            );
            let preview: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(preview.version, protocol);
            assert_eq!(preview.proof.as_ref(), Some(&pinned));
            assert!(matches!(
                preview.request,
                ClientRequest::UpgradeImpact {
                    policy: impact::Policy::WhenIdle
                }
            ));
            reply(
                &mut stream,
                preview.request_id,
                ServerResponse::UpgradeImpact(answer),
            );
            let prepare: ClientEnvelope =
                postcard::from_bytes(&read(&mut stream, deadline()).unwrap()).unwrap();
            assert_eq!(prepare.version, protocol);
            match prepare.request {
                ClientRequest::PrepareConfirmedUpgrade {
                    impact,
                    relaunch_agents,
                } => {
                    assert_eq!(impact, reviewed);
                    assert!(!relaunch_agents);
                }
                _ => panic!("unexpected preparation request"),
            }
            reply(
                &mut stream,
                prepare.request_id,
                ServerResponse::UpgradePrepared {
                    terminals: 1,
                    chats: 1,
                },
            );
        });
        let mut client = super::super::Client::connect(&socket).unwrap();
        assert_eq!(client.protocol(), protocol);
        assert_eq!(client.proof(), &expected_proof);
        let layout = |version: &str| crate::update::InstallLayout {
            prefix: "/owned/install".into(),
            version_root: Path::new("/owned/install/versions").join(version),
            version: version.into(),
        };
        let current = layout("new");
        let previous = layout("old");
        assert_eq!(
            super::super::super::installed_candidate_version(
                &current,
                &current.version_root,
                Some(&previous),
                client.protocol(),
                Some(client.proof()),
            )
            .unwrap(),
            "new"
        );
        let observed = client.upgrade_impact(impact::Policy::WhenIdle).unwrap();
        assert_eq!(observed, expected);
        assert_eq!(
            client.prepare_confirmed_upgrade(observed, false).unwrap(),
            (1, 1)
        );
        drop(client);
        worker.join().unwrap();
    }

    #[test]
    fn frozen77_upgrade_subset_matches_declared_request_and_impact_layouts() {
        let proof = proof();
        for (tag, policy) in [
            (36u32, impact::Policy::WhenIdle),
            (36, impact::Policy::Interrupt),
        ] {
            let fixed = postcard::to_stdvec(&(
                77u16,
                3u64,
                Some(Proof::from(&proof)),
                tag,
                Policy::from(policy),
            ))
            .unwrap();
            let declared = postcard::to_stdvec(&ClientEnvelope {
                version: 77,
                request_id: 3,
                proof: Some(proof.clone()),
                request: ClientRequest::UpgradeImpact { policy },
            })
            .unwrap();
            assert_eq!(fixed, declared);
        }
        let observed = impact();
        let fixed = postcard::to_stdvec(&(
            77u16,
            4u64,
            Some(Proof::from(&proof)),
            37u32,
            (Impact::from(observed.clone()), false),
        ))
        .unwrap();
        let declared = postcard::to_stdvec(&ClientEnvelope {
            version: 77,
            request_id: 4,
            proof: Some(proof.clone()),
            request: ClientRequest::PrepareConfirmedUpgrade {
                impact: observed.clone(),
                relaunch_agents: false,
            },
        })
        .unwrap();
        assert_eq!(fixed, declared);
        let fixed_impact = postcard::to_stdvec(&Impact::from(observed.clone())).unwrap();
        assert_eq!(fixed_impact, postcard::to_stdvec(&observed).unwrap());
        assert_eq!(
            impact::DaemonImpact::from(postcard::from_bytes::<Impact>(&fixed_impact).unwrap()),
            observed
        );
        let connected = postcard::to_stdvec(&ServerEnvelope {
            version: 77,
            request_id: Some(1),
            proof: proof.clone(),
            message: ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: Vec::new(),
                },
            },
        })
        .unwrap();
        let (header, body) = header(&connected, 1, None).unwrap();
        assert_eq!(header.response, 0);
        let (actual, rest) = postcard::take_from_bytes::<Proof>(body).unwrap();
        assert_eq!(HostSessionProof::from(actual), proof);
        assert_eq!(rest, &[0]);
    }

    #[test]
    fn frozen77_rejects_changed_version_request_or_ownership_and_nonresponses() {
        for mutation in 0..4 {
            let mut envelope = ServerEnvelope {
                version: 77,
                request_id: Some(5),
                proof: proof(),
                message: ServerMessage::Response {
                    response: ServerResponse::UpgradePrepared {
                        terminals: 1,
                        chats: 1,
                    },
                },
            };
            match mutation {
                0 => envelope.version = 78,
                1 => envelope.request_id = Some(6),
                2 => envelope.proof.host_incarnation = HostIncarnation::new("unreviewed-owner"),
                _ => envelope.message = ServerMessage::HostShuttingDown,
            }
            assert!(header(&postcard::to_stdvec(&envelope).unwrap(), 5, Some(&proof())).is_err());
        }
    }

    #[test]
    fn source77_deadline_covers_trickled_prefix_and_payload_without_resetting() {
        for prefix_trickle in [true, false] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let worker = std::thread::spawn(move || {
                let frame = if prefix_trickle {
                    [4u32.to_be_bytes().as_slice(), b"body"].concat()
                } else {
                    server.write_all(&4u32.to_be_bytes()).unwrap();
                    b"body".to_vec()
                };
                for byte in frame {
                    if server.write_all(&[byte]).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(35));
                }
            });
            let result = read(&mut client, Instant::now() + Duration::from_millis(55));
            assert!(
                result.is_err(),
                "partial frame progress must not reset the absolute deadline"
            );
            drop(client);
            worker.join().unwrap();
        }
    }
}
