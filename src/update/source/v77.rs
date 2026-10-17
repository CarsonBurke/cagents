//! Frozen protocol77 update subset. This module deliberately does not decode
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
}
impl Client {
    pub(super) fn connect(socket: &Path) -> Result<Self, UpdateError> {
        let mut stream = crate::control::connect_control_socket(socket, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let peer = Peer::observe(&stream, socket)?;
        // Postcard structs/tuples concatenate fields; enum tags are u32 varints.
        // Connect0 has exactly expected_host and an empty resume cursor vector.
        let bytes = postcard::to_stdvec(&(
            77u16,
            1u64,
            None::<Proof>,
            0u32,
            None::<Proof>,
            Vec::<()>::new(),
        ))
        .map_err(|_| invalid("Cannot encode the protocol77 source handshake"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        write(&mut stream, &bytes, deadline)?;
        let bytes = read(&mut stream, deadline)?;
        let (header, body) = header(&bytes, 1, None)?;
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
        })
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
        self.peer.verify(&self.proof)?;
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or_else(|| invalid("Source request identity exhausted"))?;
        let bytes = postcard::to_stdvec(&(77u16, id, Some(Proof::from(&self.proof)), tag, fields))
            .map_err(|_| invalid("Cannot encode the protocol77 source upgrade request"))?;
        let deadline = Instant::now() + timeout;
        write(&mut self.stream, &bytes, deadline)?;
        // Never repeat a partially written or unacknowledged preparation.
        let bytes = read(&mut self.stream, deadline).map_err(|_| invalid("The source upgrade response was not confirmed. Its checkpoint and ownership require inspection; the request was not retried"))?;
        let (header, body) = header(&bytes, id, Some(&self.proof))?;
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
        let bytes = self.request(36, Policy::from(policy), 9, Duration::from_secs(10))?;
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

fn header<'a>(
    bytes: &'a [u8],
    id: u64,
    proof: Option<&HostSessionProof>,
) -> Result<(ReplyHeader, &'a [u8]), UpdateError> {
    let (header, body) = postcard::take_from_bytes::<ReplyHeader>(bytes)
        .map_err(|_| invalid("Malformed protocol77 source upgrade response"))?;
    if header.version != 77 || header.request_id != Some(id) || header.message != 0 {
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
