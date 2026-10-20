//! Deliberately restricted reader for the published protocol-77 source schema.
//! Never use this client for ordinary connections or provider attachment.
use crate::{
    chat::Session,
    model::HostSessionProof,
    terminal::ipc::{self, ClientRequest},
};
use serde::{
    Deserialize, Serialize,
    de::{self, EnumAccess, SeqAccess, VariantAccess, Visitor},
};
use std::{
    fmt,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

const VERSION: u16 = 77;
const MAX_FRAME: usize = 4 * 1024 * 1024;

pub(super) struct Client {
    stream: UnixStream,
    proof: HostSessionProof,
    next: u64,
    deadline: Instant,
}

/// These are the only requests the compatibility inspector can construct.
enum Request<'a> {
    Connect(&'a HostSessionProof),
    ListTerminals,
    ListChats,
    Resolve(&'a Session),
}
impl Serialize for Request<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The byte tests pin the indices to source 28ae28639. No version field
        // is overridden on a general-purpose ControlClient.
        match self {
            Self::Connect(proof) => ClientRequest::Connect {
                expected_host: Some((*proof).clone()),
                resume: vec![],
            }
            .serialize(serializer),
            Self::ListTerminals => ClientRequest::List.serialize(serializer),
            Self::ListChats => ClientRequest::ChatList.serialize(serializer),
            Self::Resolve(source) => ClientRequest::ResolveUpgradeChat {
                source: (*source).clone(),
            }
            .serialize(serializer),
        }
    }
}

#[derive(Serialize)]
struct RequestEnvelope<'a> {
    version: u16,
    request_id: u64,
    proof: Option<&'a HostSessionProof>,
    request: Request<'a>,
}

struct Envelope {
    request_id: Option<u64>,
    proof: HostSessionProof,
    message: Message,
}
impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Envelope;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a protocol-77 read-only response")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut fields: A) -> Result<Self::Value, A::Error> {
                let version: u16 = fields
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("missing source version"))?;
                if version != VERSION {
                    return Err(de::Error::custom("unsupported source protocol"));
                }
                Ok(Envelope {
                    request_id: fields
                        .next_element()?
                        .ok_or_else(|| de::Error::custom("missing request identity"))?,
                    proof: fields
                        .next_element()?
                        .ok_or_else(|| de::Error::custom("missing source proof"))?,
                    message: fields
                        .next_element()?
                        .ok_or_else(|| de::Error::custom("missing source response"))?,
                })
            }
        }
        deserializer.deserialize_struct(
            "ServerEnvelope",
            &["version", "request_id", "proof", "message"],
            Fields,
        )
    }
}

// Response is variant zero in 77. Notifications, errors and snapshots are all
// rejected before their payload can be interpreted using a newer schema.
#[derive(Deserialize)]
enum Message {
    Response { response: Response },
}
enum Response {
    Connected(HostSessionProof),
    Terminals(Vec<Terminal>),
    Listed(Vec<Session>),
    Resolved(Session),
}
// Field order and types below are the complete protocol-77 TerminalDescriptor
// layout from 28ae28639. It contains no terminal or native-chat snapshot.
#[derive(Deserialize)]
pub(super) struct Terminal {
    _screen_lost_after_crash: bool,
    pub(super) session: crate::model::TerminalSession,
    _generation: u64,
    _event_sequence: u64,
    _oldest_event_sequence: u64,
    _attached: bool,
    _transcript: Option<crate::model::TranscriptMetadata>,
    _activity: Option<crate::model::TerminalOutputActivity>,
    _agent_activity: Option<crate::agent::Activity>,
    pub(super) upgrade_sources: Vec<crate::model::TerminalSession>,
    _upgrade_relaunches: Vec<ipc::UpgradeRelaunch>,
}
impl From<ipc::TerminalDescriptor> for Terminal {
    fn from(value: ipc::TerminalDescriptor) -> Self {
        Self {
            _screen_lost_after_crash: value.screen_lost_after_crash,
            session: value.session,
            _generation: value.generation,
            _event_sequence: value.event_sequence,
            _oldest_event_sequence: value.oldest_event_sequence,
            _attached: value.attached,
            _transcript: value.transcript,
            _activity: value.activity,
            _agent_activity: value.agent_activity,
            upgrade_sources: value.upgrade_sources,
            _upgrade_relaunches: value.upgrade_relaunches,
        }
    }
}
#[derive(Deserialize)]
struct Terminals {
    terminals: Vec<Terminal>,
}
#[derive(Deserialize)]
struct Connected {
    proof: HostSessionProof,
    resumed: EmptyResume,
}
#[derive(Deserialize)]
struct Listed {
    sessions: Vec<Session>,
}
#[derive(Deserialize)]
struct Resolved {
    session: Session,
}
struct EmptyResume;
impl<'de> Deserialize<'de> for EmptyResume {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Empty;
        impl<'de> Visitor<'de> for Empty {
            type Value = EmptyResume;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an empty resume inventory")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, entries: A) -> Result<Self::Value, A::Error> {
                if entries.size_hint() != Some(0) {
                    return Err(de::Error::custom("unexpected source resume payload"));
                }
                Ok(EmptyResume)
            }
        }
        deserializer.deserialize_seq(Empty)
    }
}
impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Allowed;
        impl<'de> Visitor<'de> for Allowed {
            type Value = Response;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("an allowed protocol-77 response")
            }
            fn visit_enum<A: EnumAccess<'de>>(self, value: A) -> Result<Self::Value, A::Error> {
                let (index, payload) = value.variant::<u32>()?;
                match index {
                    0 => {
                        let value: Connected = payload.newtype_variant()?;
                        let _ = value.resumed;
                        Ok(Response::Connected(value.proof))
                    }
                    1 => {
                        let value: Terminals = payload.newtype_variant()?;
                        Ok(Response::Terminals(value.terminals))
                    }
                    13 => {
                        let value: Listed = payload.newtype_variant()?;
                        Ok(Response::Listed(value.sessions))
                    }
                    18 => {
                        let value: Resolved = payload.newtype_variant()?;
                        Ok(Response::Resolved(value.session))
                    }
                    _ => Err(de::Error::custom(
                        "source response is outside the read-only compatibility contract",
                    )),
                }
            }
        }
        deserializer.deserialize_enum("ServerResponse", &[], Allowed)
    }
}

impl Client {
    pub(super) fn connect(
        socket: &Path,
        expected: &HostSessionProof,
        deadline: Instant,
    ) -> Result<Self, String> {
        if !expected.is_complete() {
            return Err("Source service has no complete ownership proof".into());
        }
        let stream = crate::control::connect_control_socket(socket, Duration::from_secs(5))
            .map_err(|_| "Cannot connect to the source service")?;
        let mut client = Self {
            stream,
            proof: expected.clone(),
            next: 1,
            deadline,
        };
        // Authenticate this exact connection against the exclusive kernel lease.
        let pid = crate::daemon::lease_owner(&socket.with_file_name("cagentsd.lock"), expected)
            .map_err(|_| "Source service ownership cannot be verified")?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                client.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
            || peer.uid != unsafe { libc::geteuid() }
            || u32::try_from(peer.pid).ok() != Some(pid)
        {
            return Err("Source connection does not belong to its authenticated service".into());
        }
        match client.request(Request::Connect(expected), None)? {
            Response::Connected(proof) if proof == *expected => Ok(client),
            _ => Err("Source handshake did not confirm the expected owner".into()),
        }
    }

    fn request(
        &mut self,
        request: Request<'_>,
        proof: Option<HostSessionProof>,
    ) -> Result<Response, String> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(
                "Source migration inspection timed out; source windows remain usable".into(),
            );
        }
        let request_id = self.next;
        self.next += 1;
        let mut stream = DeadlineStream {
            stream: &mut self.stream,
            deadline: self.deadline,
        };
        ipc::write_frame(
            &mut stream,
            &RequestEnvelope {
                version: VERSION,
                request_id,
                proof: proof.as_ref(),
                request,
            },
        )
        .map_err(|_| "Cannot send source inspection")?;
        let envelope: Envelope = ipc::read_frame_with_limit(&mut stream, MAX_FRAME)
            .map_err(|_| "Source response is unsupported or unverifiable")?
            .ok_or("Source inspection disconnected")?;
        if envelope.request_id != Some(request_id) || envelope.proof != self.proof {
            return Err("Source inspection ownership or request identity changed".into());
        }
        let Message::Response { response } = envelope.message;
        Ok(response)
    }

    pub(super) fn list(&mut self) -> Result<Vec<Session>, String> {
        match self.request(Request::ListChats, Some(self.proof.clone()))? {
            Response::Listed(sessions) if sessions.len() <= crate::upgrade::handoff::MAX_CHATS => {
                Ok(sessions)
            }
            _ => Err("Source chat inventory is unsupported or exceeds the inspection limit".into()),
        }
    }

    pub(super) fn terminals(&mut self) -> Result<Vec<Terminal>, String> {
        match self.request(Request::ListTerminals, Some(self.proof.clone()))? {
            Response::Terminals(terminals) if terminals.len() <= 1024 => Ok(terminals),
            _ => Err(
                "Source terminal inventory is unsupported or exceeds its inspection limit".into(),
            ),
        }
    }

    pub(super) fn resolve(&mut self, source: &Session) -> Result<Session, String> {
        match self.request(Request::Resolve(source), Some(self.proof.clone()))? {
            Response::Resolved(session) => Ok(session),
            _ => Err("Source did not authenticate a saved conversation's lineage".into()),
        }
    }
}

struct DeadlineStream<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl Read for DeadlineStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.stream.set_write_timeout(Some(remaining))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ipc::{ServerEnvelope, ServerMessage, ServerResponse};

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("source"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("owner"),
        }
    }

    fn frame(response: ServerResponse) -> Vec<u8> {
        postcard::to_allocvec(&ServerEnvelope {
            version: VERSION,
            request_id: Some(1),
            proof: proof(),
            message: ServerMessage::Response { response },
        })
        .unwrap()
    }

    #[test]
    fn restricted_request_indices_match_the_published_77_contract() {
        assert_eq!(postcard::to_allocvec(&Request::ListChats).unwrap(), [29]);
        assert_eq!(postcard::to_allocvec(&Request::ListTerminals).unwrap(), [1]);
        assert_eq!(
            postcard::to_allocvec(&Request::Connect(&proof())).unwrap()[0],
            0
        );
        let source = super::super::tests::session();
        assert_eq!(
            postcard::to_allocvec(&Request::Resolve(&source)).unwrap()[0],
            38
        );
    }

    #[test]
    fn allowed_responses_decode_without_any_snapshot_schema() {
        let envelope: Envelope = postcard::from_bytes(&frame(ServerResponse::Connected {
            proof: proof(),
            resumed: vec![],
        }))
        .unwrap();
        assert!(matches!(
            envelope.message,
            Message::Response {
                response: Response::Connected(_)
            }
        ));
        let session = super::super::tests::session();
        let envelope: Envelope = postcard::from_bytes(&frame(ServerResponse::ChatListed {
            sessions: vec![session.clone()],
        }))
        .unwrap();
        assert!(
            matches!(envelope.message, Message::Response { response: Response::Listed(sessions) } if sessions == vec![session.clone()])
        );
        let envelope: Envelope =
            postcard::from_bytes(&frame(ServerResponse::UpgradeChatResolved {
                session: session.clone(),
            }))
            .unwrap();
        assert!(
            matches!(envelope.message, Message::Response { response: Response::Resolved(resolved) } if resolved == session)
        );
        let descriptor =
            ipc::TerminalDescriptor::relaunchable(session.id, session.launch.recipe.clone(), None);
        let envelope: Envelope = postcard::from_bytes(&frame(ServerResponse::Listed {
            terminals: vec![descriptor.clone()],
        }))
        .unwrap();
        assert!(
            matches!(envelope.message, Message::Response { response: Response::Terminals(terminals) } if terminals.len() == 1 && terminals[0].session == descriptor.session)
        );
    }

    #[test]
    fn incompatible_versions_and_all_other_response_kinds_are_refused() {
        let mut wrong_version = frame(ServerResponse::ChatListed { sessions: vec![] });
        wrong_version[0] = VERSION as u8 + 1;
        assert!(postcard::from_bytes::<Envelope>(&wrong_version).is_err());
        for response in [
            ServerResponse::Accepted,
            ServerResponse::ChatSnapshot {
                chat_id: crate::model::TerminalId::new(),
                snapshot: None,
            },
        ] {
            assert!(postcard::from_bytes::<Envelope>(&frame(response)).is_err());
        }
    }

    #[test]
    fn correlated_inspection_rejects_owner_changes_and_streaming() {
        for change_proof in [false, true] {
            let (stream, mut server) = UnixStream::pair().unwrap();
            let worker = std::thread::spawn(move || {
                let request: ipc::ClientEnvelope = ipc::read_frame(&mut server).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::ChatList));
                let mut owner = proof();
                if change_proof {
                    owner.host_incarnation = crate::model::HostIncarnation::new("other");
                }
                ipc::write_frame(
                    &mut server,
                    &ServerEnvelope {
                        version: VERSION,
                        request_id: Some(request.request_id + u64::from(!change_proof)),
                        proof: owner,
                        message: ServerMessage::Response {
                            response: ServerResponse::ChatListed { sessions: vec![] },
                        },
                    },
                )
                .unwrap();
            });
            let mut client = Client {
                stream,
                proof: proof(),
                next: 1,
                deadline: Instant::now() + Duration::from_secs(2),
            };
            assert!(client.list().is_err());
            worker.join().unwrap();
        }
    }
}
