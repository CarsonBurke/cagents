//! Deliberately restricted reader for the protocol-68 source schema at 503a3319573cfa19d54e4450894bd94ded0c4d65.
//! Never use this client for ordinary connections or provider attachment.
use crate::{chat::Session, model::HostSessionProof, terminal::ipc};
use serde::{
    Deserialize, Serialize,
    de::{self, EnumAccess, SeqAccess, VariantAccess, Visitor},
    ser::SerializeStructVariant,
};
use std::{
    fmt,
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

const VERSION: u16 = 68;
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
        // Exact variants/field order from the authentic 68 declaration. Do not
        // serialize through the current general-purpose ClientRequest enum.
        match self {
            Self::Connect(proof) => {
                let mut value =
                    serializer.serialize_struct_variant("ClientRequest", 0, "Connect", 2)?;
                value.serialize_field("expected_host", &Some(proof))?;
                value.serialize_field("resume", &[] as &[()])?;
                value.end()
            }
            Self::ListTerminals => serializer.serialize_unit_variant("ClientRequest", 1, "List"),
            Self::ListChats => serializer.serialize_unit_variant("ClientRequest", 29, "ChatList"),
            Self::Resolve(source) => {
                let mut value = serializer.serialize_struct_variant(
                    "ClientRequest",
                    38,
                    "ResolveUpgradeChat",
                    1,
                )?;
                value.serialize_field("source", source)?;
                value.end()
            }
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
                formatter.write_str("a protocol-68 read-only response")
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

// Response is variant zero in 68. Notifications, errors and snapshots are all
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
// The complete terminal layout and every transitive Session/recipe/proof type
// used here are identical at source 503a331 and frozen 77. Source hashes and
// declaration equality are retained in the review packet; independent 68 byte
// fixtures below pin populated values. No snapshot or arbitrary response is read.
use super::v77::Terminal;
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
                formatter.write_str("an allowed protocol-68 response")
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
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Source migration inspection timed out".into());
        }
        let stream =
            crate::control::connect_control_socket(socket, remaining.min(Duration::from_secs(5)))
                .map_err(|_| "Cannot connect to the source service")?;
        let mut client = Self {
            stream,
            proof: expected.clone(),
            next: 1,
            deadline,
        };
        client.verify_owner(socket)?;
        match client.request(Request::Connect(expected), None)? {
            Response::Connected(proof) if proof == *expected => {
                client.verify_owner(socket)?;
                Ok(client)
            }
            _ => Err("Source handshake did not confirm the expected owner".into()),
        }
    }

    fn verify_owner(&self, socket: &Path) -> Result<(), String> {
        let pid = crate::daemon::lease_owner(&socket.with_file_name("cagentsd.lock"), &self.proof)
            .map_err(|_| "Source service ownership cannot be verified")?;
        let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: initialized buffers have the exact kernel credential layout.
        if unsafe {
            libc::getsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of::<libc::ucred>()
            || peer.uid != crate::daemon::current_uid()
            || u32::try_from(peer.pid).ok() != Some(pid)
        {
            return Err("Source connection does not belong to its authenticated service".into());
        }
        Ok(())
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
mod tests;
