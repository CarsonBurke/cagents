//! Clients of an older protocol that this daemon still serves.
//!
//! An update carries a running provider process across, and that process keeps
//! the `PATH` its daemon gave it at launch, so it keeps running the older
//! `cagents` executable. When the older protocol is wire-compatible with this
//! one for everything it can say, the daemon answers it in its own envelope
//! version instead of refusing every command the carried session runs.
//!
//! Only that CLI is served. An older `cagents-ui` must keep failing its
//! handshake: that is how it learns an update superseded it and hands over
//! to the installed window. The CLI may not stop or update the daemon either,
//! since it was built for another one.
//!
//! Admission is per protocol and proven against that protocol's recorded wire
//! format (`tests/fixtures/wire-formats/<protocol>.json`):
//! - every request the older client can send decodes here as it meant it;
//! - every message the daemon delivers to it is one it decodes as meant.
//!
//! Requests, replies and notifications this protocol added after it are
//! refused or withheld at the connection, since the older client cannot
//! encode or decode them. Payloads carried as JSON text inside a message
//! (device account reports) are not part of the recorded format.

use std::path::Path;

use serde::{
    Serialize, Serializer,
    ser::{self, Impossible},
};

use super::{
    CANDIDATE_LINE_PROTOCOL, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ServerMessage,
};

/// What an older protocol's client can decode, as variant counts of the
/// three top-level enums. Later variants exist only in newer protocols.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OlderClient {
    pub protocol: u16,
    /// The protocol at which this client's compatibility was last proven. A
    /// protocol bump must prove it again, or remove the entry, before release.
    proven_at: u16,
    client_requests: u32,
    server_messages: u32,
    server_responses: u32,
}

/// The install candidate line, 0.1.3+local.c68275e94. Its chats are carried
/// across an update to this release and keep running its CLI.
const CANDIDATE_LINE: OlderClient = OlderClient {
    protocol: CANDIDATE_LINE_PROTOCOL,
    proven_at: 129,
    client_requests: 74,
    server_messages: 17,
    server_responses: 40,
};

const OLDER_CLIENTS: &[OlderClient] = &[CANDIDATE_LINE];

/// The older protocol a client opened its connection with, when this daemon
/// serves it.
pub fn older_client(protocol: u16) -> Option<OlderClient> {
    if protocol == PROTOCOL_VERSION {
        return None;
    }
    OLDER_CLIENTS
        .iter()
        .copied()
        .find(|client| client.protocol == protocol)
}

impl OlderClient {
    /// A client that knows only the first `server_messages` messages and
    /// `server_responses` replies.
    #[cfg(all(test, feature = "host"))]
    pub(crate) fn knowing(protocol: u16, server_messages: u32, server_responses: u32) -> Self {
        Self {
            protocol,
            proven_at: PROTOCOL_VERSION,
            client_requests: u32::MAX,
            server_messages,
            server_responses,
        }
    }

    /// Whether this request exists in the client's protocol and leaves the
    /// daemon to the CLI that matches it.
    pub fn admits_request(&self, request: &ClientRequest) -> bool {
        variant_index(request).is_some_and(|index| index < self.client_requests)
            && !changes_the_daemon(request)
    }

    /// Whether the client can decode this message.
    pub fn admits_message(&self, message: &ServerMessage) -> bool {
        let Some(index) = variant_index(message) else {
            return false;
        };
        if index >= self.server_messages {
            return false;
        }
        match message {
            ServerMessage::Response { response } => {
                variant_index(response).is_some_and(|index| index < self.server_responses)
            }
            _ => true,
        }
    }

    /// Answers a request whose reply exists only in a newer protocol.
    pub fn refusal(&self) -> ProtocolError {
        ProtocolError::new(
            ProtocolErrorCode::UnsupportedVersion,
            format!(
                "this cagents CLI speaks protocol {}, and this request needs protocol \
                 {PROTOCOL_VERSION}. Run the cagents CLI installed with the running daemon.",
                self.protocol
            ),
            false,
        )
    }
}

/// Requests that stop, update or hand off the daemon itself.
fn changes_the_daemon(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Shutdown { .. }
            | ClientRequest::ShutdownV1 { .. }
            | ClientRequest::PrepareUpgrade { .. }
            | ClientRequest::UpgradeImpact { .. }
            | ClientRequest::PrepareConfirmedUpgrade { .. }
            | ClientRequest::ResolveUpgradeChat { .. }
    )
}

/// Whether `executable`, a peer's `/proc/<pid>/exe`, is the `cagents` CLI:
/// the only older client this daemon serves. A pruned version directory
/// leaves the link marked deleted.
pub fn is_cli_executable(executable: &Path) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.strip_suffix(" (deleted)").unwrap_or(name) == "cagents")
}

/// The postcard variant index of an enum value: its position in the
/// declaration, which is what an older peer decodes.
pub(super) fn variant_index<T: Serialize + ?Sized>(value: &T) -> Option<u32> {
    match value.serialize(VariantIndex) {
        Err(Captured::Variant(index)) => Some(index),
        Ok(()) | Err(Captured::NotAnEnum) => None,
    }
}

/// A serializer that stops at the outermost enum variant and reports its
/// index, without encoding the payload.
struct VariantIndex;

#[derive(Debug)]
enum Captured {
    Variant(u32),
    NotAnEnum,
}

impl std::fmt::Display for Captured {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("variant index captured")
    }
}

impl std::error::Error for Captured {}

impl ser::Error for Captured {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Self::NotAnEnum
    }
}

macro_rules! not_an_enum {
    ($($method:ident($($argument:ty),*)),* $(,)?) => {
        $(fn $method(self, $(_: $argument),*) -> Result<(), Captured> {
            Err(Captured::NotAnEnum)
        })*
    };
}

impl Serializer for VariantIndex {
    type Ok = ();
    type Error = Captured;
    type SerializeSeq = Impossible<(), Captured>;
    type SerializeTuple = Impossible<(), Captured>;
    type SerializeTupleStruct = Impossible<(), Captured>;
    type SerializeTupleVariant = Impossible<(), Captured>;
    type SerializeMap = Impossible<(), Captured>;
    type SerializeStruct = Impossible<(), Captured>;
    type SerializeStructVariant = Impossible<(), Captured>;

    not_an_enum! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16), serialize_i32(i32),
        serialize_i64(i64), serialize_u8(u8), serialize_u16(u16), serialize_u32(u32),
        serialize_u64(u64), serialize_f32(f32), serialize_f64(f64), serialize_char(char),
        serialize_str(&str), serialize_bytes(&[u8]), serialize_none(), serialize_unit(),
        serialize_unit_struct(&'static str),
    }

    fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<(), Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<(), Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_unit_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
    ) -> Result<(), Captured> {
        Err(Captured::Variant(index))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), Captured> {
        Err(Captured::Variant(index))
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Captured> {
        Err(Captured::Variant(index))
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStruct, Captured> {
        Err(Captured::NotAnEnum)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Captured> {
        Err(Captured::Variant(index))
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod wire_format_tests;
#[cfg(test)]
mod wire_shape;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ipc::{ServerResponse, TerminalCloseResult};

    #[test]
    fn variant_index_is_the_declaration_position_at_every_payload_shape() {
        assert_eq!(variant_index(&ClientRequest::List), Some(1));
        assert_eq!(
            variant_index(&ClientRequest::Connect {
                expected_host: None,
                resume: Vec::new(),
            }),
            Some(0)
        );
        let response = ServerResponse::Detached;
        let detached = variant_index(&response).unwrap();
        assert_eq!(
            variant_index(&ServerMessage::Response { response }),
            Some(0)
        );
        assert_eq!(detached, 4);
        assert_eq!(variant_index(&ServerMessage::HostShuttingDown), Some(5));
        assert_eq!(variant_index(&TerminalCloseResult::Closed), Some(0));
        assert_eq!(variant_index("not an enum"), None);
        assert_eq!(variant_index(&Some(ClientRequest::List)), None);
    }

    #[test]
    fn only_the_cli_executable_is_served_as_an_older_client() {
        for cli in [
            "/home/user/.local/share/cagents/versions/0.1.3/bin/cagents",
            "/usr/bin/cagents",
            "/home/user/.local/share/cagents/versions/0.1.3/bin/cagents (deleted)",
        ] {
            assert!(is_cli_executable(Path::new(cli)), "{cli}");
        }
        for other in [
            "/usr/bin/cagents-ui",
            "/usr/bin/cagentsd",
            "/usr/bin/cagents-ui (deleted)",
            "/usr/bin/python3",
            "/",
        ] {
            assert!(!is_cli_executable(Path::new(other)), "{other}");
        }
    }

    #[test]
    fn only_this_protocol_and_the_listed_older_ones_are_served() {
        assert_eq!(older_client(PROTOCOL_VERSION), None);
        assert_eq!(
            older_client(CANDIDATE_LINE_PROTOCOL).map(|client| client.protocol),
            Some(CANDIDATE_LINE_PROTOCOL)
        );
        for protocol in [0, 77, 119, 127, PROTOCOL_VERSION + 1, u16::MAX] {
            assert_eq!(older_client(protocol), None, "{protocol}");
        }
    }

    #[test]
    fn variants_after_the_older_protocols_are_refused_or_withheld() {
        let detached = variant_index(&ServerResponse::Detached).unwrap();
        let list = variant_index(&ClientRequest::List).unwrap();
        let client = OlderClient {
            protocol: 1,
            proven_at: PROTOCOL_VERSION,
            client_requests: list,
            server_messages: 1,
            server_responses: detached,
        };
        assert!(!client.admits_request(&ClientRequest::List));
        assert!(client.admits_request(&ClientRequest::Connect {
            expected_host: None,
            resume: Vec::new(),
        }));
        assert!(!client.admits_message(&ServerMessage::Response {
            response: ServerResponse::Detached,
        }));
        let earlier = OlderClient {
            server_responses: detached + 1,
            ..client
        };
        assert!(earlier.admits_message(&ServerMessage::Response {
            response: ServerResponse::Detached,
        }));
        assert!(!earlier.admits_message(&ServerMessage::Error(earlier.refusal())));
        let shutdown = ClientRequest::ShutdownV1 {
            version: crate::terminal_ipc::SHUTDOWN_VERSION,
            operation: crate::terminal_ipc::ShutdownOperation::Preflight,
        };
        let every_request = OlderClient {
            client_requests: u32::MAX,
            ..client
        };
        assert!(every_request.admits_request(&ClientRequest::List));
        assert!(!every_request.admits_request(&shutdown));
        let refusal = client.refusal();
        assert_eq!(refusal.code, ProtocolErrorCode::UnsupportedVersion);
        assert!(!refusal.retryable);
        assert!(refusal.message.contains(&PROTOCOL_VERSION.to_string()));
    }
}
