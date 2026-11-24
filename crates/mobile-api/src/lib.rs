//! The messages a phone exchanges with `cagents daemon bridge --mobile`.
//!
//! The daemon's own protocol is a positional binary encoding of Rust types
//! that changes with nearly every release, and both ends must be the same
//! build. A phone app updates on its own schedule, so it speaks this API
//! instead: JSON, one object per line, over the bridge's standard input and
//! output. The bridge ships with the daemon and translates.
//!
//! # Compatibility
//!
//! Within an API version the contract only grows:
//!
//! - Fields are never removed, renamed or retyped. New fields are optional
//!   (absent means the old behaviour) or always sent by the server.
//! - Both sides ignore object fields they don't know.
//! - A string enum can gain values. A client maps a value it doesn't know to
//!   its own "unknown" case; every value enum here decodes one as
//!   [`Unknown`] too.
//! - A client ignores an event it doesn't know, and a server answers a
//!   request whose `op` it doesn't know with [`ErrorCode::UnknownOp`].
//! - An op can gain optional parameters. Every op takes a `params` object,
//!   which may be omitted when it would be empty.
//!
//! A change that can't be made this way needs a new API version, which the
//! `hello` / `welcome` exchange negotiates. `tests/mobile_api_contract.rs` in
//! the root package holds every recorded message to this.
//!
//! [`Unknown`]: Activity::Unknown

const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

use serde::{Deserialize, Serialize};

pub mod pairing;

/// The newest API version this build speaks.
pub const API_VERSION: u32 = 1;
/// The oldest API version this build still serves.
pub const MIN_API_VERSION: u32 = 1;
/// The longest line a client may send, newline excluded.
pub const MAX_CLIENT_LINE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Client to server
// ---------------------------------------------------------------------------

/// One line from the phone. The first must be [`ClientLine::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientLine {
    Hello(Hello),
    Request(Request),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The range of API versions the client speaks, inclusive.
    pub api_min: u32,
    pub api_max: u32,
    pub client: ClientInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Such as `cagents-android`.
    pub name: String,
    pub version: String,
}

/// A request. The server answers each with [`ServerLine::Ok`] or
/// [`ServerLine::Error`] carrying the same `id`; what a request makes
/// available arrives as events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRequest")]
pub struct Request {
    /// Chosen by the client; echoed in the answer.
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

/// A request as it arrives, with `params` optional, so an op that takes
/// none today can gain one without an older server refusing it.
#[derive(Deserialize)]
struct RawRequest {
    id: u64,
    op: String,
    #[serde(default)]
    params: serde_json::Value,
}

impl TryFrom<RawRequest> for Request {
    type Error = serde_json::Error;

    fn try_from(raw: RawRequest) -> Result<Self, Self::Error> {
        let params = match raw.params {
            serde_json::Value::Null => serde_json::Value::Object(serde_json::Map::new()),
            params => params,
        };
        let call = Call::deserialize(serde_json::json!({ "op": raw.op, "params": params }))?;
        Ok(Self { id: raw.id, call })
    }
}

/// What a [`Request`] asks for. An op without parameters is an empty struct
/// variant, so that it reads an empty or future `params` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "params")]
pub enum Call {
    /// Answered with `ok`; useful as a keepalive and to measure latency.
    #[serde(rename = "ping")]
    Ping {},
    /// Send a [`Event::Sessions`] now and whenever the list changes.
    #[serde(rename = "sessions.watch")]
    WatchSessions {},
    #[serde(rename = "sessions.unwatch")]
    UnwatchSessions {},
    /// Send a [`Event::Notifications`] now and whenever the set of
    /// unacknowledged notices changes.
    #[serde(rename = "notifications.watch")]
    WatchNotifications {},
    #[serde(rename = "notifications.unwatch")]
    UnwatchNotifications {},
    /// Dismiss a notice everywhere, including on the desktop.
    #[serde(rename = "notification.acknowledge")]
    AcknowledgeNotification { id: String },
    /// Send a [`Event::Conversation`] with `reset` now, then one for each
    /// change. Only sessions with `watchable` set can be watched.
    #[serde(rename = "conversation.watch")]
    WatchConversation { session: String },
    #[serde(rename = "conversation.unwatch")]
    UnwatchConversation { session: String },
    /// Extend a watched conversation's window with older items, when its
    /// state says `has_earlier`.
    #[serde(rename = "conversation.load_earlier")]
    LoadEarlier { session: String },
    /// Send the user's message to a watched conversation.
    #[serde(rename = "conversation.send")]
    Send {
        session: String,
        text: String,
        #[serde(default)]
        mode: SendMode,
    },
    /// Stop a watched conversation's current turn.
    #[serde(rename = "conversation.interrupt")]
    Interrupt { session: String },
    /// Answer one of a watched conversation's pending [`PendingRequest`]s.
    #[serde(rename = "conversation.answer")]
    Answer {
        session: String,
        request: String,
        /// The `id` of the chosen [`PendingRequest::actions`] entry.
        action: String,
        /// Values for the request's fields; a field left out is answered
        /// with no values.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answers: Vec<FieldAnswer>,
    },
}

impl Call {
    /// Every `op` this version knows, so a server can tell a request it
    /// doesn't know from a malformed one.
    pub const OPS: &[&str] = &[
        "ping",
        "sessions.watch",
        "sessions.unwatch",
        "notifications.watch",
        "notifications.unwatch",
        "notification.acknowledge",
        "conversation.watch",
        "conversation.unwatch",
        "conversation.load_earlier",
        "conversation.send",
        "conversation.interrupt",
        "conversation.answer",
    ];
}

/// How a message reaches a conversation that is already working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// Start a turn, or follow the provider's own default while one runs.
    #[default]
    Send,
    /// Steer the running turn at its next step.
    Steer,
    /// Hold the message until the running turn ends.
    Queue,
    /// A mode from a newer client; the server refuses the message rather
    /// than guess.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldAnswer {
    pub field: String,
    pub values: Vec<String>,
}

// ---------------------------------------------------------------------------
// Server to client
// ---------------------------------------------------------------------------

/// One line to the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerLine {
    /// The answer to `hello`.
    Welcome(Welcome),
    Ok {
        id: u64,
    },
    /// `id` is absent for an error that answers no request, such as a line
    /// that isn't JSON or a refused `hello`.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        error: Error,
    },
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The API version both sides now speak.
    pub api: u32,
    /// The cagents release serving the bridge.
    pub server_version: String,
    /// The computer's host name, for telling paired computers apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: ErrorCode,
    /// Written for the person using the phone.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line isn't a well-formed message.
    BadRequest,
    /// No API version is common to both sides; the server closes after this.
    UnsupportedApi,
    /// The server doesn't know this request's `op`.
    UnknownOp,
    /// The session or notice no longer exists.
    NotFound,
    /// The request needs a `conversation.watch` of that session first.
    NotWatching,
    /// The session can't be watched, such as a provider running in a terminal.
    NotWatchable,
    /// cagents refused, such as an agent another session owns.
    NotAllowed,
    /// cagents can't do it now; trying again later may work.
    Unavailable,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    Sessions(Sessions),
    Conversation(Box<ConversationUpdate>),
    /// A watched conversation can no longer be followed: it closed, or its
    /// provider was replaced. The watch has ended.
    ConversationEnded {
        session: String,
        message: String,
    },
    Notifications(Notifications),
    /// The bridge is about to exit, such as when the daemon stops or is
    /// updated. Reconnecting starts a bridge of the new version.
    Closing {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sessions {
    /// Every session the daemon hosts, whoever started it.
    pub sessions: Vec<Session>,
}

/// A provider conversation: a native chat, or a provider CLI in a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Addresses this session until it exits.
    pub id: String,
    /// Stable across the session's relaunches, which get a new `id`.
    pub lineage: String,
    pub surface: Surface,
    /// The provider's command name, such as `claude` or `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The provider's display name, such as `Claude Code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The open project's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    pub activity: Activity,
    /// The provider has exited; its conversation may still be readable.
    pub exited: bool,
    /// The provider's last reported failure, such as a usage limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Unix milliseconds of the conversation's last saved activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_at: Option<u64>,
    /// Present when the session is a delegated agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
    /// Whether `conversation.watch` can follow it.
    pub watchable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Chat,
    Terminal,
    #[serde(other)]
    Unknown,
}

/// What a conversation is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Idle,
    Working,
    /// It needs the person: an approval, a question, or its turn ended
    /// waiting for an answer.
    Waiting,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub owner: AgentOwner,
    /// The owning session's lineage, when `owner` is `session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_lineage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOwner {
    /// Started by the person, from the command line.
    User,
    /// Started by another session.
    Session,
    Automation,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Accepted,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

/// A change to a watched conversation. A client keeps the items by `id` and
/// shows them in `order`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationUpdate {
    pub session: String,
    /// Replace everything held for this conversation: `items` is the whole
    /// window and `order` is present.
    pub reset: bool,
    /// The conversation's current state, sent whole with every update.
    pub state: ConversationState,
    /// Every item's ID in display order, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// Items that are new or changed, replacing any held under their ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
    /// Text streamed onto held text items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub appends: Vec<TextAppend>,
}

/// More text for a held [`ItemBody::Text`] item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextAppend {
    pub id: String,
    /// The held text's length in UTF-16 code units, as a check. A client
    /// whose copy differs has missed something and should watch again.
    pub offset: u64,
    pub text: String,
    /// The item's new [`Item::at`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub activity: Activity,
    /// A turn is running.
    pub busy: bool,
    /// The provider takes messages now.
    pub ready: bool,
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why the conversation is shown read-only, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<String>,
    /// Older items exist beyond the window; `conversation.load_earlier`
    /// fetches them.
    pub has_earlier: bool,
    pub loading_earlier: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the conversation is waiting for the person to decide.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requests: Vec<PendingRequest>,
    /// Messages held until the running turn can take them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedMessage>,
    /// The plan the conversation is working to, newest only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<PlanStep>,
    /// Agents this conversation started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<Subagent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Active,
    Done,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    pub id: String,
    /// The provider's agent type, such as `Explore`.
    pub kind: String,
    pub description: String,
    pub status: ToolStatus,
    pub waiting: bool,
}

/// An approval, a question, or a consent the provider asks the person for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRequest {
    pub id: String,
    pub kind: RequestKind,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<RequestField>,
    /// The decisions offered. There is no implicit default: an approval is
    /// given only by choosing one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Choice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Authorize a tool action or more permissions.
    Approval,
    Question,
    Consent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestField {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Offered values; free text when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    pub multiple: bool,
    /// Don't echo the answer.
    pub secret: bool,
    pub initial: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// Transcript items
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub role: Role,
    #[serde(flatten)]
    pub body: ItemBody,
    /// Unix milliseconds when the item first appeared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// Unix milliseconds when it last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    #[serde(other)]
    Unknown,
}

/// What an item holds. Long tool output is clipped to its end, with
/// `clipped` set; the full text stays on the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemBody {
    Text {
        text: String,
        /// The model's reasoning rather than what it said.
        thinking: bool,
        /// Narration while the turn works, rather than its answer.
        commentary: bool,
    },
    /// A shell command the provider ran.
    Command {
        /// What it was applied to or the provider's summary; may be empty.
        label: String,
        command: String,
        action: CommandAction,
        status: ToolStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        output: String,
        clipped: bool,
    },
    /// Edits the provider made.
    Changes {
        files: Vec<FileChange>,
        status: ToolStatus,
    },
    WebSearch {
        query: String,
        results: Vec<SearchResult>,
    },
    /// Any other tool call.
    Tool {
        name: String,
        input: String,
        output: String,
        clipped: bool,
        status: ToolStatus,
    },
    Image {
        description: String,
    },
    /// Something delivered to the conversation that the person didn't write:
    /// an agent's report, another session's message, or a notice.
    Delivery {
        delivery: DeliveryKind,
        /// The sender's name; may be empty.
        from: String,
        /// What tells two senders of one name apart, never for display: a
        /// message's sending session as its short ID, or a report's task ID.
        /// Empty when there is none, and absent from older servers.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        sender: String,
        summary: String,
        body: String,
        clipped: bool,
    },
    /// A kind this server can't describe to API version 1.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Denied,
    Stopped,
    /// Stopped with background work of its own still running.
    Waiting,
    /// Superseded, with nothing left to wait for.
    Ended,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandAction {
    Run,
    Read,
    Search,
    List,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub change: ChangeKind,
    /// The path before a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub added_lines: u64,
    pub removed_lines: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Deleted,
    Updated,
    Renamed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    /// A delegated agent's final report.
    Report,
    /// A message from another session.
    Message,
    /// A provider notice, such as background work finishing or compaction.
    Notice,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notifications {
    /// Unacknowledged notices, newest first.
    pub notices: Vec<Notice>,
}

/// Something that wants the person's attention, as the desktop would notify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub id: String,
    pub kind: NoticeKind,
    /// Whether the notice asks something of the person reading it.
    #[serde(default)]
    pub asks: bool,
    /// A short heading, such as `Agent needs your attention`.
    pub title: String,
    /// The session it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The conversation's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// An excerpt, when the person allows previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// What a `waiting` notice asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    Succeeded,
    Failed,
    Cancelled,
    Waiting,
    ReviewRequested,
    Interrupted,
    MessageReceived,
    AutomationPaused,
    AutomationBlocked,
    #[serde(other)]
    Unknown,
}
