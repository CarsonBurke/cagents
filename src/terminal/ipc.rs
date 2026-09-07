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
        HostSessionProof, LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession,
        TranscriptMetadata,
    },
    terminal::{
        CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, KeyInput, MouseInput,
        RenderSnapshot, Rgb, RowSnapshot, TerminalConfig, TerminalEvent, TerminalSize, Underline,
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
pub const PROTOCOL_VERSION: u16 = 3;
/// Postcard encodes an 80x24 grid in roughly 20 KiB and a typical single-row
/// delta in a few hundred bytes, but a full repaint of a very large grid after
/// a resize still has to fit, so keep generous headroom.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Client requests carry at most a one-MiB byte vector plus small scalars.
pub const MAX_CLIENT_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_RESUME_TERMINALS: usize = 1024;
/// Production full and browserless builds retain this namespace while protocol
/// version 3 remains current so upgrades can resume existing terminals.
#[cfg(test)]
const RUNTIME_NAMESPACE_PROTOCOL_VERSION: u16 = 3;
pub const SOCKET_FILE_NAME: &str = "cagentsd.sock";

/// Returns the current user's cagents daemon socket path.
pub fn daemon_socket_path() -> io::Result<PathBuf> {
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerEnvelope {
    pub version: u16,
    pub request_id: Option<u64>,
    pub proof: HostSessionProof,
    pub message: ServerMessage,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalDescriptor {
    pub session: TerminalSession,
    pub generation: u64,
    pub event_sequence: u64,
    /// The first semantic-event sequence still available for reconnect replay.
    /// A cursor below this value must treat the descriptor/snapshot as the
    /// source of truth rather than assuming that every transient event exists.
    pub oldest_event_sequence: u64,
    pub attached: bool,
    pub transcript: Option<TranscriptMetadata>,
}

impl TerminalDescriptor {
    pub fn relaunchable(
        terminal_id: TerminalId,
        recipe: LaunchRecipe,
        transcript_path: Option<PathBuf>,
    ) -> Self {
        Self {
            session: TerminalSession {
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
            event_sequence: 0,
            oldest_event_sequence: 1,
            attached: false,
            transcript: None,
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
    pub columns: u16,
    pub rows: u16,
    pub foreground: WireRgb,
    pub background: WireRgb,
    pub cursor: Option<WireCursorSnapshot>,
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
            columns: value.columns,
            rows: value.rows,
            foreground: value.foreground.into(),
            background: value.background.into(),
            cursor: value.cursor.map(Into::into),
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
            columns: value.columns,
            rows: value.rows,
            foreground: value.foreground.into(),
            background: value.background.into(),
            cursor: value.cursor.map(Into::into),
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
            columns,
            rows,
            foreground,
            background,
            cursor,
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
            columns,
            rows,
            foreground: foreground.into(),
            background: background.into(),
            cursor: cursor.map(Into::into),
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
}

impl From<&RowSnapshot> for WireRowSnapshot {
    fn from(value: &RowSnapshot) -> Self {
        Self {
            cells: value.cells.iter().map(Into::into).collect(),
        }
    }
}

impl From<WireRowSnapshot> for RowSnapshot {
    fn from(value: WireRowSnapshot) -> Self {
        Self {
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
    Bell,
    Exited {
        exit_code: u32,
        signal: Option<String>,
    },
    Error {
        operation: String,
        message: String,
    },
}

impl From<TerminalEvent> for WireTerminalEvent {
    fn from(value: TerminalEvent) -> Self {
        match value {
            TerminalEvent::TitleChanged(title) => Self::TitleChanged { title },
            TerminalEvent::Bell => Self::Bell,
            TerminalEvent::Exited { exit_code, signal } => Self::Exited { exit_code, signal },
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
            WireTerminalEvent::Bell => Self::Bell,
            WireTerminalEvent::Exited { exit_code, signal } => Self::Exited { exit_code, signal },
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
    buffer: Vec<u8>,
    maximum: usize,
}

impl FrameCodec {
    pub fn new(maximum: usize) -> Self {
        Self {
            buffer: Vec::new(),
            maximum,
        }
    }

    pub fn read<T: DeserializeOwned>(
        &mut self,
        reader: &mut impl Read,
    ) -> Result<Option<T>, FrameError> {
        let Some(length) = read_frame_length(reader)? else {
            return Ok(None);
        };
        validate_frame_length(length, self.maximum)?;
        if self.buffer.len() < length {
            let additional = length - self.buffer.len();
            self.buffer
                .try_reserve(additional)
                .map_err(|_| FrameError::Allocation { requested: length })?;
        }
        self.buffer.resize(length, 0);
        reader.read_exact(&mut self.buffer[..length])?;
        Ok(Some(postcard::from_bytes(&self.buffer[..length])?))
    }

    pub fn write<T: Serialize>(
        &mut self,
        writer: &mut impl Write,
        value: &T,
    ) -> Result<(), FrameError> {
        let payload_length = encode_frame(&mut self.buffer, self.maximum, value)?;
        self.buffer[..LENGTH_PREFIX_BYTES].copy_from_slice(&(payload_length as u32).to_be_bytes());
        writer.write_all(&self.buffer)?;
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
    use std::io::Cursor;

    use crate::model::{BootId, HostIncarnation, HostSessionId};

    use super::*;

    fn proof(session: &str, boot: &str, incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    #[test]
    fn runtime_namespace_version_matches_the_wire_protocol() {
        assert_eq!(
            RUNTIME_NAMESPACE_PROTOCOL_VERSION, PROTOCOL_VERSION,
            "a protocol change must deliberately retain or migrate the daemon namespace"
        );
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
            columns: rows.first().map_or(0, |first| first.chars().count() as u16),
            rows: rows.len() as u16,
            foreground: Rgb::default(),
            background: Rgb::default(),
            cursor: None,
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
