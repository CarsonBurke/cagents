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
}

/// The install candidate line, 0.1.3+local.c68275e94. Its chats are carried
/// across an update to this release and keep running its CLI.
const CANDIDATE_LINE: OlderClient = OlderClient {
    protocol: CANDIDATE_LINE_PROTOCOL,
    proven_at: 166,
    client_requests: 74,
    server_messages: 17,
    server_responses: 40,
    work_responses: 6,
    agent_replies: 11,
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
const MODEL_EDGES_WITH_EFFECTS: u16 = 166;

/// The `ServerMessage` variants that changed shape in place for clients before
/// [`MODEL_EDGES_WITH_EFFECTS`]. The wire format proof skips exactly these.
// Wire shape tracing uses Serde variant names, including rename_all.
const MODEL_EDGES: &[&str] = &["workspace_model_edited"];

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
        }
    }

    /// Whether this request exists in the client's protocol and leaves the
    /// daemon to the CLI that matches it.
    pub fn admits_request(&self, request: &ClientRequest) -> bool {
        variant_index(request).is_some_and(|index| index < self.client_requests)
            && !changes_the_daemon(request)
            && !self.cannot_read_the_firing_for(request)
            && !self.cannot_read_model_edges_for(request)
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
        match message {
            ServerMessage::Response { response } => {
                let nested = match response {
                    ServerResponse::Work(work) => {
                        variant_index(work).is_some_and(|index| index < self.work_responses)
                    }
                    ServerResponse::Automation(automation) => {
                        self.protocol >= FIRING_WITH_SKIPPED_OCCURRENCES
                            || !holds_a_firing(automation)
                    }
                    ServerResponse::Agent(reply) => {
                        variant_index(reply).is_some_and(|index| index < self.agent_replies)
                    }
                    _ => true,
                };
                nested && variant_index(response).is_some_and(|index| index < self.server_responses)
            }
            ServerMessage::WorkspaceModelEdited { .. } => self.protocol >= MODEL_EDGES_WITH_EFFECTS,
            _ => true,
        }
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
        ClientRequest::Shutdown { .. }
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
                }),
            }))
        );
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
