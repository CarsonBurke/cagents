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
//! encode or decode them. What a later protocol only added detail to is
//! encoded for it as it was before: see [`encoding_for`]. Payloads carried as
//! JSON text inside a message (device account reports) are not part of the
//! recorded format.

use std::{cell::Cell, path::Path};

use serde::{
    Serialize, Serializer,
    ser::{self, Impossible},
};

use super::{
    CANDIDATE_LINE_PROTOCOL, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ServerMessage, ServerResponse,
};

/// What an older protocol's client can decode, as variant counts of the three
/// top-level enums and of the work and agent replies nested in one of them.
/// Later variants exist only in newer protocols.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OlderClient {
    pub protocol: u16,
    /// The protocol at which this client's compatibility was last proven. A
    /// protocol bump must prove it again, or remove the entry, before release.
    proven_at: u16,
    client_requests: u32,
    server_messages: u32,
    server_responses: u32,
    work_responses: u32,
    /// Variants of `agent_execution::Reply`. A later one answers only an
    /// operation the older client cannot encode.
    agent_replies: u32,
    /// Variants of `device_service::Reply`. A later one answers only an
    /// operation the older client cannot encode.
    device_replies: u32,
}

/// The install candidate line, 0.1.3+local.c68275e94. Its chats are carried
/// across an update to this release and keep running its CLI.
const CANDIDATE_LINE: OlderClient = OlderClient {
    protocol: CANDIDATE_LINE_PROTOCOL,
    proven_at: 202,
    client_requests: 74,
    server_messages: 17,
    server_responses: 40,
    work_responses: 6,
    agent_replies: 11,
    device_replies: 15,
};

const OLDER_CLIENTS: &[OlderClient] = &[CANDIDATE_LINE];

/// The first protocol whose `Firing` carries its skipped occurrences. Postcard
/// is positional, so a client before this one cannot read a reply holding one.
const FIRING_WITH_SKIPPED_OCCURRENCES: u16 = 156;

/// The `automations::Response` variants holding a `Firing`. The wire format
/// proof skips exactly these, so a variant here is one no older client gets.
const FIRING_REPLIES: &[&str] = &["Firing", "History", "Detail"];

/// The first protocol whose `WorkspaceModelEdited` carries the edit's effects.
/// A client before it cannot read the edge, so it may not watch the model.
const MODEL_EDGES_WITH_EFFECTS: u16 = 175;

/// The `ServerMessage` variants that changed shape in place for clients before
/// [`MODEL_EDGES_WITH_EFFECTS`]. The wire format proof skips exactly these.
// Wire shape tracing uses Serde variant names, including rename_all.
#[cfg(test)]
const MODEL_EDGES: &[&str] = &["workspace_model_edited"];

/// Work boards include the expanded check counts from this protocol onward.
const WORK_CHECK_COUNTS: u16 = 176;
const WORK_CHECK_REPLIES: &[&str] = &["Board", "Pinned", "Watching"];
#[cfg(test)]
const WORK_BOARD_MESSAGES: &[&str] = &["work_board"];

/// The first protocol with `ToolStatus::Waiting`. An older client is told such
/// an agent is running, which is what it was before: it hasn't finished.
pub(crate) const WAITING_TOOL_STATUS: u16 = 166;

/// Earlier runs are shown as stopped to clients that cannot name Ended.
pub(crate) const ENDED_TOOL_STATUS: u16 = 177;

/// Earlier clients receive retryable Internal errors for store contention.
pub(super) const STORE_BUSY_ERROR: u16 = 179;

/// Earlier clients receive `InvalidRequest`, with the same sentence and
/// retryability, for a refusal that came before anything changed.
pub(super) const NOT_ADMITTED_ERROR: u16 = 188;

/// The first protocol whose `Provider` names Antigravity and Muse. A daemon
/// before it recognised neither, so its clients were never told of them; a
/// message naming one is refused or withheld for them (see
/// [`OlderClient::admits_message`]).
pub(crate) const ANTIGRAVITY_AND_MUSE: u16 = 191;

/// The first protocol whose account usage readings carry reset inventories and
/// reported plan details. An earlier client is sent the reading without them.
pub(crate) const PLAN_AND_RESET_READINGS: u16 = 184;

/// The first protocol whose file changes say what their line numbers count
/// from. A client before it is sent them without; one from a source before it
/// is read as an excerpt, whose numbers place nothing.
pub(crate) const PATCH_LINE_ORIGIN: u16 = 195;

/// The first protocol whose agent activity says what a waiting agent waits
/// for. An older client is told it waits, as it was before.
pub(crate) const WAIT_KINDS: u16 = 199;

/// The first protocol whose channels name the checkout they work in. A client
/// before it is sent channels without one, and a channel it creates works in
/// the main checkout.
pub(crate) const CHANNEL_CHECKOUT: u16 = 200;

/// The first protocol whose usage says where the provider auto-compacts the
/// conversation. A client before it is sent the usage without; one from a
/// source before it is read as compacting nowhere short of the whole window.
pub(crate) const USAGE_COMPACT_LIMIT: u16 = 201;

/// The first protocol whose chat launches carry the model and effort chosen
/// for a draft. A client before it is sent launches without them, and its
/// requests are read without them; a source before it chose none and is sent
/// none.
pub(crate) const DRAFT_MODEL_IN_LAUNCH: u16 = 202;

/// Fields a later protocol added to a struct, which a client from before it is
/// sent without, as is a request to a daemon from before it: the struct's type
/// name, the fields, and the protocol that added them. The wire format proofs
/// accept exactly these.
#[cfg(test)]
const OMITTED_FIELDS: &[(&str, &[&str], u16)] = &[
    (
        "cagents::account::info::UsageReading",
        &["resets", "plan_display_name", "rate_limit_tier"],
        PLAN_AND_RESET_READINGS,
    ),
    ("cagents::chat::FileChange", &["lines"], PATCH_LINE_ORIGIN),
    (
        "cagents::chat::FileChangePatch",
        &["lines"],
        PATCH_LINE_ORIGIN,
    ),
    (
        "cagents::workspace_model::ModelChannel",
        &["checkout"],
        CHANNEL_CHECKOUT,
    ),
    (
        "cagents::chat::Usage",
        &["compact_limit"],
        USAGE_COMPACT_LIMIT,
    ),
    (
        "cagents::chat::Launch",
        &["draft_model"],
        DRAFT_MODEL_IN_LAUNCH,
    ),
];

/// Fields a later protocol added to a variant of a request's enum, which this
/// daemon decodes an older client's request without: the enum's type name, the
/// variant, its fields, and the protocol that added them. The connection reads
/// such a client's requests in [`decoding_from`] scope, where each field's
/// decoder reads nothing and takes its default. The wire format proof accepts
/// exactly these.
#[cfg(test)]
const REQUEST_FIELDS_DECODED_WITHOUT: &[(&str, &str, &[&str], u16)] = &[(
    "cagents::workspace_model::ModelEdit",
    "CreateChannel",
    &["checkout"],
    CHANNEL_CHECKOUT,
)];

/// Fields a later protocol added to a struct that this side reads a message
/// from an update source, or a request from an older client, before it
/// without: the struct's type name, the fields, and the protocol that added
/// them. Each is decoded in [`decoding_from`] scope as that side encoded it,
/// so the frozen lifecycle and older client proofs accept exactly these.
#[cfg(test)]
const DECODED_WITHOUT: &[(&str, &[&str], u16)] = &[
    ("cagents::chat::FileChange", &["lines"], PATCH_LINE_ORIGIN),
    (
        "cagents::chat::FileChangePatch",
        &["lines"],
        PATCH_LINE_ORIGIN,
    ),
    (
        "cagents::chat::Usage",
        &["compact_limit"],
        USAGE_COMPACT_LIMIT,
    ),
    (
        "cagents::chat::Launch",
        &["draft_model"],
        DRAFT_MODEL_IN_LAUNCH,
    ),
];

/// Variants a later protocol appended that a client from before it is sent as
/// a variant it knows: the enum's type name, the variant, and the protocol that
/// added it. The wire format proof accepts exactly these.
#[cfg(test)]
const RESTATED_VARIANTS: &[(&str, &str, u16)] = &[
    ("cagents::chat::ToolStatus", "Waiting", WAITING_TOOL_STATUS),
    ("cagents::chat::ToolStatus", "Ended", ENDED_TOOL_STATUS),
    ("cagents::agent::ActivityState", "Approval", WAIT_KINDS),
    ("cagents::agent::ActivityState", "Question", WAIT_KINDS),
    (
        "cagents::terminal::ipc::ProtocolErrorCode",
        "store_busy",
        STORE_BUSY_ERROR,
    ),
    (
        "cagents::terminal::ipc::ProtocolErrorCode",
        "not_admitted",
        NOT_ADMITTED_ERROR,
    ),
];

/// Variants a later protocol appended that a client from before it is never
/// sent: the enum's type name, the variant, and the protocol that added it.
/// An optional value naming one reaches the client as absent (see
/// `crate::agent::absent_where_unknown`), such as a terminal's agent activity;
/// a reply naming one anywhere else is refused and a notification withheld,
/// since nothing the client knows could stand for it. The wire format proof
/// accepts exactly these.
///
/// An update source is never sent one either: the update sends a source back
/// only the conversations and terminals that source listed, which name
/// providers it knows.
#[cfg(test)]
const WITHHELD_VARIANTS: &[(&str, &str, u16)] = &[
    (
        "cagents::agent::Provider",
        "Antigravity",
        ANTIGRAVITY_AND_MUSE,
    ),
    ("cagents::agent::Provider", "Muse", ANTIGRAVITY_AND_MUSE),
];

thread_local! {
    /// The older protocol this thread is encoding a message for.
    static ENCODING_FOR: Cell<Option<u16>> = const { Cell::new(None) };
}

/// Runs `encode` with this thread encoding for a client of `protocol`, so that
/// values a later protocol changed encode as that client decodes them. The
/// recorded wire format proof checks the rest still matches.
///
/// The restatement applies only to what `encode` serializes on this thread.
/// A server message serialized anywhere else, such as ahead of time or on
/// another thread, is sent in the current protocol's encoding, so every
/// write to a client must serialize inside this scope (as
/// `ServerEnvelopeRef::serialize` does).
pub(crate) fn encoding_for<R>(protocol: u16, encode: impl FnOnce() -> R) -> R {
    struct Restore(Option<u16>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENCODING_FOR.set(self.0);
        }
    }
    let older = (protocol < PROTOCOL_VERSION).then_some(protocol);
    let _restore = Restore(ENCODING_FOR.replace(older));
    encode()
}

/// Whether the value being encoded goes to a client from before `protocol`.
pub(crate) fn encoding_before(protocol: u16) -> bool {
    ENCODING_FOR.get().is_some_and(|older| older < protocol)
}

thread_local! {
    /// The older protocol of the daemon this thread is decoding a message
    /// from.
    static DECODING_FROM: Cell<Option<u16>> = const { Cell::new(None) };
}

/// Runs `decode` with this thread decoding a message from a peer of
/// `protocol`, such as an update source's daemon or an older CLI this daemon
/// serves, so that values a later protocol added a field to decode as that
/// peer encoded them. The decode scope ends with `decode`, however it returns
/// or unwinds.
pub(crate) fn decoding_from<R>(protocol: u16, decode: impl FnOnce() -> R) -> R {
    struct Restore(Option<u16>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DECODING_FROM.set(self.0);
        }
    }
    let older = (protocol < PROTOCOL_VERSION).then_some(protocol);
    let _restore = Restore(DECODING_FROM.replace(older));
    decode()
}

/// Whether the value being decoded comes from a peer before `protocol`.
pub(crate) fn decoding_before(protocol: u16) -> bool {
    DECODING_FROM.get().is_some_and(|older| older < protocol)
}

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
            work_responses: u32::MAX,
            agent_replies: u32::MAX,
            device_replies: u32::MAX,
        }
    }

    /// Whether this request exists in the client's protocol and leaves the
    /// daemon to the CLI that matches it.
    pub fn admits_request(&self, request: &ClientRequest) -> bool {
        variant_index(request).is_some_and(|index| index < self.client_requests)
            && !changes_the_daemon(request)
            && !self.cannot_read_the_firing_for(request)
            && !self.cannot_read_model_edges_for(request)
            && !self.cannot_read_work_checks_for(request)
    }

    /// Refuse before execution: an older CLI must not pin a card and then
    /// discover it cannot decode the result.
    fn cannot_read_work_checks_for(&self, request: &ClientRequest) -> bool {
        self.protocol < WORK_CHECK_COUNTS
            && matches!(
                request,
                ClientRequest::WatchWork
                    | ClientRequest::Work(
                        crate::work::Request::Board { .. } | crate::work::Request::Pin { .. }
                    )
            )
    }

    /// A watch would only deliver edges the client cannot read.
    fn cannot_read_model_edges_for(&self, request: &ClientRequest) -> bool {
        self.protocol < MODEL_EDGES_WITH_EFFECTS
            && matches!(request, ClientRequest::WatchWorkspaceModel)
    }

    /// A run-now started here would look refused, so its request is turned
    /// away before anything starts rather than at its reply.
    fn cannot_read_the_firing_for(&self, request: &ClientRequest) -> bool {
        use crate::automations::Request;
        self.protocol < FIRING_WITH_SKIPPED_OCCURRENCES
            && matches!(
                request,
                ClientRequest::Automation {
                    operation: Request::RunNow { .. }
                        | Request::History { .. }
                        | Request::ShowFiring { .. }
                        | Request::ShowRun { .. },
                    ..
                }
            )
    }

    /// Whether the client can decode this message.
    pub fn admits_message(&self, message: &ServerMessage) -> bool {
        let Some(index) = variant_index(message) else {
            return false;
        };
        if index >= self.server_messages {
            return false;
        }
        let known = match message {
            ServerMessage::Response { response } => {
                let nested = match response {
                    ServerResponse::Work(work) => {
                        variant_index(work).is_some_and(|index| index < self.work_responses)
                            && (self.protocol >= WORK_CHECK_COUNTS
                                || !variant_name(work)
                                    .is_some_and(|name| WORK_CHECK_REPLIES.contains(&name)))
                    }
                    ServerResponse::Automation(automation) => {
                        self.protocol >= FIRING_WITH_SKIPPED_OCCURRENCES
                            || !holds_a_firing(automation)
                    }
                    ServerResponse::Agent(reply) => {
                        variant_index(reply).is_some_and(|index| index < self.agent_replies)
                    }
                    ServerResponse::DeviceOperation { reply } => {
                        variant_index(reply).is_some_and(|index| index < self.device_replies)
                    }
                    _ => true,
                };
                nested && variant_index(response).is_some_and(|index| index < self.server_responses)
            }
            ServerMessage::WorkBoard { .. } => self.protocol >= WORK_CHECK_COUNTS,
            ServerMessage::WorkspaceModelEdited { .. } => self.protocol >= MODEL_EDGES_WITH_EFFECTS,
            _ => true,
        };
        // Last, as the costliest: a value with no encoding for this client,
        // such as a provider it predates, refuses to encode for it (see
        // `WITHHELD_VARIANTS`).
        known && encoding_for(self.protocol, || postcard::to_io(message, std::io::sink())).is_ok()
    }

    /// Answers a request this client may not make, or whose reply it cannot
    /// read.
    pub fn refusal(&self) -> ProtocolError {
        ProtocolError::new(
            ProtocolErrorCode::UnsupportedVersion,
            format!(
                "this cagents CLI is from an older release (protocol {}) and the running daemon \
                 (protocol {PROTOCOL_VERSION}) can't serve this request to it. Start a new \
                 session, or run `bin/cagents` in the daemon's runtime directory \
                 ($XDG_RUNTIME_DIR/cagents by default).",
                self.protocol
            ),
            false,
        )
    }
}

fn holds_a_firing(response: &crate::automations::Response) -> bool {
    variant_name(response).is_some_and(|name| FIRING_REPLIES.contains(&name))
}

/// Requests that stop, update or hand off the daemon itself.
fn changes_the_daemon(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::Shutdown
            | ClientRequest::ShutdownV1 { .. }
            | ClientRequest::PrepareUpgrade { .. }
            | ClientRequest::UpgradeImpact { .. }
            | ClientRequest::PrepareConfirmedUpgrade { .. }
            | ClientRequest::ResolveUpgradeChat { .. }
    )
}

/// Whether a peer, by its `/proc/<pid>/exe` and NUL-separated
/// `/proc/<pid>/cmdline`, is the `cagents` CLI: the only older client this
/// daemon serves. A pruned version directory leaves the link marked deleted.
///
/// `cagents daemon bridge` is excluded: it relays a remote device's window,
/// or any other client, which must meet this protocol as it did before.
pub fn is_cli_peer(executable: &Path, command_line: &[u8]) -> bool {
    let cli = executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.strip_suffix(" (deleted)").unwrap_or(name) == "cagents");
    let arguments: Vec<&[u8]> = command_line.split(|byte| *byte == 0).skip(1).collect();
    let bridge = arguments
        .windows(2)
        .any(|pair| pair == [&b"daemon"[..], &b"bridge"[..]]);
    cli && !bridge
}

/// The postcard variant index of an enum value: its position in the
/// declaration, which is what an older peer decodes.
pub(super) fn variant_index<T: Serialize + ?Sized>(value: &T) -> Option<u32> {
    match value.serialize(VariantIndex) {
        Err(Captured::Variant(index, _)) => Some(index),
        Ok(()) | Err(Captured::NotAnEnum) => None,
    }
}

/// The name of this value's enum variant.
fn variant_name<T: Serialize + ?Sized>(value: &T) -> Option<&'static str> {
    match value.serialize(VariantIndex) {
        Err(Captured::Variant(_, name)) => Some(name),
        Ok(()) | Err(Captured::NotAnEnum) => None,
    }
}

/// A serializer that stops at the outermost enum variant and reports its
/// index, without encoding the payload.
struct VariantIndex;

#[derive(Debug)]
enum Captured {
    Variant(u32, &'static str),
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
        name: &'static str,
    ) -> Result<(), Captured> {
        Err(Captured::Variant(index, name))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        index: u32,
        name: &'static str,
        _: &T,
    ) -> Result<(), Captured> {
        Err(Captured::Variant(index, name))
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
        name: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Captured> {
        Err(Captured::Variant(index, name))
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
        name: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Captured> {
        Err(Captured::Variant(index, name))
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

    /// Each error code a later protocol appended reaches an older client as
    /// the code it knows, with its sentence and retryability, while the
    /// current protocol and unscoped JSON keep the new code.
    #[test]
    fn appended_error_codes_reach_carried_clis_as_codes_they_know() {
        use crate::terminal_ipc::{ServerEnvelope, ServerEnvelopeRef, read_frame, write_frame};
        let proof = crate::model::HostSessionProof {
            host_session_id: "appended-error-wire".into(),
            boot_id: "boot".into(),
            host_incarnation: "incarnation".into(),
        };
        let cases = [
            (
                ProtocolError::new(ProtocolErrorCode::StoreBusy, "database is locked", true),
                "store_busy",
                vec![
                    (CANDIDATE_LINE_PROTOCOL, ProtocolErrorCode::Internal),
                    (177, ProtocolErrorCode::Internal),
                    (178, ProtocolErrorCode::Internal),
                    (STORE_BUSY_ERROR, ProtocolErrorCode::StoreBusy),
                    (PROTOCOL_VERSION, ProtocolErrorCode::StoreBusy),
                ],
            ),
            (
                ProtocolError::new(
                    ProtocolErrorCode::NotAdmitted,
                    "This conversation has an explicitly pinned account",
                    false,
                ),
                "not_admitted",
                vec![
                    (CANDIDATE_LINE_PROTOCOL, ProtocolErrorCode::InvalidRequest),
                    (STORE_BUSY_ERROR, ProtocolErrorCode::InvalidRequest),
                    (NOT_ADMITTED_ERROR - 1, ProtocolErrorCode::InvalidRequest),
                    (NOT_ADMITTED_ERROR, ProtocolErrorCode::NotAdmitted),
                    (PROTOCOL_VERSION, ProtocolErrorCode::NotAdmitted),
                ],
            ),
        ];
        for (error, json, versions) in cases {
            for (version, expected) in versions {
                let message = ServerMessage::Error(error.clone());
                let mut frame = Vec::new();
                write_frame(
                    &mut frame,
                    &ServerEnvelopeRef {
                        version,
                        request_id: Some(17),
                        proof: &proof,
                        message: &message,
                    },
                )
                .unwrap();
                let decoded: ServerEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
                assert_eq!(decoded.version, version);
                assert_eq!(decoded.request_id, Some(17));
                assert_eq!(decoded.proof, proof);
                assert_eq!(
                    decoded.message,
                    ServerMessage::Error(ProtocolError::new(
                        expected,
                        error.message.clone(),
                        error.retryable,
                    )),
                    "{json} at protocol {version}"
                );
                // Encoding an older frame must not change unscoped JSON output.
                assert_eq!(serde_json::to_value(&error).unwrap()["code"], json);
            }
        }
    }

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
    fn an_older_client_reads_usage_without_resets_or_plan_details() {
        use crate::account::{
            UsageSource, UsageStatus,
            info::UsageReading,
            resets::{ResetKind, ResetScope, UsageReset},
        };
        /// The reading as protocol 183 and earlier decode it.
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Before {
            windows: Vec<crate::account::UsageWindow>,
            credits: Option<crate::account::Credits>,
            status: UsageStatus,
            ordinary_usage_allowed: Option<bool>,
            observed_at: u64,
            source: UsageSource,
            plan: Option<String>,
            detail: Option<String>,
        }
        let reading = |observed_at| UsageReading {
            resets: vec![UsageReset {
                id: "fable-fixture".into(),
                kind: ResetKind::Granted,
                scopes: vec![ResetScope::Model("Fable".into())],
                available_count: 2,
                expires_at: Some(1_900_000_000),
                usable: true,
            }],
            windows: Vec::new(),
            credits: None,
            status: UsageStatus::Limited,
            ordinary_usage_allowed: Some(false),
            observed_at,
            source: UsageSource::Probe,
            plan: Some("Max".into()),
            plan_display_name: Some("Ultra".into()),
            rate_limit_tier: Some("default_claude_max_20x".into()),
            detail: Some("Fable limit".into()),
        };
        // Two in a row: a misaligned first reading would corrupt the second.
        let readings = vec![reading(1), reading(2)];
        let current = postcard::to_stdvec(&readings).unwrap();
        assert_eq!(
            postcard::from_bytes::<Vec<UsageReading>>(&current).unwrap(),
            readings
        );
        let json = serde_json::to_value(&readings[0]).unwrap();
        assert_eq!(json["plan_display_name"], "Ultra");
        assert_eq!(json["resets"][0]["id"], "fable-fixture");
        let older = encoding_for(CANDIDATE_LINE_PROTOCOL, || {
            postcard::to_stdvec(&readings).unwrap()
        });
        let (decoded, rest) = postcard::take_from_bytes::<Vec<Before>>(&older).unwrap();
        assert!(rest.is_empty(), "the older reading has trailing bytes");
        assert_eq!(
            decoded,
            [1, 2].map(|observed_at| Before {
                windows: Vec::new(),
                credits: None,
                status: UsageStatus::Limited,
                ordinary_usage_allowed: Some(false),
                observed_at,
                source: UsageSource::Probe,
                plan: Some("Max".into()),
                detail: Some("Fable limit".into()),
            })
        );
        assert_eq!(
            encoding_for(PLAN_AND_RESET_READINGS, || postcard::to_stdvec(&readings)
                .unwrap()),
            current
        );
    }

    #[test]
    fn a_device_reply_after_the_older_protocol_is_refused() {
        use crate::device_service::Reply;
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let reply = |reply| ServerMessage::Response {
            response: ServerResponse::DeviceOperation { reply },
        };
        assert!(client.admits_message(&reply(Reply::Saved)));
        assert!(!client.admits_message(&reply(Reply::AccountReset(serde_json::Value::Null))));
    }

    #[test]
    fn a_waiting_agent_reaches_an_older_client_as_running() {
        use crate::chat::ToolStatus;
        let encode = |status: ToolStatus| postcard::to_stdvec(&status).unwrap();
        let every = [
            ToolStatus::Running,
            ToolStatus::Succeeded,
            ToolStatus::Failed,
            ToolStatus::Denied,
            ToolStatus::Stopped,
            ToolStatus::Waiting,
        ];
        // Encoded as derived: by declaration position, and by name in JSON.
        for status in every {
            let encoded = encode(status);
            assert_eq!(
                postcard::from_bytes::<ToolStatus>(&encoded).unwrap(),
                status
            );
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{status:?}\""));
            assert_eq!(serde_json::from_str::<ToolStatus>(&json).unwrap(), status);
        }
        let waiting = encode(ToolStatus::Waiting);
        assert_eq!(waiting, [5]);
        assert_eq!(
            postcard::from_bytes::<ToolStatus>(&waiting).unwrap(),
            ToolStatus::Waiting
        );
        assert_eq!(
            serde_json::to_string(&ToolStatus::Waiting).unwrap(),
            "\"Waiting\""
        );
        assert_eq!(
            encoding_for(CANDIDATE_LINE_PROTOCOL, || encode(ToolStatus::Waiting)),
            encode(ToolStatus::Running)
        );
        assert_eq!(
            encoding_for(WAITING_TOOL_STATUS, || encode(ToolStatus::Waiting)),
            waiting
        );
        // The scope ends with the encoding it was entered for.
        assert!(!encoding_before(PROTOCOL_VERSION + 1));
        assert_eq!(encode(ToolStatus::Waiting), waiting);
    }

    /// What a waiting agent waits for reaches a client that knows the kinds,
    /// and an older one is told it waits, as it was before.
    #[test]
    fn a_wait_kind_reaches_an_older_client_as_waiting() {
        use crate::agent::ActivityState;
        let encode = |state: ActivityState| postcard::to_stdvec(&state).unwrap();
        // Encoded as derived: by declaration position, and by name in JSON.
        for state in [
            ActivityState::Unknown,
            ActivityState::Idle,
            ActivityState::Working,
            ActivityState::Waiting,
            ActivityState::Approval,
            ActivityState::Question,
        ] {
            let encoded = encode(state);
            assert_eq!(
                postcard::from_bytes::<ActivityState>(&encoded).unwrap(),
                state
            );
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{state:?}\""));
            assert_eq!(serde_json::from_str::<ActivityState>(&json).unwrap(), state);
        }
        assert_eq!(encode(ActivityState::Approval), [4]);
        assert_eq!(encode(ActivityState::Question), [5]);
        for state in [ActivityState::Approval, ActivityState::Question] {
            assert_eq!(
                encoding_for(WAIT_KINDS - 1, || encode(state)),
                encode(ActivityState::Waiting)
            );
            assert_eq!(encoding_for(WAIT_KINDS, || encode(state)), encode(state));
        }
    }

    /// Usage crosses the protocol that added its compaction point both ways:
    /// an older client is sent the fields it decodes, and usage read from an
    /// older update source has no compaction point, which leaves the whole
    /// window as the budget. What follows it in the message stays where it was.
    #[test]
    fn usage_crosses_the_compaction_point_protocol_in_both_directions() {
        use crate::chat::Usage;
        let older = USAGE_COMPACT_LIMIT - 1;
        let usage = |compact_limit| Usage {
            input_tokens: 2_000,
            output_tokens: 800,
            context_tokens: 230_000,
            context_limit: 1_000_000,
            cost_microusd: Some(12),
            cost_estimate: None,
            compact_limit,
        };
        // As the older protocol has it: six fields, then the next value.
        let before = postcard::to_stdvec(&(
            (
                2_000u64,
                800u64,
                230_000u64,
                1_000_000u64,
                Some(12u64),
                None::<crate::chat::pricing::CostEstimate>,
            ),
            7u8,
        ))
        .unwrap();
        for compact_limit in [Some(267_000), None] {
            let message = (usage(compact_limit), 7u8);
            assert_eq!(
                encoding_for(older, || postcard::to_stdvec(&message).unwrap()),
                before
            );
            let current = postcard::to_stdvec(&message).unwrap();
            assert_ne!(current, before);
            assert_eq!(
                postcard::from_bytes::<(Usage, u8)>(&current).unwrap(),
                message
            );
        }
        assert_eq!(
            decoding_from(older, || postcard::from_bytes::<(Usage, u8)>(&before)).unwrap(),
            (usage(None), 7)
        );
        // JSON written before the field reads without it.
        assert_eq!(
            serde_json::from_str::<Usage>(
                r#"{"input_tokens":2000,"output_tokens":800,"context_tokens":230000,"context_limit":1000000,"cost_microusd":12}"#
            )
            .unwrap(),
            usage(None)
        );
    }

    /// A file change crosses the protocol that added its line origin both
    /// ways: an older client is sent the fields it decodes, and one read
    /// from an older update source, which never said where its lines are, is
    /// taken as an excerpt rather than as file positions. What follows it in
    /// the message stays where it was.
    #[test]
    fn file_changes_cross_the_line_origin_protocol_in_both_directions() {
        use crate::chat::{ChangeKind, FileChange, FileChangePatch, LineOrigin, ToolStatus};
        let older = PATCH_LINE_ORIGIN - 1;
        let change = |lines| FileChange {
            path: "src/lib.rs".into(),
            kind: ChangeKind::Renamed {
                from: "src/old.rs".into(),
            },
            diff: "@@ -4 +4 @@\n-a\n+b\n".into(),
            lines,
        };
        // As the older protocol has it: three fields, then the next value.
        let before = postcard::to_stdvec(&(
            vec![(
                "src/lib.rs",
                ChangeKind::Renamed {
                    from: "src/old.rs".into(),
                },
                "@@ -4 +4 @@\n-a\n+b\n",
            )],
            7u8,
        ))
        .unwrap();
        for lines in [LineOrigin::File, LineOrigin::Excerpt] {
            let message = (vec![change(lines)], 7u8);
            assert_eq!(
                encoding_for(older, || postcard::to_stdvec(&message).unwrap()),
                before
            );
            let current = postcard::to_stdvec(&message).unwrap();
            assert_ne!(current, before);
            assert_eq!(
                postcard::from_bytes::<(Vec<FileChange>, u8)>(&current).unwrap(),
                message
            );
            assert_eq!(
                decoding_from(PROTOCOL_VERSION, || {
                    postcard::from_bytes::<(Vec<FileChange>, u8)>(&current)
                })
                .unwrap(),
                message
            );
        }
        assert_eq!(
            decoding_from(older, || postcard::from_bytes::<(Vec<FileChange>, u8)>(
                &before
            ))
            .unwrap(),
            (vec![change(LineOrigin::Excerpt)], 7)
        );
        // A record from before the field is unplaced as well.
        assert_eq!(
            serde_json::from_str::<FileChange>(
                r#"{"path":"src/lib.rs","kind":{"Renamed":{"from":"src/old.rs"}},"diff":"@@ -4 +4 @@\n-a\n+b\n"}"#
            )
            .unwrap(),
            change(LineOrigin::Excerpt)
        );
        let json = serde_json::to_string(&change(LineOrigin::File)).unwrap();
        assert_eq!(
            serde_json::from_str::<FileChange>(&json).unwrap(),
            change(LineOrigin::File)
        );

        let patch = |lines| FileChangePatch {
            ownership_unverified: false,
            item_id: "edit".into(),
            kind: ChangeKind::Updated,
            status: ToolStatus::Succeeded,
            imported: true,
            offset: 3,
            total_bytes: 40,
            diff: "+b\n".into(),
            lines,
        };
        let before = postcard::to_stdvec(&(
            vec![(
                false,
                "edit",
                ChangeKind::Updated,
                ToolStatus::Succeeded,
                true,
                3usize,
                40usize,
                "+b\n",
            )],
            7u8,
        ))
        .unwrap();
        for lines in [LineOrigin::File, LineOrigin::Excerpt] {
            let message = (vec![patch(lines)], 7u8);
            assert_eq!(
                encoding_for(older, || postcard::to_stdvec(&message).unwrap()),
                before
            );
            let current = postcard::to_stdvec(&message).unwrap();
            assert_eq!(
                postcard::from_bytes::<(Vec<FileChangePatch>, u8)>(&current).unwrap(),
                message
            );
        }
        assert_eq!(
            decoding_from(older, || {
                postcard::from_bytes::<(Vec<FileChangePatch>, u8)>(&before)
            })
            .unwrap(),
            (vec![patch(LineOrigin::Excerpt)], 7)
        );
    }

    /// A chat launch crosses the protocol that added its draft model both
    /// ways: a request or reply for an older peer leaves the choice out, and
    /// one read from an older peer has none. What follows it stays in place.
    #[test]
    fn launches_cross_the_draft_model_protocol_in_both_directions() {
        use crate::{
            agent::Provider,
            chat::Launch,
            model::{ChatDraftModel, LaunchRecipe, TerminalId},
        };
        let older = DRAFT_MODEL_IN_LAUNCH - 1;
        let recipe = LaunchRecipe {
            program: "claude".into(),
            arguments: vec!["--print".into()],
            working_directory: "/work".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let launch = Launch {
            provider: Provider::Claude,
            recipe: recipe.clone(),
            session_id: Some("resumed".into()),
            draft_model: Some(Box::new(ChatDraftModel {
                session: TerminalId::new(),
                provider: Provider::Claude,
                model: Some("sonnet".into()),
                reasoning: Some("high".into()),
            })),
        };
        let without = Launch {
            draft_model: None,
            ..launch.clone()
        };
        // As the older protocol has it: three fields, then the next value.
        let before =
            postcard::to_stdvec(&((Provider::Claude, &recipe, Some("resumed")), 7u8)).unwrap();
        let message = (launch.clone(), 7u8);
        assert_eq!(
            encoding_for(older, || postcard::to_stdvec(&message).unwrap()),
            before
        );
        let current = postcard::to_stdvec(&message).unwrap();
        assert_ne!(current, before);
        assert_eq!(
            postcard::from_bytes::<(Launch, u8)>(&current).unwrap(),
            message
        );
        assert_eq!(
            decoding_from(older, || postcard::from_bytes::<(Launch, u8)>(&before)).unwrap(),
            (without.clone(), 7)
        );
        // A saved launch from before the field has no choice either.
        let mut json = serde_json::to_value(&launch).unwrap();
        json.as_object_mut().unwrap().remove("draft_model");
        assert_eq!(
            serde_json::from_value::<Launch>(json.clone()).unwrap(),
            without
        );
        // One saved without a choice is written as before the field, so a
        // record an older release digested digests alike.
        assert_eq!(serde_json::to_value(&without).unwrap(), json);
    }

    /// The decode scope belongs to the decode it was entered for: it ends
    /// when that returns, fails or unwinds, and an inner scope restores the
    /// outer one.
    #[test]
    fn the_decoding_scope_ends_however_its_decode_ends() {
        let older = PATCH_LINE_ORIGIN - 1;
        assert!(!decoding_before(PATCH_LINE_ORIGIN));
        let failed = decoding_from(older, || {
            assert!(decoding_before(PATCH_LINE_ORIGIN));
            postcard::from_bytes::<crate::chat::FileChange>(&[0xff])
        });
        assert!(failed.is_err());
        assert!(!decoding_before(PATCH_LINE_ORIGIN));
        decoding_from(older, || {
            decoding_from(PROTOCOL_VERSION, || {
                assert!(!decoding_before(PATCH_LINE_ORIGIN));
            });
            assert!(decoding_before(PATCH_LINE_ORIGIN));
        });
        let unwound = std::panic::catch_unwind(|| {
            decoding_from(older, || panic!("a decode that unwinds"));
        });
        assert!(unwound.is_err());
        assert!(!decoding_before(PATCH_LINE_ORIGIN));
        // Encoding for an older client is a separate scope.
        encoding_for(older, || assert!(!decoding_before(PATCH_LINE_ORIGIN)));
    }

    #[test]
    fn only_the_cli_executable_is_served_as_an_older_client() {
        let command = b"cagents\0agent\0list\0";
        for cli in [
            "/home/user/.local/share/cagents/versions/0.1.3/bin/cagents",
            "/usr/bin/cagents",
            "/home/user/.local/share/cagents/versions/0.1.3/bin/cagents (deleted)",
        ] {
            assert!(is_cli_peer(Path::new(cli), command), "{cli}");
        }
        for other in [
            "/usr/bin/cagents-ui",
            "/usr/bin/cagentsd",
            "/usr/bin/cagents-ui (deleted)",
            "/usr/bin/python3",
            "/",
        ] {
            assert!(!is_cli_peer(Path::new(other), command), "{other}");
        }
        // A bridge relays a remote window, whatever protocol it speaks.
        let cli = Path::new("/usr/bin/cagents");
        assert!(!is_cli_peer(cli, b"cagents\0daemon\0bridge\0"));
        assert!(!is_cli_peer(cli, b"/usr/bin/cagents\0daemon\0bridge"));
        // A message that merely mentions it is still the CLI.
        assert!(is_cli_peer(cli, b"cagents\0send\0abcd\0daemon bridge\0"));
        assert!(is_cli_peer(cli, b""));
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

    /// How a provider addresses a chat's subagents reaches only a client of
    /// this protocol: no listed older client could decode it, so it is
    /// withheld rather than sent, and a card there names such an agent only
    /// as "an agent".
    #[test]
    fn a_chats_agent_addresses_are_withheld_from_every_older_client() {
        let addresses = ServerMessage::ChatAgentAddresses {
            chat_id: crate::model::TerminalId::new(),
            addresses: vec![crate::chat::AgentAddress {
                address: "a1892b93e88999a7d".into(),
                subagent: "toolu_agent".into(),
            }],
        };
        assert!(!OLDER_CLIENTS.is_empty());
        for client in OLDER_CLIENTS {
            assert!(client.protocol < 173);
            assert!(
                !client.admits_message(&addresses),
                "protocol {} clients cannot decode it",
                client.protocol
            );
        }
        // Appended after every message the oldest listed client knows.
        assert!(variant_index(&addresses).unwrap() >= CANDIDATE_LINE.server_messages);
    }

    /// A change to a project's pending automation requests is told only to a
    /// window of this protocol. No listed older client could decode it, so it
    /// is withheld, and such a window keeps finding requests by reading them.
    #[test]
    fn automation_request_changes_are_withheld_from_every_older_client() {
        let changed = ServerMessage::AutomationProposalsChanged {
            repository: "/fixture".into(),
        };
        assert!(!OLDER_CLIENTS.is_empty());
        for client in OLDER_CLIENTS {
            assert!(client.protocol < 178);
            assert!(
                !client.admits_message(&changed),
                "protocol {} clients cannot decode it",
                client.protocol
            );
        }
        assert!(variant_index(&changed).unwrap() >= CANDIDATE_LINE.server_messages);
    }

    #[test]
    fn a_work_reply_after_the_older_protocol_is_refused() {
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let reply = |response| ServerMessage::Response {
            response: ServerResponse::Work(response),
        };
        assert!(client.admits_message(&reply(crate::work::Response::Snapshots(Vec::new()))));
        assert!(
            !client.admits_message(&reply(crate::work::Response::WatchingConversation {
                revision: 0,
                pull_requests: Box::new(crate::work::ConversationPullRequests {
                    generated_at_unix_ms: 0,
                    repository: None,
                    pull_requests: Vec::new(),
                    transcript: crate::work::TranscriptCoverage::Complete,
                    forge_error: None,
                    worktrees: Vec::new(),
                }),
            }))
        );
    }

    /// A client from before a provider was appended cannot decode a value
    /// naming it. Where absence means something it understands, as a
    /// terminal's agent activity does, it is sent nothing in that place;
    /// any other message naming the provider is refused. A current client
    /// receives everything.
    #[test]
    fn older_clients_are_never_sent_a_provider_they_predate() {
        use crate::agent::{Activity, ActivityState, Provider};
        use crate::terminal_ipc::{ChatCapacity, ChatCapacityResident, TerminalDescriptor};
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        assert!(client.protocol < ANTIGRAVITY_AND_MUSE);
        let current = OlderClient {
            protocol: PROTOCOL_VERSION,
            ..client
        };
        let encode = |protocol, message: &ServerMessage| {
            encoding_for(protocol, || postcard::to_allocvec(message)).unwrap()
        };
        for provider in Provider::ALL {
            let known = !matches!(provider, Provider::Antigravity | Provider::Muse);
            let activity = Some(Activity {
                provider,
                state: ActivityState::Working,
            });
            let mut terminal = TerminalDescriptor::relaunchable(
                crate::model::TerminalId::new(),
                crate::model::LaunchRecipe {
                    program: provider.executable().into(),
                    arguments: Vec::new(),
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                None,
            );
            terminal.agent_activity = activity;
            let listed = ServerMessage::Response {
                response: ServerResponse::Listed {
                    terminals: vec![terminal],
                },
            };
            assert!(client.admits_message(&listed), "{provider:?}");
            assert!(current.admits_message(&listed), "{provider:?}");
            let read = |bytes: Vec<u8>| match postcard::from_bytes(&bytes).unwrap() {
                ServerMessage::Response {
                    response: ServerResponse::Listed { mut terminals },
                } => terminals.remove(0).agent_activity,
                other => panic!("{other:?}"),
            };
            assert_eq!(
                read(encode(client.protocol, &listed)),
                activity.filter(|_| known),
                "{provider:?}"
            );
            assert_eq!(read(encode(PROTOCOL_VERSION, &listed)), activity);
            // Nothing stands in for a chat's provider.
            let capacity = ServerMessage::Response {
                response: ServerResponse::ChatCapacity {
                    capacity: ChatCapacity {
                        used: 1,
                        resident: 1,
                        spawning: 0,
                        limit: 8,
                        residents: vec![ChatCapacityResident {
                            chat_id: crate::model::TerminalId::new(),
                            provider,
                            status: "idle".into(),
                            release_blocked: None,
                        }],
                        omitted_residents: 0,
                    },
                    persistence_warning: None,
                },
            };
            assert!(current.admits_message(&capacity), "{provider:?}");
            assert_eq!(client.admits_message(&capacity), known, "{provider:?}");
        }
    }

    #[test]
    fn expanded_work_counts_require_a_matching_client_before_reading_or_pinning() {
        use crate::work::{Board, Request, Response, Scope};
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let current = OlderClient {
            protocol: WORK_CHECK_COUNTS,
            ..client
        };
        for request in [
            ClientRequest::WatchWork,
            ClientRequest::Work(Request::Board {
                scope: Scope::All,
                fresh: false,
            }),
            ClientRequest::Work(Request::Pin {
                scope: Scope::All,
                item: "branch".into(),
                column: None,
            }),
        ] {
            assert!(!client.admits_request(&request));
            assert!(current.admits_request(&request));
        }
        for request in [
            ClientRequest::Work(Request::RemovalPlan {
                scope: Scope::All,
                item: "branch".into(),
                fetch: false,
            }),
            ClientRequest::Work(Request::Snapshots { scope: Scope::All }),
        ] {
            assert!(client.admits_request(&request));
        }
        let board = Board {
            generated_at_unix_ms: 0,
            stale_after_days: 7,
            projects: Vec::new(),
            items: Vec::new(),
            linear_unavailable: None,
        };
        for message in [
            ServerMessage::WorkBoard {
                revision: 1,
                board: Box::new(board.clone()),
            },
            ServerMessage::Response {
                response: ServerResponse::Work(Response::Board(board.clone())),
            },
            ServerMessage::Response {
                response: ServerResponse::Work(Response::Watching {
                    revision: 1,
                    board: Box::new(board),
                }),
            },
        ] {
            assert!(!client.admits_message(&message));
            assert!(current.admits_message(&message));
        }
        assert!(!client.admits_message(&ServerMessage::Response {
            response: ServerResponse::Work(Response::Pinned(Box::new(crate::work::Item {
                project: "/repo".into(),
                project_name: "repo".into(),
                key: crate::work::ItemKey::Branch("branch".into()),
                branch: None,
                worktree: None,
                head: None,
                comparison: None,
                dirty: None,
                pull_request: None,
                linear: None,
                sessions: Vec::new(),
                last_activity: None,
                error: None,
                column: crate::work::Column::InProgress,
                derived: crate::work::Column::InProgress,
                pinned: false,
            }))),
        }));
    }

    #[test]
    fn an_automation_reply_holding_a_firing_is_refused_before_it_changed() {
        use crate::automations::{HistoryPage, Response};

        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let reply = |response| ServerMessage::Response {
            response: ServerResponse::Automation(response),
        };
        let history = || {
            Response::History(HistoryPage {
                firings: Vec::new(),
                next: None,
            })
        };
        assert!(!client.admits_message(&reply(history())));
        assert!(client.admits_message(&reply(Response::Listed(Vec::new()))));
        assert!(client.admits_message(&reply(Response::Removed)));
        assert!(client.admits_message(&reply(Response::Proposals(Vec::new()))));
        let changed = OlderClient {
            protocol: FIRING_WITH_SKIPPED_OCCURRENCES,
            ..client
        };
        assert!(changed.admits_message(&reply(history())));
    }

    #[test]
    fn a_request_answered_with_a_firing_is_turned_away_before_it_changed() {
        use crate::{agent_execution::Authority, automations::Request};
        use uuid::Uuid;

        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let asking = |operation| ClientRequest::Automation {
            authority: Authority::default(),
            operation,
        };
        assert!(!client.admits_request(&asking(Request::RunNow {
            id: Uuid::nil(),
            execution_revision: 0,
            request: Uuid::nil(),
            confirmed: true,
        })));
        assert!(!client.admits_request(&asking(Request::History {
            id: None,
            before: None,
            limit: 1,
        })));
        assert!(!client.admits_request(&asking(Request::ShowFiring { id: Uuid::nil() })));
        assert!(!client.admits_request(&asking(Request::ShowRun {
            run: crate::model::RunId::new(),
        })));
        assert!(client.admits_request(&asking(Request::Cancel {
            id: Uuid::nil(),
            firing: Uuid::nil(),
        })));
        let changed = OlderClient {
            protocol: FIRING_WITH_SKIPPED_OCCURRENCES,
            ..client
        };
        assert!(changed.admits_request(&asking(Request::ShowFiring { id: Uuid::nil() })));
    }

    #[test]
    fn model_edges_are_neither_watched_nor_sent_before_they_carried_effects() {
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let edge = ServerMessage::WorkspaceModelEdited {
            sequence: 1,
            edit: crate::workspace_model::ModelEdit::AgentsChanged,
            effects: Default::default(),
        };
        // Its index predates the older protocol; its shape does not.
        assert!(variant_index(&edge).unwrap() < client.server_messages);
        assert!(!client.admits_request(&ClientRequest::WatchWorkspaceModel));
        assert!(!client.admits_message(&edge));
        assert!(client.admits_request(&ClientRequest::ReadWorkspaceModel));
        assert!(client.admits_request(&ClientRequest::EditWorkspaceModel {
            edit: crate::workspace_model::ModelEdit::AgentsChanged,
        }));
        assert!(client.admits_message(&ServerMessage::Response {
            response: ServerResponse::WorkspaceModel {
                model: Default::default(),
            },
        }));
        // The legacy read and edit contract stays available, but none of the
        // new subscriber-only requests or replies may reach a carried CLI.
        let scope = crate::workspace_model::views::Scope::Channel(crate::model::ChannelId::new());
        assert!(!client.admits_request(&ClientRequest::ReadWorkspaceView { scope }));
        assert!(!client.admits_request(&ClientRequest::ReadWorkspaceSnapshot));
        assert!(
            !client.admits_request(&ClientRequest::ReadWorkspaceMetadata {
                query: crate::workspace_model::metadata::Query::Identity
            })
        );
        assert!(!client.admits_request(&ClientRequest::ApplyWorkspaceEdit {
            edit: crate::workspace_model::ModelEdit::AgentsChanged,
        }));
        for response in [
            ServerResponse::WorkspaceSnapshot {
                model: Default::default(),
                closed: Vec::new(),
            },
            ServerResponse::WorkspaceView {
                view: None,
                sequence: 0,
            },
            ServerResponse::WorkspaceEdit(crate::workspace_model::delta::EditReceipt {
                identity: uuid::Uuid::new_v4(),
                sequence: 0,
                effects: Default::default(),
            }),
        ] {
            assert!(!client.admits_message(&ServerMessage::Response { response }));
        }
        let changed = OlderClient {
            protocol: MODEL_EDGES_WITH_EFFECTS,
            ..client
        };
        assert!(changed.admits_request(&ClientRequest::WatchWorkspaceModel));
        assert!(changed.admits_message(&edge));
    }

    #[test]
    fn an_agent_reply_after_the_older_protocol_is_refused() {
        use crate::agent_execution::{
            Reply,
            directory::{Directory, Members, Scope, Sessions},
        };
        let client = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
        let reply = |reply| ServerMessage::Response {
            response: ServerResponse::Agent(reply),
        };
        assert!(client.admits_message(&reply(Reply::Directory(Directory {
            caller: None,
            scope: Scope::All,
            sessions: Vec::new(),
        }))));
        assert!(client.admits_message(&reply(Reply::InboxAcknowledged)));
        assert!(!client.admits_message(&reply(Reply::Members(Members {
            channel: crate::model::ChannelId::new(),
            caller: None,
            sessions: Vec::new(),
        }))));
        // A carried CLI lists sessions with the read it knows, which reports
        // no usage limit or account.
        assert!(!client.admits_message(&reply(Reply::Sessions(Sessions {
            caller: None,
            scope: Scope::All,
            sessions: Vec::new(),
        }))));
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
            work_responses: u32::MAX,
            agent_replies: u32::MAX,
            device_replies: u32::MAX,
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
