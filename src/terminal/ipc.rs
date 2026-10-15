use std::{
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    instance,
    model::{
        HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalOutputActivity,
        TerminalSession, TranscriptMetadata,
    },
    terminal::{
        CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, KeyInput, MouseInput,
        PasteOrigin, RenderSnapshot, Rgb, RowSnapshot, ScrollSnapshot, TerminalConfig,
        TerminalEvent, TerminalSize, TerminalTheme, Underline,
    },
};

/// Version 3 replaced the JSON payload codec with postcard and made terminal
/// snapshots row-delta capable.
///
/// Both are breaking changes with no negotiation path. Because the codec itself
/// changed, a frame from a version-2 peer cannot be decoded at all: it fails as
/// `FrameError::InvalidPayload`, not as a version mismatch. The `version` field
/// therefore only diagnoses skew between versions that share a codec, and every
/// reader must treat an undecodable frame as a possible version skew rather than
/// as a corrupt peer.
/// Version 4 adds daemon-owned output activity to descriptors and snapshots.
/// Version 5 adds exact agent recovery identity semantic events.
/// Version 6 adds parser themes, viewport scrolling, and proof-bound native chat.
/// Version 7 adds PTY-owner-checked terminal close decisions.
/// Version 8 adds durable upgrade preparation and restored source linkage.
/// Version 9 adds read-only conversation preview and explicit bound activation.
/// Version 10 adds intentional exits, transient native environments and host-owned CLI returns.
/// Version 11 adds semantic chat items: commands, file changes, web searches, and a
/// commentary/final phase on assistant text. `ItemContent` gained variants ahead of
/// `Tool` and `Image` and a third field on `Text`; postcard is not self-describing, so
/// neither the shifted variant indices nor the added field can be tolerated by a peer
/// that predates them - a `serde(default)` never fires because a missing trailing field
/// is indistinguishable from the next value in the frame.
/// Version 12 adds read-only conversation listing, a provider skill catalog and an
/// observation stamp on every item. The listing request and response sit beside their
/// preview neighbours rather than at the end of their enums, and both `Snapshot` and
/// `Item` gained a field, so a peer that predates this reads the wrong layout at the
/// same advertised version unless the version itself moves.
/// Terminal-inventory observation - `ClientRequest::WatchTerminals` and
/// `ServerMessage::TerminalCreated` - deliberately does not move the version.
/// Both are appended variants, so no existing variant index shifts, and the
/// broadcast only ever reaches a connection that subscribed with a request a
/// peer predating this change cannot issue. Such a peer therefore never has to
/// decode a variant index it does not know.
/// The shared workspace model - `ClientRequest::WatchWorkspaceModel`,
/// `ReadWorkspaceModel` and `EditWorkspaceModel`,
/// `ServerResponse::WorkspaceModel` and `ServerMessage::WorkspaceModelEdited` -
/// deliberately does not move the version either, for the same two reasons
/// terminal-inventory observation did not. Every one of them is appended at the
/// end of its enum, so no existing variant index shifts and no field is added
/// to a type a peer already decodes. And none of them ever reaches a peer that
/// predates them: the edge only goes to a connection that subscribed with a
/// request such a peer cannot issue, and the response only answers a request
/// such a peer cannot issue. A peer therefore never has to decode a variant
/// index it does not know, which is the only thing the version could warn it
/// about.
/// Version 14 distinguishes root process exit from final PTY completion and
/// publishes semantic conversation titles independently of terminal titles.
/// Version 15 adds terminal palettes and durable recovery-origin identity.
/// Version 16 carries the name of the signal that ended a terminal.
/// Version 17 adds `ClientRequest::Paste`, which the PTY owner encodes against
/// the child's own bracketed-paste mode and the paste's origin. It is
/// appended, but unlike the observation requests above a current client issues
/// it on every paste, so a daemon that predates it would be sent a variant
/// index it cannot decode.
/// Version 18 adds `ServerMessage::ChatAccountUsage`. It is appended, but it
/// reaches every chat subscriber unasked, so a client that predates it would
/// be sent a variant index it cannot decode.
/// Version 19 adds the reach of a provider's skills to its chat capabilities.
/// Version 20 says in a chat snapshot whether its provider takes commands.
/// Version 21 carries an explicit project kind in the shared workspace model.
/// Version 22 carries daemon-issued owned Codex listener evidence in chat and
/// terminal descriptors; postcard fields require a matched daemon and UI.
/// Version 23 carries explicit environment removals in launch recipes.
/// Version 24 adds provider-confirmed account recovery and removed-account errors.
/// Version 26 adds daemon-owned durable lifecycle attention and notification control.
/// Version 27 reports screen loss after authenticated crash adoption and adds
/// exact-recipe cleanup for retained completed terminals.
/// Version 28 adds durable agent commands, execution identity, and hosted consoles.
/// Version 29 carries native-provider crash-continuity capabilities.
/// Version 30 makes notification activation and lifecycle identity typed.
/// Version 31 adds explicit user automation management and durable run routing.
/// Version 32 binds update preparation to typed, daemon-owned session impact.
/// Version 33 carries an optional reasoning effort in agent and automation specs.
/// Version 34 answers a test notification request with its own outcome.
/// Version 35 carries row wrapping, OSC 8 links, bracketed-paste state, live cwd,
/// and terminal-owner scrollback search, clearing, and selection copying.
/// Version 38 adds a read-only directory of hosted sessions to agent operations.
/// Version 39 adds transcript items another agent delivered.
/// Version 40 carries provider history-loading state in native chat snapshots.
/// Snapshots require a matched set.
/// Version 41 lets an agent's owner read its requests, answer and interrupt it.
/// Version 42 adds native resume preparation, queue mutation and branching.
/// Version 43 keys delegated ownership to a session lineage and lists each
/// session's lineage.
/// Version 44 adds messages between sessions and reading the messages left
/// for the caller.
/// Version 45 lets a connection act for an agent session, which may then
/// change only the terminals it controls.
/// Version 47 carries native permission denials and scoped permission settings.
/// Version 48 adds device workspace operations.
/// Version 49 carries host-owned account homes and fences idle account handoffs.
/// Version 50 frames device account JSON values as strings for binary IPC.
/// Version 51 carries native chat start timestamps and publication clock samples.
/// Device workspace and account operations require a matched executable set.
/// Version 52 shares durable pane views across clients with revision-fenced edits.
/// Version 53 adds worktree view scopes and durable automatic-terminal reservations.
/// Version 55 carries the delivery and attachments of queued chat messages.
/// Version 56 shares manually arranged worktree and channel navigation.
/// Version 57 distinguishes published device saves with unconfirmed durability.
/// Version 58 acknowledges inbox messages after clients receive their bodies.
/// Version 59 preserves raw Unix selected-file bytes in shared repository panes.
/// Normal UTF-8 paths retain their framing; older readers reject raw paths.
/// Version 60 stops one subagent, says which subagents run in the background
/// and whether the provider's interrupt stops them, reports a stopped one,
/// and carries process-scoped turn generations for cancellation confirmation.
/// Version 61 carries owner-scoped agent names and directory channel membership.
/// Version 62 guards non-UI interrupts and binds stop/update consent to
/// the running background subagents it would terminate.
/// Version 63 carries the daemon-observed completion time of a native turn.
/// Version 64 carries immutable hosted automation proposals and user decisions.
/// Version 65 carries ephemeral provider prompt suggestions in native snapshots.
/// Version 66 adds daemon-fenced optional merged-worktree cleanup.
/// Version 67 carries original-writer transcript location identity receipts.
/// Version 68 adds lineage-scoped terminal mailbox attention.
/// Version 69 adds the reason a native chat is a read-only history viewer to
/// `Snapshot`. Postcard peers require the matched layout even though older
/// JSON snapshots can omit the defaulted field.
/// Version 70 carries native subagent model, waiting state and status detail
/// in sparse events and snapshots.
/// Version 72 carries native fast-mode capabilities/state, its setting command,
/// and separately labeled API-equivalent Codex cost estimates.
/// Version 76 exposes non-secret account usage through delegated execution.
/// Version 77 carries typed delegated-agent view placement and directory scope.
/// Version 78 exposes helper build metadata and conditional idle rollover.
pub const PROTOCOL_VERSION: u16 = 78;
/// Postcard encodes an 80x24 grid in roughly 20 KiB and a typical single-row
/// delta in a few hundred bytes, but a full repaint of a very large grid after
/// a resize still has to fit, so keep generous headroom.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Client requests carry at most a one-MiB byte vector plus small scalars.
pub const MAX_CLIENT_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_RESUME_TERMINALS: usize = 1024;
/// Production full and browserless builds retain the runtime namespace; a
/// mismatched daemon is preserved and reported, never automatically replaced.
pub const SOCKET_FILE_NAME: &str = "cagentsd.sock";

/// Returns the current user's cagents daemon socket path.
pub fn daemon_socket_path() -> io::Result<PathBuf> {
    if let Some(path) = crate::device::socket_path()? {
        return Ok(path);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "XDG_RUNTIME_DIR is not set to an absolute path",
            )
        })?;
    Ok(runtime
        .join(instance::RUNTIME_SUBDIRECTORY)
        .join(SOCKET_FILE_NAME))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientEnvelope {
    pub version: u16,
    pub request_id: u64,
    pub proof: Option<HostSessionProof>,
    pub request: ClientRequest,
}

/// Transport-only execution settings; never embed this in a durable session.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatExecutionEnvironment(pub std::collections::BTreeMap<String, String>);

impl std::fmt::Debug for ChatExecutionEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ChatExecutionEnvironment(<redacted>)")
    }
}

// Boxing the large payload would change this public wire type's construction and
// pattern-matching API; keep the stable API intact.
//
// The codec is postcard, which encodes enums externally with a varint variant
// index. Variant *order* is therefore the compatibility surface: append new
// variants at the end and bump `PROTOCOL_VERSION` when removing or reordering.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientRequest {
    Connect {
        expected_host: Option<HostSessionProof>,
        resume: Vec<ResumeCursor>,
    },
    List,
    Spawn {
        session: TerminalSession,
        options: TerminalSpawnOptions,
    },
    Attach {
        terminal_id: TerminalId,
        after_generation: u64,
        after_event_sequence: u64,
    },
    Detach {
        terminal_id: TerminalId,
    },
    Input {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
    },
    Key {
        terminal_id: TerminalId,
        input: KeyInput,
    },
    Mouse {
        terminal_id: TerminalId,
        input: MouseInput,
    },
    Focus {
        terminal_id: TerminalId,
        focused: bool,
    },
    Scroll {
        terminal_id: TerminalId,
        scroll: TerminalScroll,
    },
    Resize {
        terminal_id: TerminalId,
        size: WireTerminalSize,
    },
    SetVisibility {
        terminal_id: TerminalId,
        visibility: TerminalVisibility,
    },
    Hibernate {
        terminal_id: TerminalId,
    },
    Resume {
        terminal_id: TerminalId,
        visible: bool,
    },
    Snapshot {
        terminal_id: TerminalId,
        after_generation: u64,
    },
    AcknowledgeRedraw {
        terminal_id: TerminalId,
        generation: u64,
    },
    ShutdownTerminal {
        terminal_id: TerminalId,
    },
    Shutdown,
    /// Additive shutdown contract; unsupported revisions never fall back to force.
    ShutdownV1 {
        version: u16,
        operation: ShutdownOperation,
    },
    SetTheme {
        terminal_id: TerminalId,
        theme: TerminalTheme,
    },
    ChatStart {
        session: crate::chat::Session,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        execution_environment: ChatExecutionEnvironment,
    },
    /// Explicitly try the exact saved conversation on a different account.
    /// The reply is gated on provider-confirmed identity, not the launch hint.
    ChatRecoverAccount {
        source: crate::chat::Session,
        replacement: crate::chat::Session,
        /// Automatic handoff fences the exact idle actor before replacing it.
        #[serde(default)]
        idle_generation: Option<u64>,
        execution_environment: ChatExecutionEnvironment,
    },
    ChatAttach {
        session: crate::chat::Session,
    },
    ChatDetach {
        chat_id: TerminalId,
    },
    ChatCommand {
        chat_id: TerminalId,
        command: crate::chat::Command,
    },
    ChatVisibility {
        chat_id: TerminalId,
        visible: bool,
    },
    ChatRead {
        chat_id: TerminalId,
        after_generation: Option<u64>,
    },
    ChatClose {
        chat_id: TerminalId,
    },
    /// Closes only the exact authenticated retained native owner. Used when
    /// restoring its actor is impossible; absence is not teardown evidence.
    ChatCloseRetained {
        session: crate::chat::Session,
    },
    ChatList,
    /// Acknowledge activity without enabling or reading presentation.
    ChatAcknowledge {
        chat_id: TerminalId,
        epoch: u64,
    },
    /// Inspects at the PTY owner and closes only when the chosen policy permits.
    CloseTerminalChecked {
        terminal_id: TerminalId,
        allow_active: bool,
    },
    /// Retires only an already completed result with the exact saved recipe.
    /// This grants no authority to stop a live replacement.
    ReleaseCompletedTerminal {
        session: TerminalSession,
        /// Explicit confirmation may also close its exact paired native actor.
        allow_active: bool,
    },
    /// Rearm hidden output observation without granting presentation credit.
    TerminalAcknowledge {
        terminal_id: TerminalId,
        epoch: u64,
    },
    RefreshAgentActivity {
        terminal_id: TerminalId,
    },
    PrepareUpgrade {
        capability: ShutdownCapability,
        relaunch_agents: bool,
    },
    UpgradeImpact {
        policy: crate::update::impact::Policy,
    },
    PrepareConfirmedUpgrade {
        impact: crate::update::impact::DaemonImpact,
        relaunch_agents: bool,
    },
    ResolveUpgradeChat {
        source: crate::chat::Session,
    },
    ChatPreview {
        session: crate::chat::Session,
        source: HostSessionProof,
    },
    /// Read-only listing of the source account's open conversations. It issues
    /// no token, so it cannot stand in for ChatPreview when authorizing.
    ChatCandidates {
        session: crate::chat::Session,
        source: HostSessionProof,
    },
    ChatStartSelected {
        session: crate::chat::Session,
        source: HostSessionProof,
        token: String,
    },
    /// Replace an attached native actor using its host-retained execution context.
    ChatReturnTerminal {
        chat_id: TerminalId,
    },
    /// GUI acknowledgement after the replacement terminal is durably saved.
    ChatCommitTerminal {
        terminal_id: TerminalId,
    },
    /// Subscribe this connection to terminal-inventory edges caused by other
    /// clients. A connection that never asks receives none, so a strict
    /// request/response client - every CLI invocation - can never be surprised
    /// by an unsolicited frame its state machine has no place for.
    WatchTerminals,
    /// Subscribe this connection to shared-workspace-model edges and return the
    /// model in the same response.
    ///
    /// One request does both so a client cannot miss an edge in the gap between
    /// subscribing and reading - the ordering hazard `watch_terminals` then
    /// `list` has to work around with two requests.
    WatchWorkspaceModel,
    /// Read the shared model without subscribing, for a client - every CLI
    /// invocation - whose state machine has no place for an unsolicited frame.
    ReadWorkspaceModel,
    /// Apply one edit and return the resulting model. The originating
    /// connection is not sent its own echo.
    EditWorkspaceModel {
        edit: crate::workspace_model::ModelEdit,
    },
    BuildInfo,
    /// Text to paste, which the PTY owner encodes for the paste mode the child
    /// is in: framed as a bracketed paste when it asked for one, and otherwise
    /// typed, with what `origin` allows to act as keys. Only the owner can
    /// decide this: the mode is the child's, and a snapshot of it would be
    /// stale by the time the paste arrived.
    Paste {
        terminal_id: TerminalId,
        text: String,
        origin: PasteOrigin,
    },
    ObserveReviewRequests {
        observed: crate::notifications::review::ObservedRequests,
    },
    WatchNotifications {
        presence: crate::notifications::Presence,
    },
    NotificationPresence {
        presence: crate::notifications::Presence,
    },
    ReadNotifications,
    NotificationPreferencesChanged {
        preferences: Option<crate::notifications::Preferences>,
    },
    AcknowledgeNotification {
        id: uuid::Uuid,
    },
    Agent {
        authority: crate::agent_execution::Authority,
        operation: crate::agent_execution::Operation,
    },
    Automation {
        authority: crate::agent_execution::Authority,
        operation: crate::automations::Request,
    },
    /// Shows a desktop notification and answers with whether the desktop
    /// accepted it, answered to the requesting connection alone.
    SendTestNotification,
    SearchTerminal {
        terminal_id: TerminalId,
        request: u64,
        query: String,
        case_sensitive: bool,
    },
    ClearTerminalScrollback {
        terminal_id: TerminalId,
    },
    CopyTerminalSelection {
        terminal_id: TerminalId,
        request: uuid::Uuid,
        selection: crate::terminal::SearchMatch,
    },
    /// Authenticate old ownership before allocating a new resume transport.
    ChatPrepareResume {
        source: crate::chat::Session,
    },
    /// Binds an agent session's capability to this connection, which may then
    /// read terminals and change only those that session controls; it is
    /// refused every other request. A connection that never authenticates
    /// acts for the user.
    Authenticate {
        authority: crate::agent_execution::Authority,
    },
    DeviceOperation {
        operation: crate::device_service::Operation,
    },
    /// Complete the daemon's durable intent, using its authoritative recipe.
    CompletePendingTerminal {
        pane: crate::model::CardId,
        terminal_id: TerminalId,
        options: TerminalSpawnOptions,
    },
    /// Replace an outdated native helper only after the actor fences this
    /// exact idle generation. Never interrupts a provider turn.
    ChatRestartHelper {
        source: crate::chat::Session,
        generation: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeCursor {
    pub terminal_id: TerminalId,
    pub generation: u64,
    pub event_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSpawnOptions {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub max_scrollback: usize,
    pub initially_visible: bool,
    pub theme: TerminalTheme,
}

impl Default for TerminalSpawnOptions {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            cell_width_px: 9,
            cell_height_px: 18,
            max_scrollback: 10_000,
            initially_visible: true,
            theme: TerminalTheme::default(),
        }
    }
}

impl From<TerminalConfig> for TerminalSpawnOptions {
    fn from(value: TerminalConfig) -> Self {
        Self {
            columns: value.columns,
            rows: value.rows,
            cell_width_px: value.cell_width_px,
            cell_height_px: value.cell_height_px,
            max_scrollback: value.max_scrollback,
            initially_visible: value.initially_visible,
            theme: value.theme,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTerminalSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

impl From<WireTerminalSize> for TerminalSize {
    fn from(value: WireTerminalSize) -> Self {
        Self {
            columns: value.columns,
            rows: value.rows,
            cell_width_px: value.cell_width_px,
            cell_height_px: value.cell_height_px,
        }
    }
}

impl From<TerminalSize> for WireTerminalSize {
    fn from(value: TerminalSize) -> Self {
        Self {
            columns: value.columns,
            rows: value.rows,
            cell_width_px: value.cell_width_px,
            cell_height_px: value.cell_height_px,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalVisibility {
    Visible,
    Hidden,
    Hibernated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalScroll {
    Lines(i64),
    Top,
    Bottom,
    Row(u64),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerEnvelope {
    pub version: u16,
    pub request_id: Option<u64>,
    pub proof: HostSessionProof,
    pub message: ServerMessage,
}

/// A borrowed, serialize-only view of [`ServerEnvelope`].
///
/// The daemon fans one broadcast out to every subscriber of a terminal, and the
/// session proof is three `String`s. Cloning the owned envelope per subscriber
/// made a redraw edge allocate proportionally to the subscriber count, so the
/// per-client queue keeps the proof behind an `Arc` and the writer serializes
/// through this view instead.
///
/// The field order, names and types are exactly [`ServerEnvelope`]'s, so the
/// postcard encoding is byte-identical; `borrowed_envelope_encodes_identically`
/// pins that.
#[derive(Debug, Serialize)]
pub struct ServerEnvelopeRef<'envelope> {
    pub version: u16,
    pub request_id: Option<u64>,
    pub proof: &'envelope HostSessionProof,
    pub message: &'envelope ServerMessage,
}

// Boxing the large payload would change this public wire type's construction and
// pattern-matching API; keep the stable API intact.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        response: ServerResponse,
    },
    Redraw {
        terminal_id: TerminalId,
        generation: u64,
    },
    Snapshot {
        terminal_id: TerminalId,
        snapshot: TerminalSnapshot,
    },
    Event {
        terminal_id: TerminalId,
        sequence: u64,
        event: WireTerminalEvent,
    },
    Synchronized {
        terminal_id: TerminalId,
        generation: u64,
        event_sequence: u64,
    },
    HostShuttingDown,
    Error(ProtocolError),
    ChatChanged {
        chat_id: TerminalId,
    },
    /// An observed provider update, coalesced until this subscriber acknowledges.
    /// Unlike ChatChanged, this never authorizes a presentation read.
    ChatObserved {
        chat_id: TerminalId,
        epoch: u64,
    },
    /// First actual output in this subscriber's hidden observation epoch.
    /// This never authorizes a snapshot read or a redraw acknowledgement.
    TerminalObserved {
        terminal_id: TerminalId,
        epoch: u64,
    },
    /// Replaceable native work state, independent of presentation and unread ACKs.
    ChatActivity {
        chat_id: TerminalId,
        state: crate::agent::ActivityState,
        generation: u64,
    },
    /// A terminal some other client created, delivered only to a connection
    /// that subscribed with [`ClientRequest::WatchTerminals`].
    ///
    /// The descriptor is the one an unattached observer is given, so a receiver
    /// holds the launch recipe and the continuity proof it needs to decide
    /// whether it cares and to attach, without listing the whole inventory.
    TerminalCreated {
        terminal: TerminalDescriptor,
    },
    /// One shared-model edit some other client applied, delivered only to a
    /// connection that subscribed with [`ClientRequest::WatchWorkspaceModel`].
    ///
    /// The edit travels rather than the whole model so an edge stays the size
    /// of what changed, and `sequence` is the registry's value after applying
    /// it, which is what lets a receiver tell an edge it has already seen from
    /// one it has not.
    WorkspaceModelEdited {
        sequence: u64,
        edit: crate::workspace_model::ModelEdit,
    },
    /// What a native chat's provider reported about the account the chat runs
    /// on, delivered to every subscriber whatever its visibility. It names no
    /// account: the subscriber maps the chat's launch environment to one.
    ChatAccountUsage {
        chat_id: TerminalId,
        report: crate::account::LiveReport,
        /// Unix milliseconds at which the daemon read the report.
        observed_at: u64,
    },
    NotificationsChanged,
    NotificationActivated {
        target: crate::notifications::Target,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerResponse {
    Connected {
        proof: HostSessionProof,
        resumed: Vec<TerminalDescriptor>,
    },
    Listed {
        terminals: Vec<TerminalDescriptor>,
    },
    Spawned {
        terminal: TerminalDescriptor,
    },
    Attached {
        terminal: TerminalDescriptor,
    },
    Detached,
    Accepted,
    Snapshot {
        terminal_id: TerminalId,
        snapshot: Option<TerminalSnapshot>,
    },
    ShutdownAccepted,
    ShutdownPreflight(ShutdownPreflight),
    UpgradeImpact(crate::update::impact::DaemonImpact),
    ShutdownOutcome(ShutdownOutcome),
    ChatAttached {
        session: crate::chat::Session,
        snapshot: crate::chat::Snapshot,
    },
    ChatSnapshot {
        chat_id: TerminalId,
        snapshot: Option<crate::chat::Snapshot>,
    },
    ChatListed {
        sessions: Vec<crate::chat::Session>,
    },
    ChatClosed {
        chat_id: TerminalId,
        snapshot: crate::chat::Snapshot,
    },
    TerminalCloseChecked {
        terminal_id: TerminalId,
        outcome: TerminalCloseResult,
    },
    AgentActivity {
        terminal_id: TerminalId,
        activity: Option<crate::agent::Activity>,
    },
    UpgradePrepared {
        terminals: u64,
        chats: u64,
    },
    UpgradeChatResolved {
        session: crate::chat::Session,
    },
    ChatPreviewed {
        preview: crate::chat::BindingPreview,
    },
    ChatCandidates {
        candidates: Vec<crate::chat::BindingCandidate>,
    },
    ChatTerminal {
        chat_id: TerminalId,
        session: TerminalSession,
        snapshot: crate::chat::Snapshot,
    },
    WorkspaceModel {
        model: crate::workspace_model::WorkspaceModel,
    },
    BuildInfo {
        version: String,
    },
    Notifications {
        snapshot: crate::notifications::Snapshot,
    },
    Agent(crate::agent_execution::Reply),
    Automation(crate::automations::Response),
    NotificationTest {
        outcome: crate::notifications::TestOutcome,
    },
    ChatResumePrepared {
        session: crate::chat::Session,
    },
    ChatUpdate {
        chat_id: TerminalId,
        update: crate::chat::presentation::Presentation,
    },
    DeviceOperation {
        reply: crate::device_service::Reply,
    },
    /// Cache-query replies go only to the requesting connection and request ID.
    ChatFileChanges {
        chat_id: TerminalId,
        summary: crate::chat::FileChangesSummary,
    },
    ChatFileChangePatches {
        chat_id: TerminalId,
        page: crate::chat::FileChangePage,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalCloseResult {
    Closed,
    Active,
    Unknown(String),
}

pub const SHUTDOWN_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShutdownOperation {
    Preflight,
    Stop {
        require_empty: bool,
        expected_generation: Option<u64>,
    },
    Confirm {
        capability: ShutdownCapability,
    },
}

/// A short-lived, daemon-bound authority for precisely one observed inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownCapability {
    pub proof: HostSessionProof,
    pub token: String,
    pub inventory_generation: u64,
    pub live_terminals: u64,
    pub background_subagents: u64,
    /// Exact running background tasks, independent of transcript tokens.
    pub background_identity: String,
    pub expires_at_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownPreflight {
    pub capability: ShutdownCapability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownOutcome {
    Accepted {
        live_terminals_before: u64,
        termination_requested: bool,
    },
    /// The caller asked for an empty daemon and the daemon is not empty.
    EmptyRequired {
        current_count: u64,
    },
    InventoryChanged {
        current_count: u64,
    },
    Expired,
    DaemonRestarted,
    InvalidCapability,
    AlreadyShuttingDown,
    /// Native chats still own work, so an unconditional stop was refused: the
    /// operator has to observe the inventory and re-issue through the
    /// preflight/confirm capability. Appended last, so the postcard variant
    /// index of every variant above is unchanged.
    ConfirmationRequired {
        current_count: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ProtocolError {
    pub fn new(code: ProtocolErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    AccountRemoved,
    UnsupportedVersion,
    HandshakeRequired,
    AlreadyConnected,
    ProofMismatch,
    InvalidRequest,
    TerminalNotFound,
    TerminalAlreadyExists,
    TerminalUnavailable,
    QueueFull,
    PermissionDenied,
    Internal,
    ViewConflict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalDescriptor {
    /// The process survived an unplanned daemon loss, but its old screen did not.
    #[serde(default)]
    pub screen_lost_after_crash: bool,
    pub session: TerminalSession,
    pub generation: u64,
    pub event_sequence: u64,
    /// The first semantic-event sequence still available for reconnect replay.
    /// A cursor below this value must treat the descriptor/snapshot as the
    /// source of truth rather than assuming that every transient event exists.
    pub oldest_event_sequence: u64,
    pub attached: bool,
    pub transcript: Option<TranscriptMetadata>,
    pub activity: Option<TerminalOutputActivity>,
    pub agent_activity: Option<crate::agent::Activity>,
    /// Authenticated former daemon identities retained by durable upgrade restore.
    #[serde(default)]
    pub upgrade_sources: Vec<TerminalSession>,
    #[serde(default)]
    pub upgrade_relaunches: Vec<UpgradeRelaunch>,
}

/// Explicitly accepted resume-picker or fresh agent launches, bound to the
/// exact former daemon source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpgradeRelaunch {
    pub source: TerminalSession,
    pub recipe: LaunchRecipe,
}

impl TerminalDescriptor {
    pub fn relaunchable(
        terminal_id: TerminalId,
        recipe: LaunchRecipe,
        transcript_path: Option<PathBuf>,
    ) -> Self {
        Self {
            session: TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: terminal_id,
                launch: recipe.clone(),
                transcript_path,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            },
            generation: 0,
            screen_lost_after_crash: false,
            event_sequence: 0,
            oldest_event_sequence: 1,
            attached: false,
            transcript: None,
            activity: None,
            agent_activity: None,
            upgrade_sources: Vec::new(),
            upgrade_relaunches: Vec::new(),
        }
    }
}

/// A terminal repaint on the wire.
///
/// `VisualResources::capture` already rebuilds only the rows Ghostty marked
/// dirty and reuses the `Arc<RowSnapshot>` of every clean row, so a cursor
/// blink or a single appended line changes one row out of fifty. Sending the
/// whole grid discarded that knowledge and made every repaint cost the full
/// grid; `content` preserves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub generation: u64,
    pub activity: Option<TerminalOutputActivity>,
    pub columns: u16,
    pub rows: u16,
    pub foreground: WireRgb,
    pub background: WireRgb,
    pub cursor: Option<WireCursorSnapshot>,
    pub scroll: ScrollSnapshot,
    pub content: SnapshotContent,
    pub detached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotContent {
    /// Every row, self-contained. Always valid to apply.
    Full { lines: Vec<WireRowSnapshot> },
    /// Only the rows that differ from `base_generation`. The receiver must hold
    /// exactly that generation, at these dimensions, or ask for a full repaint.
    Delta {
        base_generation: u64,
        rows: Vec<WireRowDelta>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRowDelta {
    pub index: u16,
    pub row: WireRowSnapshot,
}

/// Why a receiver could not apply a delta and needs a full repaint instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotApplyError {
    /// The receiver's current generation is not the delta's base.
    BaseMismatch { expected: u64, actual: u64 },
    /// The grid was resized, so row indices do not refer to the same rows.
    DimensionsChanged,
    /// A row index is outside the advertised grid.
    RowOutOfRange { index: u16, rows: u16 },
}

impl TerminalSnapshot {
    /// Builds a full repaint from an owned capture.
    pub fn full(value: &RenderSnapshot) -> Self {
        Self {
            generation: value.generation,
            activity: value.activity,
            columns: value.columns,
            rows: value.rows,
            foreground: value.foreground.into(),
            background: value.background.into(),
            cursor: value.cursor.map(Into::into),
            scroll: value.scroll,
            content: SnapshotContent::Full {
                lines: value
                    .lines
                    .iter()
                    .map(|row| WireRowSnapshot::from(row.as_ref()))
                    .collect(),
            },
            detached: value.detached,
        }
    }

    /// Builds the smallest repaint that carries `value` to a receiver already
    /// holding `base`, falling back to a full repaint when a delta cannot
    /// describe the change.
    ///
    /// Row identity is compared by pointer: `capture` hands back the very same
    /// `Arc` for a row it did not rebuild, so this diff is a pointer scan over
    /// the row list rather than a cell comparison.
    pub fn diff(value: &RenderSnapshot, base: &RenderSnapshot) -> Self {
        if value.detached
            || base.detached
            || base.columns != value.columns
            || base.rows != value.rows
            || base.lines.len() != value.lines.len()
        {
            return Self::full(value);
        }

        let rows = value
            .lines
            .iter()
            .zip(base.lines.iter())
            .enumerate()
            .filter(|(_, (next, previous))| !Arc::ptr_eq(next, previous))
            .map(|(index, (next, _))| WireRowDelta {
                index: index as u16,
                row: WireRowSnapshot::from(next.as_ref()),
            })
            .collect::<Vec<_>>();

        Self {
            generation: value.generation,
            activity: value.activity,
            columns: value.columns,
            rows: value.rows,
            foreground: value.foreground.into(),
            background: value.background.into(),
            cursor: value.cursor.map(Into::into),
            scroll: value.scroll,
            content: SnapshotContent::Delta {
                base_generation: base.generation,
                rows,
            },
            detached: false,
        }
    }

    /// Resolves this repaint against the receiver's current snapshot.
    ///
    /// Applying a delta clones the row pointer list and replaces the changed
    /// entries, so an unchanged row costs one atomic increment instead of a
    /// full row rebuild.
    pub fn apply(
        self,
        current: Option<&RenderSnapshot>,
    ) -> Result<RenderSnapshot, SnapshotApplyError> {
        let Self {
            generation,
            activity,
            columns,
            rows,
            foreground,
            background,
            cursor,
            scroll,
            content,
            detached,
        } = self;
        let lines = match content {
            SnapshotContent::Full { lines } => lines
                .into_iter()
                .map(|row| Arc::new(RowSnapshot::from(row)))
                .collect::<Vec<_>>(),
            SnapshotContent::Delta {
                base_generation,
                rows: changed,
            } => {
                let base = current.ok_or(SnapshotApplyError::BaseMismatch {
                    expected: base_generation,
                    actual: 0,
                })?;
                if base.generation != base_generation {
                    return Err(SnapshotApplyError::BaseMismatch {
                        expected: base_generation,
                        actual: base.generation,
                    });
                }
                if base.columns != columns || base.rows != rows {
                    return Err(SnapshotApplyError::DimensionsChanged);
                }
                let mut lines = base.lines.as_ref().to_vec();
                for WireRowDelta { index, row } in changed {
                    let slot = lines
                        .get_mut(usize::from(index))
                        .ok_or(SnapshotApplyError::RowOutOfRange { index, rows })?;
                    *slot = Arc::new(RowSnapshot::from(row));
                }
                lines
            }
        };

        Ok(RenderSnapshot {
            generation,
            activity,
            columns,
            rows,
            foreground: foreground.into(),
            background: background.into(),
            cursor: cursor.map(Into::into),
            scroll,
            lines: lines.into(),
            detached,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl From<Rgb> for WireRgb {
    fn from(value: Rgb) -> Self {
        Self {
            red: value.red,
            green: value.green,
            blue: value.blue,
        }
    }
}

impl From<WireRgb> for Rgb {
    fn from(value: WireRgb) -> Self {
        Self {
            red: value.red,
            green: value.green,
            blue: value.blue,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCursorSnapshot {
    pub column: u16,
    pub row: u16,
    pub shape: WireCursorShape,
    pub color: WireRgb,
    pub visible: bool,
    pub blinking: bool,
}

impl From<CursorSnapshot> for WireCursorSnapshot {
    fn from(value: CursorSnapshot) -> Self {
        Self {
            column: value.column,
            row: value.row,
            shape: value.shape.into(),
            color: value.color.into(),
            visible: value.visible,
            blinking: value.blinking,
        }
    }
}

impl From<WireCursorSnapshot> for CursorSnapshot {
    fn from(value: WireCursorSnapshot) -> Self {
        Self {
            column: value.column,
            row: value.row,
            shape: value.shape.into(),
            color: value.color.into(),
            visible: value.visible,
            blinking: value.blinking,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireCursorShape {
    Bar,
    Block,
    Underline,
    HollowBlock,
}

impl From<CursorShape> for WireCursorShape {
    fn from(value: CursorShape) -> Self {
        match value {
            CursorShape::Bar => Self::Bar,
            CursorShape::Block => Self::Block,
            CursorShape::Underline => Self::Underline,
            CursorShape::HollowBlock => Self::HollowBlock,
        }
    }
}

impl From<WireCursorShape> for CursorShape {
    fn from(value: WireCursorShape) -> Self {
        match value {
            WireCursorShape::Bar => Self::Bar,
            WireCursorShape::Block => Self::Block,
            WireCursorShape::Underline => Self::Underline,
            WireCursorShape::HollowBlock => Self::HollowBlock,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRowSnapshot {
    pub cells: Vec<WireCellSnapshot>,
    pub wrapped: bool,
    pub hyperlinks: Vec<crate::terminal::snapshot::HyperlinkSpan>,
}

impl From<&RowSnapshot> for WireRowSnapshot {
    fn from(value: &RowSnapshot) -> Self {
        Self {
            wrapped: value.wrapped,
            hyperlinks: value.hyperlinks.clone(),
            cells: value.cells.iter().map(Into::into).collect(),
        }
    }
}

impl From<WireRowSnapshot> for RowSnapshot {
    fn from(value: WireRowSnapshot) -> Self {
        Self {
            wrapped: value.wrapped,
            hyperlinks: value.hyperlinks,
            cells: value.cells.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCellSnapshot {
    pub text: CellText,
    pub foreground: WireRgb,
    pub background: WireRgb,
    pub style: WireCellStyle,
    pub selected: bool,
}

impl From<&CellSnapshot> for WireCellSnapshot {
    fn from(value: &CellSnapshot) -> Self {
        Self {
            text: value.text.clone(),
            foreground: value.foreground.into(),
            background: value.background.into(),
            style: value.style.into(),
            selected: value.selected,
        }
    }
}

impl From<WireCellSnapshot> for CellSnapshot {
    fn from(value: WireCellSnapshot) -> Self {
        Self {
            text: value.text,
            foreground: value.foreground.into(),
            background: value.background.into(),
            style: value.style.into(),
            selected: value.selected,
        }
    }
}

bitflags::bitflags! {
    /// The boolean half of a cell's style, packed so that a cell costs two
    /// wire bytes instead of nine and so that comparing two cells' styles —
    /// which the renderer does for every column to build text runs — is a
    /// single integer compare.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct WireStyleFlags: u8 {
        const BOLD = 1 << 0;
        const ITALIC = 1 << 1;
        const FAINT = 1 << 2;
        const BLINK = 1 << 3;
        const INVISIBLE = 1 << 4;
        const STRIKETHROUGH = 1 << 5;
        const OVERLINE = 1 << 6;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireCellStyle {
    pub flags: WireStyleFlags,
    pub underline: WireUnderline,
}

impl From<CellStyle> for WireCellStyle {
    fn from(value: CellStyle) -> Self {
        let mut flags = WireStyleFlags::empty();
        flags.set(WireStyleFlags::BOLD, value.bold);
        flags.set(WireStyleFlags::ITALIC, value.italic);
        flags.set(WireStyleFlags::FAINT, value.faint);
        flags.set(WireStyleFlags::BLINK, value.blink);
        flags.set(WireStyleFlags::INVISIBLE, value.invisible);
        flags.set(WireStyleFlags::STRIKETHROUGH, value.strikethrough);
        flags.set(WireStyleFlags::OVERLINE, value.overline);
        Self {
            flags,
            underline: value.underline.into(),
        }
    }
}

impl From<WireCellStyle> for CellStyle {
    fn from(value: WireCellStyle) -> Self {
        Self {
            bold: value.flags.contains(WireStyleFlags::BOLD),
            italic: value.flags.contains(WireStyleFlags::ITALIC),
            faint: value.flags.contains(WireStyleFlags::FAINT),
            blink: value.flags.contains(WireStyleFlags::BLINK),
            invisible: value.flags.contains(WireStyleFlags::INVISIBLE),
            strikethrough: value.flags.contains(WireStyleFlags::STRIKETHROUGH),
            overline: value.flags.contains(WireStyleFlags::OVERLINE),
            underline: value.underline.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireUnderline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl From<Underline> for WireUnderline {
    fn from(value: Underline) -> Self {
        match value {
            Underline::None => Self::None,
            Underline::Single => Self::Single,
            Underline::Double => Self::Double,
            Underline::Curly => Self::Curly,
            Underline::Dotted => Self::Dotted,
            Underline::Dashed => Self::Dashed,
        }
    }
}

impl From<WireUnderline> for Underline {
    fn from(value: WireUnderline) -> Self {
        match value {
            WireUnderline::None => Self::None,
            WireUnderline::Single => Self::Single,
            WireUnderline::Double => Self::Double,
            WireUnderline::Curly => Self::Curly,
            WireUnderline::Dotted => Self::Dotted,
            WireUnderline::Dashed => Self::Dashed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireTerminalEvent {
    TitleChanged {
        title: String,
    },
    ResumeHintChanged {
        hint: Option<String>,
    },
    Bell,
    Exited {
        exit_code: u32,
        signal: Option<String>,
        #[serde(default)]
        intentional: bool,
    },
    Error {
        operation: String,
        message: String,
    },
    AgentActivityChanged {
        activity: Option<crate::agent::Activity>,
    },
    Completed {
        exit_code: u32,
        signal: Option<String>,
        intentional: bool,
    },
    ConversationTitleChanged {
        title: String,
    },
    WorkingDirectoryChanged {
        path: std::path::PathBuf,
    },
    ClipboardText {
        request: uuid::Uuid,
        text: String,
    },
    SearchResults {
        request: u64,
        query: String,
        matches: Vec<crate::terminal::SearchMatch>,
        truncated: bool,
    },
}

impl From<TerminalEvent> for WireTerminalEvent {
    fn from(value: TerminalEvent) -> Self {
        match value {
            TerminalEvent::TitleChanged(title) => Self::TitleChanged { title },
            TerminalEvent::ClipboardText { request, text } => Self::ClipboardText { request, text },
            TerminalEvent::SearchResults {
                request,
                query,
                matches,
                truncated,
                ..
            } => Self::SearchResults {
                request,
                query,
                matches,
                truncated,
            },
            TerminalEvent::WorkingDirectoryChanged(path) => Self::WorkingDirectoryChanged { path },
            TerminalEvent::ConversationTitleChanged(title) => {
                Self::ConversationTitleChanged { title }
            }
            TerminalEvent::Completed {
                exit_code,
                signal,
                intentional,
            } => Self::Completed {
                exit_code,
                signal,
                intentional,
            },
            TerminalEvent::ResumeHintChanged(hint) => Self::ResumeHintChanged { hint },
            TerminalEvent::AgentActivityChanged(activity) => {
                Self::AgentActivityChanged { activity }
            }
            TerminalEvent::Bell => Self::Bell,
            TerminalEvent::Exited {
                exit_code,
                signal,
                intentional,
            } => Self::Exited {
                exit_code,
                signal,
                intentional,
            },
            TerminalEvent::Error { operation, message } => Self::Error {
                operation: operation.to_owned(),
                message,
            },
        }
    }
}

impl From<WireTerminalEvent> for TerminalEvent {
    fn from(value: WireTerminalEvent) -> Self {
        match value {
            WireTerminalEvent::TitleChanged { title } => Self::TitleChanged(title),
            WireTerminalEvent::ClipboardText { request, text } => {
                Self::ClipboardText { request, text }
            }
            WireTerminalEvent::SearchResults {
                request,
                query,
                matches,
                truncated,
            } => Self::SearchResults {
                owner: None,
                request,
                query,
                matches,
                truncated,
            },
            WireTerminalEvent::WorkingDirectoryChanged { path } => {
                Self::WorkingDirectoryChanged(path)
            }
            WireTerminalEvent::ConversationTitleChanged { title } => {
                Self::ConversationTitleChanged(title)
            }
            WireTerminalEvent::Completed {
                exit_code,
                signal,
                intentional,
            } => Self::Completed {
                exit_code,
                signal,
                intentional,
            },
            WireTerminalEvent::ResumeHintChanged { hint } => Self::ResumeHintChanged(hint),
            WireTerminalEvent::AgentActivityChanged { activity } => {
                Self::AgentActivityChanged(activity)
            }
            WireTerminalEvent::Bell => Self::Bell,
            WireTerminalEvent::Exited {
                exit_code,
                signal,
                intentional,
            } => Self::Exited {
                exit_code,
                signal,
                intentional,
            },
            WireTerminalEvent::Error { operation, message } => Self::Error {
                operation: match operation.as_str() {
                    "read PTY" => "read PTY",
                    "write terminal input" => "write terminal input",
                    "encode key" => "encode key",
                    "encode mouse" => "encode mouse",
                    "resize terminal" => "resize terminal",
                    "wait for child" => "wait for child",
                    "terminate child" => "terminate child",
                    "reap child" => "reap child",
                    "write transcript" => "write transcript",
                    _ => "remote terminal",
                },
                message,
            },
        }
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("I/O error while transferring IPC frame: {0}")]
    Io(#[from] io::Error),
    #[error("IPC frame length {actual} is outside 1..={maximum}")]
    InvalidLength { actual: usize, maximum: usize },
    #[error("could not allocate {requested} bytes for an IPC frame")]
    Allocation { requested: usize },
    #[error("invalid IPC payload: {0}")]
    InvalidPayload(#[from] postcard::Error),
}

/// Frame scratch space owned by one long-lived connection.
///
/// The two hot loops — the daemon's per-client writer and the UI's per-terminal
/// reader — move one frame at a time, so a single buffer per connection removes
/// the per-frame allocate-and-zero-fill that the stateless helpers below still
/// perform for cold paths and tests. The buffer also lets a write emit its
/// length prefix and payload in one `write_all`.
pub struct FrameCodec {
    read_buffer: Vec<u8>,
    write_buffer: Vec<u8>,
    prefix: [u8; LENGTH_PREFIX_BYTES],
    prefix_read: usize,
    payload_length: Option<usize>,
    payload_read: usize,
    maximum: usize,
}

impl FrameCodec {
    pub fn new(maximum: usize) -> Self {
        Self {
            read_buffer: Vec::new(),
            write_buffer: Vec::new(),
            prefix: [0; LENGTH_PREFIX_BYTES],
            prefix_read: 0,
            payload_length: None,
            payload_read: 0,
            maximum,
        }
    }

    pub fn read<T: DeserializeOwned>(
        &mut self,
        reader: &mut impl Read,
    ) -> Result<Option<T>, FrameError> {
        let length = if let Some(length) = self.payload_length {
            length
        } else {
            while self.prefix_read < LENGTH_PREFIX_BYTES {
                match reader.read(&mut self.prefix[self.prefix_read..]) {
                    Ok(0) if self.prefix_read == 0 => return Ok(None),
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated IPC frame length",
                        )
                        .into());
                    }
                    Ok(read) => self.prefix_read += read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let length = u32::from_be_bytes(self.prefix) as usize;
            validate_frame_length(length, self.maximum)?;
            if self.read_buffer.len() < length {
                self.read_buffer
                    .try_reserve(length - self.read_buffer.len())
                    .map_err(|_| FrameError::Allocation { requested: length })?;
            }
            self.read_buffer.resize(length, 0);
            self.payload_length = Some(length);
            length
        };
        while self.payload_read < length {
            match reader.read(&mut self.read_buffer[self.payload_read..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated IPC frame payload",
                    )
                    .into());
                }
                Ok(read) => self.payload_read += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        let started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        let value = postcard::from_bytes(&self.read_buffer[..length]);
        self.prefix_read = 0;
        self.payload_length = None;
        self.payload_read = 0;
        if let Some(started) = started {
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "ipc_decode", elapsed_ns = started.elapsed().as_nanos() as u64, bytes = length);
        }
        Ok(Some(value?))
    }

    pub fn write<T: Serialize>(
        &mut self,
        writer: &mut impl Write,
        value: &T,
    ) -> Result<(), FrameError> {
        let started =
            tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE)
                .then(std::time::Instant::now);
        let payload_length = encode_frame(&mut self.write_buffer, self.maximum, value)?;
        self.write_buffer[..LENGTH_PREFIX_BYTES]
            .copy_from_slice(&(payload_length as u32).to_be_bytes());
        if let Some(started) = started {
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "ipc_encode", elapsed_ns = started.elapsed().as_nanos() as u64, bytes = payload_length);
        }
        writer.write_all(&self.write_buffer)?;
        writer.flush()?;
        Ok(())
    }
}

const LENGTH_PREFIX_BYTES: usize = 4;

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<Option<T>, FrameError> {
    read_frame_with_limit(reader, MAX_FRAME_BYTES)
}

pub(crate) fn read_frame_with_limit<T: DeserializeOwned>(
    reader: &mut impl Read,
    maximum: usize,
) -> Result<Option<T>, FrameError> {
    let Some(length) = read_frame_length(reader)? else {
        return Ok(None);
    };
    validate_frame_length(length, maximum)?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(length)
        .map_err(|_| FrameError::Allocation { requested: length })?;
    payload.resize(length, 0);
    reader.read_exact(&mut payload)?;
    Ok(Some(postcard::from_bytes(&payload)?))
}

/// Reads the big-endian length prefix, returning `None` only for a clean
/// end-of-stream before any prefix byte arrived.
fn read_frame_length(reader: &mut impl Read) -> Result<Option<usize>, FrameError> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    let mut offset = 0;
    while offset < prefix.len() {
        match reader.read(&mut prefix[offset..]) {
            Ok(0) if offset == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated IPC frame length",
                )
                .into());
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some(u32::from_be_bytes(prefix) as usize))
}

/// Serializes `value` into `buffer` after a reserved length prefix, enforcing
/// `maximum` on the payload, and returns the payload length.
///
/// The prefix is reserved up front so the caller can patch it in place and emit
/// the whole frame with one `write_all`. The cap is enforced by a capped
/// `io::Write` sink rather than after the fact, so a hostile or buggy payload
/// can never allocate past the limit.
fn encode_frame<T: Serialize>(
    buffer: &mut Vec<u8>,
    maximum: usize,
    value: &T,
) -> Result<usize, FrameError> {
    buffer.clear();
    buffer.extend_from_slice(&[0; LENGTH_PREFIX_BYTES]);
    let mut sink = CappedSink {
        buffer,
        maximum,
        failure: None,
    };
    if let Err(error) = postcard::to_io(value, &mut sink) {
        return Err(match sink.failure {
            Some(CappedSinkFailure::TooLarge { attempted }) => FrameError::InvalidLength {
                actual: attempted,
                maximum,
            },
            Some(CappedSinkFailure::Allocation { requested }) => {
                FrameError::Allocation { requested }
            }
            None => FrameError::InvalidPayload(error),
        });
    }
    let payload_length = buffer.len() - LENGTH_PREFIX_BYTES;
    validate_frame_length(payload_length, maximum)?;
    Ok(payload_length)
}

#[derive(Clone, Copy)]
enum CappedSinkFailure {
    TooLarge { attempted: usize },
    Allocation { requested: usize },
}

/// An `io::Write` sink that appends to a caller-owned buffer and refuses to
/// grow past `maximum` payload bytes.
struct CappedSink<'buffer> {
    buffer: &'buffer mut Vec<u8>,
    maximum: usize,
    failure: Option<CappedSinkFailure>,
}

impl Write for CappedSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let payload_length = self.buffer.len() - LENGTH_PREFIX_BYTES;
        let Some(required) = payload_length.checked_add(bytes.len()) else {
            self.failure = Some(CappedSinkFailure::TooLarge {
                attempted: usize::MAX,
            });
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame length overflow",
            ));
        };
        if required > self.maximum {
            self.failure = Some(CappedSinkFailure::TooLarge {
                attempted: required,
            });
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame exceeds its maximum length",
            ));
        }
        if self.buffer.try_reserve(bytes.len()).is_err() {
            self.failure = Some(CappedSinkFailure::Allocation {
                requested: required + LENGTH_PREFIX_BYTES,
            });
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "could not allocate IPC frame",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), FrameError> {
    let mut buffer = Vec::new();
    let payload_length = encode_frame(&mut buffer, MAX_FRAME_BYTES, value)?;
    buffer[..LENGTH_PREFIX_BYTES].copy_from_slice(&(payload_length as u32).to_be_bytes());
    writer.write_all(&buffer)?;
    writer.flush()?;
    Ok(())
}

fn validate_frame_length(length: usize, maximum: usize) -> Result<(), FrameError> {
    if length == 0 || length > maximum {
        return Err(FrameError::InvalidLength {
            actual: length,
            maximum,
        });
    }
    Ok(())
}

pub fn validate_proof(
    supplied: Option<&HostSessionProof>,
    current: &HostSessionProof,
) -> Result<(), ProtocolError> {
    match supplied {
        Some(supplied) if supplied == current => Ok(()),
        Some(_) => Err(ProtocolError::new(
            ProtocolErrorCode::ProofMismatch,
            "the request targets a different cagents daemon incarnation",
            false,
        )),
        None => Err(ProtocolError::new(
            ProtocolErrorCode::ProofMismatch,
            "a cagents daemon proof is required after the handshake",
            false,
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use crate::model::{BootId, HostIncarnation, HostSessionId};

    use super::*;

    #[test]
    fn frame_codec_resumes_after_timeouts_inside_prefix_and_payload() {
        struct TimeoutReader {
            bytes: Vec<u8>,
            offset: usize,
            timeouts: Vec<usize>,
        }

        impl Read for TimeoutReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.timeouts.first() == Some(&self.offset) {
                    self.timeouts.remove(0);
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                if self.offset == self.bytes.len() {
                    return Ok(0);
                }
                let next_timeout = self.timeouts.first().copied().unwrap_or(self.bytes.len());
                let length = output
                    .len()
                    .min(next_timeout - self.offset)
                    .min(self.bytes.len() - self.offset);
                output[..length].copy_from_slice(&self.bytes[self.offset..self.offset + length]);
                self.offset += length;
                Ok(length)
            }
        }

        let mut bytes = Vec::new();
        write_frame(&mut bytes, &"a split payload".to_owned()).unwrap();
        write_frame(&mut bytes, &"next frame".to_owned()).unwrap();
        let mut reader = TimeoutReader {
            bytes,
            offset: 0,
            timeouts: vec![2, LENGTH_PREFIX_BYTES + 2],
        };
        let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
        assert!(matches!(
            codec.read::<String>(&mut reader),
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
        ));
        // A writer on the same connection must not overwrite a partial read.
        codec.write(&mut Vec::new(), &"outbound").unwrap();
        assert!(matches!(
            codec.read::<String>(&mut reader),
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert_eq!(
            codec.read::<String>(&mut reader).unwrap(),
            Some("a split payload".into())
        );
        assert_eq!(
            codec.read::<String>(&mut reader).unwrap(),
            Some("next frame".into())
        );
        assert_eq!(codec.read::<String>(&mut reader).unwrap(), None);
    }

    #[test]
    fn terminal_recovery_origin_round_trips_in_framed_inventory() {
        for origin in [None, Some(TerminalId::new())] {
            let mut terminal = TerminalDescriptor::relaunchable(
                TerminalId::new(),
                LaunchRecipe::shell(PathBuf::from("/tmp")).unwrap(),
                None,
            );
            terminal.session.recovery_origin = origin;
            let message = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(1),
                proof: proof("session", "boot", "incarnation"),
                message: ServerMessage::Response {
                    response: ServerResponse::Listed {
                        terminals: vec![terminal],
                    },
                },
            };
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &message).unwrap();
            let decoded: ServerEnvelope = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn intentional_exit_provenance_survives_a_framed_daemon_event() {
        for intentional in [false, true] {
            let event = TerminalEvent::Exited {
                exit_code: 1,
                signal: Some("Killed".into()),
                intentional,
            };
            let message = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof("session", "boot", "incarnation"),
                message: ServerMessage::Event {
                    terminal_id: TerminalId::new(),
                    sequence: 1,
                    event: event.clone().into(),
                },
            };
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &message).unwrap();
            let decoded: ServerEnvelope = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
            let ServerMessage::Event { event: decoded, .. } = decoded.message else {
                panic!("exit event changed kind");
            };
            assert_eq!(TerminalEvent::from(decoded), event);
        }
    }

    /// The daemon's writer serializes `ServerEnvelopeRef`, but every peer
    /// decodes `ServerEnvelope`. A field reorder or type change on either side
    /// would silently mis-decode, so pin the encoding rather than the shape.
    #[test]
    fn borrowed_envelope_encodes_identically() {
        let envelope = ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(7),
            proof: proof("session", "boot", "incarnation"),
            message: ServerMessage::Redraw {
                terminal_id: TerminalId::new(),
                generation: 42,
            },
        };
        let mut owned = Vec::new();
        write_frame(&mut owned, &envelope).unwrap();
        let mut borrowed = Vec::new();
        write_frame(
            &mut borrowed,
            &ServerEnvelopeRef {
                version: envelope.version,
                request_id: envelope.request_id,
                proof: &envelope.proof,
                message: &envelope.message,
            },
        )
        .unwrap();
        assert_eq!(owned, borrowed);
    }

    #[test]
    fn scroll_only_deltas_survive_the_wire_without_replacing_grid_rows() {
        let base = grid(1, &["ab"]);
        let next = RenderSnapshot {
            generation: 2,
            scroll: ScrollSnapshot {
                total: 100,
                offset: 40,
                len: 1,
                alternate_screen: false,
                mouse_tracking: true,
                bracketed_paste: false,
            },
            ..base.clone()
        };
        let delta = TerminalSnapshot::diff(&next, &base);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &delta).unwrap();
        let wire: TerminalSnapshot = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        let applied = wire.apply(Some(&base)).unwrap();
        assert_eq!(applied.scroll, next.scroll);
        assert!(Arc::ptr_eq(&applied.lines[0], &base.lines[0]));
        assert_eq!(
            TerminalSnapshot::full(&next).apply(None).unwrap().scroll,
            next.scroll
        );
    }

    fn proof(session: &str, boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }
    #[test]
    fn activity_survives_full_and_unchanged_row_delta_snapshots() {
        let mut base = grid(1, &["ab"]);
        base.activity = Some(TerminalOutputActivity {
            sequence: 10,
            last_output_at_unix_ms: 100,
        });
        let full = TerminalSnapshot::full(&base);
        let restored = full.apply(None).unwrap();
        assert_eq!(restored.activity, base.activity);
        let next = RenderSnapshot {
            generation: 2,
            activity: Some(TerminalOutputActivity {
                sequence: 11,
                last_output_at_unix_ms: 101,
            }),
            ..base.clone()
        };
        let delta = TerminalSnapshot::diff(&next, &restored);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &delta).unwrap();
        let wire: TerminalSnapshot = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        let applied = wire.apply(Some(&restored)).unwrap();
        assert_eq!(applied.activity, next.activity);
        assert_eq!(applied.lines, restored.lines);
    }

    #[test]
    fn chat_fast_mode_and_estimate_round_trip_with_legacy_json_defaults() {
        let snapshot = crate::chat::Snapshot {
            fast_mode: Some(crate::chat::FastMode {
                state: crate::chat::FastModeState::Cooldown,
                disabled_reason: None,
            }),
            capabilities: crate::chat::Capabilities {
                fast_mode: true,
                ..Default::default()
            },
            usage: crate::chat::Usage {
                cost_estimate: Some(crate::chat::pricing::CostEstimate {
                    microusd: 12345,
                    partial: true,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let response = ServerResponse::ChatSnapshot {
            chat_id: TerminalId::new(),
            snapshot: Some(snapshot.clone()),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &response).unwrap();
        let decoded: ServerResponse = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, response);
        let request = ClientRequest::ChatCommand {
            chat_id: TerminalId::new(),
            command: crate::chat::Command::SetFastMode { enabled: true },
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let decoded: ClientRequest = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, request);
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("fast_mode");
        legacy["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove("fast_mode");
        legacy["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cost_estimate");
        let restored: crate::chat::Snapshot = serde_json::from_value(legacy).unwrap();
        assert!(restored.fast_mode.is_none());
        assert!(!restored.capabilities.fast_mode);
        assert!(restored.usage.cost_estimate.is_none());
    }

    #[test]
    fn chat_completion_time_round_trips_and_old_json_remains_unknown() {
        let snapshot = crate::chat::Snapshot {
            finished_at: Some(1_700_000_000_123),
            turn_generation: 7,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        let response = ServerResponse::ChatSnapshot {
            chat_id: TerminalId::new(),
            snapshot: Some(snapshot.clone()),
        };
        write_frame(&mut bytes, &response).unwrap();
        let decoded: ServerResponse = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, response);
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("finished_at");
        let restored: crate::chat::Snapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.finished_at, None);
        assert_eq!(restored.turn_generation, 7);
    }

    #[test]
    fn chat_prompt_suggestion_round_trips_and_old_json_remains_absent() {
        let snapshot = crate::chat::Snapshot {
            session_id: Some("session-exact".into()),
            turn_generation: 7,
            prompt_suggestion: Some(crate::chat::PromptSuggestion {
                session_id: "session-exact".into(),
                source: "turn-exact".into(),
                turn_generation: 7,
                text: "Check the resulting diff\nThen run tests".into(),
            }),
            ..Default::default()
        };
        let response = ServerResponse::ChatSnapshot {
            chat_id: TerminalId::new(),
            snapshot: Some(snapshot.clone()),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &response).unwrap();
        let decoded: ServerResponse = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, response);
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy.as_object_mut().unwrap().remove("prompt_suggestion");
        let restored: crate::chat::Snapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.prompt_suggestion, None);
        assert_eq!(restored.turn_generation, 7);
    }

    #[test]
    fn chat_file_changes_query_replies_and_old_snapshot_defaults_round_trip() {
        use crate::chat::*;
        let chat_id = TerminalId::new();
        let key = FileChangeKey {
            source: FileChangeSource::Subagent {
                id: "delegate".into(),
            },
            path: "a.rs".into(),
        };
        let cursor = FilePatchCursor {
            ordinal: 4,
            file_index: 1,
            offset: 9,
        };
        let commands = [
            Command::LoadFileChanges {
                request_id: 7,
                after: Some(key.clone()),
            },
            Command::LoadFileChangePatches {
                request_id: 8,
                key: key.clone(),
                cursor: Some(cursor.clone()),
                revision: 5,
            },
        ];
        for command in commands {
            let request = ClientRequest::ChatCommand { chat_id, command };
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &request).unwrap();
            assert_eq!(
                read_frame::<ClientRequest>(&mut Cursor::new(bytes))
                    .unwrap()
                    .unwrap(),
                request
            );
        }
        let responses = [
            ServerResponse::ChatFileChanges {
                chat_id,
                summary: FileChangesSummary {
                    request_id: 7,
                    revision: 5,
                    total_files: 1,
                    applied_files: 1,
                    ..Default::default()
                },
            },
            ServerResponse::ChatFileChangePatches {
                chat_id,
                page: FileChangePage {
                    request_id: 8,
                    key,
                    revision: 5,
                    patches: vec![FileChangePatch {
                        ownership_unverified: true,
                        item_id: "edit".into(),
                        kind: ChangeKind::Updated,
                        status: ToolStatus::Succeeded,
                        imported: false,
                        offset: 9,
                        total_bytes: 99,
                        diff: "+λ\n".into(),
                    }],
                    next: Some(cursor),
                    error: None,
                },
            },
        ];
        for response in responses {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &response).unwrap();
            let decoded: ServerResponse = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
            assert_eq!(decoded, response);
        }
        let mut legacy = serde_json::to_value(Snapshot::default()).unwrap();
        legacy.as_object_mut().unwrap().remove("file_changes");
        let restored: Snapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.file_changes.total_files, 0);
    }
    #[test]
    fn framing_round_trips_and_rejects_oversized_prefixes() {
        let message = ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 19,
            proof: None,
            request: ClientRequest::List,
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &message).unwrap();
        let decoded: ClientEnvelope = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(decoded, message);

        let oversized = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes();
        let error = read_frame::<ClientEnvelope>(&mut Cursor::new(oversized)).unwrap_err();
        assert!(matches!(error, FrameError::InvalidLength { .. }));
    }

    #[test]
    fn client_frame_limit_is_checked_before_payload_allocation() {
        let declared = 65u32;
        let error =
            read_frame_with_limit::<ClientEnvelope>(&mut Cursor::new(declared.to_be_bytes()), 64)
                .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength {
                actual: 65,
                maximum: 64
            }
        ));
    }

    #[test]
    fn frame_encoding_refuses_to_grow_past_its_ceiling() {
        let mut buffer = Vec::new();
        // `MAX_INPUT_BYTES` of payload cannot fit in a 64-byte frame.
        let error = encode_frame(
            &mut buffer,
            64,
            &ClientRequest::Input {
                terminal_id: TerminalId::new(),
                bytes: vec![0u8; 1024],
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength { maximum: 64, .. }
        ));
    }

    #[test]
    fn a_repaint_round_trips_through_the_binary_codec() {
        let snapshot = grid(7, &["ab", "cd"]);
        let wire = TerminalSnapshot::full(&snapshot);
        let mut bytes = Vec::new();
        write_frame(
            &mut bytes,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof("session", "boot", "incarnation"),
                message: ServerMessage::Snapshot {
                    terminal_id: TerminalId::new(),
                    snapshot: wire.clone(),
                },
            },
        )
        .unwrap();
        let decoded: ServerEnvelope = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        let ServerMessage::Snapshot {
            snapshot: decoded, ..
        } = decoded.message
        else {
            panic!("expected a snapshot");
        };
        assert_eq!(decoded, wire);
        assert_eq!(decoded.apply(None).unwrap(), snapshot);
    }

    #[test]
    fn a_delta_carries_only_the_rows_whose_arc_changed() {
        let base = grid(7, &["ab", "cd"]);
        // Rebuild row 1 only, exactly as `VisualResources::capture` does for a
        // row Ghostty marked dirty, and share row 0's `Arc`.
        let mut lines = base.lines.as_ref().to_vec();
        lines[1] = Arc::new(row("ef"));
        let next = RenderSnapshot {
            generation: 8,
            lines: lines.into(),
            ..base.clone()
        };

        let delta = TerminalSnapshot::diff(&next, &base);
        let SnapshotContent::Delta {
            base_generation,
            rows,
        } = &delta.content
        else {
            panic!("expected a delta");
        };
        assert_eq!(*base_generation, 7);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].index, 1);

        let applied = delta.apply(Some(&base)).unwrap();
        assert_eq!(applied, next);
        // The unchanged row must be shared, not rebuilt: that sharing is the
        // entire point of sending a delta.
        assert!(Arc::ptr_eq(&applied.lines[0], &base.lines[0]));
    }

    #[test]
    fn an_unapplicable_delta_is_rejected_rather_than_misapplied() {
        let base = grid(7, &["ab", "cd"]);
        let mut lines = base.lines.as_ref().to_vec();
        lines[1] = Arc::new(row("ef"));
        let next = RenderSnapshot {
            generation: 8,
            lines: lines.into(),
            ..base.clone()
        };
        let delta = TerminalSnapshot::diff(&next, &base);

        assert!(matches!(
            delta.clone().apply(None),
            Err(SnapshotApplyError::BaseMismatch { expected: 7, .. })
        ));

        let stale = RenderSnapshot {
            generation: 6,
            ..base.clone()
        };
        assert!(matches!(
            delta.clone().apply(Some(&stale)),
            Err(SnapshotApplyError::BaseMismatch {
                expected: 7,
                actual: 6
            })
        ));

        let resized = RenderSnapshot {
            columns: 3,
            ..base.clone()
        };
        assert!(matches!(
            delta.apply(Some(&resized)),
            Err(SnapshotApplyError::DimensionsChanged)
        ));
    }

    #[test]
    fn a_resize_or_detach_forces_a_full_repaint() {
        let base = grid(7, &["ab", "cd"]);
        let resized = grid(8, &["abc", "def"]);
        assert!(matches!(
            TerminalSnapshot::diff(&resized, &base).content,
            SnapshotContent::Full { .. }
        ));

        let detached = RenderSnapshot {
            generation: 8,
            detached: true,
            ..base.clone()
        };
        assert!(matches!(
            TerminalSnapshot::diff(&detached, &base).content,
            SnapshotContent::Full { .. }
        ));
    }

    fn row(text: &str) -> RowSnapshot {
        RowSnapshot {
            wrapped: false,
            hyperlinks: Vec::new(),
            cells: text
                .chars()
                .map(|character| CellSnapshot {
                    text: CellText::new(&character.to_string()),
                    ..CellSnapshot::default()
                })
                .collect(),
        }
    }

    fn grid(generation: u64, rows: &[&str]) -> RenderSnapshot {
        RenderSnapshot {
            generation,
            activity: None,
            columns: rows.first().map_or(0, |first| first.chars().count() as u16),
            rows: rows.len() as u16,
            foreground: Rgb::default(),
            background: Rgb::default(),
            cursor: None,
            scroll: ScrollSnapshot::default(),
            lines: rows
                .iter()
                .map(|text| Arc::new(row(text)))
                .collect::<Vec<_>>()
                .into(),
            detached: false,
        }
    }

    #[test]
    fn every_host_proof_component_fences_stale_clients() {
        let current = proof("session", "boot", "incarnation");
        assert!(validate_proof(Some(&current), &current).is_ok());
        for supplied in [
            Some(proof("old", "boot", "incarnation")),
            Some(proof("session", "old", "incarnation")),
            Some(proof("session", "boot", "old")),
            None,
        ] {
            let error = validate_proof(supplied.as_ref(), &current).unwrap_err();
            assert_eq!(error.code, ProtocolErrorCode::ProofMismatch);
            assert!(!error.retryable);
        }
    }
}
