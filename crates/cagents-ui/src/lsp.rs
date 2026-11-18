//! One shared, configurable Language Server Protocol client runtime.
//!
//! [`LspManager`] is the single JSON-RPC client for the whole application. It
//! owns one session per `(server id, discovered root)` and shares it across
//! every editor tab that resolves to that pair, so opening ten Rust files in
//! one Cargo workspace starts exactly one `rust-analyzer`.
//!
//! # Threading
//!
//! Every public method is synchronous and returns immediately: the COSMIC
//! update thread calls them directly. All protocol traffic runs on Tokio tasks
//! spawned from the ambient runtime, and server-pushed data reaches the
//! application through the [`LspManager::events`] channel.
//!
//! # Coordinates
//!
//! The editor addresses text in Unicode-scalar offsets ([`LspPosition`]); the
//! protocol addresses text in UTF-16 code units. Conversion happens only here,
//! against a bounded ring of versioned snapshots kept per document URI. Data
//! that cannot be converted exactly is rejected with a typed [`LspError`]
//! rather than clamped, because a clamped position silently corrupts the
//! buffer the user is looking at.
//!
//! # Optional servers
//!
//! Language servers are external executables that this application neither
//! bundles nor installs. A bare program name is resolved against `PATH` here,
//! in this process, before the child exists: the child's working directory is
//! inside a repository nobody has vouched for, and on Unix the `chdir`
//! precedes the `execvp` `PATH` search, so a `PATH` carrying an empty or
//! relative component would otherwise let repository content supply the
//! binary. An absent binary is reported by that resolution and degrades to
//! [`LspStatus::Unavailable`] while editing, saving and syntax highlighting
//! keep working.

use crate::repository_widget::text_editor::{Position as NativePosition, TextSnapshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::future::Future;
use std::ops::{ControlFlow, Range};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;
use std::{fmt, io};

use async_lsp::router::Router;
use async_lsp::{MainLoop, ServerSocket};
use lsp_types::{notification, request};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;

use crate::repository::{SIGKILL, SIGTERM, TERMINATION_GRACE, signal_process_group};
use crate::settings::LanguageServerSettings;

mod commands;
mod discovery;

/// Capacity of the application event channel. Diagnostics are coalesced by
/// document once it is full; results and status changes apply backpressure to
/// the session task instead of being dropped.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// How many stderr lines of a language server are retained for diagnosis.
pub const STDERR_TAIL_LINES: usize = 200;

/// Longest stderr line retained. A server that writes a megabyte without a
/// newline must not be able to grow the tail without bound.
const STDERR_TAIL_LINE_BYTES: usize = 4096;

/// Largest message a language server may declare on stdout. The framing layer
/// allocates the whole declared length before it reads one body byte, and an
/// allocation this process cannot satisfy aborts it instead of unwinding, so
/// nothing further up could turn that into a saved buffer. Generous next to
/// the largest real payloads — a workspace-wide symbol answer or a semantic
/// token map — and far below anything that threatens the address space.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Longest header line accepted on stdout. The framing layer accumulates one
/// until a newline arrives, so this is the same bound as
/// [`STDERR_TAIL_LINE_BYTES`] applied to the side that actually allocates.
const MAX_HEADER_LINE_BYTES: usize = 4096;

/// How many versioned snapshots are retained per document URI. Inbound data is
/// converted against the snapshot it was computed from, so a small ring
/// absorbs in-flight requests and diagnostics without retaining a full edit
/// history of every open file.
const MAX_RETAINED_SNAPSHOTS: usize = 4;

/// How long `textDocument/didChange` traffic is coalesced after the last edit.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(120);

/// How long `initialize` may take before the session is declared failed.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the `shutdown` response is awaited before `exit` is sent anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the server is given to acknowledge `exit` by closing the
/// connection before its process group is terminated.
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the stderr reader is given to finish the bytes already in the
/// pipe before a failure status is published without them.
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// How long every retiring session together is given to finish its own
/// teardown before whatever is left has its process group killed outright.
///
/// Comfortably above what one session needs — the `shutdown` response, the
/// `exit` flush and two termination grace periods — because this is not that
/// bound but the one that keeps a quit finite: a session task only makes
/// progress while something polls it, and it publishes into a channel an
/// application on its way out has already stopped draining.
const SESSION_REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on non-diagnostic events staged by synchronous callers when the
/// event channel is full.
const MAX_PENDING_EVENTS: usize = EVENT_CHANNEL_CAPACITY;

/// Stable identifier of one editor document at the LSP boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LspDocumentId(pub u64);

/// Stable identifier of one client-initiated request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LspRequestId(pub u64);

/// A position in editor-native coordinates: `line` is zero-based, `character`
/// counts Unicode scalar values from the start of the line, and a line's
/// terminator is not part of the line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

impl LspPosition {
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A half-open range in editor-native coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

impl LspRange {
    #[must_use]
    pub const fn new(start: LspPosition, end: LspPosition) -> Self {
        Self { start, end }
    }
}

/// One editor edit, in editor-native coordinates. `range` addresses the text
/// as it was *before* this edit and after every preceding edit of the same
/// [`LspManager::change`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEdit {
    pub range: LspRange,
    pub new_text: String,
}

/// The language server and LSP `languageId` selected for a file extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLanguage {
    /// [`LanguageServerSettings::id`] of the server that claims the extension.
    pub server_id: String,
    /// LSP `languageId` sent in `textDocument/didOpen`.
    pub language_id: String,
}

/// Liveness of the language server backing one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspStatus {
    /// No server is configured for this document, or LSP is switched off
    /// entirely by an empty `language_servers` list.
    Disabled,
    /// The server could not be used at all: the executable is absent, the path
    /// cannot be addressed as a document URI, or no async runtime is running.
    Unavailable { reason: String },
    /// The process is spawned and `initialize` is in flight.
    Starting,
    /// `initialize` succeeded and the document is synchronised.
    Ready,
    /// The session initialised badly or died. `stderr_tail` is the last
    /// [`STDERR_TAIL_LINES`] lines the server wrote.
    Failed {
        reason: String,
        stderr_tail: Vec<String>,
    },
}

impl LspStatus {
    /// Whether the surface should offer a `Restart` action.
    #[must_use]
    pub fn can_restart(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Diagnostic severity, defaulting to [`LspSeverity::Error`] when the server
/// omits it, as the specification leaves that choice to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LspSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// Routine diagnostic previews are bounded independently of the wire frame.
pub const DIAGNOSTIC_PREVIEW_BYTES: usize = 16 * 1024;
const DIAGNOSTIC_TRUNCATED: &str =
    "\n… [preview truncated; copy full diagnostic for complete text]";

/// A UTF-8-safe, visibly truncated prefix. No allocation or full-text scan is
/// needed for a short value; a long value allocates at most the preview budget.
pub fn diagnostic_preview(text: &str) -> std::borrow::Cow<'_, str> {
    if text.len() <= DIAGNOSTIC_PREVIEW_BYTES {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut end = DIAGNOSTIC_PREVIEW_BYTES - DIAGNOSTIC_TRUNCATED.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{DIAGNOSTIC_TRUNCATED}", &text[..end]))
}

/// One diagnostic with editor-native coordinates and its complete, immutable
/// protocol payload. Mirrors and shifted ranges share the original strings,
/// related information and opaque server data rather than copying them.
#[derive(Clone)]
pub struct LspDiagnostic {
    pub range: LspRange,
    pub severity: LspSeverity,
    pub protocol: Arc<lsp_types::Diagnostic>,
}

impl std::ops::Deref for LspDiagnostic {
    type Target = lsp_types::Diagnostic;

    fn deref(&self) -> &Self::Target {
        &self.protocol
    }
}

impl PartialEq for LspDiagnostic {
    fn eq(&self, other: &Self) -> bool {
        self.range == other.range
            && self.severity == other.severity
            && (Arc::ptr_eq(&self.protocol, &other.protocol) || self.protocol == other.protocol)
    }
}
impl Eq for LspDiagnostic {}

impl std::fmt::Debug for LspDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspDiagnostic")
            .field("range", &self.range)
            .field("severity", &self.severity)
            .field("message", &diagnostic_preview(&self.message))
            .finish_non_exhaustive()
    }
}

impl LspDiagnostic {
    /// Construct a local diagnostic; server diagnostics retain the actual wire
    /// object instead, including its original UTF-16 range and opaque data.
    pub fn new(
        range: LspRange,
        severity: LspSeverity,
        message: String,
        source: Option<String>,
        code: Option<String>,
    ) -> Self {
        Self {
            range,
            severity,
            protocol: Arc::new(lsp_types::Diagnostic {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(range.start.line, range.start.character),
                    lsp_types::Position::new(range.end.line, range.end.character),
                ),
                severity: Some(match severity {
                    LspSeverity::Error => lsp_types::DiagnosticSeverity::ERROR,
                    LspSeverity::Warning => lsp_types::DiagnosticSeverity::WARNING,
                    LspSeverity::Information => lsp_types::DiagnosticSeverity::INFORMATION,
                    LspSeverity::Hint => lsp_types::DiagnosticSeverity::HINT,
                }),
                message,
                source,
                code: code.map(lsp_types::NumberOrString::String),
                ..Default::default()
            }),
        }
    }

    pub fn message_preview(&self) -> std::borrow::Cow<'_, str> {
        diagnostic_preview(&self.message)
    }

    /// Bounded origin text; combine only bounded pieces before formatting so a
    /// pathological source/code cannot allocate its complete value in a view.
    pub fn origin_preview(&self) -> String {
        let source = self.source.as_deref().unwrap_or("");
        let code = match &self.code {
            Some(lsp_types::NumberOrString::String(code)) => {
                std::borrow::Cow::Borrowed(code.as_str())
            }
            Some(lsp_types::NumberOrString::Number(code)) => {
                std::borrow::Cow::Owned(code.to_string())
            }
            None => std::borrow::Cow::Borrowed(""),
        };
        if source.is_empty() {
            return diagnostic_preview(&code).into_owned();
        }
        if code.is_empty() {
            return diagnostic_preview(source).into_owned();
        }
        diagnostic_preview(&format!(
            "{} {}",
            diagnostic_preview(source),
            diagnostic_preview(&code)
        ))
        .into_owned()
    }

    /// Full protocol JSON is materialized only after an explicit copy action.
    /// It preserves message, source, numeric/string code, related info and data.
    pub fn copy_text(&self) -> String {
        // Compact JSON avoids multiplying deeply nested opaque data with
        // indentation when a user requests the complete payload.
        serde_json::to_string(self.protocol.as_ref()).expect("diagnostic JSON is serializable")
    }
}

/// Retains standing diagnostics through a sequence of exact editor changes.
/// Insertions at a diagnostic's beginning move it with the original text;
/// insertions at its end stay outside the underline. Replaced portions clamp
/// to the replacement rather than leaving ranges aimed at unrelated lines.
pub fn shift_diagnostics(items: &mut [LspDiagnostic], edits: &[LspTextEdit]) {
    for edit in edits {
        let mut ending = edit.range.start;
        for character in edit.new_text.chars() {
            if character == '\n' {
                ending.line = ending.line.saturating_add(1);
                ending.character = 0;
            } else {
                ending.character = ending.character.saturating_add(1);
            }
        }
        // CRLF's carriage return is not counted on its completed line. The
        // final line contains no LF, so only its scalar count is used above.
        for item in items.iter_mut() {
            let point = item.range.start == item.range.end;
            item.range.start =
                shift_diagnostic_position(item.range.start, edit.range, ending, true);
            item.range.end = shift_diagnostic_position(item.range.end, edit.range, ending, point);
            item.range.end = item.range.end.max(item.range.start);
        }
    }
}

fn shift_diagnostic_position(
    at: LspPosition,
    old: LspRange,
    new_end: LspPosition,
    right: bool,
) -> LspPosition {
    if at < old.start {
        return at;
    }
    if old.start == old.end && at == old.start {
        return if right { new_end } else { old.start };
    }
    if at < old.end {
        return if right { old.start } else { new_end };
    }
    if at.line == old.end.line {
        LspPosition::new(
            new_end.line,
            new_end
                .character
                .saturating_add(at.character.saturating_sub(old.end.character)),
        )
    } else {
        LspPosition::new(
            new_end
                .line
                .saturating_add(at.line.saturating_sub(old.end.line)),
            at.character,
        )
    }
}

/// Completion item kind, used only to pick an icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LspCompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

/// One snippet tab stop, as a byte range into [`LspSnippet::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSnippetStop {
    /// Tab-stop index. `$0` is the final cursor position and sorts last.
    pub index: u32,
    pub range: Range<usize>,
}

/// An expanded snippet: literal text plus the tab stops the editor cycles
/// through, in visit order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspSnippet {
    pub text: String,
    pub stops: Vec<LspSnippetStop>,
}

/// How the editor must apply one completion.
///
/// `range` is always populated: when the server supplies neither `textEdit`
/// nor `insertText`, it is the identifier prefix immediately before the
/// requested position, so the editor never has to guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCompletionEdit {
    /// The range to replace when inserting.
    pub range: LspRange,
    /// The wider range offered by an `InsertReplaceEdit`, when the server
    /// distinguished insert from replace.
    pub replace_range: Option<LspRange>,
    /// Literal replacement text. Equal to [`LspSnippet::text`] when `snippet`
    /// is set, so a caller without snippet support stays correct.
    pub new_text: String,
    /// Present only when the server marked the item as a snippet.
    pub snippet: Option<LspSnippet>,
}

/// One completion candidate, already converted to editor-native coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCompletionItem {
    pub label: String,
    pub kind: Option<LspCompletionKind>,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    pub sort_text: Option<String>,
    pub filter_text: Option<String>,
    pub preselect: bool,
    pub deprecated: bool,
    pub edit: LspCompletionEdit,
    /// Extra edits (imports, `use` lines) applied together with `edit`.
    pub additional_edits: Vec<LspCompletionEdit>,
}

/// Which navigation request produced a location list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocationKind {
    Definition,
    References,
}

/// What a location points at, and therefore how it may be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspTarget {
    /// A file inside the requesting worktree: open a normal editable tab.
    Workspace { path: PathBuf },
    /// A file outside the requesting worktree: open a read-only
    /// external-source tab.
    External { path: PathBuf },
    /// A non-`file:` URI. Listed for the user, never written.
    Uri { uri: String },
}

/// Which coordinate space a [`LspLocation::range`] is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspCoordinates {
    /// Converted against a known snapshot; editor-native scalar offsets.
    Scalar,
    /// The target's text is not open at the LSP boundary, so the range is
    /// still in protocol UTF-16 code units and must be converted by whoever
    /// loads the file.
    Utf16,
}

/// One definition or reference result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub target: LspTarget,
    pub range: LspRange,
    pub coordinates: LspCoordinates,
}

/// Server progress report, forwarded for a status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspProgress {
    pub token: String,
    pub title: Option<String>,
    pub message: Option<String>,
    pub percentage: Option<u32>,
    pub done: bool,
}

/// Authority to deliver one document's current server status or progress.
/// Only the manager and its bound producer can mint this capability.
#[derive(Clone)]
pub struct LspDelivery(Arc<DeliveryState>);

struct DeliveryState {
    active: AtomicBool,
    session: Option<Arc<AtomicBool>>,
}

impl fmt::Debug for LspDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LspDelivery").finish_non_exhaustive()
    }
}

impl PartialEq for LspDelivery {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl LspDelivery {
    fn active(&self) -> bool {
        self.0.active.load(Ordering::Acquire)
            && self
                .0
                .session
                .as_ref()
                .is_none_or(|retired| !retired.load(Ordering::Acquire))
    }
}

/// Everything the runtime pushes to the application.
#[derive(Debug, Clone, PartialEq)]
pub enum LspEvent {
    /// Discovery completed off the UI thread. The application acknowledges it
    /// with `LspManager::finish_discovery`; closed/replaced documents are ignored.
    RootDiscovered {
        doc: LspDocumentId,
        ticket: u64,
        root: PathBuf,
    },
    /// Cached process metrics were refreshed by a background worker.
    Resources { doc: LspDocumentId },
    /// Current diagnostics for `doc` as of `version`. Always a full
    /// replacement, never a delta.
    Diagnostics {
        doc: LspDocumentId,
        version: i32,
        items: Vec<LspDiagnostic>,
    },
    Completion {
        request: LspRequestId,
        doc: LspDocumentId,
        items: Vec<LspCompletionItem>,
    },
    Hover {
        request: LspRequestId,
        doc: LspDocumentId,
        markdown: Option<String>,
        range: Option<LspRange>,
    },
    Locations {
        request: LspRequestId,
        doc: LspDocumentId,
        kind: LocationKind,
        items: Vec<LspLocation>,
    },
    Status {
        doc: LspDocumentId,
        status: LspStatus,
        delivery: LspDelivery,
    },
    Progress {
        doc: LspDocumentId,
        progress: LspProgress,
        delivery: LspDelivery,
    },
    /// A request produced no usable answer. Superseded requests report
    /// [`LspError::Cancelled`].
    RequestFailed {
        request: LspRequestId,
        doc: LspDocumentId,
        error: LspError,
    },
    /// The editor's edits could not be converted against this runtime's
    /// snapshot, so the whole document was resent. The buffer is intact; the
    /// two sides had drifted apart.
    DocumentDesynced {
        doc: LspDocumentId,
        version: i32,
        error: LspError,
    },
}

/// Why a conversion or request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LspError {
    #[error("document {0} is not open at the LSP boundary")]
    UnknownDocument(u64),
    #[error("no language server is running for this document")]
    NoServer,
    #[error("result applies to version {result} but the document is at version {current}")]
    StaleVersion { result: i32, current: i32 },
    #[error("line {line} character {character} is outside the version {version} snapshot")]
    PositionOutOfRange {
        line: u32,
        character: u32,
        version: i32,
    },
    #[error("UTF-16 offset {character} on line {line} splits a surrogate pair")]
    SplitSurrogate { line: u32, character: u32 },
    #[error("range start is after range end")]
    InvertedRange,
    #[error("the edit batch does not reconstruct the document it claims to produce")]
    InconsistentEdits,
    #[error("the document's LSP version stream is exhausted")]
    VersionExhausted,
    #[error("path {0} is not valid UTF-8 and cannot be used as an LSP document URI")]
    NonUtf8Path(String),
    #[error("path {0} cannot be represented as a file URI")]
    UnrepresentablePath(String),
    #[error("the server replied with an error: {0}")]
    Protocol(String),
    #[error("superseded by a newer request")]
    Cancelled,
}

// ---------------------------------------------------------------------------
// Versioned snapshots and coordinate conversion
// ---------------------------------------------------------------------------

/// One immutable version of a document's text plus its line index.
#[derive(Debug)]
struct Snapshot {
    version: i32,
    text: OnceLock<Arc<str>>,
    native: Option<TextSnapshot>,
    /// Populated on demand for non-native or incompatible line topology.
    line_starts: OnceLock<Arc<[u32]>>,
}

impl Snapshot {
    fn new(version: i32, text: Arc<str>) -> Self {
        let result = Self {
            version,
            text: OnceLock::from(text),
            native: None,
            line_starts: OnceLock::new(),
        };
        result.protocol_lines();
        result
    }

    fn with_version(&self, version: i32) -> Self {
        Self {
            version,
            native: self.native.clone(),
            text: self
                .text
                .get()
                .cloned()
                .map(OnceLock::from)
                .unwrap_or_default(),
            line_starts: self
                .line_starts
                .get()
                .cloned()
                .map(OnceLock::from)
                .unwrap_or_default(),
        }
    }

    fn from_native(version: i32, native: TextSnapshot) -> Self {
        Self {
            version,
            text: OnceLock::new(),
            native: Some(native.without_cached_text()),
            line_starts: OnceLock::new(),
        }
    }

    fn full_text(&self) -> &Arc<str> {
        self.text.get_or_init(|| {
            self.native
                .as_ref()
                .expect("lazy snapshot has native source")
                .text()
        })
    }

    fn wire_text(&self) -> String {
        self.native.as_ref().map_or_else(
            || self.full_text().to_string(),
            TextSnapshot::to_string_uncached,
        )
    }

    fn byte_len(&self) -> usize {
        self.native
            .as_ref()
            .map_or_else(|| self.full_text().len(), TextSnapshot::len)
    }

    fn compatible_native(&self) -> Option<&TextSnapshot> {
        self.native
            .as_ref()
            .filter(|source| source.protocol_compatible_lines())
    }

    fn protocol_lines(&self) -> &[u32] {
        self.line_starts.get_or_init(|| {
            let mut lines = vec![0];
            for (index, byte) in self.full_text().bytes().enumerate() {
                if byte == b'\n' {
                    lines.push(u32::try_from(index + 1).unwrap_or(u32::MAX));
                }
            }
            lines.into()
        })
    }

    fn line_count(&self) -> u32 {
        u32::try_from(
            self.compatible_native()
                .map_or_else(|| self.protocol_lines().len(), TextSnapshot::line_count),
        )
        .unwrap_or(u32::MAX)
    }

    fn line_span(&self, line: u32) -> Option<Range<usize>> {
        if let Some(source) = self.compatible_native() {
            let line = usize::try_from(line).ok()?;
            let start = source.byte_offset(NativePosition { line, column: 0 })?;
            return Some(start..start + source.line(line)?.text.len());
        }
        let lines = self.protocol_lines();
        let start = *lines.get(line as usize)? as usize;
        let end = lines
            .get(line as usize + 1)
            .map_or(self.byte_len(), |next| *next as usize - 1);
        let end = if end > start && self.full_text().as_bytes().get(end - 1) == Some(&b'\r') {
            end - 1
        } else {
            end
        };
        Some(start..end)
    }

    fn line_content(&self, line: u32) -> Option<std::borrow::Cow<'_, str>> {
        if let Some(source) = self.compatible_native() {
            return Some(source.line(usize::try_from(line).ok()?)?.text);
        }
        let span = self.line_span(line)?;
        Some(std::borrow::Cow::Borrowed(&self.full_text()[span]))
    }

    fn out_of_range(&self, line: u32, character: u32) -> LspError {
        LspError::PositionOutOfRange {
            line,
            character,
            version: self.version,
        }
    }

    /// Editor-native position to protocol UTF-16 position.
    fn to_utf16(&self, position: LspPosition) -> Result<lsp_types::Position, LspError> {
        let content = self
            .line_content(position.line)
            .ok_or_else(|| self.out_of_range(position.line, position.character))?;
        let mut scalars = 0u32;
        let mut units = 0u32;
        for character in content.chars() {
            if scalars == position.character {
                return Ok(lsp_types::Position::new(position.line, units));
            }
            scalars += 1;
            units += u32::try_from(character.len_utf16()).unwrap_or(1);
        }
        if scalars == position.character {
            Ok(lsp_types::Position::new(position.line, units))
        } else {
            Err(self.out_of_range(position.line, position.character))
        }
    }

    /// Protocol UTF-16 position to editor-native position. An offset landing
    /// inside a surrogate pair is refused rather than rounded.
    fn from_utf16(&self, position: lsp_types::Position) -> Result<LspPosition, LspError> {
        if position.line >= self.line_count() {
            return Err(self.out_of_range(position.line, position.character));
        }
        let content = self
            .line_content(position.line)
            .ok_or_else(|| self.out_of_range(position.line, position.character))?;
        let mut scalars = 0u32;
        let mut units = 0u32;
        for character in content.chars() {
            if units == position.character {
                return Ok(LspPosition::new(position.line, scalars));
            }
            let width = u32::try_from(character.len_utf16()).unwrap_or(1);
            if position.character > units && position.character < units + width {
                return Err(LspError::SplitSurrogate {
                    line: position.line,
                    character: position.character,
                });
            }
            units += width;
            scalars += 1;
        }
        if units == position.character {
            Ok(LspPosition::new(position.line, scalars))
        } else {
            Err(self.out_of_range(position.line, position.character))
        }
    }

    fn range_to_utf16(&self, range: LspRange) -> Result<lsp_types::Range, LspError> {
        if range.start > range.end {
            return Err(LspError::InvertedRange);
        }
        Ok(lsp_types::Range::new(
            self.to_utf16(range.start)?,
            self.to_utf16(range.end)?,
        ))
    }

    fn range_from_utf16(&self, range: lsp_types::Range) -> Result<LspRange, LspError> {
        let start = self.from_utf16(range.start)?;
        let end = self.from_utf16(range.end)?;
        if start > end {
            return Err(LspError::InvertedRange);
        }
        Ok(LspRange::new(start, end))
    }

    /// Editor-native position to a byte offset into [`Snapshot::text`].
    fn byte_offset(&self, position: LspPosition) -> Result<usize, LspError> {
        let span = self
            .line_span(position.line)
            .ok_or_else(|| self.out_of_range(position.line, position.character))?;
        let content = self
            .line_content(position.line)
            .ok_or_else(|| self.out_of_range(position.line, position.character))?;
        let mut scalars = 0u32;
        for (offset, _) in content.char_indices() {
            if scalars == position.character {
                return Ok(span.start + offset);
            }
            scalars += 1;
        }
        if scalars == position.character {
            Ok(span.end)
        } else {
            Err(self.out_of_range(position.line, position.character))
        }
    }

    /// The identifier characters immediately before `position`, which is the
    /// range a completion without `textEdit` must replace.
    fn identifier_prefix(&self, position: LspPosition) -> LspRange {
        let Some(content) = self.line_content(position.line) else {
            return LspRange::new(position, position);
        };
        let mut start = position.character;
        let before: Vec<char> = content.chars().take(position.character as usize).collect();
        for character in before.iter().rev() {
            if character.is_alphanumeric() || *character == '_' {
                start -= 1;
            } else {
                break;
            }
        }
        LspRange::new(LspPosition::new(position.line, start), position)
    }
}

/// Whether replacing `range` of `text` with `insert` produces `expected`,
/// decided without building the spliced document.
fn splices_into(text: &str, range: Range<usize>, insert: &str, expected: &str) -> bool {
    let head = range.start;
    let tail = text.len() - range.end;
    if expected.len() != head + insert.len() + tail {
        return false;
    }
    let (text, insert, expected) = (text.as_bytes(), insert.as_bytes(), expected.as_bytes());
    expected[..head] == text[..head]
        && expected[head..head + insert.len()] == *insert
        && expected[expected.len() - tail..] == text[range.end..]
}

/// Converts a batch of editor edits into protocol content changes, measuring
/// each edit against the text that edit actually applied to.
///
/// `expected` is the authoritative post-edit buffer. The reconstruction is
/// compared against it, so an edit list that does not describe the transition
/// it claims is refused here instead of silently desynchronising the server.
///
/// The comparison never materialises the result. An edit arrives per
/// keystroke on the COSMIC update thread, and the editor's own synchronisation
/// avoids rewriting its shadow copy for the same reason: a copy and a line
/// re-index per character are work proportional to the file, not to the edit.
/// The last edit is therefore measured against `expected` in place, and an
/// intermediate document exists only where a following edit has to be
/// addressed in the coordinates it produced.
fn build_incremental(
    base: &Snapshot,
    edits: &[LspTextEdit],
    expected: &str,
) -> Result<Vec<lsp_types::TextDocumentContentChangeEvent>, LspError> {
    let Some((last, leading)) = edits.split_last() else {
        // No edits describe no transition, so the buffer must not have moved.
        return if **base.full_text() == *expected {
            Ok(Vec::new())
        } else {
            Err(LspError::InconsistentEdits)
        };
    };
    let mut changes = Vec::with_capacity(edits.len());
    let mut intermediate: Option<Snapshot> = None;
    for edit in leading {
        let working = intermediate.as_ref().unwrap_or(base);
        let (start, end) = measure_edit(working, edit, &mut changes)?;
        let mut next = String::with_capacity(working.full_text().len() + edit.new_text.len());
        next.push_str(&working.full_text()[..start]);
        next.push_str(&edit.new_text);
        next.push_str(&working.full_text()[end..]);
        intermediate = Some(Snapshot::new(base.version, Arc::from(next)));
    }
    let working = intermediate.as_ref().unwrap_or(base);
    let (start, end) = measure_edit(working, last, &mut changes)?;
    if !splices_into(working.full_text(), start..end, &last.new_text, expected) {
        return Err(LspError::InconsistentEdits);
    }
    Ok(changes)
}

/// Convert only affected source lines. Unusual native line topology or a
/// legacy source without persistent intermediate lines uses the lazy full
/// fallback, leaving serialization at the existing debounce boundary.
fn native_incremental(
    base: &Snapshot,
    edits: &[LspTextEdit],
    expected: &TextSnapshot,
) -> Option<Vec<lsp_types::TextDocumentContentChangeEvent>> {
    if edits.is_empty() {
        return None;
    }
    let mut changes = Vec::with_capacity(edits.len());
    let mut working = base.compatible_native()?.clone();
    for edit in edits {
        let snapshot = Snapshot::from_native(base.version, working.clone());
        changes.push(lsp_types::TextDocumentContentChangeEvent {
            range: Some(snapshot.range_to_utf16(edit.range).ok()?),
            range_length: None,
            text: edit.new_text.clone(),
        });
        let native_position = |at: LspPosition| -> Option<NativePosition> {
            let line = usize::try_from(at.line).ok()?;
            let content = working.line(line)?;
            let scalar = usize::try_from(at.character).ok()?;
            let column = if scalar == content.text.chars().count() {
                content.text.len()
            } else {
                content.text.char_indices().nth(scalar)?.0
            };
            Some(NativePosition { line, column })
        };
        working = working.replaced(
            native_position(edit.range.start)?,
            native_position(edit.range.end)?,
            &edit.new_text,
        )?;
    }
    working.same_text(expected).then_some(changes)
}

/// Records `edit` as a protocol content change measured against `working`,
/// and reports the byte range it replaces there.
fn measure_edit(
    working: &Snapshot,
    edit: &LspTextEdit,
    changes: &mut Vec<lsp_types::TextDocumentContentChangeEvent>,
) -> Result<(usize, usize), LspError> {
    let range = working.range_to_utf16(edit.range)?;
    let start = working.byte_offset(edit.range.start)?;
    let end = working.byte_offset(edit.range.end)?;
    changes.push(lsp_types::TextDocumentContentChangeEvent {
        range: Some(range),
        range_length: None,
        text: edit.new_text.clone(),
    });
    Ok((start, end))
}

// ---------------------------------------------------------------------------
// Snapshot store
// ---------------------------------------------------------------------------

/// Identity of one language-server session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    server_id: Arc<str>,
    root: Arc<Path>,
}

/// Per-URI text state shared between the manager and its session tasks.
///
/// Two editor tabs may address the same file. They share one URI on the wire,
/// one refcounted `didOpen`, and one strictly increasing version stream;
/// diagnostics fan out to every document bound to the URI. The most recent
/// [`LspManager::change`] wins, which matches what the user sees because the
/// repository widget focuses an existing tab instead of opening a second
/// buffer for the same path.
#[derive(Debug)]
struct UriState {
    session: SessionKey,
    docs: Vec<LspDocumentId>,
    /// Newest last, at most [`MAX_RETAINED_SNAPSHOTS`] entries.
    snapshots: VecDeque<Arc<Snapshot>>,
}

impl UriState {
    fn latest(&self) -> Arc<Snapshot> {
        Arc::clone(
            self.snapshots
                .back()
                .expect("a URI always retains its newest snapshot"),
        )
    }

    fn push(&mut self, snapshot: Arc<Snapshot>) {
        if self.snapshots.len() == MAX_RETAINED_SNAPSHOTS {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(snapshot);
    }
}

#[derive(Debug, Default)]
struct SnapshotStoreInner {
    uris: HashMap<Arc<Url>, UriState>,
    docs: HashMap<LspDocumentId, Arc<Url>>,
}

/// Result of recording a new document version.
enum RecordOutcome {
    Recorded {
        previous: Arc<Snapshot>,
        snapshot: Arc<Snapshot>,
    },
    /// The URI is not bound to any document at this boundary.
    UnknownUri,
    /// `i32::MAX` was reached, so no strictly greater version exists.
    VersionExhausted,
}

/// Versioned document text at the LSP boundary.
#[derive(Debug, Default)]
struct SnapshotStore {
    inner: Mutex<SnapshotStoreInner>,
}

impl SnapshotStore {
    /// Poison only means some other task panicked while holding this lock; the
    /// store's invariants do not depend on that task finishing, and refusing to
    /// lock would turn one panicked session into an abort of the whole UI
    /// process on the user's next keystroke.
    fn lock(&self) -> std::sync::MutexGuard<'_, SnapshotStoreInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Binds `doc` to `uri`, seeding or extending the URI's version stream.
    /// `None` when the URI's version stream is exhausted.
    #[cfg(test)]
    fn register(
        &self,
        doc: LspDocumentId,
        uri: &Arc<Url>,
        session: &SessionKey,
        requested_version: i32,
        text: Arc<str>,
    ) -> Option<Arc<Snapshot>> {
        self.register_source(
            doc,
            uri,
            session,
            requested_version,
            Arc::new(Snapshot::new(requested_version, text)),
        )
    }

    fn register_source(
        &self,
        doc: LspDocumentId,
        uri: &Arc<Url>,
        session: &SessionKey,
        requested_version: i32,
        source: Arc<Snapshot>,
    ) -> Option<Arc<Snapshot>> {
        let mut inner = self.lock();
        match inner.uris.get_mut(uri) {
            Some(state) => {
                let version = next_version(state.latest().version, requested_version)?;
                if !state.docs.contains(&doc) {
                    state.docs.push(doc);
                }
                let snapshot = Arc::new(source.with_version(version));
                state.push(Arc::clone(&snapshot));
                inner.docs.insert(doc, Arc::clone(uri));
                Some(snapshot)
            }
            None => {
                let snapshot = Arc::new(source.with_version(requested_version));
                let mut snapshots = VecDeque::with_capacity(MAX_RETAINED_SNAPSHOTS);
                snapshots.push_back(Arc::clone(&snapshot));
                inner.uris.insert(
                    Arc::clone(uri),
                    UriState {
                        session: session.clone(),
                        docs: vec![doc],
                        snapshots,
                    },
                );
                inner.docs.insert(doc, Arc::clone(uri));
                Some(snapshot)
            }
        }
    }

    /// Records a new version of `uri`'s text.
    fn record(&self, uri: &Arc<Url>, requested_version: i32, text: Arc<str>) -> RecordOutcome {
        let mut inner = self.lock();
        let Some(state) = inner.uris.get_mut(uri) else {
            return RecordOutcome::UnknownUri;
        };
        let previous = state.latest();
        let Some(version) = next_version(previous.version, requested_version) else {
            return RecordOutcome::VersionExhausted;
        };
        let snapshot = Arc::new(Snapshot::new(version, text));
        state.push(Arc::clone(&snapshot));
        RecordOutcome::Recorded { previous, snapshot }
    }

    fn record_native(
        &self,
        uri: &Arc<Url>,
        requested_version: i32,
        source: TextSnapshot,
    ) -> RecordOutcome {
        let mut inner = self.lock();
        let Some(state) = inner.uris.get_mut(uri) else {
            return RecordOutcome::UnknownUri;
        };
        let previous = state.latest();
        let Some(version) = next_version(previous.version, requested_version) else {
            return RecordOutcome::VersionExhausted;
        };
        let snapshot = Arc::new(Snapshot::from_native(version, source));
        state.push(Arc::clone(&snapshot));
        RecordOutcome::Recorded { previous, snapshot }
    }

    fn latest(&self, uri: &Url) -> Option<Arc<Snapshot>> {
        Some(self.lock().uris.get(uri)?.latest())
    }

    fn bound_to_other_session(&self, uri: &Url, session: &SessionKey) -> bool {
        self.lock()
            .uris
            .get(uri)
            .is_some_and(|state| &state.session != session)
    }

    fn snapshot_at(&self, uri: &Url, version: i32) -> Option<Arc<Snapshot>> {
        let inner = self.lock();
        let state = inner.uris.get(uri)?;
        state
            .snapshots
            .iter()
            .find(|snapshot| snapshot.version == version)
            .map(Arc::clone)
    }

    /// Admit a diagnostic batch only from the session that owns this URI.
    /// Recipients and text must come from the same publication, rather than
    /// independently looking up a URI that another live server can also name.
    fn diagnostic_snapshot(
        &self,
        uri: &Url,
        session: &SessionKey,
    ) -> Option<(Vec<LspDocumentId>, Arc<Snapshot>)> {
        let inner = self.lock();
        let state = inner.uris.get(uri)?;
        (&state.session == session).then(|| (state.docs.clone(), state.latest()))
    }

    fn docs_for_session(&self, session: &SessionKey) -> Vec<LspDocumentId> {
        let inner = self.lock();
        let mut docs: Vec<LspDocumentId> = inner
            .uris
            .values()
            .filter(|state| &state.session == session)
            .flat_map(|state| state.docs.iter().copied())
            .collect();
        docs.sort_unstable();
        docs
    }

    /// Unbinds `doc`, returning its URI and whether the URI has no documents
    /// left.
    fn unregister(&self, doc: LspDocumentId) -> Option<(Arc<Url>, bool)> {
        let mut inner = self.lock();
        let uri = inner.docs.remove(&doc)?;
        let mut last = false;
        if let Some(state) = inner.uris.get_mut(&uri) {
            state.docs.retain(|entry| *entry != doc);
            last = state.docs.is_empty();
        }
        if last {
            inner.uris.remove(&uri);
        }
        Some((uri, last))
    }

    fn clear(&self) {
        let mut inner = self.lock();
        inner.uris.clear();
        inner.docs.clear();
    }
}

/// The wire version is strictly increasing and follows the editor's own
/// numbering whenever the editor is ahead, so `LspEvent` versions stay
/// comparable with the buffer revision the application holds.
///
/// `None` once the stream is exhausted: repeating a version would break the
/// invariant every stale-result check depends on, so the caller reports it
/// instead of sending a second snapshot under the same number.
fn next_version(previous: i32, requested: i32) -> Option<i32> {
    if previous == i32::MAX {
        return None;
    }
    Some(requested.max(previous + 1))
}

// ---------------------------------------------------------------------------
// Event sink
// ---------------------------------------------------------------------------

/// Which staged class an event belongs to when the channel is full.
///
/// Diagnostics and progress describe *current* state, so only the newest
/// matters and older entries are replaced. A request answer or a desync
/// report is *terminal*: the application is waiting for exactly that message,
/// so these are queued in order and never replaced by unrelated traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StagedClass {
    Diagnostics(LspDocumentId),
    Status(LspDocumentId),
    Progress(LspDocumentId),
    Resources(LspDocumentId),
}

#[derive(Debug, Default)]
struct PendingInner {
    /// One producer per current document binding, including unbacked refusals.
    deliveries: HashMap<LspDocumentId, LspDelivery>,
    /// Newest replaceable event per class. Bounded by the number of open
    /// documents, and a document's entries are dropped when it closes.
    replaceable: HashMap<StagedClass, LspEvent>,
    /// Insertion order of `replaceable`, so a flood of one document's
    /// progress cannot starve another document's status.
    order: VecDeque<StagedClass>,
    /// Terminal events, oldest first.
    terminal: VecDeque<LspEvent>,
    /// Newest diagnostics version staged per document, so a batch that was
    /// computed for an older version can never replace a newer one.
    diagnostic_versions: HashMap<LspDocumentId, i32>,
}

impl PendingInner {
    fn bind(&mut self, doc: LspDocumentId, session: Option<Arc<AtomicBool>>) -> LspDelivery {
        let delivery = LspDelivery(Arc::new(DeliveryState {
            active: AtomicBool::new(true),
            session,
        }));
        if let Some(previous) = self.deliveries.insert(doc, delivery.clone()) {
            previous.0.active.store(false, Ordering::Release);
        }
        delivery
    }

    fn accepts(&self, event: &LspEvent) -> bool {
        match event {
            LspEvent::Status { doc, delivery, .. } | LspEvent::Progress { doc, delivery, .. } => {
                delivery.active() && self.deliveries.get(doc) == Some(delivery)
            }
            _ => true,
        }
    }

    fn stage(&mut self, event: LspEvent) {
        // Check under the same lock as rebinding, before replacing a current
        // class. Dropping stale traffic only at delivery would lose that class.
        if !self.accepts(&event) {
            return;
        }
        let class = match &event {
            LspEvent::Diagnostics { doc, version, .. } => {
                let staged = self.diagnostic_versions.entry(*doc).or_insert(i32::MIN);
                if *version < *staged {
                    return;
                }
                *staged = *version;
                StagedClass::Diagnostics(*doc)
            }
            LspEvent::Status { doc, .. } => StagedClass::Status(*doc),
            LspEvent::Progress { doc, .. } => StagedClass::Progress(*doc),
            LspEvent::Resources { doc } => StagedClass::Resources(*doc),
            _ => {
                // Terminal: dropping one would leave the application waiting
                // for an answer that never comes, so the oldest is only
                // evicted once the queue is genuinely exhausted.
                if self.terminal.len() >= MAX_PENDING_EVENTS {
                    self.terminal.pop_front();
                }
                self.terminal.push_back(event);
                return;
            }
        };
        if self.replaceable.insert(class, event).is_none() {
            self.order.push_back(class);
        }
    }

    /// Alternates classes so neither can starve the other.
    fn take(&mut self, prefer_terminal: bool) -> Option<LspEvent> {
        let replaceable = |inner: &mut Self| loop {
            let class = inner.order.pop_front()?;
            if let Some(event) = inner.replaceable.remove(&class) {
                if let StagedClass::Diagnostics(doc) = class {
                    inner.diagnostic_versions.remove(&doc);
                }
                if inner.accepts(&event) {
                    return Some(event);
                }
            }
        };
        if prefer_terminal {
            self.terminal.pop_front().or_else(|| replaceable(self))
        } else {
            replaceable(self).or_else(|| self.terminal.pop_front())
        }
    }

    fn forget(&mut self, doc: LspDocumentId) {
        if let Some(delivery) = self.deliveries.remove(&doc) {
            delivery.0.active.store(false, Ordering::Release);
        }
        for class in [
            StagedClass::Diagnostics(doc),
            StagedClass::Status(doc),
            StagedClass::Progress(doc),
            StagedClass::Resources(doc),
        ] {
            self.replaceable.remove(&class);
        }
        self.order.retain(|class| match class {
            StagedClass::Diagnostics(at)
            | StagedClass::Status(at)
            | StagedClass::Progress(at)
            | StagedClass::Resources(at) => *at != doc,
        });
        self.diagnostic_versions.remove(&doc);
        self.terminal.retain(|event| event_doc(event) != Some(doc));
    }
}

/// The document an event concerns, when it concerns exactly one.
fn event_doc(event: &LspEvent) -> Option<LspDocumentId> {
    match event {
        LspEvent::RootDiscovered { doc, .. }
        | LspEvent::Resources { doc }
        | LspEvent::Diagnostics { doc, .. }
        | LspEvent::Completion { doc, .. }
        | LspEvent::Hover { doc, .. }
        | LspEvent::Locations { doc, .. }
        | LspEvent::Status { doc, .. }
        | LspEvent::Progress { doc, .. }
        | LspEvent::RequestFailed { doc, .. }
        | LspEvent::DocumentDesynced { doc, .. } => Some(*doc),
    }
}

#[derive(Debug, Default)]
struct PendingEvents {
    inner: Mutex<PendingInner>,
    notify: Notify,
    closed: AtomicBool,
}

/// Publishes [`LspEvent`]s without ever blocking the COSMIC update thread.
#[derive(Debug, Clone)]
struct EventSink {
    tx: async_channel::Sender<LspEvent>,
    pending: Arc<PendingEvents>,
}

impl EventSink {
    fn bind(&self, doc: LspDocumentId, session: Option<Arc<AtomicBool>>) -> LspDelivery {
        self.pending
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bind(doc, session)
    }

    fn session_delivery(
        &self,
        doc: LspDocumentId,
        retired: &Arc<AtomicBool>,
    ) -> Option<LspDelivery> {
        let inner = self.pending.inner.lock().unwrap_or_else(|p| p.into_inner());
        let delivery = inner.deliveries.get(&doc)?;
        (delivery.active()
            && delivery
                .0
                .session
                .as_ref()
                .is_some_and(|session| Arc::ptr_eq(session, retired)))
        .then(|| delivery.clone())
    }

    fn accepts(&self, event: &LspEvent) -> bool {
        self.pending
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .accepts(event)
    }

    /// Awaits channel capacity. Only session and request tasks call this, so
    /// backpressure slows protocol traffic rather than losing results.
    async fn emit(&self, event: LspEvent) {
        if !self.accepts(&event) {
            return;
        }
        let _ = self.tx.send(event).await;
    }

    /// Never blocks. When the channel is full the event is staged by class,
    /// and [`drain_pending`] moves it on as capacity appears.
    fn emit_now(&self, event: LspEvent) {
        if !self.accepts(&event) {
            return;
        }
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(async_channel::TrySendError::Closed(_)) => {}
            Err(async_channel::TrySendError::Full(event)) => {
                self.pending
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .stage(event);
                self.pending.notify.notify_one();
            }
        }
    }

    /// Discards everything staged for a document that is no longer open.
    fn forget(&self, doc: LspDocumentId) {
        self.pending
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .forget(doc);
    }
}

/// Moves staged events into the channel as capacity appears.
async fn drain_pending(tx: async_channel::Sender<LspEvent>, pending: Arc<PendingEvents>) {
    let mut prefer_terminal = true;
    loop {
        pending.notify.notified().await;
        loop {
            let next = pending
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(prefer_terminal);
            prefer_terminal = !prefer_terminal;
            match next {
                Some(event) => {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
                None => break,
            }
        }
        if pending.closed.load(Ordering::Acquire) {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Bounded ring of a server's most recent stderr output.
#[derive(Debug, Default)]
struct StderrTail {
    lines: Mutex<VecDeque<String>>,
}

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Retains at most [`STDERR_TAIL_LINE_BYTES`] bytes of decoded text. Lossy
/// decoding expands each invalid byte to a three-byte replacement character,
/// so the budget is applied *after* decoding, not before.
fn bounded_line(bytes: &[u8]) -> String {
    let end = bytes.len().min(STDERR_TAIL_LINE_BYTES);
    let mut line = String::from_utf8_lossy(&bytes[..end])
        .trim_end_matches('\r')
        .to_owned();
    if line.len() > STDERR_TAIL_LINE_BYTES {
        let mut cut = STDERR_TAIL_LINE_BYTES;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
    }
    line
}

/// Reads a server's stderr forever, retaining only the tail. Neither a missing
/// newline nor a flood can grow memory without bound.
async fn drain_stderr(mut reader: Box<dyn AsyncRead + Send + Unpin>, tail: Arc<StderrTail>) {
    let mut buffer = [0u8; 4096];
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let mut rest = &buffer[..read];
        while let Some(index) = rest.iter().position(|byte| *byte == b'\n') {
            partial.extend_from_slice(&rest[..index]);
            let line = std::mem::take(&mut partial);
            tail.push(bounded_line(&line));
            rest = &rest[index + 1..];
        }
        partial.extend_from_slice(rest);
        if partial.len() > STDERR_TAIL_LINE_BYTES {
            let line = std::mem::take(&mut partial);
            tail.push(bounded_line(&line));
        }
    }
    if !partial.is_empty() {
        tail.push(bounded_line(&partial));
    }
}

/// How a server process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildStop {
    /// Exited on its own after `exit`.
    Exited,
    /// Needed `SIGTERM` to the process group.
    Terminated,
    /// Needed `SIGKILL` to the process group.
    Killed,
    /// The original child identity or final reap could not be confirmed.
    Unconfirmed,
}

/// Shared shutdown authority. Claiming closes publication permanently, and a
/// fallback keeps the lock through its signal so the child owner cannot reap
/// the leader while a claimed numeric signal is still pending.
#[derive(Debug, Default)]
struct PublishedGroup(Mutex<GroupPublication>);

#[derive(Debug, Default)]
enum GroupPublication {
    #[default]
    Empty,
    Live(u32),
    Closed,
}

impl PublishedGroup {
    fn publish(&self, group: u32) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, GroupPublication::Empty) {
            return false;
        }
        *state = GroupPublication::Live(group);
        true
    }

    fn claim(&self) -> Option<u32> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, GroupPublication::Closed) {
            GroupPublication::Live(group) => Some(group),
            GroupPublication::Empty | GroupPublication::Closed => None,
        }
    }

    fn kill(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let GroupPublication::Live(group) =
            std::mem::replace(&mut *state, GroupPublication::Closed)
        {
            // Keep the lock through the syscall. A losing child owner must
            // observe the completed signal before its final wait can reap.
            signal_process_group(group, SIGKILL);
        }
    }

    #[cfg(test)]
    fn current(&self) -> u32 {
        match *self.0.lock().unwrap() {
            GroupPublication::Live(group) => group,
            GroupPublication::Empty | GroupPublication::Closed => 0,
        }
    }
}

/// A spawned server process that leads its own process group.
#[derive(Debug)]
struct ChildProcess {
    child: tokio::process::Child,
    identity: tokio::io::unix::AsyncFd<OwnedFd>,
    published: Option<Arc<PublishedGroup>>,
    /// Process-group id, captured at spawn because `Child::id` is cleared by
    /// the first successful wait.
    process_group: u32,
}

impl ChildProcess {
    /// Capture the original child before publishing it or polling Tokio's
    /// reaping wait. The child still reserves its numeric PID even if exited.
    fn new(mut child: tokio::process::Child) -> io::Result<Self> {
        let process_group = child
            .id()
            .ok_or_else(|| io::Error::other("Child already reaped"))?;
        let identity = (|| {
            // SAFETY: this is our freshly spawned, still-unreaped child.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process_group, 0) };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: pidfd_open returned a new descriptor owned here.
            let identity = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
            tokio::io::unix::AsyncFd::new(identity)
        })();
        match identity {
            Ok(identity) => Ok(Self {
                child,
                identity,
                published: None,
                process_group,
            }),
            Err(error) => {
                // No other owner has received this child. Its unreaped leader
                // reserves the original group until Tokio's drop reaper runs.
                signal_process_group(process_group, SIGKILL);
                if let Err(cleanup) = child.start_kill() {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{error}; child cleanup is unconfirmed: {cleanup}"),
                    ));
                }
                Err(error)
            }
        }
    }

    fn publish(&mut self, published: Arc<PublishedGroup>) -> bool {
        if !published.publish(self.process_group) {
            return false;
        }
        self.published = Some(published);
        true
    }

    /// Observe the original child without reaping. A numeric wait target could
    /// instead observe another direct child after the old PID was recycled.
    fn observed_exit(&self) -> io::Result<bool> {
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: the owned pidfd identifies the original child. WNOWAIT
            // keeps its zombie reserved until all group signals are finished.
            let result = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.identity.get_ref().as_raw_fd() as libc::id_t,
                    &mut information,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                // SAFETY: successful waitid initialized this status field.
                return Ok(unsafe { information.si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Give the child time to exit, end its original group, then reap it.
    /// Taking the published id is a claim against other shutdown callers;
    /// retaining the unreaped leader additionally reserves the kernel ID.
    async fn stop(mut self, published: &PublishedGroup) -> ChildStop {
        let group = published.claim();
        // Preserve Child::wait's stdin-close behavior without polling its
        // reaping future until all group signals have finished.
        drop(self.child.stdin.take());
        let signal = |number: i32| {
            if let Some(group) = group {
                signal_process_group(group, number);
            }
        };
        let outcome = match self.settled(TERMINATION_GRACE).await {
            Ok(true) => ChildStop::Exited,
            Ok(false) => {
                signal(SIGTERM);
                match self.settled(TERMINATION_GRACE).await {
                    Ok(true) => ChildStop::Terminated,
                    Ok(false) => ChildStop::Killed,
                    Err(error) => {
                        tracing::warn!(%error, "language-server child identity is unconfirmed");
                        return ChildStop::Unconfirmed;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "language-server child identity is unconfirmed");
                return ChildStop::Unconfirmed;
            }
        };
        // The original leader remains unreaped while ending proc-macro servers
        // and other descendants, even when the leader itself exited promptly.
        signal(SIGKILL);
        if let Err(error) = self.child.wait().await {
            tracing::warn!(%error, "language-server final reap is unconfirmed");
            return ChildStop::Unconfirmed;
        }
        #[cfg(all(test, target_os = "linux"))]
        tests::LSP_AFTER_REAP.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        outcome
    }

    async fn settled(&self, grace: Duration) -> io::Result<bool> {
        if self.observed_exit()? {
            return Ok(true);
        }
        // pidfd readiness is terminal and never needs clearing. No blocking
        // wait or periodic polling occupies the shared Tokio runtime.
        match tokio::time::timeout(grace, self.identity.readable()).await {
            Ok(Ok(_ready)) => self.observed_exit(),
            Ok(Err(error)) => Err(error),
            Err(_) => Ok(false),
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // Disarm fallback authority before Tokio's Child drop can reap. Taking
        // the lock also waits for any already-claimed fallback signal to finish.
        if let Some(published) = &self.published {
            published.claim();
        }
        // Cancellation may occur before stop, or while its grace future waits.
        // Preserve cleanup without ever signalling after the final reap.
        if self.observed_exit().is_ok() {
            signal_process_group(self.process_group, SIGKILL);
        }
        // Tokio retains its original kill_on_drop and asynchronous orphan reap.
    }
}

/// One header line of a message frame, as far as this guard judges it.
enum HeaderLine {
    /// Ends the header block: the body of the declared length follows.
    Blank,
    /// Declares the body length of the frame being read.
    Length(usize),
    /// Nothing this guard has an opinion about.
    Other,
}

/// Reader that refuses the two frame headers no language server writes by
/// accident, before the framing layer downstream acts on them.
///
/// `async-lsp` allocates a declared `Content-Length` in full before reading a
/// single body byte, and accumulates a header line until a newline arrives.
/// Both are unbounded, and a failed allocation aborts this process rather
/// than unwinding, so every unsaved buffer in every card goes with it. A
/// server's stdout is the output of an arbitrary configured executable, so
/// the bound belongs on it, exactly as one already bounds its stderr.
///
/// Nothing else is judged here. The parser downstream is strict about
/// `<name>: <value>\r\n`, and ordinary garbage is left to fail there, where
/// the reported reason is the accurate one. A `Content-Length` header is the
/// one exception: whatever this guard cannot accept as a length it refuses,
/// because a length it merely passes on is a length nothing bounds.
struct GuardedFrames<R> {
    inner: R,
    /// The header line being read, without its terminator.
    header: Vec<u8>,
    /// Length the current header block has declared, if any.
    declared: Option<usize>,
    /// Body bytes still to be passed through unexamined.
    body: usize,
}

impl<R> GuardedFrames<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            header: Vec::new(),
            declared: None,
            body: 0,
        }
    }

    /// Tracks the framing of bytes about to be handed downstream.
    fn inspect(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.body > 0 {
                let step = self.body.min(rest.len());
                self.body -= step;
                rest = &rest[step..];
                continue;
            }
            let Some(index) = rest.iter().position(|byte| *byte == b'\n') else {
                self.extend(rest)?;
                break;
            };
            self.extend(&rest[..index])?;
            match Self::classify(&self.header)? {
                HeaderLine::Blank => self.body = self.declared.take().unwrap_or(0),
                HeaderLine::Length(length) => self.declared = Some(length),
                HeaderLine::Other => {}
            }
            self.header.clear();
            rest = &rest[index + 1..];
        }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.header.len() + bytes.len() > MAX_HEADER_LINE_BYTES {
            return Err(io::Error::other(format!(
                "the language server wrote a header line of more than \
                 {MAX_HEADER_LINE_BYTES} bytes without ending it"
            )));
        }
        self.header.extend_from_slice(bytes);
        Ok(())
    }

    /// Reads one complete header line, in the one shape the framing layer
    /// accepts.
    ///
    /// The value of a `Content-Length` header is judged by the very call the
    /// framing layer makes on it — `usize::from_str` — and never by a
    /// character predicate of this guard's own. That is what keeps the two
    /// from drifting apart: every value `async_lsp` will honour is a value
    /// this returns as [`HeaderLine::Length`] and therefore caps, including
    /// the `+60` that `usize::from_str` accepts and a digit test does not,
    /// and every value it will refuse is refused here too.
    fn classify(line: &[u8]) -> io::Result<HeaderLine> {
        let Some(line) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.strip_suffix('\r'))
        else {
            return Ok(HeaderLine::Other);
        };
        if line.is_empty() {
            return Ok(HeaderLine::Blank);
        }
        let Some((name, value)) = line.split_once(": ") else {
            return Ok(HeaderLine::Other);
        };
        if !name.eq_ignore_ascii_case("Content-Length") {
            return Ok(HeaderLine::Other);
        }
        match value.parse::<usize>() {
            Ok(length) if length <= MAX_CONTENT_LENGTH => Ok(HeaderLine::Length(length)),
            Ok(length) => Err(io::Error::other(format!(
                "the language server declared a {length}-byte message, \
                 over the {MAX_CONTENT_LENGTH}-byte limit"
            ))),
            // A digit string too large for `usize` lands here too: it is out
            // of range by definition.
            Err(_) => Err(io::Error::other(format!(
                "the language server declared {value:?} as a message length, \
                 which is not one"
            ))),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for GuardedFrames<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let already = buf.filled().len();
        let guard = self.get_mut();
        match std::pin::Pin::new(&mut guard.inner).poll_read(context, buf) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        // Judged before the caller can act on it: an error here discards the
        // bytes it was about to read, which is the point.
        guard.inspect(&buf.filled()[already..])?;
        Poll::Ready(Ok(()))
    }
}

/// Everything a session needs to speak to one server instance.
struct Transport {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    child: Option<ChildProcess>,
}

/// What to launch, resolved before the session task starts.
#[derive(Debug, Clone)]
struct LaunchSpec {
    initialization_options: Option<serde_json::Value>,
    command: Vec<String>,
    root: PathBuf,
}

type TransportFactory = Arc<dyn Fn(&LaunchSpec) -> Result<Transport, String> + Send + Sync>;

/// Keep the original directory inode allocated while a server owns its root.
/// A path can be renamed and replaced between watchdog polls; existence alone
/// would then keep a server for the previous checkout alive indefinitely.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

impl RootIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

async fn pin_session_root(root: &Path) -> io::Result<(tokio::fs::File, RootIdentity)> {
    // Linux can pin identity without directory read permissions. Other Unix
    // systems retain a read-only directory descriptor. Both paths run off the
    // application update thread and keep the inode alive through teardown.
    let flags = libc::O_DIRECTORY | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    let flags = flags | libc::O_PATH;
    let directory = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(root)
        .await?;
    let identity = RootIdentity::from_metadata(&directory.metadata().await?);
    Ok((directory, identity))
}

/// Anchors `path` to `working_directory` without consulting the filesystem.
fn absolute_in(working_directory: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
    }
}

/// Whether the file `path` names carries an execute bit for anyone.
#[cfg(unix)]
fn has_execute_permission(metadata: &std::fs::Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 != 0
}

#[cfg(not(unix))]
fn has_execute_permission(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// Whether `path` names something this process could execute.
fn is_executable_file(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && has_execute_permission(&metadata),
        Err(_) => false,
    }
}

/// Resolves a configured language-server program to an absolute path, using
/// this process's `PATH` and working directory.
///
/// See [`resolve_program_in`] for why resolution cannot be left to the child.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    let working_directory = std::env::current_dir()
        .map_err(|error| format!("the working directory could not be read: {error}"))?;
    resolve_program_in(
        program,
        std::env::var_os("PATH").as_deref(),
        &working_directory,
    )
}

/// Resolves `program` against `search_path`, anchoring every relative result
/// to `working_directory`.
///
/// The child is launched with its working directory inside a repository, and
/// on Unix the `chdir` happens before the `execvp` `PATH` search — so leaving
/// a bare program name to the child would resolve every relative `PATH` entry,
/// including the empty entry POSIX defines as "the current directory", against
/// repository content. Resolving in the parent keeps the search anchored to
/// the directory the application itself runs in, and the absolute result the
/// child receives cannot be reinterpreted by its own chdir.
fn resolve_program_in(
    program: &str,
    search_path: Option<&OsStr>,
    working_directory: &Path,
) -> Result<PathBuf, String> {
    let named = Path::new(program);
    // A command naming a path is honoured as written, only anchored: the
    // configuration named a file rather than a program to search for.
    if named
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return Ok(absolute_in(working_directory, named));
    }
    for entry in std::env::split_paths(search_path.unwrap_or_else(|| OsStr::new(""))) {
        let candidate = absolute_in(working_directory, &entry.join(program));
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!("`{program}` was not found on PATH"))
}

/// Spawns the configured executable directly: never a shell, always its own
/// process group so a misbehaving server tree can be terminated wholesale.
fn spawn_process_transport(spec: &LaunchSpec) -> Result<Transport, String> {
    let (program, arguments) = spec
        .command
        .split_first()
        .ok_or_else(|| "the language server command is empty".to_owned())?;
    // Resolved in this process, before the child chdirs into the worktree.
    let resolved = resolve_program(program)?;
    let mut command = tokio::process::Command::new(&resolved);
    command
        .args(arguments)
        .current_dir(&spec.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            format!("`{program}` was not found on PATH")
        } else {
            format!("`{program}` could not be started: {error}")
        }
    })?;
    let mut child = ChildProcess::new(child)
        .map_err(|error| format!("`{program}` process identity could not be retained: {error}"))?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| format!("`{program}` has no stdout pipe"))?;
    let stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| format!("`{program}` has no stdin pipe"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| format!("`{program}` has no stderr pipe"))?;
    Ok(Transport {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        stderr: Some(Box::new(stderr)),
        child: Some(child),
    })
}

// ---------------------------------------------------------------------------
// Negotiated synchronisation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncKind {
    None,
    Full,
    Incremental,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SaveSupport {
    include_text: bool,
}

/// Exactly what the server agreed to receive. Nothing outside this is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyncSupport {
    open_close: bool,
    change: SyncKind,
    save: Option<SaveSupport>,
}

impl SyncSupport {
    /// The specification's default for a server that says nothing: no
    /// synchronisation at all.
    const SILENT: Self = Self {
        open_close: false,
        change: SyncKind::None,
        save: None,
    };

    fn negotiate(capability: Option<&lsp_types::TextDocumentSyncCapability>) -> Self {
        match capability {
            None => Self::SILENT,
            // The shorthand form states only a change kind. Open/close is
            // implied, because a change notification for a document the
            // server was never told about is meaningless.
            Some(lsp_types::TextDocumentSyncCapability::Kind(kind)) => Self {
                open_close: true,
                change: Self::kind(Some(*kind)),
                save: None,
            },
            Some(lsp_types::TextDocumentSyncCapability::Options(options)) => Self {
                open_close: options.open_close.unwrap_or(false),
                change: Self::kind(options.change),
                save: match &options.save {
                    None | Some(lsp_types::TextDocumentSyncSaveOptions::Supported(false)) => None,
                    Some(lsp_types::TextDocumentSyncSaveOptions::Supported(true)) => {
                        Some(SaveSupport {
                            include_text: false,
                        })
                    }
                    Some(lsp_types::TextDocumentSyncSaveOptions::SaveOptions(save)) => {
                        Some(SaveSupport {
                            include_text: save.include_text.unwrap_or(false),
                        })
                    }
                },
            },
        }
    }

    fn kind(kind: Option<lsp_types::TextDocumentSyncKind>) -> SyncKind {
        match kind {
            Some(lsp_types::TextDocumentSyncKind::FULL) => SyncKind::Full,
            Some(lsp_types::TextDocumentSyncKind::INCREMENTAL) => SyncKind::Incremental,
            _ => SyncKind::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Session protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RequestKind {
    Completion,
    Hover,
    Definition,
    References,
}

/// One client request, already converted to protocol coordinates.
#[derive(Debug)]
struct PendingRequest {
    id: LspRequestId,
    doc: LspDocumentId,
    uri: Arc<Url>,
    worktree: Arc<Path>,
    kind: RequestKind,
    position: lsp_types::Position,
    /// Snapshot version the position was converted against; a result that
    /// arrives after the document moved on is refused.
    version: i32,
}

enum SessionCommand {
    Open {
        doc: LspDocumentId,
        uri: Arc<Url>,
        language_id: String,
        version: i32,
        text: Arc<Snapshot>,
    },
    Change {
        uri: Arc<Url>,
        version: i32,
        /// `None` when the manager could not build incremental changes, in
        /// which case the whole document is resent.
        incremental: Option<Vec<lsp_types::TextDocumentContentChangeEvent>>,
        full: Arc<Snapshot>,
    },
    Save {
        uri: Arc<Url>,
        text: Arc<str>,
    },
    Close {
        doc: LspDocumentId,
        uri: Arc<Url>,
    },
    Request(PendingRequest),
    Cancel {
        request: LspRequestId,
    },
    Shutdown,
}

struct PendingChange {
    version: i32,
    incremental: Option<Vec<lsp_types::TextDocumentContentChangeEvent>>,
    full: Arc<Snapshot>,
    /// When this URI's coalesced edits are due. Per URI, so a file being
    /// typed into cannot postpone another file's pending notification.
    due: tokio::time::Instant,
}

struct InFlight {
    id: LspRequestId,
    doc: LspDocumentId,
    /// JSON-RPC id the request was sent with, needed for `$/cancelRequest`.
    wire_id: i32,
    abort: tokio::task::AbortHandle,
}

/// Raw server answers, unified so one code path converts all four.
enum RawResult {
    Completion(Option<lsp_types::CompletionResponse>),
    Hover(Option<lsp_types::Hover>),
    Definition(Option<lsp_types::GotoDefinitionResponse>),
    References(Option<Vec<lsp_types::Location>>),
}

/// Issues one position request against an owned socket, so the returned
/// future borrows nothing from the session state.
async fn perform(
    server: ServerSocket,
    kind: RequestKind,
    position: lsp_types::TextDocumentPositionParams,
) -> Result<RawResult, async_lsp::Error> {
    match kind {
        RequestKind::Completion => server
            .request::<request::Completion>(lsp_types::CompletionParams {
                text_document_position: position,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: None,
            })
            .await
            .map(RawResult::Completion),
        RequestKind::Hover => server
            .request::<request::HoverRequest>(lsp_types::HoverParams {
                text_document_position_params: position,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            })
            .await
            .map(RawResult::Hover),
        RequestKind::Definition => server
            .request::<request::GotoDefinition>(lsp_types::GotoDefinitionParams {
                text_document_position_params: position,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
            })
            .await
            .map(RawResult::Definition),
        RequestKind::References => server
            .request::<request::References>(lsp_types::ReferenceParams {
                text_document_position: position,
                work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_types::PartialResultParams::default(),
                context: lsp_types::ReferenceContext {
                    include_declaration: true,
                },
            })
            .await
            .map(RawResult::References),
    }
}

struct SessionContext {
    key: SessionKey,
    launch: LaunchSpec,
    transport: TransportFactory,
    snapshots: Arc<SnapshotStore>,
    events: EventSink,
    status: Arc<Mutex<LspStatus>>,
    completion_triggers: Arc<Mutex<Option<Vec<String>>>>,
    /// Set the moment the manager stops using this session, so a process that
    /// is still winding down cannot publish into the documents its
    /// replacement now owns.
    retired: Arc<AtomicBool>,
    retirement: Arc<tokio::sync::Notify>,
    /// Shutdown authority shared with exit paths that stop awaiting this task.
    /// The group is published once, and closing the authority prevents both
    /// later signals and late publication after retirement. Fallback claims
    /// hold the lock through their signal before the child owner may reap.
    process_group: Arc<PublishedGroup>,
    stderr: Arc<StderrTail>,
}

impl SessionContext {
    async fn set_status(&self, status: LspStatus) {
        if self.retired.load(Ordering::Acquire) {
            return;
        }
        *self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status.clone();
        for doc in self.snapshots.docs_for_session(&self.key) {
            let Some(delivery) = self.events.session_delivery(doc, &self.retired) else {
                continue;
            };
            self.events
                .emit(LspEvent::Status {
                    doc,
                    status: status.clone(),
                    delivery,
                })
                .await;
        }
    }
}

/// Shared state for the client-side router handling server-initiated traffic.
struct RouterState {
    key: SessionKey,
    snapshots: Arc<SnapshotStore>,
    events: EventSink,
    retired: Arc<AtomicBool>,
}

fn build_router(context: &SessionContext) -> Router<RouterState> {
    let mut router = Router::new(RouterState {
        key: context.key.clone(),
        snapshots: Arc::clone(&context.snapshots),
        events: context.events.clone(),
        retired: Arc::clone(&context.retired),
    });
    router.notification::<notification::PublishDiagnostics>(|state, params| {
        publish_diagnostics(state, params);
        ControlFlow::Continue(())
    });
    router.notification::<notification::Progress>(|state, params| {
        publish_progress(state, &params);
        ControlFlow::Continue(())
    });
    router.notification::<notification::ShowMessage>(|_, _| ControlFlow::Continue(()));
    router.notification::<notification::LogMessage>(|_, _| ControlFlow::Continue(()));
    router.notification::<notification::TelemetryEvent>(|_, _| ControlFlow::Continue(()));
    router.notification::<notification::LogTrace>(|_, _| ControlFlow::Continue(()));
    // Dynamic registration is accepted and ignored: every capability this
    // client uses is negotiated statically in `initialize`.
    router.request::<request::RegisterCapability, _>(|_, _| std::future::ready(Ok(())));
    router.request::<request::UnregisterCapability, _>(|_, _| std::future::ready(Ok(())));
    router.request::<request::WorkDoneProgressCreate, _>(|_, _| std::future::ready(Ok(())));
    // No per-server configuration is exposed, so every requested section is
    // answered with null rather than an error, which servers read as "use
    // your defaults".
    router.request::<request::WorkspaceConfiguration, _>(|_, params| {
        std::future::ready(Ok(vec![serde_json::Value::Null; params.items.len()]))
    });
    // Unknown notifications must never break the main loop: an unsupported
    // notification is not a reason to lose a working language server.
    router.unhandled_notification(|_, _| ControlFlow::Continue(()));
    router.unhandled_event(|_, _| ControlFlow::Continue(()));
    router
}

fn publish_diagnostics(state: &mut RouterState, params: lsp_types::PublishDiagnosticsParams) {
    if state.retired.load(Ordering::Acquire) {
        return;
    }
    let Some((docs, snapshot)) = state.snapshots.diagnostic_snapshot(&params.uri, &state.key)
    else {
        return;
    };
    // A versioned batch is only valid for the text it was computed from.
    // Anything older is stale by definition: a newer `didChange` is already
    // on the wire and the server will republish against it. A server that
    // sends no version leaves the client no choice but the newest text, which
    // is why `version_support` is advertised in `initialize`.
    if let Some(version) = params.version {
        if version != snapshot.version {
            return;
        }
    }
    let items: Vec<LspDiagnostic> = params
        .diagnostics
        .into_iter()
        .filter_map(|diagnostic| convert_diagnostic(&snapshot, diagnostic))
        .collect();
    for doc in docs {
        state.events.emit_now(LspEvent::Diagnostics {
            doc,
            version: snapshot.version,
            items: items.clone(),
        });
    }
}

fn convert_diagnostic(
    snapshot: &Snapshot,
    diagnostic: lsp_types::Diagnostic,
) -> Option<LspDiagnostic> {
    // One unconvertible range must not discard the whole batch.
    let range = snapshot.range_from_utf16(diagnostic.range).ok()?;
    Some(LspDiagnostic {
        range,
        severity: match diagnostic.severity {
            Some(lsp_types::DiagnosticSeverity::WARNING) => LspSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => LspSeverity::Information,
            Some(lsp_types::DiagnosticSeverity::HINT) => LspSeverity::Hint,
            _ => LspSeverity::Error,
        },
        protocol: Arc::new(diagnostic),
    })
}

fn publish_progress(state: &mut RouterState, params: &lsp_types::ProgressParams) {
    if state.retired.load(Ordering::Acquire) {
        return;
    }
    let token = match &params.token {
        lsp_types::NumberOrString::Number(value) => value.to_string(),
        lsp_types::NumberOrString::String(value) => value.clone(),
    };
    let progress = match &params.value {
        lsp_types::ProgressParamsValue::WorkDone(lsp_types::WorkDoneProgress::Begin(begin)) => {
            LspProgress {
                token,
                title: Some(begin.title.clone()),
                message: begin.message.clone(),
                percentage: begin.percentage,
                done: false,
            }
        }
        lsp_types::ProgressParamsValue::WorkDone(lsp_types::WorkDoneProgress::Report(report)) => {
            LspProgress {
                token,
                title: None,
                message: report.message.clone(),
                percentage: report.percentage,
                done: false,
            }
        }
        lsp_types::ProgressParamsValue::WorkDone(lsp_types::WorkDoneProgress::End(end)) => {
            LspProgress {
                token,
                title: None,
                message: end.message.clone(),
                percentage: None,
                done: true,
            }
        }
    };
    // Progress is per server rather than per document, so every document of
    // *this* session sees it and the surface can show one indexing indicator.
    // Documents belonging to another server or root are none of its business.
    for doc in state.snapshots.docs_for_session(&state.key) {
        let Some(delivery) = state.events.session_delivery(doc, &state.retired) else {
            continue;
        };
        state.events.emit_now(LspEvent::Progress {
            doc,
            progress: progress.clone(),
            delivery,
        });
    }
}

fn client_capabilities() -> lsp_types::ClientCapabilities {
    lsp_types::ClientCapabilities {
        text_document: Some(lsp_types::TextDocumentClientCapabilities {
            synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                did_save: Some(true),
            }),
            publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                version_support: Some(true),
                code_description_support: Some(true),
                ..Default::default()
            }),
            completion: Some(lsp_types::CompletionClientCapabilities {
                completion_item: Some(lsp_types::CompletionItemCapability {
                    snippet_support: Some(true),
                    deprecated_support: Some(true),
                    preselect_support: Some(true),
                    insert_replace_support: Some(true),
                    documentation_format: Some(vec![
                        lsp_types::MarkupKind::Markdown,
                        lsp_types::MarkupKind::PlainText,
                    ]),
                    ..Default::default()
                }),
                context_support: Some(true),
                ..Default::default()
            }),
            hover: Some(lsp_types::HoverClientCapabilities {
                content_format: Some(vec![
                    lsp_types::MarkupKind::Markdown,
                    lsp_types::MarkupKind::PlainText,
                ]),
                ..Default::default()
            }),
            definition: Some(lsp_types::GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            references: Some(lsp_types::ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            ..Default::default()
        }),
        window: Some(lsp_types::WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
        // No alternative position encoding is advertised, so the server must
        // use UTF-16, which is what the conversion layer above implements.
        ..Default::default()
    }
}

#[allow(deprecated)]
fn initialize_params(root: &Path) -> Result<lsp_types::InitializeParams, String> {
    let root_uri = Url::from_file_path(root)
        .map_err(|()| LspError::UnrepresentablePath(root.display().to_string()).to_string())?;
    Ok(lsp_types::InitializeParams {
        process_id: Some(std::process::id()),
        root_path: None,
        root_uri: Some(root_uri.clone()),
        initialization_options: None,
        capabilities: client_capabilities(),
        trace: None,
        workspace_folders: Some(vec![lsp_types::WorkspaceFolder {
            uri: root_uri,
            name: root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string()),
        }]),
        client_info: Some(lsp_types::ClientInfo {
            name: "cagents".to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
        locale: None,
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    })
}

/// Drives one language-server session for its whole life.
async fn run_session(context: SessionContext, mut commands: commands::Receiver) {
    if context.retired.load(Ordering::Acquire) {
        return;
    }
    let transport = match (context.transport)(&context.launch) {
        Ok(transport) => transport,
        Err(reason) => {
            context.set_status(LspStatus::Unavailable { reason }).await;
            return;
        }
    };
    let Transport {
        reader,
        writer,
        stderr,
        child,
    } = transport;
    let mut child = child;
    if let Some(child) = &mut child
        && (context.retired.load(Ordering::Acquire)
            || !child.publish(Arc::clone(&context.process_group)))
    {
        // Retirement may have claimed the empty slot while synchronous spawn
        // was finishing. Never republish a group after the fallback has closed.
        return;
    }
    // Retained rather than detached: a fatal message a server writes just
    // before closing stdout is exactly the evidence a `Failed` status
    // promises, so the drain is awaited before the tail is snapshotted.
    let stderr_drain =
        stderr.map(|stderr| tokio::spawn(drain_stderr(stderr, Arc::clone(&context.stderr))));

    let (mainloop, server) = MainLoop::new_client(|_server| build_router(&context));
    // `tokio_util::compat` bridges Tokio's byte streams to the `futures-io`
    // traits `async-lsp` frames Content-Length messages over. Every reader
    // reaches it through the guard: the bound belongs to the boundary, not to
    // one way of producing a server.
    let mut protocol = tokio::spawn(async move {
        mainloop
            .run_buffered(GuardedFrames::new(reader).compat(), writer.compat_write())
            .await
    });

    // Mirrors `async_lsp::MainLoop`'s outgoing request counter, which starts
    // at zero and advances once per request in submission order. Every request
    // this client makes is created in this task, so the mirror is exact;
    // `a_superseded_request_is_cancelled_on_the_wire` locks the coupling.
    let mut next_wire_id: i32 = 0;

    let mut params = match initialize_params(&context.launch.root) {
        Ok(params) => params,
        Err(reason) => {
            teardown(protocol, child, stderr_drain, &context.process_group).await;
            context.set_status(LspStatus::Unavailable { reason }).await;
            return;
        }
    };
    params.initialization_options = context.launch.initialization_options.clone();
    let initialize = server.request::<request::Initialize>(params);
    next_wire_id += 1;
    let initialized = tokio::select! {
        biased;
        _ = async {
            if !context.retired.load(Ordering::Acquire) {
                context.retirement.notified().await;
            }
        } => {
            teardown(protocol, child, stderr_drain, &context.process_group).await;
            return;
        }
        _ = commands.closed() => {
            teardown(protocol, child, stderr_drain, &context.process_group).await;
            if let Some(reason) = commands.failure() {
                fail_queued_requests(&commands, &context, reason).await;
                context.set_status(LspStatus::Failed {
                    reason: reason.to_owned(),
                    stderr_tail: context.stderr.snapshot(),
                }).await;
            }
            return;
        }
        result = tokio::time::timeout(INITIALIZE_TIMEOUT, initialize) => result,
    };
    let negotiated = match initialized {
        Ok(Ok(result)) => result,
        outcome => {
            let reason = match outcome {
                Ok(Err(error)) => format!("initialize failed: {error}"),
                _ => format!(
                    "initialize did not answer within {} seconds",
                    INITIALIZE_TIMEOUT.as_secs()
                ),
            };
            // Cleanup first, unconditionally: publishing the status can wait
            // for the application to drain its channel, and a process must
            // never be left running because nobody read an event.
            teardown(protocol, child, stderr_drain, &context.process_group).await;
            if let Some(overload) = commands.failure() {
                fail_queued_requests(&commands, &context, overload).await;
            }
            context
                .set_status(LspStatus::Failed {
                    reason,
                    stderr_tail: context.stderr.snapshot(),
                })
                .await;
            return;
        }
    };
    let sync = SyncSupport::negotiate(negotiated.capabilities.text_document_sync.as_ref());
    *context
        .completion_triggers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = negotiated
        .capabilities
        .completion_provider
        .map(|provider| provider.trigger_characters.unwrap_or_default());
    // `initialized` is legal only after the response has been received.
    let _ = server.notify::<notification::Initialized>(lsp_types::InitializedParams {});
    context.set_status(LspStatus::Ready).await;

    let mut state = SessionState {
        server,
        sync,
        next_wire_id,
        open: HashMap::new(),
        bindings: HashMap::new(),
        pending: HashMap::new(),
        in_flight: HashMap::new(),
        by_request: HashMap::new(),
    };
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<LspRequestId>();
    let mut graceful = false;

    loop {
        tokio::select! {
            biased;
            outcome = &mut protocol => {
                let reason = match outcome {
                    Ok(Ok(())) => "the language server closed the connection".to_owned(),
                    Ok(Err(error)) => format!("the language server connection failed: {error}"),
                    Err(error) => format!("the language server client task stopped: {error}"),
                };
                // The process is stopped and reaped, and its stderr drained,
                // before anything is published: every emit below can block on
                // the application draining its channel.
                if let Some(child) = child {
                    child.stop(&context.process_group).await;
                }
                await_stderr(stderr_drain).await;
                state
                    .fail_all_in_flight(&context.events, LspError::Protocol(reason.clone()))
                    .await;
                context
                    .set_status(LspStatus::Failed {
                        reason,
                        stderr_tail: context.stderr.snapshot(),
                    })
                    .await;
                return;
            }
            finished = done_rx.recv() => {
                if let Some(request) = finished {
                    state.forget(request);
                }
            }
            () = async {
                let due = state.next_due().expect("guarded by the select condition");
                tokio::time::sleep_until(due).await;
            }, if state.next_due().is_some() => {
                state.flush_due_changes();
            }
            command = commands.recv() => {
                match command {
                    None => break,
                    Some(SessionCommand::Shutdown) => {
                        graceful = true;
                        break;
                    }
                    Some(command) => state.handle(command, &context, &done_tx).await,
                }
            }
        }
    }

    // Graceful stop: `shutdown`, then `exit`, then process-group termination.
    if graceful {
        state.cancel_all();
        let shutdown = state.server.request::<request::Shutdown>(());
        state.next_wire_id += 1;
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown).await;
    }
    let _ = state.server.notify::<notification::Exit>(());
    // A conforming server closes its side of the connection once it has seen
    // `exit`, which ends the main loop and therefore proves the notification
    // was written. Waiting for that instead of aborting immediately is what
    // makes the sequence observable; the wait is capped because an
    // uncooperative server must not delay teardown, and it is followed by
    // process-group termination either way.
    //
    // The socket is deliberately still held here: `async_lsp`'s main loop
    // panics if every socket clone is dropped while it runs, so it must be
    // stopped by the peer closing the connection or by `teardown`.
    let _ = tokio::time::timeout(EXIT_FLUSH_TIMEOUT, &mut protocol).await;
    teardown(protocol, child, stderr_drain, &context.process_group).await;
    if let Some(reason) = commands.failure() {
        fail_queued_requests(&commands, &context, reason).await;
        state
            .fail_all_in_flight(&context.events, LspError::Protocol(reason.to_owned()))
            .await;
        context
            .set_status(LspStatus::Failed {
                reason: reason.to_owned(),
                stderr_tail: context.stderr.snapshot(),
            })
            .await;
        return;
    }
    *context
        .status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = LspStatus::Disabled;
}

/// Requests admitted before overload must settle explicitly too, even when
/// initialization had not yet moved them into the session's in-flight map.
async fn fail_queued_requests(
    commands: &commands::Receiver,
    context: &SessionContext,
    reason: &str,
) {
    if context.retired.load(Ordering::Acquire) {
        return;
    }
    for request in commands.rejected_requests() {
        context
            .events
            .emit(LspEvent::RequestFailed {
                request: request.id,
                doc: request.doc,
                error: LspError::Protocol(reason.to_owned()),
            })
            .await;
    }
}

/// Stops the protocol task, then terminates and reaps the process, then
/// finishes draining its stderr. Nothing here can block on the application.
async fn teardown(
    protocol: tokio::task::JoinHandle<Result<(), async_lsp::Error>>,
    child: Option<ChildProcess>,
    stderr_drain: Option<tokio::task::JoinHandle<()>>,
    process_group: &PublishedGroup,
) {
    protocol.abort();
    if let Some(child) = child {
        child.stop(process_group).await;
    }
    await_stderr(stderr_drain).await;
}

/// Lets the stderr reader finish the bytes already in the pipe, so a failure
/// status carries the message the server died with. Bounded: a grandchild
/// holding the pipe open must not stall teardown.
async fn await_stderr(drain: Option<tokio::task::JoinHandle<()>>) {
    let Some(drain) = drain else {
        return;
    };
    let abort = drain.abort_handle();
    if tokio::time::timeout(STDERR_DRAIN_TIMEOUT, drain)
        .await
        .is_err()
    {
        abort.abort();
    }
}

struct SessionState {
    server: ServerSocket,
    sync: SyncSupport,
    next_wire_id: i32,
    /// Refcount per open URI, so two tabs on one file produce one `didOpen`.
    open: HashMap<Arc<Url>, usize>,
    /// Documents bound to each URI, so closing one tab cancels only its own
    /// requests.
    bindings: HashMap<Arc<Url>, Vec<LspDocumentId>>,
    /// Coalesced changes, each with its own debounce deadline.
    pending: HashMap<Arc<Url>, PendingChange>,
    in_flight: HashMap<(LspDocumentId, RequestKind), InFlight>,
    by_request: HashMap<LspRequestId, (LspDocumentId, RequestKind)>,
}

impl SessionState {
    async fn handle(
        &mut self,
        command: SessionCommand,
        context: &SessionContext,
        done: &mpsc::UnboundedSender<LspRequestId>,
    ) {
        match command {
            SessionCommand::Open {
                doc,
                uri,
                language_id,
                version,
                text,
            } => {
                let count = self.open.entry(Arc::clone(&uri)).or_insert(0);
                *count += 1;
                let first = *count == 1;
                self.bindings.entry(Arc::clone(&uri)).or_default().push(doc);
                if !self.sync.open_close {
                    return;
                }
                if first {
                    let _ = self.server.notify::<notification::DidOpenTextDocument>(
                        lsp_types::DidOpenTextDocumentParams {
                            text_document: lsp_types::TextDocumentItem {
                                uri: (*uri).clone(),
                                language_id,
                                version,
                                text: text.wire_text(),
                            },
                        },
                    );
                } else {
                    // A second buffer on the same file may hold different
                    // text, and the server has only the first one. Resending
                    // the whole document under the new version keeps both
                    // sides on the text every later request is converted
                    // against.
                    self.pending.remove(&uri);
                    self.send_change(
                        &uri,
                        PendingChange {
                            version,
                            incremental: None,
                            full: text,
                            due: tokio::time::Instant::now(),
                        },
                    );
                }
            }
            SessionCommand::Change {
                uri,
                version,
                incremental,
                full,
            } => {
                let due = tokio::time::Instant::now() + CHANGE_DEBOUNCE;
                match self.pending.get_mut(&uri) {
                    Some(existing) => {
                        existing.version = version;
                        existing.full = full;
                        existing.due = due;
                        // Continuous typing may postpone this deadline for a
                        // long time. Bound pending ranges here as well as at
                        // admission; full text remains authoritative.
                        existing.incremental =
                            commands::merge_incremental(existing.incremental.take(), incremental);
                    }
                    None => {
                        self.pending.insert(
                            Arc::clone(&uri),
                            PendingChange {
                                version,
                                incremental,
                                full,
                                due,
                            },
                        );
                    }
                }
            }
            SessionCommand::Save { uri, text } => {
                self.flush_changes(&uri);
                if let Some(save) = self.sync.save {
                    let _ = self.server.notify::<notification::DidSaveTextDocument>(
                        lsp_types::DidSaveTextDocumentParams {
                            text_document: lsp_types::TextDocumentIdentifier {
                                uri: (*uri).clone(),
                            },
                            text: save.include_text.then(|| text.to_string()),
                        },
                    );
                }
            }
            SessionCommand::Close { doc, uri } => {
                // Only this binding's own requests are cancelled: another tab
                // on the same file is still waiting for its answers.
                self.cancel_for_doc(doc, &context.events).await;
                if let Some(bound) = self.bindings.get_mut(&uri) {
                    if let Some(index) = bound.iter().position(|entry| *entry == doc) {
                        bound.remove(index);
                    }
                }
                let remaining = match self.open.get_mut(&uri) {
                    Some(count) => {
                        *count = count.saturating_sub(1);
                        *count
                    }
                    None => 0,
                };
                if remaining > 0 {
                    // The file is still open in another tab, so its debounced
                    // edits must still be delivered.
                    return;
                }
                self.open.remove(&uri);
                self.bindings.remove(&uri);
                self.pending.remove(&uri);
                if self.sync.open_close {
                    let _ = self.server.notify::<notification::DidCloseTextDocument>(
                        lsp_types::DidCloseTextDocumentParams {
                            text_document: lsp_types::TextDocumentIdentifier {
                                uri: (*uri).clone(),
                            },
                        },
                    );
                }
            }
            SessionCommand::Request(request) => self.dispatch(request, context, done).await,
            SessionCommand::Cancel { request } => {
                if let Some(slot) = self.by_request.remove(&request) {
                    if let Some(entry) = self.in_flight.remove(&slot) {
                        self.cancel_wire(entry.wire_id);
                        entry.abort.abort();
                        context
                            .events
                            .emit(LspEvent::RequestFailed {
                                request: entry.id,
                                doc: entry.doc,
                                error: LspError::Cancelled,
                            })
                            .await;
                    }
                }
            }
            SessionCommand::Shutdown => {}
        }
    }

    async fn dispatch(
        &mut self,
        request: PendingRequest,
        context: &SessionContext,
        done: &mpsc::UnboundedSender<LspRequestId>,
    ) {
        // The server must see the text the position refers to before it is
        // asked about it.
        self.flush_changes(&request.uri);
        // Supersession is per document and kind, as two tabs on one file each
        // get their own answers.
        let slot = (request.doc, request.kind);
        if let Some(previous) = self.in_flight.remove(&slot) {
            self.by_request.remove(&previous.id);
            self.cancel_wire(previous.wire_id);
            previous.abort.abort();
            context
                .events
                .emit(LspEvent::RequestFailed {
                    request: previous.id,
                    doc: previous.doc,
                    error: LspError::Cancelled,
                })
                .await;
        }

        let position = lsp_types::TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: (*request.uri).clone(),
            },
            position: request.position,
        };
        let wire_id = self.next_wire_id;
        self.next_wire_id += 1;

        let id = request.id;
        let doc = request.doc;
        let events = context.events.clone();
        let snapshots = Arc::clone(&context.snapshots);
        let done = done.clone();
        let server = self.server.clone();
        // `async_lsp::MainLoop` numbers outgoing requests from a counter, in
        // the order it receives them, so mirroring that counter means knowing
        // the order this session's own requests reach it. Every one of them
        // is created in this task, and what orders them is not where
        // `ServerSocket::request` is written but when it runs: `perform` is
        // an `async fn`, so the call happens on its first poll, and in 0.2.4
        // the socket enqueues from inside that call rather than from a later
        // poll of what it returns. Forcing that poll below and waiting on
        // `started_rx` therefore fixes the submission at this point in the
        // loop, and no second request can be created before it. That, plus
        // `next_wire_id` starting at zero exactly like the main loop's own
        // counter, makes `wire_id` the real JSON-RPC id and `$/cancelRequest`
        // meaningful.
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let mut call = Box::pin(perform(server, request.kind, position));
            let mut settled: Option<Result<RawResult, async_lsp::Error>> = None;
            std::future::poll_fn(|context| {
                if let Poll::Ready(value) = call.as_mut().poll(context) {
                    settled = Some(value);
                }
                Poll::Ready(())
            })
            .await;
            let _ = started_tx.send(());
            let outcome = match settled {
                Some(value) => value,
                None => call.await,
            };
            let event = match outcome {
                Ok(raw) => convert_result(&snapshots, &request, raw),
                Err(error) => LspEvent::RequestFailed {
                    request: request.id,
                    doc: request.doc,
                    error: LspError::Protocol(error.to_string()),
                },
            };
            events.emit(event).await;
            let _ = done.send(request.id);
        });
        let _ = started_rx.await;
        self.in_flight.insert(
            slot.clone(),
            InFlight {
                id,
                doc,
                wire_id,
                abort: task.abort_handle(),
            },
        );
        self.by_request.insert(id, slot);
    }

    fn cancel_wire(&self, wire_id: i32) {
        let _ = self
            .server
            .notify::<notification::Cancel>(lsp_types::CancelParams {
                id: lsp_types::NumberOrString::Number(wire_id),
            });
    }

    fn forget(&mut self, request: LspRequestId) {
        if let Some(slot) = self.by_request.remove(&request) {
            if self
                .in_flight
                .get(&slot)
                .is_some_and(|entry| entry.id == request)
            {
                self.in_flight.remove(&slot);
            }
        }
    }

    async fn cancel_for_doc(&mut self, doc: LspDocumentId, events: &EventSink) {
        for kind in [
            RequestKind::Completion,
            RequestKind::Hover,
            RequestKind::Definition,
            RequestKind::References,
        ] {
            if let Some(entry) = self.in_flight.remove(&(doc, kind)) {
                self.by_request.remove(&entry.id);
                self.cancel_wire(entry.wire_id);
                entry.abort.abort();
                events
                    .emit(LspEvent::RequestFailed {
                        request: entry.id,
                        doc: entry.doc,
                        error: LspError::Cancelled,
                    })
                    .await;
            }
        }
    }

    fn cancel_all(&mut self) {
        let entries: Vec<InFlight> = self.in_flight.drain().map(|(_, entry)| entry).collect();
        for entry in entries {
            self.cancel_wire(entry.wire_id);
            entry.abort.abort();
        }
        self.by_request.clear();
    }

    async fn fail_all_in_flight(&mut self, events: &EventSink, error: LspError) {
        let entries: Vec<InFlight> = self.in_flight.drain().map(|(_, entry)| entry).collect();
        for entry in entries {
            entry.abort.abort();
            events
                .emit(LspEvent::RequestFailed {
                    request: entry.id,
                    doc: entry.doc,
                    error: error.clone(),
                })
                .await;
        }
        self.by_request.clear();
    }

    /// Earliest pending due time, which is what the session loop sleeps until.
    fn next_due(&self) -> Option<tokio::time::Instant> {
        self.pending.values().map(|change| change.due).min()
    }

    /// Sends every coalesced change whose own debounce window has elapsed.
    fn flush_due_changes(&mut self) {
        let now = tokio::time::Instant::now();
        let ready: Vec<Arc<Url>> = self
            .pending
            .iter()
            .filter(|(_, change)| change.due <= now)
            .map(|(uri, _)| Arc::clone(uri))
            .collect();
        for uri in ready {
            self.flush_changes(&uri);
        }
    }

    fn flush_changes(&mut self, uri: &Arc<Url>) {
        let Some(change) = self.pending.remove(uri) else {
            return;
        };
        self.send_change(uri, change);
    }

    fn send_change(&mut self, uri: &Arc<Url>, change: PendingChange) {
        let full_content = || lsp_types::TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: change.full.wire_text(),
        };
        let content_changes = match self.sync.change {
            SyncKind::None => return,
            SyncKind::Full => vec![full_content()],
            SyncKind::Incremental => match change.incremental.clone() {
                Some(changes) if !changes.is_empty() => changes,
                // A whole-document event is legal under incremental sync and
                // is the only correct recovery when ranges cannot be trusted.
                _ => vec![full_content()],
            },
        };
        let _ = self.server.notify::<notification::DidChangeTextDocument>(
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier {
                    uri: (**uri).clone(),
                    version: change.version,
                },
                content_changes,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Inbound conversion
// ---------------------------------------------------------------------------

fn convert_result(snapshots: &SnapshotStore, request: &PendingRequest, raw: RawResult) -> LspEvent {
    let Some(current) = snapshots
        .latest(&request.uri)
        .map(|snapshot| snapshot.version)
    else {
        return LspEvent::RequestFailed {
            request: request.id,
            doc: request.doc,
            error: LspError::UnknownDocument(request.doc.0),
        };
    };
    let stale = LspEvent::RequestFailed {
        request: request.id,
        doc: request.doc,
        error: LspError::StaleVersion {
            result: request.version,
            current,
        },
    };
    // A result computed for text the user has already changed must not be
    // projected onto the current buffer.
    if current != request.version {
        return stale;
    }
    let Some(snapshot) = snapshots.snapshot_at(&request.uri, request.version) else {
        return stale;
    };

    match raw {
        RawResult::Completion(response) => {
            let raw_items = match response {
                None => Vec::new(),
                Some(lsp_types::CompletionResponse::Array(items)) => items,
                Some(lsp_types::CompletionResponse::List(list)) => list.items,
            };
            let mut items = Vec::with_capacity(raw_items.len());
            for item in &raw_items {
                match convert_completion(&snapshot, request.position, item) {
                    Ok(converted) => items.push(converted),
                    // Refused, not silently truncated: the item is omitted and
                    // the reason is logged, because a half-applied completion
                    // corrupts the buffer.
                    Err(error) => tracing::warn!(
                        label = %item.label,
                        %error,
                        "refused a completion item whose edits could not be converted"
                    ),
                }
            }
            LspEvent::Completion {
                request: request.id,
                doc: request.doc,
                items,
            }
        }
        RawResult::Hover(None) => LspEvent::Hover {
            request: request.id,
            doc: request.doc,
            markdown: None,
            range: None,
        },
        RawResult::Hover(Some(hover)) => {
            let range = match hover.range.map(|range| snapshot.range_from_utf16(range)) {
                None => None,
                Some(Ok(range)) => Some(range),
                Some(Err(error)) => {
                    return LspEvent::RequestFailed {
                        request: request.id,
                        doc: request.doc,
                        error,
                    };
                }
            };
            LspEvent::Hover {
                request: request.id,
                doc: request.doc,
                markdown: hover_markdown(&hover.contents),
                range,
            }
        }
        RawResult::Definition(response) => {
            let locations = match response {
                None => Vec::new(),
                Some(lsp_types::GotoDefinitionResponse::Scalar(location)) => vec![location],
                Some(lsp_types::GotoDefinitionResponse::Array(locations)) => locations,
                Some(lsp_types::GotoDefinitionResponse::Link(links)) => links
                    .into_iter()
                    .map(|link| lsp_types::Location {
                        uri: link.target_uri,
                        range: link.target_selection_range,
                    })
                    .collect(),
            };
            LspEvent::Locations {
                request: request.id,
                doc: request.doc,
                kind: LocationKind::Definition,
                items: convert_locations(&snapshot, request, &locations),
            }
        }
        RawResult::References(locations) => LspEvent::Locations {
            request: request.id,
            doc: request.doc,
            kind: LocationKind::References,
            items: convert_locations(&snapshot, request, &locations.unwrap_or_default()),
        },
    }
}

fn hover_markdown(contents: &lsp_types::HoverContents) -> Option<String> {
    fn marked(value: &lsp_types::MarkedString) -> String {
        match value {
            lsp_types::MarkedString::String(text) => text.clone(),
            lsp_types::MarkedString::LanguageString(block) => {
                format!("```{}\n{}\n```", block.language, block.value)
            }
        }
    }
    let text = match contents {
        lsp_types::HoverContents::Scalar(value) => marked(value),
        lsp_types::HoverContents::Array(values) => values
            .iter()
            .map(marked)
            .collect::<Vec<String>>()
            .join("\n\n"),
        lsp_types::HoverContents::Markup(markup) => markup.value.clone(),
    };
    (!text.trim().is_empty()).then_some(text)
}

fn convert_locations(
    snapshot: &Snapshot,
    request: &PendingRequest,
    locations: &[lsp_types::Location],
) -> Vec<LspLocation> {
    locations
        .iter()
        .filter_map(|location| {
            // Only the requesting document's text is known here, so only its
            // ranges can be converted to editor coordinates.
            let (range, coordinates) = if location.uri == *request.uri {
                (
                    snapshot.range_from_utf16(location.range).ok()?,
                    LspCoordinates::Scalar,
                )
            } else {
                (
                    LspRange::new(
                        LspPosition::new(location.range.start.line, location.range.start.character),
                        LspPosition::new(location.range.end.line, location.range.end.character),
                    ),
                    LspCoordinates::Utf16,
                )
            };
            let target = match location.uri.to_file_path() {
                // A path that cannot be *proved* to resolve inside the
                // worktree opens read-only: guessing wrong here would offer a
                // writable buffer over a file outside the user's worktree.
                Ok(path) if contains_path_provably(&request.worktree, &path) => {
                    LspTarget::Workspace { path }
                }
                Ok(path) => LspTarget::External { path },
                Err(()) => LspTarget::Uri {
                    uri: location.uri.to_string(),
                },
            };
            Some(LspLocation {
                target,
                range,
                coordinates,
            })
        })
        .collect()
}

fn completion_kind(kind: lsp_types::CompletionItemKind) -> Option<LspCompletionKind> {
    Some(match kind {
        lsp_types::CompletionItemKind::TEXT => LspCompletionKind::Text,
        lsp_types::CompletionItemKind::METHOD => LspCompletionKind::Method,
        lsp_types::CompletionItemKind::FUNCTION => LspCompletionKind::Function,
        lsp_types::CompletionItemKind::CONSTRUCTOR => LspCompletionKind::Constructor,
        lsp_types::CompletionItemKind::FIELD => LspCompletionKind::Field,
        lsp_types::CompletionItemKind::VARIABLE => LspCompletionKind::Variable,
        lsp_types::CompletionItemKind::CLASS => LspCompletionKind::Class,
        lsp_types::CompletionItemKind::INTERFACE => LspCompletionKind::Interface,
        lsp_types::CompletionItemKind::MODULE => LspCompletionKind::Module,
        lsp_types::CompletionItemKind::PROPERTY => LspCompletionKind::Property,
        lsp_types::CompletionItemKind::UNIT => LspCompletionKind::Unit,
        lsp_types::CompletionItemKind::VALUE => LspCompletionKind::Value,
        lsp_types::CompletionItemKind::ENUM => LspCompletionKind::Enum,
        lsp_types::CompletionItemKind::KEYWORD => LspCompletionKind::Keyword,
        lsp_types::CompletionItemKind::SNIPPET => LspCompletionKind::Snippet,
        lsp_types::CompletionItemKind::COLOR => LspCompletionKind::Color,
        lsp_types::CompletionItemKind::FILE => LspCompletionKind::File,
        lsp_types::CompletionItemKind::REFERENCE => LspCompletionKind::Reference,
        lsp_types::CompletionItemKind::FOLDER => LspCompletionKind::Folder,
        lsp_types::CompletionItemKind::ENUM_MEMBER => LspCompletionKind::EnumMember,
        lsp_types::CompletionItemKind::CONSTANT => LspCompletionKind::Constant,
        lsp_types::CompletionItemKind::STRUCT => LspCompletionKind::Struct,
        lsp_types::CompletionItemKind::EVENT => LspCompletionKind::Event,
        lsp_types::CompletionItemKind::OPERATOR => LspCompletionKind::Operator,
        lsp_types::CompletionItemKind::TYPE_PARAMETER => LspCompletionKind::TypeParameter,
        _ => return None,
    })
}

/// Converts one completion item, or reports why it cannot be offered.
///
/// Unlike a diagnostic batch — where dropping one unconvertible range still
/// leaves the other real problems visible — a completion is applied as a
/// whole. An item whose primary edit or *any* of whose `additionalTextEdits`
/// cannot be converted is therefore refused outright: applying the
/// convertible subset would silently corrupt the buffer (a missing import
/// edit, an inserted call without its argument list).
fn convert_completion(
    snapshot: &Snapshot,
    at: lsp_types::Position,
    item: &lsp_types::CompletionItem,
) -> Result<LspCompletionItem, LspError> {
    let is_snippet = item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET);
    // Application order is fixed by the protocol: `textEdit` wins over
    // `insertText`, which wins over `label`.
    let edit = match &item.text_edit {
        Some(lsp_types::CompletionTextEdit::Edit(text_edit)) => LspCompletionEdit {
            range: snapshot.range_from_utf16(text_edit.range)?,
            replace_range: None,
            new_text: text_edit.new_text.clone(),
            snippet: None,
        },
        Some(lsp_types::CompletionTextEdit::InsertAndReplace(both)) => LspCompletionEdit {
            range: snapshot.range_from_utf16(both.insert)?,
            replace_range: Some(snapshot.range_from_utf16(both.replace)?),
            new_text: both.new_text.clone(),
            snippet: None,
        },
        None => {
            let position = snapshot.from_utf16(at)?;
            LspCompletionEdit {
                range: snapshot.identifier_prefix(position),
                replace_range: None,
                new_text: item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone()),
                snippet: None,
            }
        }
    };
    let mut additional_edits = Vec::new();
    for text_edit in item.additional_text_edits.iter().flatten() {
        additional_edits.push(LspCompletionEdit {
            range: snapshot.range_from_utf16(text_edit.range)?,
            replace_range: None,
            new_text: text_edit.new_text.clone(),
            snippet: None,
        });
    }
    Ok(LspCompletionItem {
        label: item.label.clone(),
        kind: item.kind.and_then(completion_kind),
        detail: item.detail.clone(),
        documentation: item
            .documentation
            .as_ref()
            .map(|documentation| match documentation {
                lsp_types::Documentation::String(text) => text.clone(),
                lsp_types::Documentation::MarkupContent(markup) => markup.value.clone(),
            }),
        sort_text: item.sort_text.clone(),
        filter_text: item.filter_text.clone(),
        preselect: item.preselect.unwrap_or(false),
        deprecated: item.deprecated.unwrap_or(false)
            || item
                .tags
                .iter()
                .flatten()
                .any(|tag| *tag == lsp_types::CompletionItemTag::DEPRECATED),
        edit: expand_edit(edit, is_snippet),
        additional_edits,
    })
}

/// Expands snippet syntax so `new_text` is always literal text a caller can
/// insert verbatim, with tab stops offered alongside.
fn expand_edit(mut edit: LspCompletionEdit, is_snippet: bool) -> LspCompletionEdit {
    if !is_snippet {
        return edit;
    }
    let snippet = parse_snippet(&edit.new_text);
    edit.new_text = snippet.text.clone();
    edit.snippet = Some(snippet);
    edit
}

/// Parses the LSP snippet grammar subset servers actually emit: `$n`, `${n}`,
/// `${n:default}` with nesting, `${n|a,b|}` choices, `${VAR}` and
/// `${VAR:default}` variables, and `\$` / `\}` / `\\` escapes. Anything
/// unterminated stays literal.
fn parse_snippet(source: &str) -> LspSnippet {
    let mut text = String::with_capacity(source.len());
    let mut stops: Vec<LspSnippetStop> = Vec::new();
    expand_snippet(source, &mut text, &mut stops);
    // Visit order is ascending index with the final `$0` last.
    stops.sort_by_key(|stop| {
        if stop.index == 0 {
            u64::MAX
        } else {
            u64::from(stop.index)
        }
    });
    LspSnippet { text, stops }
}

fn expand_snippet(source: &str, text: &mut String, stops: &mut Vec<LspSnippetStop>) {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() && matches!(bytes[index + 1], b'$' | b'}' | b'\\') => {
                text.push(char::from(bytes[index + 1]));
                index += 2;
            }
            b'$' if index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() => {
                let mut end = index + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let number: u32 = source[index + 1..end].parse().unwrap_or(0);
                let start = text.len();
                stops.push(LspSnippetStop {
                    index: number,
                    range: start..start,
                });
                index = end;
            }
            b'$' if index + 1 < bytes.len() && bytes[index + 1] == b'{' => {
                match snippet_placeholder_end(bytes, index + 1) {
                    Some(close) => {
                        expand_placeholder(&source[index + 2..close], text, stops);
                        index = close + 1;
                    }
                    None => {
                        text.push('$');
                        index += 1;
                    }
                }
            }
            _ => {
                let character = source[index..]
                    .chars()
                    .next()
                    .expect("index is on a character boundary");
                text.push(character);
                index += character.len_utf8();
            }
        }
    }
}

/// Index of the `}` matching the `{` at `open`, honouring nesting and escapes.
fn snippet_placeholder_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Offset and kind of the first unescaped `:` or `|` in a placeholder body.
/// Escaped delimiters belong to the text, not to the grammar.
fn snippet_delimiter(body: &str) -> Option<(usize, char)> {
    let mut escaped = false;
    for (offset, character) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            ':' | '|' => return Some((offset, character)),
            _ => {}
        }
    }
    None
}

/// The first element of a choice list, split on the first *unescaped* comma.
///
/// `\,` and `\|` are unescaped here because they are only special inside a
/// choice list; every other escape is left for [`expand_snippet`].
fn first_snippet_choice(choices: &str) -> String {
    let mut first = String::with_capacity(choices.len());
    let mut characters = choices.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.peek() {
                Some(',' | '|') => {
                    first.push(characters.next().expect("peeked"));
                }
                _ => first.push('\\'),
            },
            // An unescaped comma ends the element and an unescaped bar ends
            // the list.
            ',' | '|' => break,
            _ => first.push(character),
        }
    }
    first
}

fn expand_placeholder(body: &str, text: &mut String, stops: &mut Vec<LspSnippetStop>) {
    let (head, tail) = match snippet_delimiter(body) {
        Some((offset, delimiter)) => (&body[..offset], Some((delimiter, &body[offset + 1..]))),
        None => (body, None),
    };
    let numbered: Option<u32> = head.parse().ok();
    let start = text.len();
    match tail {
        // `${1:default}` and `${VAR:default}`.
        Some((':', default)) => expand_snippet(default, text, stops),
        // `${1|a,b|}` renders its first choice.
        Some(('|', choices)) => {
            expand_snippet(&first_snippet_choice(choices), text, stops);
        }
        // A bare `${VAR}` with no known value contributes nothing.
        _ => {}
    }
    if let Some(index) = numbered {
        stops.push(LspSnippetStop {
            index,
            range: start..text.len(),
        });
    }
}

// ---------------------------------------------------------------------------
// Root discovery
// ---------------------------------------------------------------------------

/// Canonical containment; an unresolvable path is never treated as inside.
/// Used where being wrong means offering a writable editor for a file outside
/// the worktree.
fn contains_path_provably(worktree: &Path, candidate: &Path) -> bool {
    match (worktree.canonicalize(), candidate.canonicalize()) {
        (Ok(root), Ok(inside)) => inside.starts_with(&root),
        _ => false,
    }
}

#[cfg(test)]
use discovery::{discover_root, workspace_manifest};

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// A session that has been told to stop, kept until its process is reaped.
struct RetiringSession {
    task: tokio::task::JoinHandle<()>,
    process_group: Arc<PublishedGroup>,
}

#[derive(Default)]
struct SessionActivity {
    documents: HashSet<LspDocumentId>,
    // Resource refreshes also need a routing identity after the final tab closes.
    // The application refreshes global metrics before resolving that tab.
    last_document: Option<LspDocumentId>,
    // Monotonic ticks relative to the manager's epoch, convenient for fake-clock tests.
    last_used: Duration,
}

impl SessionActivity {
    fn idle_expired(&self, now: Duration, timeout: Duration) -> bool {
        self.documents.is_empty() && now.saturating_sub(self.last_used) >= timeout
    }
}

#[derive(Clone)]
struct SessionHandle {
    worktree: Arc<Path>,
    activity: Arc<Mutex<SessionActivity>>,
    resident_bytes: Arc<AtomicU64>,
    commands: commands::Sender,
    status: Arc<Mutex<LspStatus>>,
    completion_triggers: Arc<Mutex<Option<Vec<String>>>>,
    /// Shared with the session's router: set before the handle is dropped so
    /// a retiring server cannot publish into its replacement's documents.
    retired: Arc<AtomicBool>,
    retirement: Arc<tokio::sync::Notify>,
    /// Shared with the session task; see [`SessionContext::process_group`].
    process_group: Arc<PublishedGroup>,
    /// The session task, so teardown can be awaited to completion.
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SessionHandle {
    fn status(&self) -> LspStatus {
        if self.retired.load(Ordering::Acquire) {
            return LspStatus::Disabled;
        }
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Stops using this session. The task keeps running until it has stopped
    /// its process; [`LspManager::wait_for_shutdown`] awaits that.
    fn request_retirement(&self) {
        self.retired.store(true, Ordering::Release);
        self.retirement.notify_one();
        let _ = self.commands.send(SessionCommand::Shutdown);
    }

    fn retire(&self) -> Option<RetiringSession> {
        self.request_retirement();
        self.task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .map(|task| RetiringSession {
                task,
                process_group: Arc::clone(&self.process_group),
            })
    }
}

/// Requests idle/deleted-root shutdown while leaving the join handle in its
/// manager-owned slot. A concurrent application exit can still await it.
async fn retire_from_watchdog(handle: SessionHandle) {
    handle.request_retirement();
    let deadline = tokio::time::Instant::now() + SESSION_REAP_TIMEOUT;
    loop {
        {
            let task = handle
                .task
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(task) = task.as_ref() else {
                return;
            }; // manager now owns the reap
            if task.is_finished() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                handle.process_group.kill();
                task.abort();
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Awaits every retiring session, then kills whatever is left.
///
/// Each session bounds its own `shutdown`, `exit` and termination sequence,
/// but it can only run that sequence while something polls it, and it
/// publishes into a channel an exiting application has stopped draining. So
/// the wait is finite and its last resort is the one
/// [`ChildProcess::stop`] already uses: `SIGKILL` to the process group, which
/// reaches the server's own children — a `rust-analyzer` proc-macro server,
/// for one — where killing the direct child would not.
async fn reap_sessions(sessions: Vec<RetiringSession>) {
    let deadline = tokio::time::Instant::now() + SESSION_REAP_TIMEOUT;
    for RetiringSession {
        mut task,
        process_group,
    } in sessions
    {
        if tokio::time::timeout_at(deadline, &mut task).await.is_ok() {
            continue;
        }
        // Serialize the fallback's claim and signal with the child owner's
        // claim. The owner cannot reap while this signal is still pending.
        process_group.kill();
        task.abort();
    }
}

/// Pre-effect authority for one worktree removal. Reusing the same path or
/// document ID does not transfer this authority to a later registration.
#[derive(Clone, Debug)]
pub struct LspWorktreeRemoval {
    manager: u64,
    registrations: Vec<(LspDocumentId, Arc<RegistrationOwner>)>,
    sessions: Vec<CapturedSession>,
}

#[derive(Debug, Default)]
struct RegistrationOwner {
    // Discovery can create its first session after removal was captured. Only
    // a session actually created by this registration belongs to that capture;
    // joining an already existing successor does not grant authority over it.
    created_session: Mutex<Option<CapturedSession>>,
}

#[derive(Clone, Debug)]
struct CapturedSession {
    key: SessionKey,
    retired: Arc<AtomicBool>,
}

struct PendingOpen {
    path: PathBuf,
    worktree: PathBuf,
    language: LspLanguage,
    version: i32,
    text: Arc<Snapshot>,
    saved_text: Option<String>,
    unavailable_reason: Option<String>,
    ticket: u64,
    owner: Arc<RegistrationOwner>,
    _discovery: Option<DiscoveryTask>,
}

/// Cancels both admission waiters and cooperative work when a registration ends.
struct DiscoveryTask {
    cancelled: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

impl Drop for DiscoveryTask {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.task.abort();
    }
}

#[derive(Clone)]
struct DocumentRef {
    session: SessionKey,
    uri: Arc<Url>,
    path: PathBuf,
    worktree: Arc<Path>,
    language: LspLanguage,
    owner: Arc<RegistrationOwner>,
}

/// Hands every manager its own event-channel identity.
///
/// A configuration change replaces the manager, and the replacement carries a
/// fresh channel that the old one's subscribers can never see. Deriving the
/// identity from a counter here rather than from a field the application
/// maintains means no caller can install a new manager and forget to
/// distinguish it from the one it retired.
static NEXT_MANAGER_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Shares an outgoing manager's bounded reap without letting cancellation of
/// any one new session cancel that reap or release other sessions early.
struct LaunchBarrier {
    predecessor: Mutex<Option<std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>,
    ready: AtomicBool,
    changed: Notify,
}
impl LaunchBarrier {
    async fn wait(self: Arc<Self>) {
        let predecessor = {
            self.predecessor
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        };
        if let Some(predecessor) = predecessor {
            let barrier = Arc::clone(&self);
            tokio::spawn(async move {
                predecessor.await;
                barrier.ready.store(true, Ordering::Release);
                barrier.changed.notify_waiters();
            });
        }
        loop {
            let changed = self.changed.notified();
            if self.ready.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}

/// The application's single LSP client.
pub struct LspManager {
    launch_barrier: Option<Arc<LaunchBarrier>>,
    servers: Vec<LanguageServerSettings>,
    /// Lowercase extension to the first server and language that claims it.
    extensions: HashMap<String, LspLanguage>,
    events: EventSink,
    receiver: async_channel::Receiver<LspEvent>,
    sessions: HashMap<SessionKey, SessionHandle>,
    documents: HashMap<LspDocumentId, DocumentRef>,
    pending_opens: HashMap<LspDocumentId, PendingOpen>,
    next_discovery: u64,
    discovery_slots: Arc<tokio::sync::Semaphore>,
    discovery_admissions: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    discovery_enter: Option<Arc<dyn Fn() + Send + Sync>>,
    idle_timeout: Duration,
    session_cap: usize,
    process_slots: Arc<tokio::sync::Semaphore>,
    epoch: std::time::Instant,
    snapshots: Arc<SnapshotStore>,
    transport: TransportFactory,
    /// Sessions told to stop, awaited by [`LspManager::wait_for_shutdown`] or
    /// [`LspManager::begin_shutdown`] so no server outlives the manager
    /// unreaped.
    retiring: Vec<RetiringSession>,
    flusher: Option<tokio::task::JoinHandle<()>>,
    next_request: u64,
    /// Identity of this manager's event channel; see
    /// [`NEXT_MANAGER_GENERATION`].
    generation: u64,
    /// Whether [`LspManager::shutdown`] has been called.
    ///
    /// A shutdown is one-way. A manager an application reconfigures is not
    /// restarted but replaced, and a manager an application exits with is
    /// reaped exactly once - so a session started after that reap has nobody
    /// left to stop it. See [`LspManager::ensure_session`].
    stopping: bool,
}

impl LspManager {
    /// `servers` is the validated
    /// [`crate::settings::AppSettings::language_servers`] list. An empty list
    /// disables LSP entirely. No executable is looked up here: nothing is
    /// spawned until a document is opened.
    #[must_use]
    pub fn new(servers: Vec<LanguageServerSettings>) -> Self {
        let mut extensions: HashMap<String, LspLanguage> = HashMap::new();
        for server in &servers {
            for language in &server.languages {
                for extension in &language.extensions {
                    // Declaration order decides: the first claim wins.
                    extensions
                        .entry(extension.to_ascii_lowercase())
                        .or_insert_with(|| LspLanguage {
                            server_id: server.id.clone(),
                            language_id: language.language_id.clone(),
                        });
                }
            }
        }
        let (tx, rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
        Self {
            servers,
            extensions,
            events: EventSink {
                tx,
                pending: Arc::new(PendingEvents::default()),
            },
            receiver: rx,
            sessions: HashMap::new(),
            documents: HashMap::new(),
            pending_opens: HashMap::new(),
            next_discovery: 1,
            discovery_slots: discovery::worker_slots(),
            discovery_admissions: discovery::admission_slots(),
            #[cfg(test)]
            discovery_enter: None,
            idle_timeout: Duration::from_secs(600),
            session_cap: 8,
            process_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            epoch: std::time::Instant::now(),
            snapshots: Arc::new(SnapshotStore::default()),
            transport: Arc::new(spawn_process_transport),
            retiring: Vec::new(),
            flusher: None,
            launch_barrier: None,
            next_request: 1,
            generation: NEXT_MANAGER_GENERATION.fetch_add(1, Ordering::Relaxed),
            stopping: false,
        }
    }

    /// Channel the application subscribes to for server-pushed events. Clones
    /// share one queue, so exactly one consumer should drain it.
    #[must_use]
    pub fn events(&self) -> async_channel::Receiver<LspEvent> {
        self.receiver.clone()
    }

    /// Validate status/progress immediately before applying it to a surface.
    /// A queued value can outlive its server or the document binding it named.
    #[must_use]
    pub fn event_is_current(&self, event: &LspEvent) -> bool {
        self.events.accepts(event)
    }

    /// Identity of the channel [`Self::events`] returns.
    ///
    /// Distinct for every manager ever built, so a subscription keyed on it is
    /// rebuilt exactly when the channel behind it is replaced.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The server list this manager was built with.
    ///
    /// A manager's server set is fixed at construction and a shutdown is
    /// one-way, so this is what a caller compares against to decide whether the
    /// configured list still describes the process it is talking to.
    #[must_use]
    pub fn servers(&self) -> &[LanguageServerSettings] {
        &self.servers
    }

    /// `None` when no configured server claims the extension.
    #[must_use]
    pub fn language_for(&self, path: &Path) -> Option<LspLanguage> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        self.extensions.get(&extension).cloned()
    }

    /// Registers a document and starts or joins its session. Returns the
    /// session's current status; later transitions arrive as
    /// [`LspEvent::Status`].
    pub fn open(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        text: &str,
    ) -> LspStatus {
        self.open_source(
            doc,
            path,
            worktree,
            language,
            version,
            Arc::new(Snapshot::new(version, Arc::from(text))),
        )
    }

    /// Registers an immutable editor source without serializing it on the UI
    /// thread. The worker materializes it only when didOpen is sent.
    pub fn open_snapshot(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        source: TextSnapshot,
    ) -> LspStatus {
        self.open_source(
            doc,
            path,
            worktree,
            language,
            version,
            Arc::new(Snapshot::from_native(version, source)),
        )
    }

    fn open_source(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        source: Arc<Snapshot>,
    ) -> LspStatus {
        if self.servers.is_empty() {
            return LspStatus::Disabled;
        }
        if self.stopping {
            return self.unavailable(
                doc,
                "the editor is shutting its language servers down".into(),
            );
        }
        let Some(server) = self
            .servers
            .iter()
            .find(|server| server.id == language.server_id)
            .cloned()
        else {
            return self.unavailable(doc, "no configured server claims this language".into());
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return self.unavailable(
                doc,
                "no async runtime is available for language servers".into(),
            );
        };
        self.close(doc);
        let ticket = self.next_discovery;
        self.next_discovery += 1;
        // Bound queued work before creating a task; process slots separately
        // bound blocking workers across manager replacement and cancellation.
        let Ok(admission) = Arc::clone(&self.discovery_admissions).try_acquire_owned() else {
            self.pending_opens.insert(
                doc,
                PendingOpen {
                    path: path.to_owned(),
                    worktree: worktree.to_owned(),
                    language,
                    version,
                    text: source,
                    saved_text: None,
                    unavailable_reason: Some(discovery::BUSY_REASON.into()),
                    ticket,
                    owner: Arc::default(),
                    _discovery: None,
                },
            );
            return self.unavailable(doc, discovery::BUSY_REASON.into());
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_path = path.to_owned();
        let worker_worktree = worktree.to_owned();
        let markers = server.root_markers.clone();
        let events = self.events.clone();
        let slots = Arc::clone(&self.discovery_slots);
        #[cfg(test)]
        let discovery_enter = self.discovery_enter.clone();
        let task = runtime.spawn(async move {
            let _admission = admission;
            let Ok(permit) = slots.acquire_owned().await else {
                return;
            };
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }
            let active_cancelled = Arc::clone(&worker_cancelled);
            let root = tokio::task::spawn_blocking(move || {
                // A cancelled async waiter must not release an active worker's
                // permit while its filesystem operation is still returning.
                let _permit = permit;
                #[cfg(test)]
                if let Some(enter) = discovery_enter {
                    enter();
                }
                discovery::discover(&worker_path, &worker_worktree, &markers, &active_cancelled)
            })
            .await;
            if !worker_cancelled.load(Ordering::Acquire) {
                if let Ok(Some(root)) = root {
                    events
                        .emit(LspEvent::RootDiscovered { doc, ticket, root })
                        .await;
                }
            }
        });
        self.pending_opens.insert(
            doc,
            PendingOpen {
                path: path.to_owned(),
                worktree: worktree.to_owned(),
                language,
                version,
                text: source,
                saved_text: None,
                unavailable_reason: None,
                ticket,
                owner: Arc::default(),
                _discovery: Some(DiscoveryTask {
                    cancelled,
                    task: task.abort_handle(),
                }),
            },
        );
        LspStatus::Starting
    }

    /// Return the initial status to the caller synchronously. Publishing it
    /// into the event queue could reorder it after a newer worker status.
    pub fn finish_discovery(
        &mut self,
        doc: LspDocumentId,
        ticket: u64,
        root: PathBuf,
    ) -> Option<LspStatus> {
        if self
            .pending_opens
            .get(&doc)
            .is_none_or(|pending| pending.ticket != ticket)
        {
            return None;
        }
        let pending = self.pending_opens.remove(&doc).expect("matched discovery");
        let status = self.open_source_at_root(
            doc,
            &pending.path,
            &pending.worktree,
            pending.language,
            pending.version,
            pending.text,
            root,
            pending.owner,
        );
        if let Some(saved_text) = pending.saved_text {
            self.save(doc, &saved_text);
        }
        Some(status)
    }

    #[cfg(test)]
    fn open_at_root(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        text: &str,
        root: PathBuf,
    ) -> LspStatus {
        self.open_source_at_root(
            doc,
            path,
            worktree,
            language,
            version,
            Arc::new(Snapshot::new(version, Arc::from(text))),
            root,
            Arc::default(),
        )
    }

    fn open_source_at_root(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        source: Arc<Snapshot>,
        root: PathBuf,
        owner: Arc<RegistrationOwner>,
    ) -> LspStatus {
        if self.servers.is_empty() {
            return LspStatus::Disabled;
        }
        let Some(server) = self
            .servers
            .iter()
            .find(|server| server.id == language.server_id)
            .cloned()
        else {
            return self.unavailable(
                doc,
                format!(
                    "no configured language server has the id `{}`",
                    language.server_id
                ),
            );
        };
        // A document URI is a string, so a filename this process cannot render
        // as UTF-8 cannot be addressed by a language server. LSP is declined
        // for it rather than sending something a server would mangle.
        if path.to_str().is_none() {
            return self.unavailable(
                doc,
                LspError::NonUtf8Path(path.display().to_string()).to_string(),
            );
        }
        let Ok(uri) = Url::from_file_path(path) else {
            return self.unavailable(
                doc,
                LspError::UnrepresentablePath(path.display().to_string()).to_string(),
            );
        };

        // Discovery establishes project membership. An ancestor's live server
        // is not authority to absorb an independent nested project.
        let key = SessionKey {
            server_id: Arc::from(server.id.as_str()),
            root: Arc::from(root.clone()),
        };
        // URI ownership is singular even when discovery legitimately selects
        // a different bounded workspace for a second editor of the same file.
        // Refuse before starting a backend or changing the original stream.
        if self.snapshots.bound_to_other_session(&uri, &key) {
            let reason = "This file is already open under another language-server workspace. Close its original editor before retrying.".to_owned();
            // Keep this registration independently retryable without joining
            // the original URI stream. A new ticket also rejects any queued
            // duplicate of the discovery that just refused admission.
            let ticket = self.next_discovery;
            self.next_discovery += 1;
            self.pending_opens.insert(
                doc,
                PendingOpen {
                    path: path.to_owned(),
                    worktree: worktree.to_owned(),
                    language,
                    version,
                    text: source,
                    saved_text: None,
                    unavailable_reason: Some(reason.clone()),
                    ticket,
                    owner,
                    _discovery: None,
                },
            );
            return self.unavailable(doc, reason);
        }
        // Keep a refused document registered so Restart can retry admission
        // after another workspace becomes idle, with its latest buffer text.
        // Ensure first: reopening previously blocked documents must not also
        // reopen this new registration.
        let prior_session = self
            .sessions
            .get(&key)
            .map(|handle| Arc::clone(&handle.retired));
        let handle = self.ensure_session(&key, &server, &root, worktree);
        if let Ok(handle) = &handle {
            if prior_session
                .as_ref()
                .is_none_or(|prior| !Arc::ptr_eq(prior, &handle.retired))
            {
                *owner
                    .created_session
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(CapturedSession {
                    key: key.clone(),
                    retired: Arc::clone(&handle.retired),
                });
            }
        }

        let uri = Arc::new(uri);
        let worktree: Arc<Path> = Arc::from(worktree.to_path_buf());
        let Some(snapshot) = self
            .snapshots
            .register_source(doc, &uri, &key, version, source)
        else {
            return self.unavailable(doc, LspError::VersionExhausted.to_string());
        };
        self.documents.insert(
            doc,
            DocumentRef {
                session: key,
                uri: Arc::clone(&uri),
                path: path.to_path_buf(),
                worktree,
                language: language.clone(),
                owner,
            },
        );
        let handle = match handle {
            Ok(handle) => handle,
            Err(reason) => return self.unavailable(doc, reason),
        };
        self.events.bind(doc, Some(Arc::clone(&handle.retired)));
        // Sent unconditionally: the session decides whether this is the URI's
        // first `didOpen` or a full resynchronisation of an already open file.
        let _ = handle.commands.send(SessionCommand::Open {
            doc,
            uri: Arc::clone(&uri),
            language_id: language.language_id.clone(),
            version: snapshot.version,
            text: Arc::clone(&snapshot),
        });
        {
            let mut activity = handle
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            activity.documents.insert(doc);
            activity.last_document = Some(doc);
            activity.last_used = self.epoch.elapsed();
        }
        handle.status()
    }

    /// Records a new document version. `edits` describe the transition from
    /// the previous version and `full_text` is the authoritative result;
    /// whether either reaches the server depends on the negotiated sync kind.
    pub fn change(
        &mut self,
        doc: LspDocumentId,
        version: i32,
        edits: &[LspTextEdit],
        full_text: &str,
    ) {
        if let Some(pending) = self.pending_opens.get_mut(&doc) {
            pending.version = version;
            pending.text = Arc::new(Snapshot::new(version, Arc::from(full_text)));
            return;
        }
        let Some(reference) = self.documents.get(&doc).cloned() else {
            self.events.emit_now(LspEvent::RequestFailed {
                request: LspRequestId(0),
                doc,
                error: LspError::UnknownDocument(doc.0),
            });
            return;
        };
        let (previous, snapshot) =
            match self
                .snapshots
                .record(&reference.uri, version, Arc::from(full_text))
            {
                RecordOutcome::Recorded { previous, snapshot } => (previous, snapshot),
                RecordOutcome::UnknownUri => return,
                RecordOutcome::VersionExhausted => {
                    self.events.emit_now(LspEvent::DocumentDesynced {
                        doc,
                        version: i32::MAX,
                        error: LspError::VersionExhausted,
                    });
                    return;
                }
            };
        let mut incremental = None;
        if !edits.is_empty() {
            match build_incremental(&previous, edits, snapshot.full_text()) {
                Ok(changes) => incremental = Some(changes),
                Err(error) => {
                    // The buffer is authoritative and already recorded; the
                    // server is resynchronised with the whole document.
                    self.events.emit_now(LspEvent::DocumentDesynced {
                        doc,
                        version: snapshot.version,
                        error,
                    });
                }
            }
        }
        if let Some(session) = self.sessions.get(&reference.session) {
            let _ = session.commands.send(SessionCommand::Change {
                uri: Arc::clone(&reference.uri),
                version: snapshot.version,
                incremental,
                full: Arc::clone(&snapshot),
            });
        }
    }

    /// Records a transition produced by the native recorded-edit layer.
    /// Its immutable source remains lazy for incremental synchronization.
    pub fn change_snapshot(
        &mut self,
        doc: LspDocumentId,
        version: i32,
        edits: &[LspTextEdit],
        source: TextSnapshot,
    ) {
        if let Some(pending) = self.pending_opens.get_mut(&doc) {
            pending.version = version;
            pending.text = Arc::new(Snapshot::from_native(version, source));
            return;
        }
        let Some(reference) = self.documents.get(&doc).cloned() else {
            return;
        };
        let (previous, snapshot) =
            match self
                .snapshots
                .record_native(&reference.uri, version, source)
            {
                RecordOutcome::Recorded { previous, snapshot } => (previous, snapshot),
                RecordOutcome::UnknownUri => return,
                RecordOutcome::VersionExhausted => {
                    self.events.emit_now(LspEvent::DocumentDesynced {
                        doc,
                        version: i32::MAX,
                        error: LspError::VersionExhausted,
                    });
                    return;
                }
            };
        let incremental = if previous.compatible_native().is_some() && !edits.is_empty() {
            snapshot.compatible_native().and_then(|expected| {
                let changes = native_incremental(&previous, edits, expected);
                if changes.is_none() {
                    self.events.emit_now(LspEvent::DocumentDesynced {
                        doc,
                        version: snapshot.version,
                        error: LspError::InconsistentEdits,
                    });
                }
                changes
            })
        } else {
            None
        };
        if let Some(session) = self.sessions.get(&reference.session) {
            let _ = session.commands.send(SessionCommand::Change {
                uri: Arc::clone(&reference.uri),
                version: snapshot.version,
                incremental,
                full: snapshot,
            });
        }
    }

    /// Reports that the document was written to disk.
    pub fn save(&mut self, doc: LspDocumentId, text: &str) {
        if let Some(pending) = self.pending_opens.get_mut(&doc) {
            pending.saved_text = Some(text.to_owned());
            return;
        }
        let Some(reference) = self.documents.get(&doc) else {
            return;
        };
        if let Some(session) = self.sessions.get(&reference.session) {
            let _ = session.commands.send(SessionCommand::Save {
                uri: Arc::clone(&reference.uri),
                text: Arc::from(text),
            });
        }
    }

    /// Unregisters the document and arms a bounded warm-cache period after
    /// its final tab closes.
    pub fn close(&mut self, doc: LspDocumentId) {
        self.pending_opens.remove(&doc);
        // Queued discoveries can also have staged events.
        self.events.forget(doc);
        let Some(reference) = self.documents.remove(&doc) else {
            return;
        };
        self.snapshots.unregister(doc);
        if let Some(session) = self.sessions.get(&reference.session) {
            let _ = session.commands.send(SessionCommand::Close {
                doc,
                uri: reference.uri,
            });
            let last_used = self.epoch.elapsed();
            let mut activity = session
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            activity.documents.remove(&doc);
            activity.last_used = last_used;
        }
    }

    /// Limits apply to future admissions; busy sessions are never evicted.
    #[must_use]
    pub fn with_lifecycle(mut self, idle_seconds: u64, session_cap: usize) -> Self {
        self.idle_timeout = Duration::from_secs(idle_seconds.clamp(1, 604800));
        self.session_cap = session_cap.clamp(1, 64);
        self.process_slots = Arc::new(tokio::sync::Semaphore::new(self.session_cap));
        self
    }

    /// Delays new processes until a replaced manager has reaped its servers.
    /// Configuration changes remain asynchronous while preserving the cap
    /// across the old and new managers' independent admission semaphores.
    #[must_use]
    pub fn after_shutdown(
        mut self,
        predecessor: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        self.launch_barrier = Some(Arc::new(LaunchBarrier {
            predecessor: Mutex::new(Some(Box::pin(predecessor))),
            ready: AtomicBool::new(false),
            changed: Notify::new(),
        }));
        self
    }

    /// Cached server RSS; reading this performs no filesystem I/O.
    pub fn live_servers(&self) -> Vec<String> {
        let mut servers: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, handle)| {
                !handle.retired.load(Ordering::Acquire) && !handle.commands.is_closed()
            })
            .map(|(key, handle)| {
                let rss = handle.resident_bytes.load(Ordering::Relaxed);
                let memory = if rss == 0 {
                    "RSS pending".to_owned()
                } else {
                    format!("{:.1} MiB RSS", rss as f64 / (1024.0 * 1024.0))
                };
                format!("{} · {} · {}", key.server_id, key.root.display(), memory)
            })
            .collect();
        servers.sort();
        servers
    }

    pub fn lifecycle(&self) -> (u64, usize) {
        (self.idle_timeout.as_secs(), self.session_cap)
    }

    /// Capture before starting removal, while the original registrations and
    /// sessions still own the target. Retain this token until that operation's
    /// known successful completion; dropping it has no effect.
    pub fn capture_worktree_removal(&self, worktree: &Path) -> LspWorktreeRemoval {
        let registrations = self
            .documents
            .iter()
            .filter(|(_, reference)| reference.worktree.as_ref() == worktree)
            .map(|(doc, reference)| (*doc, Arc::clone(&reference.owner)))
            .chain(
                self.pending_opens
                    .iter()
                    .filter(|(_, pending)| pending.worktree == worktree)
                    .map(|(doc, pending)| (*doc, Arc::clone(&pending.owner))),
            )
            .collect();
        let sessions = self
            .sessions
            .iter()
            .filter(|(_, handle)| handle.worktree.as_ref() == worktree)
            .map(|(key, handle)| CapturedSession {
                key: key.clone(),
                retired: Arc::clone(&handle.retired),
            })
            .collect();
        LspWorktreeRemoval {
            manager: self.generation,
            registrations,
            sessions,
        }
    }

    /// Retire only owners captured for the successful removal. An old result
    /// cannot close a replacement manager, registration, or same-key session.
    /// Surviving registrations that joined an original session are reopened
    /// with their latest immutable buffer after that old session retires.
    /// Returns the exact IDs closed, including still-pending registrations, so
    /// retained editor buffers can clear only their matching LSP attachment.
    pub fn remove_captured_worktree(&mut self, removal: LspWorktreeRemoval) -> Vec<LspDocumentId> {
        if removal.manager != self.generation {
            return Vec::new();
        }
        let mut closed = Vec::new();
        let mut sessions = removal.sessions;
        for (doc, owner) in removal.registrations {
            if let Some(created) = owner
                .created_session
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
            {
                sessions.push(created);
            }
            let matches = self
                .documents
                .get(&doc)
                .is_some_and(|reference| Arc::ptr_eq(&reference.owner, &owner))
                || self
                    .pending_opens
                    .get(&doc)
                    .is_some_and(|pending| Arc::ptr_eq(&pending.owner, &owner));
            if matches {
                self.close(doc);
                closed.push(doc);
            }
        }
        let mut reopen = Vec::new();
        for captured in sessions {
            if self
                .sessions
                .get(&captured.key)
                .is_some_and(|current| Arc::ptr_eq(&current.retired, &captured.retired))
            {
                if let Some(handle) = self.sessions.remove(&captured.key) {
                    self.retire_handle(handle);
                    reopen.push(captured.key);
                }
            }
        }
        for key in reopen {
            self.reopen_documents(&key);
        }
        closed
    }

    /// Immediate removal for callers that already hold current ownership.
    /// Asynchronous operations must capture before their effect and complete
    /// with `remove_captured_worktree`, rather than recapturing by path later.
    pub fn remove_worktree(&mut self, worktree: &Path) {
        let removal = self.capture_worktree_removal(worktree);
        let _ = self.remove_captured_worktree(removal);
    }

    pub fn request_completion(&mut self, doc: LspDocumentId, at: LspPosition) -> LspRequestId {
        self.request(doc, at, RequestKind::Completion)
    }

    /// Automatic completion starts only after the server advertises support.
    pub fn completion_trigger(&self, doc: LspDocumentId, character: char) -> bool {
        let Some(session) = self
            .documents
            .get(&doc)
            .and_then(|document| self.sessions.get(&document.session))
        else {
            return false;
        };
        let triggers = session
            .completion_triggers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        triggers.as_ref().is_some_and(|triggers| {
            character.is_alphanumeric()
                || character == '_'
                || triggers
                    .iter()
                    .any(|trigger| trigger.chars().eq(std::iter::once(character)))
        })
    }

    pub fn request_hover(&mut self, doc: LspDocumentId, at: LspPosition) -> LspRequestId {
        self.request(doc, at, RequestKind::Hover)
    }

    pub fn request_definition(&mut self, doc: LspDocumentId, at: LspPosition) -> LspRequestId {
        self.request(doc, at, RequestKind::Definition)
    }

    pub fn request_references(&mut self, doc: LspDocumentId, at: LspPosition) -> LspRequestId {
        self.request(doc, at, RequestKind::References)
    }

    /// Cancels an in-flight request, sending `$/cancelRequest` to the server
    /// that owns it.
    pub fn cancel(&mut self, request: LspRequestId) {
        for session in self.sessions.values() {
            let _ = session.commands.send(SessionCommand::Cancel { request });
        }
    }

    /// Current status of the session backing `doc`.
    #[must_use]
    pub fn status(&self, doc: LspDocumentId) -> LspStatus {
        if let Some(pending) = self.pending_opens.get(&doc) {
            return pending
                .unavailable_reason
                .as_ref()
                .map_or(LspStatus::Starting, |reason| LspStatus::Unavailable {
                    reason: reason.clone(),
                });
        }
        let Some(reference) = self.documents.get(&doc) else {
            return LspStatus::Disabled;
        };
        self.sessions
            .get(&reference.session)
            .map_or(LspStatus::Disabled, SessionHandle::status)
    }

    /// Version of `doc`'s current snapshot: the version every result this
    /// manager publishes for it is stamped with.
    ///
    /// A caller must read this rather than counting versions of its own. The
    /// manager bumps a document's version whenever it re-opens it - a restart,
    /// a crashed session respawning, a second document registering the same
    /// URI - and none of those are edits the caller made, so a private counter
    /// falls silently behind and starts rejecting live results as stale.
    #[must_use]
    pub fn document_version(&self, doc: LspDocumentId) -> Option<i32> {
        if let Some(pending) = self.pending_opens.get(&doc) {
            return Some(pending.version);
        }
        let reference = self.documents.get(&doc)?;
        Some(self.snapshots.latest(&reference.uri)?.version)
    }

    /// Converts a protocol UTF-16 position into the editor-native scalar
    /// position, against `doc`'s current snapshot.
    ///
    /// An [`LspLocation`] whose target file was not open at this boundary when
    /// the answer arrived carries [`LspCoordinates::Utf16`], and its column
    /// cannot be placed until that file is open. This is what places it once
    /// it is, using the same conversion every other position goes through, so
    /// a column landing inside a surrogate pair is refused here exactly as it
    /// is refused there rather than rounded to a caret the server never named.
    pub fn scalar_position(
        &self,
        doc: LspDocumentId,
        at: LspPosition,
    ) -> Result<LspPosition, LspError> {
        if let Some(pending) = self.pending_opens.get(&doc) {
            return pending
                .text
                .from_utf16(lsp_types::Position::new(at.line, at.character));
        }
        let reference = self
            .documents
            .get(&doc)
            .ok_or(LspError::UnknownDocument(doc.0))?;
        let snapshot = self
            .snapshots
            .latest(&reference.uri)
            .ok_or(LspError::UnknownDocument(doc.0))?;
        snapshot.from_utf16(lsp_types::Position::new(at.line, at.character))
    }

    /// Replaces the session backing `doc` with a fresh process and re-opens
    /// every document that shared it, keeping their current buffer text.
    ///
    /// The retiring process is told to shut down and terminates on its own
    /// schedule; it is marked retired first, so nothing it still publishes
    /// can reach the documents its replacement now owns.
    pub fn restart(&mut self, doc: LspDocumentId) {
        if let Some(pending) = self.pending_opens.remove(&doc) {
            // Admission refusal still owns a document and its newest immutable
            // source. Retry through the same asynchronous path as an open.
            drop(pending._discovery);
            self.open_source(
                doc,
                &pending.path,
                &pending.worktree,
                pending.language,
                pending.version,
                pending.text,
            );
            if let Some(text) = pending.saved_text {
                self.save(doc, &text);
            }
            return;
        }
        let Some(reference) = self.documents.get(&doc).cloned() else {
            return;
        };
        let key = reference.session;
        if let Some(previous) = self.sessions.remove(&key) {
            self.retire_handle(previous);
        }
        self.reopen_documents(&key);
    }

    /// Stops every session: `shutdown`, then `exit`, then process-group
    /// termination, each performed by its own session task.
    ///
    /// Returns immediately, because the COSMIC update thread must never
    /// block. [`LspManager::wait_for_shutdown`] awaits the processes actually
    /// being reaped, which an application exit path should do before it lets
    /// the runtime go away.
    ///
    /// One-way: nothing this manager is asked for afterwards may start a
    /// server, because the reap that answers for these ones has already been
    /// arranged. See [`LspManager::stopping`].
    pub fn shutdown(&mut self) {
        self.stopping = true;
        for session in self.sessions.values() {
            if let Some(retiring) = session.retire() {
                self.retiring.push(retiring);
            }
        }
        self.sessions.clear();
        self.documents.clear();
        self.pending_opens.clear();
        self.snapshots.clear();
        self.events.pending.closed.store(true, Ordering::Release);
        self.events.pending.notify.notify_one();
    }

    /// Kills the process group of every session this manager has retired,
    /// now, without awaiting anything.
    ///
    /// For an exit that cannot afford [`LspManager::wait_for_shutdown`]:
    /// either the runtime is about to go away without draining the session
    /// tasks, or the process is about to be replaced by `exit` and no
    /// destructor will run at all. Neither leaves anybody to notice that
    /// `shutdown` only *asked*, and a server that is never signalled again is
    /// exactly the orphan the reaper exists to prevent - `rust-analyzer`'s
    /// proc-macro server among them.
    ///
    /// `SIGKILL` rather than the graceful sequence because there is nothing
    /// left to wait with: [`ChildProcess::stop`]'s own last resort, taken
    /// straight away. The group id is *taken* rather than read, exactly as
    /// [`reap_sessions`] takes it, so a session task that is already stopping
    /// its child cannot be signalled twice and a reused pid cannot be
    /// signalled at all.
    pub fn kill_retired_now(&mut self) {
        self.collect_retired_sessions();
        for session in std::mem::take(&mut self.retiring) {
            session.process_group.kill();
            session.task.abort();
        }
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
    }

    /// Awaits the sessions this manager has retired, so every server it has
    /// stopped has been signalled, terminated and reaped before returning.
    ///
    /// A session that is still installed is left alone: it has never been sent
    /// `shutdown`, so waiting for it would be waiting for a server that has no
    /// reason to stop, and giving up on that wait would kill a healthy one.
    /// Call [`LspManager::shutdown`] first to wait for every session.
    ///
    /// Bounded by [`SESSION_REAP_TIMEOUT`]; see [`reap_sessions`].
    pub async fn wait_for_shutdown(&mut self) {
        if let Some(barrier) = self.launch_barrier.clone() {
            barrier.wait().await;
        }
        self.collect_retired_sessions();
        reap_sessions(std::mem::take(&mut self.retiring)).await;
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
    }

    /// Stops every session and hands back a future that awaits their
    /// teardown, owning everything that wait needs.
    ///
    /// An exit path has no `&mut self` left by the time it can await
    /// anything: the update thread returns a task and the manager stays
    /// behind. The wait is therefore spawned here, on the runtime the
    /// sessions themselves run on, so it survives the returned future being
    /// dropped and needs no runtime context of its own — and so the exit that
    /// awaits it is the last thing to happen, rather than a `Drop` that
    /// signals the servers and then takes the executor away from them.
    pub fn begin_shutdown(&mut self) -> impl Future<Output = ()> + Send + 'static {
        self.shutdown();
        let sessions = std::mem::take(&mut self.retiring);
        let launch_barrier = self.launch_barrier.clone();
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
        // Spawned when there is a runtime to spawn on, for the reason above,
        // and awaited in place when there is not: a caller with no runtime
        // context of its own must not be handed a future that reaps nothing.
        let reaper = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => Ok(runtime.spawn(reap_sessions(sessions))),
            Err(_) => Err(sessions),
        };
        async move {
            if let Some(barrier) = launch_barrier {
                barrier.wait().await;
            }
            match reaper {
                Ok(reaper) => {
                    let _ = reaper.await;
                }
                Err(sessions) => reap_sessions(sessions).await,
            }
        }
    }

    fn request(&mut self, doc: LspDocumentId, at: LspPosition, kind: RequestKind) -> LspRequestId {
        let id = LspRequestId(self.next_request);
        self.next_request += 1;
        let fail = |events: &EventSink, error: LspError| {
            events.emit_now(LspEvent::RequestFailed {
                request: id,
                doc,
                error,
            });
            id
        };
        let Some(reference) = self.documents.get(&doc).cloned() else {
            return fail(&self.events, LspError::UnknownDocument(doc.0));
        };
        let Some(session) = self.sessions.get(&reference.session) else {
            return fail(&self.events, LspError::NoServer);
        };
        let Some(snapshot) = self.snapshots.latest(&reference.uri) else {
            return fail(&self.events, LspError::UnknownDocument(doc.0));
        };
        let position = match snapshot.to_utf16(at) {
            Ok(position) => position,
            Err(error) => return fail(&self.events, error),
        };
        if let Err(reason) = session
            .commands
            .send(SessionCommand::Request(PendingRequest {
                id,
                doc,
                uri: Arc::clone(&reference.uri),
                worktree: Arc::clone(&reference.worktree),
                kind,
                position,
                version: snapshot.version,
            }))
        {
            return fail(&self.events, LspError::Protocol(reason.to_owned()));
        }
        id
    }

    fn unavailable(&self, doc: LspDocumentId, reason: String) -> LspStatus {
        let status = LspStatus::Unavailable { reason };
        let delivery = self.events.bind(doc, None);
        self.events.emit_now(LspEvent::Status {
            doc,
            status: status.clone(),
            delivery,
        });
        status
    }

    fn collect_retired_sessions(&mut self) {
        let keys: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, handle)| handle.retired.load(Ordering::Acquire))
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            if let Some(handle) = self.sessions.remove(&key) {
                if let Some(retiring) = handle.retire() {
                    self.retiring.push(retiring);
                }
            }
        }
    }

    fn retire_handle(&mut self, handle: SessionHandle) {
        if let Some(retiring) = handle.retire() {
            let process_group = Arc::clone(&retiring.process_group);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                self.retiring.push(RetiringSession {
                    task: runtime.spawn(reap_sessions(vec![retiring])),
                    process_group,
                });
            } else {
                retiring.process_group.kill();
            }
        }
    }

    /// Returns the session for `key`, starting one if necessary.
    ///
    /// A session whose task has exited is replaced, and every document that
    /// belonged to it is reopened on the replacement, because a server that
    /// never received `didOpen` for a file cannot answer anything about it.
    ///
    /// A stopped manager starts nothing. The window an application is exiting
    /// stays live and interactive for as long as the language-server reap
    /// takes, and a document opened in it - a click on a file in a card whose
    /// tabs the exit path has already removed - used to spawn a whole server
    /// tree after the reap that was supposed to answer for it had already
    /// run, leaving its process group orphaned by the very exit that ordered
    /// the teardown. The refusal is reported rather than silent: the caller
    /// turns it into [`LspStatus::Unavailable`], which the editor renders.
    fn ensure_session(
        &mut self,
        key: &SessionKey,
        server: &LanguageServerSettings,
        _root: &Path,
        worktree: &Path,
    ) -> Result<SessionHandle, String> {
        if self.stopping {
            return Err("the editor is shutting its language servers down".to_owned());
        }
        let mut orphaned = !self.snapshots.docs_for_session(key).is_empty();
        if let Some(existing) = self.sessions.get(key) {
            let mut activity = existing
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !existing.commands.is_closed() && !existing.retired.load(Ordering::Acquire) {
                // Fence the idle worker before joining this server. It holds
                // the same lock while publishing retirement.
                activity.last_used = self.epoch.elapsed();
                return Ok(existing.clone());
            }
            drop(activity);
            if let Some(dead) = self.sessions.remove(key) {
                self.retire_handle(dead);
            }
            orphaned = true;
        }
        // Idle LRU entries are the only legal victims. If every slot is busy,
        // refuse admission visibly instead of killing an actively edited server.
        while self
            .sessions
            .values()
            .filter(|handle| !handle.retired.load(Ordering::Acquire))
            .count()
            >= self.session_cap
        {
            let victim = self
                .sessions
                .iter()
                .filter(|(_, handle)| !handle.retired.load(Ordering::Acquire))
                .filter_map(|(key, handle)| {
                    let activity = handle
                        .activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    activity
                        .documents
                        .is_empty()
                        .then(|| (key.clone(), activity.last_used))
                })
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(key, _)| key);
            let Some(victim) = victim else {
                return Err(format!(
                    "language server limit ({}) reached; close a document or raise lsp_max_sessions",
                    self.session_cap
                ));
            };
            if let Some(handle) = self.sessions.remove(&victim) {
                self.retire_handle(handle);
            }
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "no async runtime is available for language servers".to_owned())?;
        if self.flusher.is_none() {
            self.flusher = Some(runtime.spawn(drain_pending(
                self.events.tx.clone(),
                Arc::clone(&self.events.pending),
            )));
        }
        let (tx, rx) = commands::channel();
        let handle = SessionHandle {
            worktree: Arc::from(worktree.to_path_buf()),
            activity: Arc::new(Mutex::new(SessionActivity::default())),
            resident_bytes: Arc::new(AtomicU64::new(0)),
            commands: tx,
            status: Arc::new(Mutex::new(LspStatus::Starting)),
            completion_triggers: Arc::new(Mutex::new(None)),
            retired: Arc::new(AtomicBool::new(false)),
            retirement: Arc::new(tokio::sync::Notify::new()),
            process_group: Arc::new(PublishedGroup::default()),
            task: Arc::new(Mutex::new(None)),
        };
        let context = SessionContext {
            key: key.clone(),
            launch: LaunchSpec {
                initialization_options: server.initialization_options.clone(),
                command: server.command.clone(),
                root: key.root.to_path_buf(),
            },
            transport: Arc::clone(&self.transport),
            snapshots: Arc::clone(&self.snapshots),
            events: self.events.clone(),
            status: Arc::clone(&handle.status),
            completion_triggers: Arc::clone(&handle.completion_triggers),
            retired: Arc::clone(&handle.retired),
            retirement: Arc::clone(&handle.retirement),
            process_group: Arc::clone(&handle.process_group),
            stderr: Arc::new(StderrTail::default()),
        };
        let process_slots = Arc::clone(&self.process_slots);
        let launch_barrier = self.launch_barrier.clone();
        let root_identity = Arc::new(Mutex::new(None));
        let launched_root_identity = Arc::clone(&root_identity);
        *handle
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime.spawn(async move {
            if let Some(barrier) = launch_barrier {
                barrier.wait().await;
            }
            // A replacement waits until an evicted process is fully reaped,
            // so graceful shutdown cannot temporarily exceed the cap.
            let Ok(_slot) = process_slots.acquire_owned().await else {
                return;
            };
            if !context.retired.load(Ordering::Acquire) {
                let (directory_pin, identity) = match pin_session_root(&context.launch.root).await {
                    Ok(pin) => pin,
                    Err(error) => {
                        context
                            .set_status(LspStatus::Unavailable {
                                reason: format!("cannot retain language server root: {error}"),
                            })
                            .await;
                        return;
                    }
                };
                *launched_root_identity
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(identity);
                run_session(context, rx).await;
                // Prevent inode reuse until protocol teardown has reaped the
                // process; the watchdog's identity comparison stays exact.
                drop(directory_pin);
            }
        }));
        let watched = handle.clone();
        let watched_root = key.root.to_path_buf();
        let events = self.events.clone();
        let epoch = self.epoch;
        let idle_timeout = self.idle_timeout;
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(idle_timeout.min(Duration::from_secs(5))).await;
                let expired = {
                    let activity = watched
                        .activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let expired = activity.idle_expired(epoch.elapsed(), idle_timeout);
                    if expired {
                        watched.request_retirement();
                    }
                    expired
                };
                if expired {
                    let doc = watched
                        .activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .last_document;
                    retire_from_watchdog(watched).await;
                    if let Some(doc) = doc {
                        events.emit_now(LspEvent::Resources { doc });
                    }
                    break;
                }
                if watched.retired.load(Ordering::Acquire) || watched.commands.is_closed() {
                    break;
                }
                // Also catches external rename/remove followed by recreation
                // at the same spelling before this poll. The original inode
                // remains pinned until the server finishes teardown.
                let root_metadata = tokio::fs::metadata(&watched_root).await;
                let original = *root_identity
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let root_changed = match root_metadata {
                    Ok(metadata) => original
                        .is_some_and(|identity| identity != RootIdentity::from_metadata(&metadata)),
                    Err(error) => error.kind() == io::ErrorKind::NotFound,
                };
                if root_changed {
                    let doc = watched
                        .activity
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .last_document;
                    retire_from_watchdog(watched).await;
                    // Refresh cached inspector metrics even when this was the
                    // final server, so no other watchdog remains to refresh it.
                    if let Some(doc) = doc {
                        events.emit_now(LspEvent::Resources { doc });
                    }
                    break;
                }
                let pid = {
                    let group = watched
                        .process_group
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    match *group {
                        GroupPublication::Live(pid) => Some(pid),
                        _ => None,
                    }
                };
                if let Some(pid) = pid {
                    if let Ok(status) =
                        tokio::fs::read_to_string(format!("/proc/{pid}/status")).await
                    {
                        let rss = status
                            .lines()
                            .find_map(|line| line.strip_prefix("VmRSS:"))
                            .and_then(|value| value.split_whitespace().next())
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or(0)
                            * 1024;
                        watched.resident_bytes.store(rss, Ordering::Relaxed);
                    }
                }
                let doc = watched
                    .activity
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .last_document;
                // One wake per server refresh, independent of its tab count.
                if let Some(doc) = doc {
                    events.emit_now(LspEvent::Resources { doc });
                }
            }
        });
        self.sessions.insert(key.clone(), handle.clone());
        if orphaned {
            self.reopen_documents(key);
        }
        Ok(handle)
    }

    /// Re-sends `didOpen` for every document bound to `key`, carrying the
    /// current buffer text under a fresh version.
    fn reopen_documents(&mut self, key: &SessionKey) {
        let mut targets: Vec<(LspDocumentId, DocumentRef, i32, Arc<Snapshot>)> = Vec::new();
        for candidate in self.snapshots.docs_for_session(key) {
            let Some(document) = self.documents.get(&candidate).cloned() else {
                continue;
            };
            let Some(snapshot) = self.snapshots.latest(&document.uri) else {
                continue;
            };
            targets.push((
                candidate,
                document,
                snapshot.version.saturating_add(1),
                Arc::clone(&snapshot),
            ));
        }
        for (candidate, ..) in &targets {
            self.snapshots.unregister(*candidate);
            self.documents.remove(candidate);
        }
        for (candidate, document, version, text) in targets {
            let _ = self.open_source_at_root(
                candidate,
                &document.path,
                &document.worktree,
                document.language,
                version,
                text,
                key.root.to_path_buf(),
                Arc::default(),
            );
        }
    }

    #[cfg(test)]
    fn open_for_test(
        &mut self,
        doc: LspDocumentId,
        path: &Path,
        worktree: &Path,
        language: LspLanguage,
        version: i32,
        text: &str,
    ) -> LspStatus {
        let markers = self
            .servers
            .iter()
            .find(|server| server.id == language.server_id)
            .map(|server| server.root_markers.clone())
            .unwrap_or_default();
        self.open_at_root(
            doc,
            path,
            worktree,
            language,
            version,
            text,
            discover_root(path, worktree, &markers),
        )
    }

    /// Replaces the process launcher. Tests inject in-process duplex
    /// transports so no language server has to be installed.
    #[cfg(test)]
    fn set_transport(&mut self, transport: TransportFactory) {
        self.transport = transport;
    }
}

/// Signals every session, which is all a `Drop` can do: the teardown it
/// starts is asynchronous, and killing the servers here instead would abort
/// the graceful handshake of a manager that is merely being replaced by a
/// configuration change. An exit path awaits
/// [`LspManager::begin_shutdown`] before the runtime goes away.
impl Drop for LspManager {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
        }
    }
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LspManager")
            .field("servers", &self.servers.len())
            .field("sessions", &self.sessions.len())
            .field("documents", &self.documents.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) mod tests;
