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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::ops::{ControlFlow, Range};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

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

mod features;
pub use features::{LspChangeAnnotation,LspCapabilities,LspCodeAction,LspCommand,LspDocumentEdits,LspEditKind,LspFormattingOptions,LspMessageLevel,LspParameter,LspServerEditId,LspSignature,LspSignatureHelp,LspWorkspaceEdit};
use features::RequestArgs;


mod commands;

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

/// One diagnostic, already converted to editor-native coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    /// Original server metadata for code-action round trips. Shared across
    /// tabs; outgoing action requests replace its range with current mapped
    /// coordinates while preserving numeric codes and opaque data.
    pub protocol: Option<Arc<lsp_types::Diagnostic>>,
    pub range: LspRange,
    pub severity: LspSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
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

/// Everything the runtime pushes to the application.
#[derive(Debug, Clone, PartialEq)]
pub enum LspEvent {
    PrepareRename {request:LspRequestId,doc:LspDocumentId,range:Option<LspRange>,placeholder:Option<String>},
    WorkspaceEdit {request:LspRequestId,doc:LspDocumentId,kind:LspEditKind,edit:LspWorkspaceEdit},
    CodeActions {request:LspRequestId,doc:LspDocumentId,items:Vec<LspCodeAction>},
    CodeActionResolved {request:LspRequestId,doc:LspDocumentId,item:LspCodeAction},
    SignatureHelp {request:LspRequestId,doc:LspDocumentId,help:Option<LspSignatureHelp>},
    ServerMessage {doc:LspDocumentId,level:LspMessageLevel,message:String},
    CommandExecuted {request:LspRequestId,doc:LspDocumentId},
    ApplyWorkspaceEdit {doc:LspDocumentId,id:LspServerEditId,label:Option<String>,edit:LspWorkspaceEdit},

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
    },
    Progress {
        doc: LspDocumentId,
        progress: LspProgress,
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
    #[error("request timed out after {milliseconds} ms")]
    TimedOut { milliseconds:u64 },
    #[error("unsupported language-server operation: {0}")]
    Unsupported(String),
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
    text: Arc<str>,
    /// Byte offset of the first byte of each line. Never empty.
    line_starts: Vec<u32>,
}

impl Snapshot {
    fn new(version: i32, text: Arc<str>) -> Self {
        let mut line_starts = Vec::with_capacity(1 + text.len() / 32);
        line_starts.push(0);
        for (index, byte) in text.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                // Editable files are capped far below `u32::MAX` bytes by the
                // repository widget, so this conversion never saturates in
                // practice.
                line_starts.push(u32::try_from(index + 1).unwrap_or(u32::MAX));
            }
        }
        Self {
            version,
            text,
            line_starts,
        }
    }

    fn line_count(&self) -> u32 {
        u32::try_from(self.line_starts.len()).unwrap_or(u32::MAX)
    }

    /// Byte range of the line's content, excluding its terminator. `\r\n` is
    /// one terminator, so the `\r` is not part of the content.
    fn line_span(&self, line: u32) -> Option<Range<usize>> {
        let start = *self.line_starts.get(line as usize)? as usize;
        let end = match self.line_starts.get(line as usize + 1) {
            // Exclude the '\n' that begins the next line.
            Some(next) => *next as usize - 1,
            None => self.text.len(),
        };
        let end = if end > start && self.text.as_bytes().get(end - 1) == Some(&b'\r') {
            end - 1
        } else {
            end
        };
        Some(start..end)
    }

    fn line_content(&self, line: u32) -> Option<&str> {
        let span = self.line_span(line)?;
        Some(&self.text[span])
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
        let content = &self.text[span.clone()];
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
        return if *base.text == *expected {
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
        let mut next = String::with_capacity(working.text.len() + edit.new_text.len());
        next.push_str(&working.text[..start]);
        next.push_str(&edit.new_text);
        next.push_str(&working.text[end..]);
        intermediate = Some(Snapshot::new(base.version, Arc::from(next)));
    }
    let working = intermediate.as_ref().unwrap_or(base);
    let (start, end) = measure_edit(working, last, &mut changes)?;
    if !splices_into(&working.text, start..end, &last.new_text, expected) {
        return Err(LspError::InconsistentEdits);
    }
    Ok(changes)
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
    fn register(
        &self,
        doc: LspDocumentId,
        uri: &Arc<Url>,
        session: &SessionKey,
        requested_version: i32,
        text: Arc<str>,
    ) -> Option<Arc<Snapshot>> {
        let mut inner = self.lock();
        match inner.uris.get_mut(uri) {
            Some(state) => {
                let version = next_version(state.latest().version, requested_version)?;
                if !state.docs.contains(&doc) {
                    state.docs.push(doc);
                }
                let snapshot = Arc::new(Snapshot::new(version, text));
                state.push(Arc::clone(&snapshot));
                inner.docs.insert(doc, Arc::clone(uri));
                Some(snapshot)
            }
            None => {
                let snapshot = Arc::new(Snapshot::new(requested_version, text));
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

    fn latest(&self, uri: &Url) -> Option<Arc<Snapshot>> {
        Some(self.lock().uris.get(uri)?.latest())
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

    fn docs_for_uri(&self, uri: &Url) -> Vec<LspDocumentId> {
        self.lock()
            .uris
            .get(uri)
            .map(|state| state.docs.clone())
            .unwrap_or_default()
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
    ServerMessage(LspDocumentId, LspMessageLevel),
}

#[derive(Debug, Default)]
struct PendingInner {
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
    fn stage(&mut self, event: LspEvent) {
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
            LspEvent::ServerMessage { doc,level,.. } => StagedClass::ServerMessage(*doc,*level),
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
                return Some(event);
            }
        };
        if prefer_terminal {
            self.terminal.pop_front().or_else(|| replaceable(self))
        } else {
            replaceable(self).or_else(|| self.terminal.pop_front())
        }
    }

    fn forget(&mut self, doc: LspDocumentId) {
        for class in [
            StagedClass::Diagnostics(doc),
            StagedClass::Status(doc),
            StagedClass::Progress(doc),
            StagedClass::Resources(doc),
            StagedClass::ServerMessage(doc,LspMessageLevel::Error),
            StagedClass::ServerMessage(doc,LspMessageLevel::Warning),
            StagedClass::ServerMessage(doc,LspMessageLevel::Info),
            StagedClass::ServerMessage(doc,LspMessageLevel::Log),
        ] {
            self.replaceable.remove(&class);
        }
        self.order.retain(|class| match class {
            StagedClass::Diagnostics(at)
            | StagedClass::Status(at)
            | StagedClass::Progress(at)
            | StagedClass::Resources(at)
            | StagedClass::ServerMessage(at,_) => *at != doc,
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
        | LspEvent::PrepareRename { doc, .. }
        | LspEvent::WorkspaceEdit { doc, .. }
        | LspEvent::CodeActions { doc, .. }
        | LspEvent::CodeActionResolved { doc, .. }
        | LspEvent::SignatureHelp { doc, .. }
        | LspEvent::ServerMessage { doc, .. }
        | LspEvent::CommandExecuted { doc, .. }
        | LspEvent::ApplyWorkspaceEdit { doc, .. }
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
    /// Awaits channel capacity. Only session and request tasks call this, so
    /// backpressure slows protocol traffic rather than losing results.
    async fn emit(&self, event: LspEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Never blocks. When the channel is full the event is staged by class,
    /// and [`drain_pending`] moves it on as capacity appears.
    fn emit_now(&self, event: LspEvent) {
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
    command: Vec<String>,
    root: PathBuf,
}

type TransportFactory = Arc<dyn Fn(&LaunchSpec) -> Result<Transport, String> + Send + Sync>;

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
    PrepareRename,
    Rename,
    Formatting,
    RangeFormatting,
    CodeActions,
    ResolveCodeAction,
    ExecuteCommand,
    SignatureHelp,
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
    args: RequestArgs,
    timeout: Duration,
    observed_versions: HashMap<Url, i32>,
}

enum SessionCommand {
    Open {
        doc: LspDocumentId,
        uri: Arc<Url>,
        language_id: String,
        version: i32,
        text: Arc<str>,
    },
    Change {
        uri: Arc<Url>,
        version: i32,
        /// `None` when the manager could not build incremental changes, in
        /// which case the whole document is resent.
        incremental: Option<Vec<lsp_types::TextDocumentContentChangeEvent>>,
        full: Arc<str>,
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
    full: Arc<str>,
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
    Feature(features::RawFeatureResult),
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
    args: RequestArgs,
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
        kind => features::perform_feature(server, kind, position, args).await.map(RawResult::Feature),
    }
}

struct SessionContext {
    server_edits:Arc<features::ServerEdits>,
    capabilities: Arc<Mutex<LspCapabilities>>,
    server_settings: LanguageServerSettings,
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
            self.events
                .emit(LspEvent::Status {
                    doc,
                    status: status.clone(),
                })
                .await;
        }
    }
}

/// Shared state for the client-side router handling server-initiated traffic.
struct RouterState {
    server_edits:Arc<features::ServerEdits>,
    configuration:serde_json::Value,
    key: SessionKey,
    snapshots: Arc<SnapshotStore>,
    events: EventSink,
    retired: Arc<AtomicBool>,
}

fn build_router(context: &SessionContext) -> Router<RouterState> {
    let mut router = Router::new(RouterState {
        server_edits:Arc::clone(&context.server_edits),
        configuration:context.server_settings.configuration.clone(),
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
    router.notification::<notification::ShowMessage>(|state, params| {features::publish_message(state,params.typ,params.message,true);ControlFlow::Continue(())});
    router.notification::<notification::LogMessage>(|state, params| {features::publish_message(state,params.typ,params.message,false);ControlFlow::Continue(())});
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
    router.request::<request::WorkspaceConfiguration, _>(|state, params| {
        std::future::ready(Ok(params.items.into_iter().map(|item|features::configuration(&state.configuration,item.section.as_deref())).collect()))
    });
    router.request::<request::ApplyWorkspaceEdit,_>(features::apply_workspace_edit);
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
    let docs = state.snapshots.docs_for_uri(&params.uri);
    if docs.is_empty() {
        return;
    }
    let Some(snapshot) = state.snapshots.latest(&params.uri) else {
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
        .iter()
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
    diagnostic: &lsp_types::Diagnostic,
) -> Option<LspDiagnostic> {
    // One unconvertible range must not discard the whole batch.
    let range = snapshot.range_from_utf16(diagnostic.range).ok()?;
    Some(LspDiagnostic {
        protocol:Some(Arc::new(diagnostic.clone())),
        range,
        severity: match diagnostic.severity {
            Some(lsp_types::DiagnosticSeverity::WARNING) => LspSeverity::Warning,
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => LspSeverity::Information,
            Some(lsp_types::DiagnosticSeverity::HINT) => LspSeverity::Hint,
            _ => LspSeverity::Error,
        },
        message: diagnostic.message.clone(),
        source: diagnostic.source.clone(),
        code: diagnostic.code.as_ref().map(|code| match code {
            lsp_types::NumberOrString::Number(value) => value.to_string(),
            lsp_types::NumberOrString::String(value) => value.clone(),
        }),
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
        state.events.emit_now(LspEvent::Progress {
            doc,
            progress: progress.clone(),
        });
    }
}

fn client_capabilities() -> lsp_types::ClientCapabilities {
    let mut caps = lsp_types::ClientCapabilities {
        text_document: Some(lsp_types::TextDocumentClientCapabilities {
            synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                did_save: Some(true),
            }),
            publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                data_support:Some(true),
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
        workspace:Some(lsp_types::WorkspaceClientCapabilities {apply_edit:Some(true),configuration:Some(true),workspace_edit:Some(lsp_types::WorkspaceEditClientCapabilities {document_changes:Some(true),change_annotation_support:Some(lsp_types::ChangeAnnotationWorkspaceEditClientCapabilities {groups_on_label:Some(true)}),..Default::default()}),..Default::default()}),
        // No alternative position encoding is advertised, so the server must
        // use UTF-16, which is what the conversion layer above implements.
        ..Default::default()
    };
    features::add_client_capabilities(&mut caps);
    caps
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
    let initialize = server.request::<request::Initialize>({ params.initialization_options = context.server_settings.initialization_options.clone(); params });
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
    *context.capabilities.lock().unwrap_or_else(|p| p.into_inner()) = features::negotiate(&negotiated.capabilities);
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
    if !context.server_settings.configuration.is_null() {let _=server.notify::<notification::DidChangeConfiguration>(lsp_types::DidChangeConfigurationParams {settings:context.server_settings.configuration.clone()});}
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
                                text: text.to_string(),
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
        if matches!(request.kind,RequestKind::Rename|RequestKind::CodeActions|RequestKind::ResolveCodeAction|RequestKind::ExecuteCommand) {
            // Cross-file results must see every already-queued buffer edit,
            // including another URI still inside its debounce window.
            let uris:Vec<_>=self.pending.keys().cloned().collect();
            for uri in uris {self.flush_changes(&uri);}
        } else {self.flush_changes(&request.uri);}
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
            let mut call = Box::pin(perform(server.clone(), request.kind, position, request.args.clone()));
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
                None => match tokio::time::timeout(request.timeout, call).await {
                    Ok(value) => value,
                    Err(_) => {
                        let _ = server.notify::<notification::Cancel>(lsp_types::CancelParams { id:lsp_types::NumberOrString::Number(wire_id) });
                        events.emit(LspEvent::RequestFailed { request:request.id,doc:request.doc,error:LspError::TimedOut { milliseconds: request.timeout.as_millis() as u64 } }).await;
                        let _ = done.send(request.id);
                        return;
                    }
                },
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
            RequestKind::PrepareRename,
            RequestKind::Rename,
            RequestKind::Formatting,
            RequestKind::RangeFormatting,
            RequestKind::CodeActions,
            RequestKind::ResolveCodeAction,
            RequestKind::ExecuteCommand,
            RequestKind::SignatureHelp,
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
            text: change.full.to_string(),
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
    // A command may legitimately complete after its server-initiated edit
    // changed the document. Its acknowledgement carries no coordinates.
    if matches!(&raw,RawResult::Feature(features::RawFeatureResult::CommandExecuted(_))) {
        return LspEvent::CommandExecuted {request:request.id,doc:request.doc};
    }
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
        RawResult::Feature(raw) => features::convert_feature(snapshots, request, &snapshot, raw),
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

/// Whether `candidate` is inside `worktree` after resolving symlinks.
///
/// Marker probing and file reads follow symlinks, so a lexical prefix test is
/// not containment: `<worktree>/link/src` can resolve anywhere. Canonical
/// paths are compared when both sides resolve; when either does not, the
/// lexical test decides and the caller treats an unproven path as outside.
fn contains_path(worktree: &Path, candidate: &Path) -> bool {
    match (worktree.canonicalize(), candidate.canonicalize()) {
        (Ok(root), Ok(inside)) => inside.starts_with(&root),
        _ => candidate.starts_with(worktree),
    }
}

/// As [`contains_path`], but an unresolvable path is never treated as inside.
/// Used where being wrong means offering a writable editor for a file outside
/// the worktree.
fn contains_path_provably(worktree: &Path, candidate: &Path) -> bool {
    match (worktree.canonicalize(), candidate.canonicalize()) {
        (Ok(root), Ok(inside)) => inside.starts_with(&root),
        _ => false,
    }
}

/// TypeScript configuration permits comments and trailing commas. Preserve
/// quoted strings byte-for-byte while normalizing those two JSON extensions.
fn workspace_manifest(mut bytes: Vec<u8>, jsonc: bool) -> Option<serde_json::Value> {
    if jsonc {
        let mut index = 0;
        let mut quoted = false;
        let mut escaped = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
                index += 1;
            } else if byte == b'"' {
                quoted = true;
                index += 1;
            } else if bytes.get(index..index + 2) == Some(b"//") {
                while index < bytes.len() && bytes[index] != b'\n' {
                    bytes[index] = b' ';
                    index += 1;
                }
            } else if bytes.get(index..index + 2) == Some(b"/*") {
                bytes[index..index + 2].fill(b' ');
                index += 2;
                loop {
                    if bytes.get(index..index + 2) == Some(b"*/") {
                        bytes[index..index + 2].fill(b' ');
                        index += 2;
                        break;
                    }
                    let byte = bytes.get_mut(index)?;
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                    index += 1;
                }
            } else {
                index += 1;
            }
        }
        quoted = false;
        escaped = false;
        for index in 0..bytes.len() {
            let byte = bytes[index];
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else if byte == b'"' {
                quoted = true;
            } else if byte == b','
                && bytes[index + 1..]
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    .is_some_and(|byte| matches!(byte, b']' | b'}'))
            {
                bytes[index] = b' ';
            }
        }
    }
    serde_json::from_slice(&bytes).ok()
}

/// Select a workspace root before falling back to the nearest project marker.
/// Called only from the blocking discovery worker in production.
fn discover_root(path: &Path, worktree: &Path, markers: &[String]) -> PathBuf {
    if markers.is_empty() {
        return worktree.to_path_buf();
    }
    let mut nearest = None;
    let mut workspace = None;
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    let rust = extension.as_deref() == Some("rs");
    let javascript = extension.as_deref().is_some_and(|extension| {
        matches!(
            extension,
            "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs"
        )
    });
    let mut current = path.parent();
    while let Some(directory) = current {
        if !contains_path(worktree, directory) {
            break;
        }
        if nearest.is_none() && markers.iter().any(|marker| directory.join(marker).exists()) {
            nearest = Some(directory.to_path_buf());
        }
        if rust
            && std::fs::read_to_string(directory.join("Cargo.toml"))
                .ok()
                .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
                .is_some_and(|manifest| manifest.get("workspace").is_some())
        {
            workspace = Some(directory.to_path_buf());
        }
        if javascript {
            let workspace_json = |name: &str, key: &str| {
                std::fs::read(directory.join(name))
                    .ok()
                    .and_then(|text| workspace_manifest(text, name == "tsconfig.json"))
                    .is_some_and(|manifest| manifest.get(key).is_some())
            };
            if directory.join("pnpm-workspace.yaml").is_file()
                || workspace_json("package.json", "workspaces")
                || workspace_json("tsconfig.json", "references")
            {
                workspace = Some(directory.to_path_buf());
            }
        }
        if directory == worktree {
            break;
        }
        current = directory.parent();
    }
    workspace
        .or(nearest)
        .unwrap_or_else(|| worktree.to_path_buf())
}

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
    capabilities: Arc<Mutex<LspCapabilities>>,
    server_settings: LanguageServerSettings,
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

struct PendingOpen {
    path: PathBuf,
    worktree: PathBuf,
    language: LspLanguage,
    version: i32,
    text: String,
    saved_text: Option<String>,
    ticket: u64,
}

#[derive(Clone)]
struct DocumentRef {
    session: SessionKey,
    uri: Arc<Url>,
    path: PathBuf,
    worktree: Arc<Path>,
    language: LspLanguage,
}

/// Hands every manager its own event-channel identity.
///
/// A configuration change replaces the manager, and the replacement carries a
/// fresh channel that the old one's subscribers can never see. Deriving the
/// identity from a counter here rather than from a field the application
/// maintains means no caller can install a new manager and forget to
/// distinguish it from the one it retired.
static NEXT_MANAGER_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The application's single LSP client.
pub struct LspManager {
    server_edits:Arc<features::ServerEdits>,
    servers: Vec<LanguageServerSettings>,
    /// Lowercase extension to the first server and language that claims it.
    extensions: HashMap<String, LspLanguage>,
    events: EventSink,
    receiver: async_channel::Receiver<LspEvent>,
    sessions: HashMap<SessionKey, SessionHandle>,
    documents: HashMap<LspDocumentId, DocumentRef>,
    pending_opens: HashMap<LspDocumentId, PendingOpen>,
    next_discovery: u64,
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
            idle_timeout: Duration::from_secs(600),
            session_cap: 8,
            process_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            epoch: std::time::Instant::now(),
            snapshots: Arc::new(SnapshotStore::default()),
            transport: Arc::new(spawn_process_transport),
            retiring: Vec::new(),
            flusher: None,
            server_edits:Arc::new(features::ServerEdits::default()),
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
        let pending = PendingOpen {
            path: path.to_owned(),
            worktree: worktree.to_owned(),
            language,
            version,
            text: text.to_owned(),
            saved_text: None,
            ticket,
        };
        let worker_path = pending.path.clone();
        let worker_worktree = pending.worktree.clone();
        let markers = server.root_markers.clone();
        let events = self.events.clone();
        self.pending_opens.insert(doc, pending);
        // Canonicalization, marker reads and manifest parsing all happen here.
        runtime.spawn(async move {
            if let Ok(root) = tokio::task::spawn_blocking(move || {
                discover_root(&worker_path, &worker_worktree, &markers)
            })
            .await
            {
                events
                    .emit(LspEvent::RootDiscovered { doc, ticket, root })
                    .await;
            }
        });
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
        let status = self.open_at_root(
            doc,
            &pending.path,
            &pending.worktree,
            pending.language,
            pending.version,
            &pending.text,
            root,
        );
        if let Some(saved_text) = pending.saved_text {
            self.save(doc, &saved_text);
        }
        Some(status)
    }

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

        let key = self
            .sessions
            .iter()
            .filter(|(key, handle)| {
                key.server_id.as_ref() == server.id
                    && !handle.retired.load(Ordering::Acquire)
                    && handle.worktree.as_ref() == worktree
                    && path.starts_with(key.root.as_ref())
            })
            .min_by_key(|(key, _)| key.root.components().count())
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| SessionKey {
                server_id: Arc::from(server.id.as_str()),
                root: Arc::from(root.clone()),
            });
        // Keep a refused document registered so Restart can retry admission
        // after another workspace becomes idle, with its latest buffer text.
        // Ensure first: reopening previously blocked documents must not also
        // reopen this new registration.
        let handle = self.ensure_session(&key, &server, &root, worktree);

        let uri = Arc::new(uri);
        let worktree: Arc<Path> = Arc::from(worktree.to_path_buf());
        let Some(snapshot) = self
            .snapshots
            .register(doc, &uri, &key, version, Arc::from(text))
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
            },
        );
        let handle = match handle {
            Ok(handle) => handle,
            Err(reason) => return self.unavailable(doc, reason),
        };
        // Sent unconditionally: the session decides whether this is the URI's
        // first `didOpen` or a full resynchronisation of an already open file.
        let _ = handle.commands.send(SessionCommand::Open {
            doc,
            uri: Arc::clone(&uri),
            language_id: language.language_id.clone(),
            version: snapshot.version,
            text: Arc::clone(&snapshot.text),
        });
        {
            let mut activity = handle
                .activity
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            activity.documents.insert(doc);
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
            pending.text = full_text.to_owned();
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
            match build_incremental(&previous, edits, &snapshot.text) {
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
                full: Arc::clone(&snapshot.text),
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
        self.server_edits.close_doc(doc);
        self.pending_opens.remove(&doc);
        let Some(reference) = self.documents.remove(&doc) else {
            return;
        };
        self.snapshots.unregister(doc);
        // Nothing staged for a closed document can still be useful, and
        // keeping it would let a churn of documents grow the backlog.
        self.events.forget(doc);
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

    /// Retire every server and pending document owned by a removed worktree.
    /// Shutdown/exit and bounded process-group termination run asynchronously.
    pub fn remove_worktree(&mut self, worktree: &Path) {
        self.pending_opens
            .retain(|_, pending| pending.worktree != worktree);
        let documents: Vec<_> = self
            .documents
            .iter()
            .filter(|(_, reference)| reference.worktree.as_ref() == worktree)
            .map(|(doc, _)| *doc)
            .collect();
        let keys: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, handle)| handle.worktree.as_ref() == worktree)
            .map(|(key, _)| key.clone())
            .collect();
        for doc in documents {
            self.close(doc);
        }
        for key in keys {
            if let Some(handle) = self.sessions.remove(&key) {
                self.retire_handle(handle);
            }
        }
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
        if self.pending_opens.contains_key(&doc) {
            return LspStatus::Starting;
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
            return Snapshot::new(pending.version, Arc::from(pending.text.as_str()))
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
        self.server_edits.clear();
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
            match reaper {
                Ok(reaper) => {
                    let _ = reaper.await;
                }
                Err(sessions) => reap_sessions(sessions).await,
            }
        }
    }

    fn request(&mut self, doc: LspDocumentId, at: LspPosition, kind: RequestKind) -> LspRequestId {
        self.request_with_args(doc, at, kind, RequestArgs::None, Duration::from_secs(15))
    }

    fn request_with_args(&mut self, doc: LspDocumentId, at: LspPosition, kind: RequestKind, args:RequestArgs, timeout:Duration) -> LspRequestId {
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
                args, timeout, observed_versions: if matches!(kind,RequestKind::Rename|RequestKind::Formatting|RequestKind::RangeFormatting|RequestKind::CodeActions|RequestKind::ResolveCodeAction|RequestKind::ExecuteCommand) {self.snapshots.lock().uris.iter().filter(|(_,state)|state.session==reference.session).map(|(uri,state)| ((**uri).clone(),state.latest().version)).collect()} else {HashMap::new()},
            }))
        {
            return fail(&self.events, LspError::Protocol(reason.to_owned()));
        }
        id
    }

    fn unavailable(&self, doc: LspDocumentId, reason: String) -> LspStatus {
        let status = LspStatus::Unavailable { reason };
        self.events.emit_now(LspEvent::Status {
            doc,
            status: status.clone(),
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
            capabilities: Arc::new(Mutex::new(LspCapabilities::default())),
            server_settings: server.clone(),
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
            server_edits:Arc::clone(&self.server_edits),
            capabilities: Arc::clone(&handle.capabilities),
            server_settings: server.clone(),
            key: key.clone(),
            launch: LaunchSpec {
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
        *handle
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(runtime.spawn(async move {
            // A replacement waits until an evicted process is fully reaped,
            // so graceful shutdown cannot temporarily exceed the cap.
            let Ok(_slot) = process_slots.acquire_owned().await else {
                return;
            };
            if !context.retired.load(Ordering::Acquire) {
                run_session(context, rx).await;
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
                    retire_from_watchdog(watched).await;
                    break;
                }
                if watched.retired.load(Ordering::Acquire) || watched.commands.is_closed() {
                    break;
                }
                // Also catches removal by git/another window outside our UI.
                if tokio::fs::metadata(&watched_root)
                    .await
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                {
                    retire_from_watchdog(watched).await;
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
                    .documents
                    .iter()
                    .next()
                    .copied();
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
        let mut targets: Vec<(LspDocumentId, DocumentRef, i32, Arc<str>)> = Vec::new();
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
                Arc::clone(&snapshot.text),
            ));
        }
        for (candidate, ..) in &targets {
            self.snapshots.unregister(*candidate);
            self.documents.remove(candidate);
        }
        for (candidate, document, version, text) in targets {
            let _ = self.open_at_root(
                candidate,
                &document.path,
                &document.worktree,
                document.language,
                version,
                &text,
                key.root.to_path_buf(),
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
mod tests {
    use super::*;

    use std::sync::atomic::AtomicUsize;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Generous upper bound for "the runtime is making progress"; every test
    /// below reaches its condition in milliseconds on an idle machine. It is
    /// this wide only so a saturated machine cannot turn a slow schedule into
    /// a spurious failure.
    const WAIT: Duration = Duration::from_secs(30);

    const CHILD_HELPER_ENV: &str = "CAGENTS_LSP_CHILD_HELPER";
    const CHILD_HELPER_TEST: &str = "lsp::tests::child_helper_sleeps_until_terminated";

    /// Holds the path the grandchild helper publishes its pid to, so its
    /// presence is proof the helper body ran.
    const GRANDCHILD_ENV: &str = "CAGENTS_LSP_GRANDCHILD_HELPER";
    const GRANDCHILD_TEST: &str = "lsp::tests::child_helper_leaves_a_grandchild_behind";

    /// Holds the path the keystroke probe writes once its assertions have
    /// passed. A filter that matches no test makes `libtest` exit zero, so
    /// this marker is what tells the parent the probe actually ran.
    const KEYSTROKE_PROBE_ENV: &str = "CAGENTS_LSP_KEYSTROKE_PROBE";
    const KEYSTROKE_PROBE_TEST: &str = "lsp::tests::child_helper_converts_one_keystroke";

    /// This crate does not enable `tokio/macros`, so async tests drive their
    /// own runtime. It is single-threaded on purpose: the whole suite is
    /// cooperative async, and dozens of concurrent multi-threaded runtimes
    /// starve each other badly enough to trip the deadlines below.
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(future)
    }

    /// Peak resident memory of this process, in bytes.
    #[cfg(target_os = "linux")]
    fn peak_resident() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").expect("proc status");
        let peak = status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .and_then(|value| value.trim().strip_suffix(" kB"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("VmHWM in kB");
        peak * 1024
    }

    fn server(id: &str, extensions: &[&str], markers: &[&str]) -> LanguageServerSettings {
        LanguageServerSettings {
            id: id.to_owned(),
            languages: vec![crate::settings::LanguageServerLanguage {
                language_id: id.to_owned(),
                extensions: extensions.iter().map(|value| (*value).to_owned()).collect(),
            }],
            command: vec![format!("{id}-binary")],
            root_markers: markers.iter().map(|value| (*value).to_owned()).collect(),
            ..LanguageServerSettings::default()
        }
    }

    // -----------------------------------------------------------------
    // Deterministic in-process fake server
    // -----------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq)]
    struct Incoming {
        method: String,
        id: Option<i64>,
        params: Value,
        result: Option<Value>,
    }

    /// What the fake server's writer task puts on the wire.
    enum Outgoing {
        /// Framed as a `Content-Length` message.
        Message(Value),
        /// Written verbatim, so a test can present the framing a hostile
        /// server would.
        Raw(Vec<u8>),
    }

    #[derive(Clone)]
    struct FakeHandle {
        log: Arc<Mutex<Vec<Incoming>>>,
        push: mpsc::UnboundedSender<Outgoing>,
        /// Framed messages the fake server has written, which only advances
        /// when a Content-Length frame was produced.
        written: Arc<AtomicUsize>,
    }

    impl FakeHandle {
        fn methods(&self) -> Vec<String> {
            self.log
                .lock()
                .expect("fake log")
                .iter()
                .map(|entry| entry.method.clone())
                .collect()
        }

        fn entries(&self, method: &str) -> Vec<Incoming> {
            self.log
                .lock()
                .expect("fake log")
                .iter()
                .filter(|entry| entry.method == method)
                .cloned()
                .collect()
        }

        fn notify(&self, method: &str, params: Value) {
            let _ = self.push.send(Outgoing::Message(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            })));
        }

        /// Puts bytes on the wire exactly as given, framing and all.
        fn write_raw(&self, bytes: Vec<u8>) {
            let _ = self.push.send(Outgoing::Raw(bytes));
        }

        async fn wait_for(&self, method: &str) -> Incoming {
            self.wait_for_count(method, 1)
                .await
                .into_iter()
                .next_back()
                .expect("at least one entry")
        }

        async fn wait_for_count(&self, method: &str, count: usize) -> Vec<Incoming> {
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let entries = self.entries(method);
                if entries.len() >= count {
                    return entries;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "expected {count} `{method}` message(s), saw {} of them in {:?}",
                    entries.len(),
                    self.methods()
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    async fn read_framed(reader: &mut (impl AsyncBufRead + Unpin)) -> Option<Value> {
        let mut length: Option<usize> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                length = value.trim().parse().ok();
            }
        }
        let mut body = vec![0u8; length?];
        reader.read_exact(&mut body).await.ok()?;
        serde_json::from_slice(&body).ok()
    }

    async fn write_framed(writer: &mut (impl AsyncWrite + Unpin), value: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(value).expect("serialise");
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        writer.write_all(&body).await?;
        writer.flush().await
    }

    /// Spawns a fake server on one half of a `tokio::io::duplex` pair and
    /// returns the transport for the client half. `run_session` wraps that
    /// half with `tokio_util::compat` before handing it to
    /// `async_lsp::MainLoop::run_buffered`, so the production framing path is
    /// what these tests exercise.
    fn fake_transport(
        handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
    ) -> (Transport, FakeHandle) {
        let (client, fake) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client);
        let (fake_read, mut fake_write) = tokio::io::split(fake);
        let log: Arc<Mutex<Vec<Incoming>>> = Arc::new(Mutex::new(Vec::new()));
        let written = Arc::new(AtomicUsize::new(0));
        let (push, mut push_rx) = mpsc::unbounded_channel::<Outgoing>();

        let write_count = Arc::clone(&written);
        tokio::spawn(async move {
            while let Some(item) = push_rx.recv().await {
                match item {
                    Outgoing::Message(message) => {
                        if write_framed(&mut fake_write, &message).await.is_err() {
                            break;
                        }
                        write_count.fetch_add(1, Ordering::Release);
                    }
                    Outgoing::Raw(bytes) => {
                        if fake_write.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let reader_log = Arc::clone(&log);
        let reader_push = push.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(fake_read);
            while let Some(message) = read_framed(&mut reader).await {
                let incoming = Incoming {
                    method: message
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    id: message.get("id").and_then(Value::as_i64),
                    params: message.get("params").cloned().unwrap_or(Value::Null),
                    result:message.get("result").cloned(),
                };
                reader_log.lock().expect("fake log").push(incoming.clone());
                for reply in handler(&incoming) {
                    if reader_push.send(Outgoing::Message(reply)).is_err() {
                        return;
                    }
                }
            }
        });

        (
            Transport {
                reader: Box::new(client_read),
                writer: Box::new(client_write),
                stderr: None,
                child: None,
            },
            FakeHandle { log, push, written },
        )
    }

    /// Wires `manager` to a fake server that answers `initialize` with
    /// `capabilities` and `shutdown` with success, delegating everything else
    /// to `handler`.
    fn install_fake(
        manager: &mut LspManager,
        capabilities: Value,
        handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
    ) -> Arc<Mutex<Option<FakeHandle>>> {
        install_fake_with_child(manager, capabilities, handler, Arc::new(Mutex::new(None)))
    }

    /// As [`install_fake`], but the first transport also carries `child`, so a
    /// test can observe what the session task does to a real process.
    fn install_fake_with_child(
        manager: &mut LspManager,
        capabilities: Value,
        handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
        child: Arc<Mutex<Option<ChildProcess>>>,
    ) -> Arc<Mutex<Option<FakeHandle>>> {
        let slot: Arc<Mutex<Option<FakeHandle>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&slot);
        let handler = Arc::new(handler);
        manager.set_transport(Arc::new(move |_spec: &LaunchSpec| {
            let capabilities = capabilities.clone();
            let handler = Arc::clone(&handler);
            let (mut transport, handle) =
                fake_transport(move |incoming| match incoming.method.as_str() {
                    "initialize" => vec![json!({
                        "jsonrpc": "2.0",
                        "id": incoming.id,
                        "result": { "capabilities": capabilities },
                    })],
                    "shutdown" => vec![json!({
                        "jsonrpc": "2.0",
                        "id": incoming.id,
                        "result": Value::Null,
                    })],
                    _ => handler(incoming),
                });
            transport.child = child.lock().expect("child slot").take();
            *sink.lock().expect("transport slot") = Some(handle);
            Ok(transport)
        }));
        slot
    }

    async fn handle_of(slot: &Arc<Mutex<Option<FakeHandle>>>) -> FakeHandle {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(handle) = slot.lock().expect("transport slot").clone() {
                return handle;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fake transport was never created"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn next_matching<T>(
        events: &async_channel::Receiver<LspEvent>,
        mut pick: impl FnMut(&LspEvent) -> Option<T>,
    ) -> T {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no matching event arrived in time");
            let Ok(Ok(event)) = tokio::time::timeout(remaining, events.recv()).await else {
                panic!("the event channel closed or timed out");
            };
            if let Some(value) = pick(&event) {
                return value;
            }
        }
    }

    async fn await_ready(events: &async_channel::Receiver<LspEvent>, doc: LspDocumentId) {
        next_matching(events, |event| match event {
            LspEvent::Status { doc: at, status } if *at == doc && status.is_ready() => Some(()),
            _ => None,
        })
        .await;
    }

    fn full_sync_capabilities() -> Value {
        json!({
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": { "includeText": true },
            }
        })
    }

    fn incremental_capabilities() -> Value {
        json!({ "textDocumentSync": { "openClose": true, "change": 2, "save": true } })
    }

    /// A worktree with a root marker and one Rust file.
    fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().to_path_buf();
        std::fs::write(worktree.join("Cargo.toml"), "[package]\n").expect("marker");
        let file = worktree.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").expect("file");
        (root, worktree, file)
    }

    fn rust_manager() -> LspManager {
        LspManager::new(vec![server("rust-analyzer", &["rs"], &["Cargo.toml"])])
    }

    fn rust_language() -> LspLanguage {
        LspLanguage {
            server_id: "rust-analyzer".to_owned(),
            language_id: "rust-analyzer".to_owned(),
        }
    }

    #[test]
    fn workspace_discovery_prefers_topmost_cargo_workspace() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("crates/nested");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/nested\"]\n",
        )
        .unwrap();
        std::fs::write(nested.join("Cargo.toml"), "[package]\nname = \"nested\"\n").unwrap();
        let markers = vec!["Cargo.toml".into()];
        assert_eq!(
            discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
            root.path()
        );
        std::fs::remove_file(root.path().join("Cargo.toml")).unwrap();
        assert_eq!(
            discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
            nested
        );
        std::fs::remove_file(nested.join("Cargo.toml")).unwrap();
        assert_eq!(
            discover_root(&nested.join("src/lib.rs"), root.path(), &markers),
            root.path()
        );
    }

    #[test]
    fn workspace_discovery_recognizes_typescript_workspace_conventions() {
        for (manifest, content) in [
            ("package.json", r#"{"workspaces":["packages/*"]}"#),
            ("pnpm-workspace.yaml", "packages:\n - packages/*\n"),
            ("tsconfig.json", r#"{"references":[{"path":"packages/a"}]}"#),
        ] {
            let root = tempfile::tempdir().unwrap();
            let package = root.path().join("packages/a/src");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(root.path().join(manifest), content).unwrap();
            std::fs::write(package.parent().unwrap().join("package.json"), "{}").unwrap();
            assert_eq!(
                discover_root(
                    &package.join("index.ts"),
                    root.path(),
                    &["package.json".into()]
                ),
                root.path()
            );
        }
    }

    #[test]
    fn workspace_jsonc_preserves_strings_and_accepts_comments_and_trailing_commas() {
        let value = workspace_manifest(
            br#"{
            // a normal TypeScript solution configuration
            "references": [{"path": "packages/a",},],
            "compilerOptions": {"baseUrl": "https://example.test/a//b",}, /* final comment */
        }"#
            .to_vec(),
            true,
        )
        .unwrap();
        assert_eq!(value["references"][0]["path"], "packages/a");
        assert_eq!(
            value["compilerOptions"]["baseUrl"],
            "https://example.test/a//b"
        );
    }

    #[test]
    fn idle_state_uses_monotonic_fake_clock_and_never_evicts_open_documents() {
        let mut activity = SessionActivity {
            documents: HashSet::new(),
            last_used: Duration::from_secs(20),
        };
        let timeout = Duration::from_secs(600);
        assert!(!activity.idle_expired(Duration::from_secs(619), timeout));
        assert!(activity.idle_expired(Duration::from_secs(620), timeout));
        activity.documents.insert(LspDocumentId(1));
        assert!(!activity.idle_expired(Duration::from_secs(9999), timeout));
        activity.documents.clear();
        activity.last_used = Duration::from_secs(9999);
        assert!(!activity.idle_expired(Duration::from_secs(10000), timeout));
    }

    #[test]
    fn session_cap_evicts_idle_lru_and_refuses_busy_eviction() {
        block_on(async {
            let (_root, worktree, _) = workspace();
            let mut manager = rust_manager().with_lifecycle(600, 2);
            manager.set_transport(Arc::new(|_| {
                Err("unused transport in admission test".into())
            }));
            let settings = manager.servers[0].clone();
            let key = |name: &str| SessionKey {
                server_id: Arc::from(settings.id.as_str()),
                root: Arc::from(worktree.join(name)),
            };
            let first = key("first");
            let second = key("second");
            let third = key("third");
            let a = manager
                .ensure_session(&first, &settings, &first.root, &worktree)
                .unwrap();
            let b = manager
                .ensure_session(&second, &settings, &second.root, &worktree)
                .unwrap();
            a.activity
                .lock()
                .unwrap()
                .documents
                .insert(LspDocumentId(1));
            b.activity
                .lock()
                .unwrap()
                .documents
                .insert(LspDocumentId(2));
            assert!(
                manager
                    .ensure_session(&third, &settings, &third.root, &worktree)
                    .is_err()
            );
            assert!(!a.retired.load(Ordering::Acquire));
            assert!(!b.retired.load(Ordering::Acquire));
            a.activity.lock().unwrap().documents.clear();
            manager
                .ensure_session(&third, &settings, &third.root, &worktree)
                .unwrap();
            assert!(a.retired.load(Ordering::Acquire));
            assert!(!b.retired.load(Ordering::Acquire));
            assert_eq!(manager.sessions.len(), 2);
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn cap_refused_document_keeps_latest_text_for_restart_and_later_join() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let second = worktree.join("other-worktree");
            std::fs::create_dir_all(&second).unwrap();
            let mut manager = rust_manager().with_lifecycle(600, 1);
            install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "first",
            );
            await_ready(&events, LspDocumentId(1)).await;
            let status = manager.open_for_test(
                LspDocumentId(2),
                &second.join("lib.rs"),
                &second,
                rust_language(),
                1,
                "blocked",
            );
            assert!(matches!(status, LspStatus::Unavailable { .. }));
            assert!(manager.documents.contains_key(&LspDocumentId(2)));
            manager.change(LspDocumentId(2), 2, &[], "edited while blocked");
            manager.close(LspDocumentId(1));
            manager.restart(LspDocumentId(2));
            await_ready(&events, LspDocumentId(2)).await;
            let reference = &manager.documents[&LspDocumentId(2)];
            assert_eq!(
                manager
                    .snapshots
                    .latest(&reference.uri)
                    .unwrap()
                    .text
                    .as_ref(),
                "edited while blocked"
            );
            assert_eq!(manager.sessions.len(), 1);
            assert!(
                manager.sessions[&reference.session]
                    .activity
                    .lock()
                    .unwrap()
                    .documents
                    .contains(&LspDocumentId(2))
            );
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn later_admission_reopens_previously_refused_documents_for_that_root() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let second = worktree.join("other-worktree");
            std::fs::create_dir_all(&second).unwrap();
            let mut manager = rust_manager().with_lifecycle(600, 1);
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "first",
            );
            await_ready(&events, LspDocumentId(1)).await;
            assert!(matches!(
                manager.open_for_test(
                    LspDocumentId(2),
                    &second.join("lib.rs"),
                    &second,
                    rust_language(),
                    1,
                    "blocked"
                ),
                LspStatus::Unavailable { .. }
            ));
            manager.close(LspDocumentId(1));
            manager.open_for_test(
                LspDocumentId(3),
                &second.join("other.rs"),
                &second,
                rust_language(),
                1,
                "new file",
            );
            await_ready(&events, LspDocumentId(3)).await;
            let session = manager.sessions.values().next().unwrap();
            assert_eq!(session.activity.lock().unwrap().documents.len(), 2);
            let fake = handle_of(&slot).await;
            fake.wait_for_count("textDocument/didOpen", 2).await;
            let opened = fake.entries("textDocument/didOpen");
            assert!(
                opened
                    .iter()
                    .any(|entry| entry.params["textDocument"]["text"] == "blocked")
            );
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn cap_selects_least_recently_used_idle_session_with_fake_clock() {
        block_on(async {
            let (_root, worktree, _) = workspace();
            let mut manager = rust_manager().with_lifecycle(600, 2);
            manager.set_transport(Arc::new(|_| {
                Err("unused transport in admission test".into())
            }));
            let settings = manager.servers[0].clone();
            let key = |name: &str| SessionKey {
                server_id: Arc::from(settings.id.as_str()),
                root: Arc::from(worktree.join(name)),
            };
            let first = key("first");
            let second = key("second");
            let third = key("third");
            let a = manager
                .ensure_session(&first, &settings, &first.root, &worktree)
                .unwrap();
            let b = manager
                .ensure_session(&second, &settings, &second.root, &worktree)
                .unwrap();
            a.activity.lock().unwrap().last_used = Duration::from_secs(20);
            b.activity.lock().unwrap().last_used = Duration::from_secs(10);
            manager
                .ensure_session(&third, &settings, &third.root, &worktree)
                .unwrap();
            assert!(!a.retired.load(Ordering::Acquire));
            assert!(b.retired.load(Ordering::Acquire));
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn asynchronous_workspace_open_shares_server_and_idle_close_restarts_it() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            std::fs::write(worktree.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
            let member = worktree.join("crates/member");
            std::fs::create_dir_all(member.join("src")).unwrap();
            std::fs::write(member.join("Cargo.toml"), "[package]\nname=\"member\"\n").unwrap();
            let nested = member.join("src/lib.rs");
            let mut manager = rust_manager().with_lifecycle(1, 2);
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            for (doc, path) in [(LspDocumentId(1), &file), (LspDocumentId(2), &nested)] {
                assert_eq!(
                    manager.open(doc, path, &worktree, rust_language(), 1, "fn test() {}"),
                    LspStatus::Starting
                );
                let (ticket, root) = next_matching(&events, |event| match event {
                    LspEvent::RootDiscovered {
                        doc: at,
                        ticket,
                        root,
                    } if *at == doc => Some((*ticket, root.clone())),
                    _ => None,
                })
                .await;
                assert_eq!(root, worktree);
                let status = manager.finish_discovery(doc, ticket, root);
                if !status.is_some_and(|status| status.is_ready()) {
                    await_ready(&events, doc).await;
                }
            }
            assert_eq!(manager.sessions.len(), 1);
            let handle = handle_of(&slot).await;
            assert_eq!(handle.entries("initialize").len(), 1);
            manager.close(LspDocumentId(1));
            manager.close(LspDocumentId(2));
            handle.wait_for("shutdown").await;
            handle.wait_for("exit").await;
            assert!(
                manager
                    .sessions
                    .values()
                    .all(|session| session.retired.load(Ordering::Acquire))
            );
            let doc = LspDocumentId(3);
            manager.open(doc, &file, &worktree, rust_language(), 1, "fn test() {}");
            let (ticket, root) = next_matching(&events, |event| match event {
                LspEvent::RootDiscovered {
                    doc: at,
                    ticket,
                    root,
                } if *at == doc => Some((*ticket, root.clone())),
                _ => None,
            })
            .await;
            let status = manager.finish_discovery(doc, ticket, root);
            if !status.is_some_and(|status| status.is_ready()) {
                await_ready(&events, doc).await;
            }
            assert!(
                manager
                    .sessions
                    .values()
                    .all(|session| !session.retired.load(Ordering::Acquire))
            );
            manager.remove_worktree(&worktree);
            assert!(manager.sessions.is_empty());
            assert!(manager.documents.is_empty());
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn nested_worktrees_keep_separate_servers_and_removal_retires_idle_owner() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            std::fs::write(worktree.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
            let linked = worktree.join(".worktrees/feature");
            std::fs::create_dir_all(&linked).unwrap();
            std::fs::write(linked.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
            let mut manager = rust_manager();
            install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "fn outer() {}",
            );
            await_ready(&events, LspDocumentId(1)).await;
            manager.open_for_test(
                LspDocumentId(2),
                &linked.join("lib.rs"),
                &linked,
                rust_language(),
                1,
                "fn linked() {}",
            );
            await_ready(&events, LspDocumentId(2)).await;
            assert_eq!(
                manager.sessions.len(),
                2,
                "a repository-local worktree is a separate workspace"
            );
            let outer = manager
                .sessions
                .values()
                .find(|handle| handle.worktree.as_ref() == worktree)
                .unwrap()
                .clone();
            let inner = manager
                .sessions
                .values()
                .find(|handle| handle.worktree.as_ref() == linked)
                .unwrap()
                .clone();
            manager.close(LspDocumentId(2));
            assert!(inner.activity.lock().unwrap().documents.is_empty());
            manager.remove_worktree(&linked);
            assert!(
                inner.retired.load(Ordering::Acquire),
                "idle server must retire immediately, before filesystem polling"
            );
            assert!(!outer.retired.load(Ordering::Acquire));
            assert_eq!(manager.sessions.len(), 1);
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn closed_ancestor_session_launches_its_canonical_key_root() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let nested = worktree.join("package");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("Cargo.toml"), "[package]\n").unwrap();
            let mut manager = rust_manager();
            install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let factory = Arc::clone(&manager.transport);
            let roots = Arc::new(Mutex::new(Vec::new()));
            let launches = Arc::clone(&roots);
            manager.set_transport(Arc::new(move |spec| {
                launches.lock().unwrap().push(spec.root.clone());
                factory(spec)
            }));
            let events = manager.events();
            manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "first",
            );
            await_ready(&events, LspDocumentId(1)).await;
            let previous = manager.sessions.values().next().unwrap().clone();
            let task = previous.task.lock().unwrap().take().unwrap();
            task.abort();
            let _ = task.await;
            assert!(previous.commands.is_closed());
            assert!(!previous.retired.load(Ordering::Acquire));
            manager.open_for_test(
                LspDocumentId(2),
                &nested.join("lib.rs"),
                &worktree,
                rust_language(),
                1,
                "nested",
            );
            await_ready(&events, LspDocumentId(2)).await;
            assert_eq!(
                *roots.lock().unwrap(),
                vec![worktree.clone(), worktree.clone()]
            );
            assert_eq!(manager.sessions.len(), 1);
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn save_during_root_discovery_is_replayed_after_open_without_losing_newer_edits() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open(doc, &file, &worktree, rust_language(), 1, "original");
            manager.save(doc, "saved while discovering");
            manager.change(doc, 2, &[], "newer unsaved edit");
            let (ticket, root) = next_matching(&events, |event| match event {
                LspEvent::RootDiscovered {
                    doc: at,
                    ticket,
                    root,
                } if *at == doc => Some((*ticket, root.clone())),
                _ => None,
            })
            .await;
            manager.finish_discovery(doc, ticket, root);
            await_ready(&events, doc).await;
            let fake = handle_of(&slot).await;
            let saved = fake.wait_for("textDocument/didSave").await;
            assert_eq!(saved.params["text"], "saved while discovering");
            assert_eq!(
                fake.entries("textDocument/didOpen")[0].params["textDocument"]["text"],
                "newer unsaved edit"
            );
            let methods = fake.methods();
            assert!(
                methods
                    .iter()
                    .position(|method| method == "textDocument/didOpen")
                    < methods
                        .iter()
                        .position(|method| method == "textDocument/didSave")
            );
            manager.shutdown();
            manager.wait_for_shutdown().await;
        });
    }

    #[test]
    fn public_lifecycle_builder_bounds_unvalidated_limits() {
        let manager = rust_manager().with_lifecycle(u64::MAX, usize::MAX);
        assert_eq!(manager.lifecycle(), (604800, 64));
        let manager = rust_manager().with_lifecycle(0, 0);
        assert_eq!(manager.lifecycle(), (1, 1));
    }

    #[test]
    fn shutdown_awaits_idle_retirement_already_started_by_watcher() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let mut manager = rust_manager().with_lifecycle(1, 1);
            let slot = Arc::new(Mutex::new(None));
            let sink = Arc::clone(&slot);
            manager.set_transport(Arc::new(move |_| {
                let (transport, handle) = fake_transport(|message| {
                    if message.method == "initialize" {
                        vec![json!({"jsonrpc":"2.0", "id":message.id,
                            "result":{"capabilities":{"textDocumentSync":2}}})]
                    } else {
                        Vec::new()
                    } // deliberately leave shutdown unanswered
                });
                *sink.lock().unwrap() = Some(handle);
                Ok(transport)
            }));
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn main() {}");
            await_ready(&events, doc).await;
            let handle = handle_of(&slot).await;
            manager.close(doc);
            handle.wait_for("shutdown").await;
            assert!(
                manager
                    .sessions
                    .values()
                    .all(|session| session.task.lock().unwrap().is_some()),
                "watcher retirement must leave the join handle reachable by the manager"
            );
            let started = tokio::time::Instant::now();
            manager.begin_shutdown().await;
            assert!(
                started.elapsed() >= Duration::from_millis(100),
                "shutdown must await the already-retiring server's handshake"
            );
            assert!(handle.entries("exit").len() == 1);
        });
    }

    #[test]
    fn asynchronous_discovery_ignores_closed_and_replaced_documents() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let mut manager = rust_manager();
            let events = manager.events();
            let doc = LspDocumentId(11);
            assert_eq!(
                manager.open(doc, &file, &worktree, rust_language(), 1, "old"),
                LspStatus::Starting
            );
            let old_ticket = manager.pending_opens[&doc].ticket;
            manager.change(doc, 2, &[], "edited during discovery");
            assert_eq!(manager.pending_opens[&doc].text, "edited during discovery");
            manager.close(doc);
            let (ticket, root) = next_matching(&events, |event| match event {
                LspEvent::RootDiscovered { ticket, root, .. } => Some((*ticket, root.clone())),
                _ => None,
            })
            .await;
            assert!(
                manager
                    .finish_discovery(doc, ticket, root.clone())
                    .is_none()
            );
            assert!(manager.sessions.is_empty());
            manager.open(doc, &file, &worktree, rust_language(), 3, "new");
            assert!(manager.finish_discovery(doc, old_ticket, root).is_none());
            assert!(
                manager.sessions.is_empty(),
                "stale discovery must not reopen a replacement"
            );
            assert_eq!(manager.pending_opens[&doc].version, 3);
            manager.close(doc);
        });
    }

    #[test]
    fn delayed_initialization_retains_only_current_changes_and_resynchronizes() {
        block_on(async {
            let (_root, worktree, first) = workspace();
            let second = worktree.join("two.rs");
            let slot = Arc::new(Mutex::new(None));
            let sink = Arc::clone(&slot);
            let mut manager = rust_manager();
            manager.set_transport(Arc::new(move |_| {
                let (transport, handle) = fake_transport(|incoming| {
                    if incoming.method == "shutdown" {
                        vec![json!({"jsonrpc":"2.0", "id":incoming.id, "result":null})]
                    } else {
                        Vec::new()
                    }
                });
                *sink.lock().unwrap() = Some(handle);
                Ok(transport)
            }));
            let initial = "a".repeat(64 * 1024);
            manager.open_for_test(
                LspDocumentId(1),
                &first,
                &worktree,
                rust_language(),
                1,
                &initial,
            );
            manager.open_for_test(
                LspDocumentId(2),
                &second,
                &worktree,
                rust_language(),
                1,
                &initial,
            );
            let handle = handle_of(&slot).await;
            let initialize = handle.wait_for("initialize").await;
            let reference = manager.documents.get(&LspDocumentId(1)).unwrap().clone();
            let mut old_versions = Vec::new();
            let latest = "b".repeat(initial.len());
            for version in 2..302 {
                for doc in [LspDocumentId(1), LspDocumentId(2)] {
                    manager.change(doc, version, &[], &latest);
                }
                old_versions.push(Arc::downgrade(
                    &manager.snapshots.latest(&reference.uri).unwrap().text,
                ));
            }
            assert!(
                old_versions[..old_versions.len() - MAX_RETAINED_SNAPSHOTS]
                    .iter()
                    .all(|snapshot| snapshot.upgrade().is_none()),
                "queued versions must not pin the snapshot history"
            );
            assert_eq!(
                manager.sessions[&reference.session]
                    .commands
                    .pending_usage(),
                (4, 4 * initial.len())
            );
            handle
                .push
                .send(Outgoing::Message(json!({
                    "jsonrpc":"2.0", "id":initialize.id,
                    "result":{"capabilities":full_sync_capabilities()},
                })))
                .unwrap();
            await_ready(&manager.events(), LspDocumentId(1)).await;
            let changes = handle.wait_for_count("textDocument/didChange", 2).await;
            for change in changes {
                assert_eq!(change.params["textDocument"]["version"], 301);
                assert_eq!(change.params["contentChanges"][0]["text"], latest);
            }
            manager.begin_shutdown().await;
        });
    }

    #[test]
    fn initializing_session_overload_settles_accepted_requests_and_remains_restartable() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let mut manager = rust_manager();
            manager.set_transport(Arc::new(|_| Ok(fake_transport(|_| Vec::new()).0)));
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "text");
            let mut expected = std::collections::HashSet::new();
            // The Open is already queued. Enough request barriers must stop
            // admission instead of retaining an arbitrary command history.
            for _ in 0..256 {
                expected.insert(manager.request_hover(doc, LspPosition::new(0, 0)));
            }
            let events = manager.events();
            let mut failed = std::collections::HashSet::new();
            let deadline = tokio::time::Instant::now() + WAIT;
            loop {
                let event = tokio::time::timeout_at(deadline, events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                match event {
                    LspEvent::RequestFailed { request, .. } => {
                        failed.insert(request);
                    }
                    LspEvent::Status {
                        status: LspStatus::Failed { reason, .. },
                        ..
                    } => {
                        assert!(reason.contains("bounded budget"));
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(failed, expected);
            assert!(manager.status(doc).can_restart());
            manager.begin_shutdown().await;
        });
    }

    // -----------------------------------------------------------------
    // Configuration, coordinates and snippets
    // -----------------------------------------------------------------

    #[test]
    fn language_for_uses_the_first_declaration_that_claims_an_extension() {
        let manager = LspManager::new(vec![
            server("first", &["ts", "tsx"], &[]),
            server("second", &["ts"], &[]),
        ]);
        let language = manager
            .language_for(Path::new("/tmp/app.TS"))
            .expect("extensions match case-insensitively");
        assert_eq!(language.server_id, "first");
        assert!(manager.language_for(Path::new("/tmp/app.rs")).is_none());
        assert!(manager.language_for(Path::new("/tmp/Makefile")).is_none());
    }

    #[test]
    fn an_empty_server_list_disables_lsp_without_spawning_anything() {
        let mut manager = LspManager::new(Vec::new());
        assert_eq!(
            manager.open_for_test(
                LspDocumentId(1),
                Path::new("/tmp/a.rs"),
                Path::new("/tmp"),
                rust_language(),
                1,
                "",
            ),
            LspStatus::Disabled
        );
        assert!(!LspStatus::Disabled.can_restart());
    }

    /// An exit reaps the servers it knows about exactly once, and the window
    /// stays live and interactive for as long as that reap takes. A document
    /// opened in it - a click on a file in a card whose tabs the exit has
    /// already removed - must not start a server tree behind the reaper.
    #[test]
    fn a_manager_that_has_shut_down_starts_no_further_server() {
        block_on(async {
            let (_root, worktree, file) = workspace();
            let mut manager = rust_manager();
            manager.shutdown();
            let status = manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "fn main() {}\n",
            );
            let LspStatus::Unavailable { reason } = &status else {
                panic!("a stopped manager must decline the document, got {status:?}");
            };
            assert!(
                reason.contains("shutting"),
                "the refusal has to be one the editor can render, got {reason:?}"
            );
            assert!(
                manager.sessions.is_empty(),
                "a stopped manager must hold no session"
            );
            assert!(
                manager.flusher.is_none(),
                "and must not restart the event flusher it just aborted"
            );
        });
    }

    #[test]
    fn root_discovery_stops_at_the_nearest_marker_and_never_leaves_the_worktree() {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path();
        let inner = worktree.join("crates/inner");
        std::fs::create_dir_all(inner.join("src")).expect("dirs");
        std::fs::write(worktree.join("Cargo.toml"), "").expect("outer marker");
        std::fs::write(inner.join("Cargo.toml"), "").expect("inner marker");
        let markers = vec!["Cargo.toml".to_owned()];

        assert_eq!(
            discover_root(&inner.join("src/lib.rs"), worktree, &markers),
            inner,
            "the nearest marker wins"
        );
        assert_eq!(
            discover_root(&worktree.join("build.rs"), worktree, &markers),
            worktree
        );
        assert_eq!(
            discover_root(Path::new("/elsewhere/main.rs"), worktree, &markers),
            worktree,
            "a marker above the worktree must never be selected"
        );
        assert_eq!(
            discover_root(&inner.join("src/lib.rs"), worktree, &[]),
            worktree,
            "a server with no markers uses the worktree"
        );
    }

    #[test]
    fn scalar_and_utf16_positions_round_trip_for_ascii_bmp_and_astral_text() {
        // "héllo" holds a two-byte BMP scalar; "𝄞" is astral and occupies two
        // UTF-16 code units.
        let snapshot = Snapshot::new(7, Arc::from("ascii\nhéllo\na𝄞b\n"));

        assert_eq!(
            snapshot
                .to_utf16(LspPosition::new(0, 5))
                .expect("ascii end"),
            lsp_types::Position::new(0, 5)
        );
        assert_eq!(
            snapshot.to_utf16(LspPosition::new(1, 2)).expect("bmp"),
            lsp_types::Position::new(1, 2)
        );
        assert_eq!(
            snapshot.byte_offset(LspPosition::new(1, 2)).expect("bytes"),
            "ascii\n".len() + 3,
            "the two-byte scalar shifts byte offsets but not UTF-16 offsets"
        );
        assert_eq!(
            snapshot.to_utf16(LspPosition::new(2, 2)).expect("astral"),
            lsp_types::Position::new(2, 3)
        );

        for (line, character) in [
            (0u32, 0u32),
            (0, 5),
            (1, 0),
            (1, 2),
            (1, 5),
            (2, 0),
            (2, 2),
            (2, 3),
        ] {
            let scalar = LspPosition::new(line, character);
            let utf16 = snapshot.to_utf16(scalar).expect("outbound");
            assert_eq!(
                snapshot.from_utf16(utf16).expect("inbound"),
                scalar,
                "{scalar:?} must round-trip"
            );
        }
    }

    #[test]
    fn a_utf16_offset_inside_a_surrogate_pair_is_rejected() {
        let snapshot = Snapshot::new(1, Arc::from("a𝄞b"));
        assert_eq!(
            snapshot.from_utf16(lsp_types::Position::new(0, 2)),
            Err(LspError::SplitSurrogate {
                line: 0,
                character: 2
            })
        );
        assert!(matches!(
            snapshot.from_utf16(lsp_types::Position::new(0, 99)),
            Err(LspError::PositionOutOfRange { .. })
        ));
        assert!(matches!(
            snapshot.to_utf16(LspPosition::new(9, 0)),
            Err(LspError::PositionOutOfRange { version: 1, .. })
        ));
        assert_eq!(
            snapshot.range_from_utf16(lsp_types::Range::new(
                lsp_types::Position::new(0, 3),
                lsp_types::Position::new(0, 0),
            )),
            Err(LspError::InvertedRange)
        );
    }

    #[test]
    fn a_crlf_terminator_is_not_part_of_its_line() {
        let snapshot = Snapshot::new(1, Arc::from("ab\r\ncd\r\n"));
        assert_eq!(snapshot.line_content(0), Some("ab"));
        assert_eq!(snapshot.line_content(1), Some("cd"));
        assert_eq!(snapshot.line_content(2), Some(""));
        assert_eq!(
            snapshot.to_utf16(LspPosition::new(0, 2)).expect("line end"),
            lsp_types::Position::new(0, 2)
        );
        assert!(snapshot.to_utf16(LspPosition::new(0, 3)).is_err());
    }

    #[test]
    fn incremental_conversion_measures_each_edit_against_the_text_it_applied_to() {
        let base = Snapshot::new(1, Arc::from("a𝄞b\n"));
        let edits = vec![
            // Delete the astral scalar.
            LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 2)),
                new_text: String::new(),
            },
            // Then append, at a position that only exists after the deletion.
            LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 2)),
                new_text: "!".to_owned(),
            },
        ];
        let changes = build_incremental(&base, &edits, "ab!\n").expect("convertible");
        assert_eq!(
            changes[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 1),
                lsp_types::Position::new(0, 3)
            )),
            "the astral scalar spans two UTF-16 units"
        );
        assert_eq!(
            changes[1].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 2),
                lsp_types::Position::new(0, 2)
            )),
            "the second edit is measured after the first applied"
        );

        let impossible = [LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 99), LspPosition::new(0, 99)),
            new_text: "x".to_owned(),
        }];
        assert_eq!(
            build_incremental(&base, &impossible, "a𝄞bx\n"),
            Err(LspError::PositionOutOfRange {
                line: 0,
                character: 99,
                version: 1
            })
        );

        // An edit list that converts cleanly but does not produce the buffer
        // the editor reports is refused, because sending it would leave the
        // server permanently out of step with a text it can no longer see.
        let lying = [LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1)),
            new_text: "z".to_owned(),
        }];
        assert_eq!(
            build_incremental(&base, &lying, "something else entirely\n"),
            Err(LspError::InconsistentEdits)
        );
        assert!(build_incremental(&base, &lying, "z𝄞b\n").is_ok());
    }

    /// Applies one keystroke to a large document and fails if that grew the
    /// process by anything like a copy of it.
    ///
    /// The editor pushes one edit per update and the widget calls the
    /// conversion from the COSMIC update thread, so work proportional to the
    /// file is felt as typing latency on every character. Peak resident
    /// memory is the observable, and it is process-wide, which is why this
    /// runs as a child of
    /// `one_keystroke_does_not_materialise_the_document`: in the suite's own
    /// process the rest of the tests would drown it.
    #[test]
    #[cfg(target_os = "linux")]
    fn child_helper_converts_one_keystroke() {
        let Some(marker) = std::env::var_os(KEYSTROKE_PROBE_ENV) else {
            return;
        };
        // Large enough that a copy of it dwarfs whatever the process happens
        // to touch while the conversion runs.
        let text = "fn main() {}\n".repeat(2_580_000);
        let expected = format!("x{text}");
        let base = Snapshot::new(1, Arc::from(text.as_str()));
        let edits = [LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
            new_text: "x".to_owned(),
        }];
        let before = peak_resident();
        let changes = build_incremental(&base, &edits, &expected).expect("one insertion converts");
        let grown = peak_resident() - before;
        assert_eq!(changes.len(), 1);
        assert!(
            grown < text.len() / 4,
            "one edit grew the process by {grown} bytes while editing {} bytes",
            text.len()
        );
        // Written last, so its presence means the measurement above happened
        // and passed. The parent has no other way to tell this body apart from
        // a filter that selected nothing.
        std::fs::write(&marker, "converted").expect("publish the probe marker");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn one_keystroke_does_not_materialise_the_document() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("probe-ran");
        let executable = std::env::current_exe().expect("the test executable");
        let output = std::process::Command::new(executable)
            .args(["--exact", KEYSTROKE_PROBE_TEST, "--test-threads=1"])
            .env(KEYSTROKE_PROBE_ENV, &marker)
            .output()
            .expect("spawn the test executable");
        let report = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "converting one edit must not cost a copy of the document: {report}"
        );
        // `libtest` exits zero when `--exact` matches no test, so the exit
        // status alone is also what a probe that never ran would earn:
        // renaming the child test or the variable it looks for would leave
        // this test passing while measuring nothing. The marker is written by
        // the probe itself, after its assertion.
        assert!(
            marker.exists(),
            "`{KEYSTROKE_PROBE_TEST}` never ran, so nothing was measured: {report}"
        );
    }

    /// Pins each comparison [`splices_into`] makes, on a batch whose last edit
    /// is measured against an intermediate document rather than against
    /// `base`.
    ///
    /// The conversion never builds the spliced text, so "the edits produce the
    /// reported buffer" is decided by a length check plus three byte-range
    /// comparisons — head, insertion, tail. Every rejection below is the same
    /// length as the buffer the batch really produces except the last, so none
    /// of them can be caught by the length check standing in for a comparison
    /// that is missing or misaddressed.
    #[test]
    fn a_batch_is_accepted_only_when_its_last_edit_lands_on_the_reported_buffer() {
        let base = Snapshot::new(1, Arc::from("hello world\n"));
        let edits = [
            LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 5)),
                new_text: "goodbye".to_owned(),
            },
            // Addressed in the coordinates the first edit produced: character
            // 13 does not exist in `base` at all.
            LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 8), LspPosition::new(0, 13)),
                new_text: "there".to_owned(),
            },
        ];
        assert!(build_incremental(&base, &edits, "goodbye there\n").is_ok());
        for (reported, comparison) in [
            ("goodbyE there\n", "the text before the last edit"),
            ("goodbye where\n", "the text the last edit inserts"),
            ("goodbye therex", "the text after the last edit"),
            ("goodbye there\n\n", "the length of the result"),
        ] {
            assert_eq!(
                build_incremental(&base, &edits, reported),
                Err(LspError::InconsistentEdits),
                "{reported:?} disagrees with the batch on {comparison}"
            );
        }
    }

    #[test]
    fn snippet_tab_stops_are_expanded_in_visit_order() {
        let snippet = parse_snippet("fn ${1:name}(${2}) {\n    $0\n}");
        assert_eq!(snippet.text, "fn name() {\n    \n}");
        assert_eq!(
            snippet
                .stops
                .iter()
                .map(|stop| stop.index)
                .collect::<Vec<u32>>(),
            vec![1, 2, 0],
            "$0 is visited last"
        );
        assert_eq!(&snippet.text[snippet.stops[0].range.clone()], "name");
        assert!(snippet.stops[1].range.is_empty());

        assert_eq!(parse_snippet("cost: \\$5").text, "cost: $5");
        assert_eq!(parse_snippet("${1|first,second|}").text, "first");
        assert_eq!(parse_snippet("${TM_FILENAME}x").text, "x");
        assert_eq!(parse_snippet("${1:${2:deep}}").text, "deep");
        assert_eq!(parse_snippet("${unterminated").text, "${unterminated");
    }

    #[test]
    fn a_document_that_was_never_opened_is_reported_rather_than_ignored() {
        let mut manager = rust_manager();
        let events = manager.events();
        let request = manager.request_hover(LspDocumentId(9), LspPosition::new(0, 0));
        let event = events.try_recv().expect("an immediate failure event");
        assert_eq!(
            event,
            LspEvent::RequestFailed {
                request,
                doc: LspDocumentId(9),
                error: LspError::UnknownDocument(9),
            }
        );
        assert_eq!(manager.status(LspDocumentId(9)), LspStatus::Disabled);
    }

    // -----------------------------------------------------------------
    // Protocol behaviour against the fake server
    // -----------------------------------------------------------------

    #[test]
    fn the_initialize_response_strictly_precedes_initialized() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            // This fake answers `initialize` and nothing else, so
            // `initialized` cannot appear before that response is decoded.
            let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            assert_eq!(
                manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "fn main() {}\n"),
                LspStatus::Starting
            );

            let handle = handle_of(&slot).await;
            handle.wait_for("initialized").await;
            let methods = handle.methods();
            let initialize = methods
                .iter()
                .position(|method| method == "initialize")
                .expect("initialize was sent");
            let initialized = methods
                .iter()
                .position(|method| method == "initialized")
                .expect("initialized was sent");
            assert!(
                initialize < initialized,
                "initialize must precede initialized, saw {methods:?}"
            );
            // Both directions of Content-Length framing worked: the client
            // parsed a framed reply and the fake parsed framed requests.
            assert!(handle.written.load(Ordering::Acquire) >= 1);
            await_ready(&events, doc).await;
            assert_eq!(manager.status(doc), LspStatus::Ready);
            manager.shutdown();
        });
    }

    #[test]
    fn shutdown_interrupts_an_unanswered_initialize() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = Arc::new(Mutex::new(None));
            let sink = Arc::clone(&slot);
            manager.set_transport(Arc::new(move |_| {
                let (transport, handle) = fake_transport(|_| Vec::new());
                *sink.lock().unwrap() = Some(handle);
                Ok(transport)
            }));
            manager.open_for_test(
                LspDocumentId(1),
                &file,
                &worktree,
                rust_language(),
                1,
                "fn main() {}\n",
            );
            let handle = handle_of(&slot).await;
            handle.wait_for("initialize").await;
            tokio::time::timeout(Duration::from_secs(1), manager.begin_shutdown())
                .await
                .expect(
                    "initializing servers must not hold the application open for the reap deadline",
                );
            assert!(
                !handle
                    .methods()
                    .iter()
                    .any(|method| method == "initialized")
            );
        });
    }

    #[test]
    fn negotiated_none_sync_suppresses_open_change_save_and_close() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            // `textDocumentSync` absent entirely: the specification default is
            // no synchronisation at all.
            let slot = install_fake(&mut manager, json!({}), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            manager.change(
                doc,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                    new_text: "b".to_owned(),
                }],
                "ab\n",
            );
            manager.save(doc, "ab\n");
            manager.close(doc);
            tokio::time::sleep(CHANGE_DEBOUNCE * 4).await;
            let methods = handle.methods();
            for suppressed in [
                "textDocument/didOpen",
                "textDocument/didChange",
                "textDocument/didSave",
                "textDocument/didClose",
            ] {
                assert!(
                    !methods.iter().any(|method| method == suppressed),
                    "{suppressed} must not be sent, saw {methods:?}"
                );
            }
            manager.shutdown();
        });
    }

    #[test]
    fn negotiated_full_sync_sends_the_whole_document_and_the_saved_text() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, full_sync_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            let open = handle.wait_for("textDocument/didOpen").await;
            assert_eq!(
                open.params["textDocument"]["languageId"],
                json!("rust-analyzer")
            );

            manager.change(
                doc,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                    new_text: "b".to_owned(),
                }],
                "ab\n",
            );
            let change = handle.wait_for("textDocument/didChange").await;
            let changes = change.params["contentChanges"]
                .as_array()
                .expect("contentChanges");
            assert_eq!(changes.len(), 1);
            assert!(
                changes[0].get("range").is_none(),
                "full sync must omit ranges, saw {:?}",
                changes[0]
            );
            assert_eq!(changes[0]["text"], json!("ab\n"));

            manager.save(doc, "ab\n");
            let save = handle.wait_for("textDocument/didSave").await;
            assert_eq!(save.params["text"], json!("ab\n"));

            manager.close(doc);
            handle.wait_for("textDocument/didClose").await;
            manager.shutdown();
        });
    }

    #[test]
    fn negotiated_incremental_sync_sends_converted_ranges_and_increasing_versions() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a𝄞b\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            let open = handle.wait_for("textDocument/didOpen").await;
            assert_eq!(open.params["textDocument"]["version"], json!(1));

            manager.change(
                doc,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 3)),
                    new_text: "B".to_owned(),
                }],
                "a𝄞B\n",
            );
            let first = handle.wait_for("textDocument/didChange").await;
            assert_eq!(first.params["textDocument"]["version"], json!(2));
            let changes = first.params["contentChanges"]
                .as_array()
                .expect("contentChanges");
            assert_eq!(
                changes[0]["range"],
                json!({
                    "start": { "line": 0, "character": 3 },
                    "end": { "line": 0, "character": 4 },
                }),
                "the astral scalar shifts UTF-16 offsets by one"
            );
            assert_eq!(changes[0]["text"], json!("B"));

            // The editor re-uses version 2; the wire version still advances.
            manager.change(doc, 2, &[], "a𝄞BC\n");
            let all = handle.wait_for_count("textDocument/didChange", 2).await;
            let versions: Vec<i64> = all
                .iter()
                .map(|entry| {
                    entry.params["textDocument"]["version"]
                        .as_i64()
                        .expect("version")
                })
                .collect();
            assert!(
                versions.windows(2).all(|pair| pair[0] < pair[1]),
                "versions must strictly increase, saw {versions:?}"
            );
            // An edit batch that cannot be converted resynchronises the whole
            // document instead of sending a wrong range.
            manager.change(
                doc,
                9,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(40, 0), LspPosition::new(40, 1)),
                    new_text: "z".to_owned(),
                }],
                "resynchronised\n",
            );
            let desync = next_matching(&events, |event| match event {
                LspEvent::DocumentDesynced { doc: at, error, .. } if *at == doc => {
                    Some(error.clone())
                }
                _ => None,
            })
            .await;
            assert!(matches!(desync, LspError::PositionOutOfRange { .. }));
            let resend = handle.wait_for_count("textDocument/didChange", 3).await;
            let last = resend.last().expect("third change");
            let changes = last.params["contentChanges"]
                .as_array()
                .expect("contentChanges");
            assert!(
                changes[0].get("range").is_none(),
                "an unconvertible batch falls back to a whole-document change"
            );
            assert_eq!(changes[0]["text"], json!("resynchronised\n"));
            manager.shutdown();
        });
    }

    #[test]
    fn debounced_edits_coalesce_into_one_notification() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            handle.wait_for("textDocument/didOpen").await;

            let mut text = String::new();
            for step in 0..5u32 {
                text.push('x');
                manager.change(
                    doc,
                    i32::try_from(step).expect("small") + 2,
                    &[LspTextEdit {
                        range: LspRange::new(LspPosition::new(0, step), LspPosition::new(0, step)),
                        new_text: "x".to_owned(),
                    }],
                    &text,
                );
            }
            let change = handle.wait_for("textDocument/didChange").await;
            assert_eq!(
                change.params["contentChanges"]
                    .as_array()
                    .expect("contentChanges")
                    .len(),
                5,
                "five debounced edits arrive as one notification"
            );
            assert_eq!(change.params["textDocument"]["version"], json!(6));
            assert_eq!(
                handle.entries("textDocument/didChange").len(),
                1,
                "debouncing must not produce one notification per keystroke"
            );
            manager.shutdown();
        });
    }

    #[test]
    fn diagnostics_round_trip_into_editor_coordinates() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "a𝄞b\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            let uri = Url::from_file_path(&file).expect("uri").to_string();
            handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": uri,
                    "version": 1,
                    "diagnostics": [
                        {
                            "range": {
                                "start": { "line": 0, "character": 3 },
                                "end": { "line": 0, "character": 4 },
                            },
                            "severity": 2,
                            "code": "E0001",
                            "source": "fake",
                            "message": "watch out",
                        },
                        {
                            "range": {
                                "start": { "line": 44, "character": 0 },
                                "end": { "line": 44, "character": 1 },
                            },
                            "message": "outside the snapshot",
                        },
                    ],
                }),
            );
            let (version, items) = next_matching(&events, |event| match event {
                LspEvent::Diagnostics {
                    doc: at,
                    version,
                    items,
                } if *at == doc => Some((*version, items.clone())),
                _ => None,
            })
            .await;
            assert_eq!(version, 1);
            assert_eq!(
                items.len(),
                1,
                "an unconvertible item is dropped, not the whole batch"
            );
            assert_eq!(
                items[0].range,
                LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 3)),
                "UTF-16 offsets convert back to scalar offsets"
            );
            assert_eq!(items[0].severity, LspSeverity::Warning);
            assert_eq!(items[0].code.as_deref(), Some("E0001"));
            assert_eq!(items[0].source.as_deref(), Some("fake"));
            manager.shutdown();
        });
    }

    #[test]
    fn hover_completion_definition_and_references_round_trip() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let outside = tempfile::tempdir().expect("outside tempdir");
            let outside_file = outside.path().join("vendor.rs");
            let document_uri = Url::from_file_path(&file).expect("uri").to_string();
            let outside_uri = Url::from_file_path(&outside_file).expect("uri").to_string();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), move |incoming| {
                let result = match incoming.method.as_str() {
                    "textDocument/hover" => json!({
                        "contents": { "kind": "markdown", "value": "**docs**" },
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 },
                        },
                    }),
                    "textDocument/completion" => json!([
                        {
                            "label": "edited",
                            "kind": 3,
                            "detail": "fn()",
                            "documentation": "call it",
                            "preselect": true,
                            "textEdit": {
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 1 },
                                },
                                "newText": "replacement",
                            },
                        },
                        { "label": "insert_text_item", "insertText": "from_insert_text" },
                        { "label": "label_only" },
                        {
                            "label": "snippet",
                            "insertTextFormat": 2,
                            "insertText": "wrap(${1:arg})$0",
                        },
                        {
                            "label": "insert_replace",
                            "textEdit": {
                                "newText": "both",
                                "insert": {
                                    "start": { "line": 0, "character": 1 },
                                    "end": { "line": 0, "character": 1 },
                                },
                                "replace": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 2 },
                                },
                            },
                        },
                    ]),
                    "textDocument/definition" => json!({
                        "uri": document_uri,
                        "range": {
                            "start": { "line": 0, "character": 1 },
                            "end": { "line": 0, "character": 2 },
                        },
                    }),
                    "textDocument/references" => json!([
                        {
                            "uri": outside_uri,
                            "range": {
                                "start": { "line": 4, "character": 0 },
                                "end": { "line": 4, "character": 3 },
                            },
                        },
                        {
                            "uri": "jdt://contents/Thing.class",
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 1 },
                            },
                        },
                    ]),
                    _ => return Vec::new(),
                };
                vec![json!({ "jsonrpc": "2.0", "id": incoming.id, "result": result })]
            });
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abcdef\n");
            let _handle = handle_of(&slot).await;
            await_ready(&events, doc).await;

            let hover = manager.request_hover(doc, LspPosition::new(0, 1));
            let (markdown, range) = next_matching(&events, |event| match event {
                LspEvent::Hover {
                    request,
                    markdown,
                    range,
                    ..
                } if *request == hover => Some((markdown.clone(), *range)),
                _ => None,
            })
            .await;
            assert_eq!(markdown.as_deref(), Some("**docs**"));
            assert_eq!(
                range,
                Some(LspRange::new(
                    LspPosition::new(0, 0),
                    LspPosition::new(0, 1)
                ))
            );

            let completion = manager.request_completion(doc, LspPosition::new(0, 3));
            let items = next_matching(&events, |event| match event {
                LspEvent::Completion { request, items, .. } if *request == completion => {
                    Some(items.clone())
                }
                _ => None,
            })
            .await;
            assert_eq!(items.len(), 5);
            // `textEdit` wins over everything else.
            assert_eq!(items[0].edit.new_text, "replacement");
            assert_eq!(
                items[0].edit.range,
                LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1))
            );
            assert_eq!(items[0].kind, Some(LspCompletionKind::Function));
            assert!(items[0].preselect);
            assert_eq!(items[0].documentation.as_deref(), Some("call it"));
            // Then `insertText`, replacing the identifier prefix.
            assert_eq!(items[1].edit.new_text, "from_insert_text");
            assert_eq!(
                items[1].edit.range,
                LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 3)),
                "the prefix `abc` before the cursor is replaced"
            );
            // Then `label`.
            assert_eq!(items[2].edit.new_text, "label_only");
            // Snippets expand to literal text plus tab stops.
            assert_eq!(items[3].edit.new_text, "wrap(arg)");
            let snippet = items[3].edit.snippet.as_ref().expect("snippet");
            assert_eq!(snippet.text, "wrap(arg)");
            assert_eq!(
                snippet
                    .stops
                    .iter()
                    .map(|stop| stop.index)
                    .collect::<Vec<u32>>(),
                vec![1, 0]
            );
            assert!(items[0].edit.snippet.is_none(), "plain text stays plain");
            // `InsertReplaceEdit` exposes both ranges so the editor chooses.
            assert_eq!(
                items[4].edit.range,
                LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1))
            );
            assert_eq!(
                items[4].edit.replace_range,
                Some(LspRange::new(
                    LspPosition::new(0, 0),
                    LspPosition::new(0, 2)
                ))
            );

            let definition = manager.request_definition(doc, LspPosition::new(0, 1));
            let items = next_matching(&events, |event| match event {
                LspEvent::Locations {
                    request,
                    kind,
                    items,
                    ..
                } if *request == definition && *kind == LocationKind::Definition => {
                    Some(items.clone())
                }
                _ => None,
            })
            .await;
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].target, LspTarget::Workspace { path: file.clone() });
            assert_eq!(items[0].coordinates, LspCoordinates::Scalar);

            let references = manager.request_references(doc, LspPosition::new(0, 1));
            let items = next_matching(&events, |event| match event {
                LspEvent::Locations {
                    request,
                    kind,
                    items,
                    ..
                } if *request == references && *kind == LocationKind::References => {
                    Some(items.clone())
                }
                _ => None,
            })
            .await;
            assert_eq!(items.len(), 2);
            assert_eq!(
                items[0].target,
                LspTarget::External {
                    path: outside_file.clone()
                },
                "a file outside the worktree opens read-only"
            );
            assert_eq!(
                items[0].coordinates,
                LspCoordinates::Utf16,
                "a target whose text is not open keeps protocol coordinates"
            );
            assert_eq!(
                items[1].target,
                LspTarget::Uri {
                    uri: "jdt://contents/Thing.class".to_owned()
                },
                "a non-file URI is listed but never written"
            );
            manager.shutdown();
        });
    }

    #[test]
    fn a_result_computed_for_an_older_version_is_rejected() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            // The fake holds the hover reply until the document has already
            // changed, which is exactly the race the version check exists for.
            let held: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
            let observer = Arc::clone(&held);
            let slot = install_fake(&mut manager, incremental_capabilities(), move |incoming| {
                match incoming.method.as_str() {
                    "textDocument/hover" => {
                        *observer.lock().expect("held") = incoming.id;
                        Vec::new()
                    }
                    "textDocument/didChange" => match observer.lock().expect("held").take() {
                        Some(id) => vec![json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": { "contents": "computed for version 1" },
                        })],
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                }
            });
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;

            let hover = manager.request_hover(doc, LspPosition::new(0, 1));
            handle.wait_for("textDocument/hover").await;
            manager.change(
                doc,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 3)),
                    new_text: "d".to_owned(),
                }],
                "abcd\n",
            );
            let error = next_matching(&events, |event| match event {
                LspEvent::RequestFailed { request, error, .. } if *request == hover => {
                    Some(error.clone())
                }
                _ => None,
            })
            .await;
            assert_eq!(
                error,
                LspError::StaleVersion {
                    result: 1,
                    current: 2
                },
                "a hover computed for version 1 must not be applied to version 2"
            );
            manager.shutdown();
        });
    }

    #[test]
    fn a_superseded_request_is_cancelled_on_the_wire() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            // Completion is never answered, so the first request stays in
            // flight until it is superseded.
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;

            let first = manager.request_completion(doc, LspPosition::new(0, 1));
            let sent = handle.wait_for("textDocument/completion").await;
            let wire_id = sent.id.expect("requests carry a JSON-RPC id");
            let second = manager.request_completion(doc, LspPosition::new(0, 2));
            assert_ne!(first, second);

            let cancel = handle.wait_for("$/cancelRequest").await;
            assert_eq!(
                cancel.params["id"],
                json!(wire_id),
                "the cancellation must name the superseded JSON-RPC id"
            );
            let error = next_matching(&events, |event| match event {
                LspEvent::RequestFailed { request, error, .. } if *request == first => {
                    Some(error.clone())
                }
                _ => None,
            })
            .await;
            assert_eq!(error, LspError::Cancelled);

            // An explicit cancel reaches the wire with the newer id.
            let second_sent = handle.wait_for_count("textDocument/completion", 2).await;
            let second_id = second_sent.last().expect("second request").id.expect("id");
            assert_ne!(second_id, wire_id, "each request gets a fresh id");
            manager.cancel(second);
            let cancels = handle.wait_for_count("$/cancelRequest", 2).await;
            assert_eq!(
                cancels.last().expect("second cancel").params["id"],
                json!(second_id)
            );
            manager.shutdown();
        });
    }

    #[test]
    fn the_event_channel_stays_bounded_under_diagnostic_flooding() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            let uri = Url::from_file_path(&file).expect("uri").to_string();

            // Comfortably past the channel bound without turning this test
            // into a load generator for the rest of the suite.
            const BATCHES: u32 = 600;
            for index in 0..BATCHES {
                handle.notify(
                    "textDocument/publishDiagnostics",
                    json!({
                        "uri": uri,
                        "diagnostics": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 1 },
                            },
                            "message": format!("batch {index}"),
                        }],
                    }),
                );
            }
            // Nothing drains the channel while the flood is processed.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(
                    events.len() <= EVENT_CHANNEL_CAPACITY,
                    "the event channel grew past its bound: {}",
                    events.len()
                );
                if events.len() == EVENT_CHANNEL_CAPACITY {
                    break;
                }
            }
            assert_eq!(
                events.len(),
                EVENT_CHANNEL_CAPACITY,
                "the flood should have filled the channel exactly to its bound"
            );
            // Coalescing keeps the newest batch per document, so the newest
            // diagnostics still reach the application once draining resumes.
            let newest = format!("batch {}", BATCHES - 1);
            let mut seen_newest = false;
            for _ in 0..(EVENT_CHANNEL_CAPACITY * 8) {
                let Ok(Ok(event)) =
                    tokio::time::timeout(Duration::from_secs(3), events.recv()).await
                else {
                    break;
                };
                if let LspEvent::Diagnostics { items, .. } = event {
                    if items.first().is_some_and(|item| item.message == newest) {
                        seen_newest = true;
                        break;
                    }
                }
            }
            assert!(
                seen_newest,
                "the newest diagnostics must survive coalescing"
            );
            manager.shutdown();
        });
    }

    #[test]
    fn the_stderr_tail_is_bounded_in_lines_and_in_line_length() {
        block_on(async {
            let (mut writer, reader) = tokio::io::duplex(1 << 16);
            let tail = Arc::new(StderrTail::default());
            let drain = tokio::spawn(drain_stderr(Box::new(reader), Arc::clone(&tail)));
            for index in 0..1_000u32 {
                writer
                    .write_all(format!("line {index}\n").as_bytes())
                    .await
                    .expect("write");
            }
            // One enormous line with no terminator until the very end.
            writer.write_all(&vec![b'x'; 200_000]).await.expect("write");
            writer.write_all(b"\n").await.expect("write");
            drop(writer);
            let _ = tokio::time::timeout(WAIT, drain).await;

            let lines = tail.snapshot();
            assert_eq!(lines.len(), STDERR_TAIL_LINES, "only the tail is retained");
            assert!(
                lines
                    .iter()
                    .all(|line| line.len() <= STDERR_TAIL_LINE_BYTES),
                "an unterminated line must not grow the tail without bound"
            );
            assert!(
                lines.iter().any(|line| line == "line 999"),
                "the newest lines are the ones kept"
            );
        });
    }

    #[test]
    fn a_crashed_server_reports_a_restartable_status_and_keeps_the_document() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            // A malformed frame after `initialized` fails the client's main
            // loop exactly the way a crashed server does.
            let slot = install_fake(&mut manager, incremental_capabilities(), |incoming| {
                if incoming.method == "initialized" {
                    return vec![json!({ "jsonrpc": "2.0", "method": 42 })];
                }
                Vec::new()
            });
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let _handle = handle_of(&slot).await;
            let status = next_matching(&events, |event| match event {
                LspEvent::Status { doc: at, status } if *at == doc => match status {
                    LspStatus::Failed { .. } => Some(status.clone()),
                    _ => None,
                },
                _ => None,
            })
            .await;
            assert!(status.can_restart(), "a crash must offer Restart");
            assert_eq!(manager.status(doc), status);

            // Editing keeps working: the buffer is still tracked and the
            // snapshot advances even though no server is listening.
            manager.change(
                doc,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 3)),
                    new_text: "d".to_owned(),
                }],
                "abcd\n",
            );
            let uri = Url::from_file_path(&file).expect("uri");
            assert_eq!(
                &*manager
                    .snapshots
                    .latest(&uri)
                    .expect("the snapshot survives the crash")
                    .text,
                "abcd\n"
            );

            *slot.lock().expect("transport slot") = None;
            manager.restart(doc);
            let restarted = handle_of(&slot).await;
            restarted.wait_for("initialize").await;
            let reopened = restarted.wait_for("textDocument/didOpen").await;
            assert_eq!(
                reopened.params["textDocument"]["text"],
                json!("abcd\n"),
                "a restart re-opens the document with the current buffer"
            );
            manager.shutdown();
        });
    }

    /// Brings up a ready session and hands back the fake behind it, so a test
    /// can put arbitrary bytes on the wire of a server the client trusts.
    async fn ready_session(
        manager: &mut LspManager,
        worktree: &Path,
        file: &Path,
        doc: LspDocumentId,
    ) -> (FakeHandle, async_channel::Receiver<LspEvent>) {
        let slot = install_fake(manager, incremental_capabilities(), |_| Vec::new());
        let events = manager.events();
        manager.open_for_test(doc, file, worktree, rust_language(), 1, "abc\n");
        let handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        (handle, events)
    }

    async fn failure_reason(
        events: &async_channel::Receiver<LspEvent>,
        doc: LspDocumentId,
    ) -> String {
        next_matching(events, |event| match event {
            LspEvent::Status {
                doc: at,
                status: LspStatus::Failed { reason, .. },
            } if *at == doc => Some(reason.clone()),
            _ => None,
        })
        .await
    }

    #[test]
    fn an_absurd_content_length_fails_the_session_instead_of_being_allocated() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let doc = LspDocumentId(1);
            let (handle, events) = ready_session(&mut manager, &worktree, &file, doc).await;
            // A terabyte, declared before a single body byte exists. The
            // framing layer allocates the declared length up front, and a
            // failed allocation aborts this process rather than unwinding.
            handle.write_raw(format!("Content-Length: {}\r\n\r\n", 1u64 << 40).into_bytes());
            let reason = failure_reason(&events, doc).await;
            assert!(
                reason.contains("1099511627776"),
                "the reported failure names the length the server declared: {reason}"
            );
            assert!(
                manager.status(doc).can_restart(),
                "a server that misframes is restartable like any other failure"
            );
        });
    }

    #[test]
    fn a_header_line_that_never_ends_fails_the_session_instead_of_being_buffered() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let doc = LspDocumentId(1);
            let (handle, events) = ready_session(&mut manager, &worktree, &file, doc).await;
            // No newline anywhere: the header reader accumulates until one
            // arrives, so this is the other way stdout grows without bound.
            handle.write_raw(vec![b'x'; 32 * 1024]);
            let reason = failure_reason(&events, doc).await;
            assert!(
                reason.contains("header line"),
                "the reported failure says the header never ended: {reason}"
            );
        });
    }

    /// Feeds `bytes` through the framing guard in small reads, reporting what
    /// a reader downstream would have seen and how the stream ended.
    async fn through_guard(bytes: &[u8]) -> (Vec<u8>, io::Result<()>) {
        let (mut writer, reader) = tokio::io::duplex(1 << 20);
        writer.write_all(bytes).await.expect("write");
        drop(writer);
        let mut guarded = GuardedFrames::new(reader);
        let mut seen = Vec::new();
        let mut chunk = [0u8; 128];
        loop {
            match guarded.read(&mut chunk).await {
                Ok(0) => return (seen, Ok(())),
                Ok(count) => seen.extend_from_slice(&chunk[..count]),
                Err(error) => return (seen, Err(error)),
            }
        }
    }

    #[test]
    fn the_framing_guard_judges_headers_and_never_message_bodies() {
        block_on(async {
            // A body carrying a newline and something that reads like a
            // header: anything scanning message content would desync here.
            let body = "{\"note\":\"x\nContent-Length: 1099511627776\r\n\"}";
            let mut stream = format!("Content-Length: {}\r\n\r\n{body}", body.len());
            stream.push_str(&format!("Content-Length: {MAX_CONTENT_LENGTH}\r\n\r\n"));
            let (seen, outcome) = through_guard(stream.as_bytes()).await;
            assert!(outcome.is_ok(), "well-formed framing passes: {outcome:?}");
            assert_eq!(
                seen.as_slice(),
                stream.as_bytes(),
                "every byte reaches the reader downstream unchanged"
            );

            // Ordinary garbage stays the framing layer's to reject, so the
            // reason it reports remains the accurate one.
            let (seen, outcome) = through_guard(b"HTTP/1.1 200 OK\r\n\r\n").await;
            assert!(outcome.is_ok(), "{outcome:?}");
            assert_eq!(seen.as_slice(), &b"HTTP/1.1 200 OK\r\n\r\n"[..]);

            let (_, outcome) = through_guard(b"Content-Length: 1099511627776\r\n\r\n").await;
            let error = outcome.expect_err("a terabyte is refused");
            assert!(error.to_string().contains("1099511627776"), "{error}");

            // A digit string too large to be a length at all is the same
            // refusal, not a parse that wraps into something plausible.
            let overflowing = format!("Content-Length: {}\r\n\r\n", "9".repeat(40));
            let (_, outcome) = through_guard(overflowing.as_bytes()).await;
            assert!(outcome.is_err(), "an unrepresentable length is refused");

            // No newline anywhere: the reader downstream would accumulate
            // this for as long as the server kept writing.
            let (_, outcome) = through_guard(&vec![b'x'; 8 * MAX_HEADER_LINE_BYTES]).await;
            let error = outcome.expect_err("an endless header line is refused");
            assert!(error.to_string().contains("header line"), "{error}");
        });
    }

    /// A length is whatever the framing layer will read as one, which is not
    /// the same set of strings as "digits".
    #[test]
    fn a_content_length_the_framing_layer_accepts_is_a_length_here_too() {
        block_on(async {
            // `usize::from_str` accepts a leading `+`, so `async_lsp` reads a
            // 60-byte body here. A guard that judged the value by its digits
            // would call this header none of its business, lose track of the
            // body, and never see the terabyte behind it as a header at all.
            let body = "x".repeat(60);
            let mut stream = format!("Content-Length: +{}\r\n\r\n{body}", body.len());
            stream.push_str("Content-Length: 1099511627776\r\n\r\n");
            let (_, outcome) = through_guard(stream.as_bytes()).await;
            let error = outcome.expect_err("the frame behind a signed length is still judged");
            assert!(error.to_string().contains("1099511627776"), "{error}");

            // And the signed length is itself capped, not merely tracked.
            let absurd = format!("Content-Length: +{}\r\n\r\n", 1u64 << 40);
            let (_, outcome) = through_guard(absurd.as_bytes()).await;
            assert!(outcome.is_err(), "a signed absurd length is refused too");

            // A `Content-Length` the framing layer cannot read at all is a
            // protocol error there, so passing it on buys nothing and costs
            // the cap: refusing it keeps the two in step by construction.
            let (_, outcome) = through_guard(b"Content-Length: 60 \r\n\r\n").await;
            assert!(outcome.is_err(), "a length that is not one is refused");
        });
    }

    #[test]
    fn shutdown_requests_shutdown_before_notifying_exit() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            manager.shutdown();
            handle.wait_for("exit").await;
            let methods = handle.methods();
            let shutdown = methods
                .iter()
                .position(|method| method == "shutdown")
                .expect("shutdown was requested");
            let exit = methods
                .iter()
                .position(|method| method == "exit")
                .expect("exit was notified");
            assert!(
                shutdown < exit,
                "shutdown must precede exit, saw {methods:?}"
            );
        });
    }

    #[test]
    fn a_missing_executable_reports_unavailable_and_leaves_editing_working() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            // The real launcher runs here: nothing with this name can exist.
            let mut manager = LspManager::new(vec![LanguageServerSettings {
                id: "rust-analyzer".to_owned(),
                languages: vec![crate::settings::LanguageServerLanguage {
                    language_id: "rust".to_owned(),
                    extensions: vec!["rs".to_owned()],
                }],
                command: vec!["cagents-no-such-language-server".to_owned()],
                root_markers: vec!["Cargo.toml".to_owned()],
                ..LanguageServerSettings::default()
            }]);
            let events = manager.events();
            let doc = LspDocumentId(1);
            // Construction never probes PATH, so opening is what discovers it.
            assert_eq!(
                manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n"),
                LspStatus::Starting
            );
            let status = next_matching(&events, |event| match event {
                LspEvent::Status { doc: at, status } if *at == doc => match status {
                    LspStatus::Unavailable { .. } => Some(status.clone()),
                    _ => None,
                },
                _ => None,
            })
            .await;
            let LspStatus::Unavailable { reason } = &status else {
                panic!("expected Unavailable, got {status:?}");
            };
            assert!(
                reason.contains("cagents-no-such-language-server"),
                "the reason must name the executable: {reason}"
            );
            assert!(status.can_restart());
            manager.change(doc, 2, &[], "abcd\n");
            assert_eq!(
                &*manager
                    .snapshots
                    .latest(&Url::from_file_path(&file).expect("uri"))
                    .expect("snapshot")
                    .text,
                "abcd\n",
                "editing continues without a server"
            );
            manager.shutdown();
        });
    }

    /// Writes a file inside `directory` that this process could execute.
    #[cfg(unix)]
    fn executable(directory: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("mark executable");
        path
    }

    #[test]
    #[cfg(unix)]
    fn a_bare_program_never_resolves_against_the_directory_the_child_will_run_in() {
        use std::ffi::OsString;

        let home = tempfile::tempdir().expect("tempdir");
        let worktree = tempfile::tempdir().expect("tempdir");
        let decoy = tempfile::tempdir().expect("tempdir");
        // The repository ships a program under the configured name, and an
        // earlier PATH entry holds a same-named file that is not executable.
        executable(worktree.path(), "cagents-server");
        std::fs::write(decoy.path().join("cagents-server"), "not a program\n").expect("write");

        // POSIX reads a trailing empty PATH entry as "the current directory",
        // and the child's current directory is the worktree - which is exactly
        // why the lookup may not be left to the child.
        let path = OsString::from(format!("{}:", decoy.path().display()));
        assert_eq!(
            resolve_program_in("cagents-server", Some(path.as_os_str()), home.path()),
            Err("`cagents-server` was not found on PATH".to_owned()),
            "neither a non-executable match nor the worktree may satisfy the lookup"
        );

        let mine = executable(home.path(), "cagents-server");
        let resolved = resolve_program_in("cagents-server", Some(path.as_os_str()), home.path())
            .expect("resolved");
        assert_eq!(resolved, mine, "the application's own directory decides");
        assert!(
            resolved.is_absolute(),
            "an absolute program cannot be reinterpreted by the child's chdir"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_command_naming_a_path_is_anchored_to_the_application_directory() {
        let home = tempfile::tempdir().expect("tempdir");
        let relative = resolve_program_in("bin/server", None, home.path()).expect("resolved");
        assert_eq!(relative, home.path().join("bin/server"));
        assert!(relative.is_absolute());
        // An absolute command is already anchored and is left alone.
        assert_eq!(
            resolve_program_in("/usr/bin/server", None, home.path()),
            Ok(PathBuf::from("/usr/bin/server"))
        );
    }

    #[test]
    fn every_manager_has_its_own_event_channel_generation() {
        let manager = LspManager::new(Vec::new());
        assert_eq!(manager.generation(), manager.generation());
        assert_ne!(
            manager.generation(),
            LspManager::new(Vec::new()).generation(),
            "a replacement manager carries a different channel"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_non_utf8_path_is_refused_instead_of_being_mangled() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut manager = rust_manager();
        let path = PathBuf::from(OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', 0xff, b'.', b'r', b's',
        ]));
        let status = manager.open_for_test(
            LspDocumentId(1),
            &path,
            Path::new("/tmp"),
            rust_language(),
            1,
            "",
        );
        let LspStatus::Unavailable { reason } = status else {
            panic!("a non-UTF-8 path must be refused, got {status:?}");
        };
        assert!(
            reason.contains("not valid UTF-8"),
            "the reason must say why: {reason}"
        );
    }

    #[test]
    fn an_unknown_server_id_is_refused_without_starting_a_session() {
        let mut manager = rust_manager();
        let status = manager.open_for_test(
            LspDocumentId(1),
            Path::new("/tmp/a.rs"),
            Path::new("/tmp"),
            LspLanguage {
                server_id: "not-configured".to_owned(),
                language_id: "rust".to_owned(),
            },
            1,
            "",
        );
        let LspStatus::Unavailable { reason } = status else {
            panic!("expected Unavailable, got {status:?}");
        };
        assert!(reason.contains("not-configured"), "{reason}");
    }

    // -----------------------------------------------------------------
    // Real child processes, reaped without needing a language server
    // -----------------------------------------------------------------

    /// Helper body executed only when this test binary is re-invoked as a
    /// child by `a_live_child_is_group_terminated_and_reaped`. As a normal
    /// test it returns immediately.
    #[test]
    fn child_helper_sleeps_until_terminated() {
        if std::env::var_os(CHILD_HELPER_ENV).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_secs(120));
    }

    /// Helper body executed only when this test binary is re-invoked as a
    /// child by `a_prompt_exit_still_ends_the_server_process_group`.
    ///
    /// Starts a grandchild that outlives it, publishes that grandchild's pid,
    /// and returns at once. The grandchild inherits this process's group,
    /// which is the shape a language server has: `rust-analyzer` starts a
    /// proc-macro server that goes on running after the server itself has
    /// answered `exit` and gone.
    #[test]
    fn child_helper_leaves_a_grandchild_behind() {
        let Some(published) = std::env::var_os(GRANDCHILD_ENV) else {
            return;
        };
        let published = PathBuf::from(published);
        let executable = std::env::current_exe().expect("the test executable");
        let grandchild = std::process::Command::new(executable)
            .args(["--exact", CHILD_HELPER_TEST, "--test-threads=1"])
            .env(CHILD_HELPER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the grandchild");
        // Renamed into place, so the parent cannot read a half-written pid.
        let staged = published.with_extension("staged");
        std::fs::write(&staged, grandchild.id().to_string()).expect("stage the grandchild pid");
        std::fs::rename(&staged, &published).expect("publish the grandchild pid");
    }

    /// Which helper body a re-invoked test binary should run. The environment
    /// variable is what the helper looks for; the test name is what selects it.
    #[cfg(unix)]
    enum Helper<'a> {
        /// Not a helper: the child does what its arguments say and exits.
        None,
        /// [`child_helper_sleeps_until_terminated`].
        Sleeps,
        /// [`child_helper_leaves_a_grandchild_behind`], publishing the
        /// grandchild's pid to this path.
        LeavesAGrandchild(&'a Path),
    }

    /// `reaper` decides whether Tokio may kill the child when the handle is
    /// dropped. A test that must prove the session *itself* stopped the
    /// process passes `false`, so nothing else can end it.
    #[cfg(unix)]
    fn spawn_test_child(arguments: &[&str], helper: Helper<'_>, reaper: bool) -> ChildProcess {
        let executable = std::env::current_exe().expect("the test executable");
        let mut command = tokio::process::Command::new(executable);
        command
            .args(arguments)
            .stdin(Stdio::null())
            // Discarded rather than piped: an unread pipe would kill libtest
            // with SIGPIPE the moment it printed, and this test is about the
            // termination sequence, not about protocol plumbing.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(reaper)
            .process_group(0);
        match helper {
            Helper::None => {}
            Helper::Sleeps => {
                command.env(CHILD_HELPER_ENV, "1");
            }
            Helper::LeavesAGrandchild(published) => {
                command.env(GRANDCHILD_ENV, published);
            }
        }
        let child = command.spawn().expect("spawn the test executable");
        ChildProcess::new(child).expect("capture freshly spawned child identity")
    }

    /// The cell [`ChildProcess::stop`] takes its group id out of, for a test
    /// that stops a child without a session task around it.
    #[cfg(unix)]
    fn published_group(child: &ChildProcess) -> PublishedGroup {
        PublishedGroup(Mutex::new(GroupPublication::Live(child.process_group)))
    }

    // A scheduler may pause shutdown after its final wait reaps the leader.
    // Only the explicit isolated namespace regression installs this one-shot
    // hook; production and ordinary tests never do.
    #[cfg(target_os = "linux")]
    thread_local! {
        pub(super) static LSP_AFTER_REAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires permitted user/PID namespaces; explicit kernel PID-reuse regression"]
    fn lsp_stop_never_signals_a_kernel_reused_group_after_reaping() {
        use std::cell::RefCell;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        use std::rc::Rc;

        const MARKER: &str = "CAGENTS_AUDIT_LSP_REUSE_CHILD";
        const PARENT_NAMESPACE: &str = "CAGENTS_AUDIT_LSP_PARENT_PID_NAMESPACE";
        const TEST: &str = "lsp::tests::lsp_stop_never_signals_a_kernel_reused_group_after_reaping";
        if std::env::var_os(MARKER).is_none() {
            for scenario in ["post-reap", "already-reaped"] {
                let output = std::process::Command::new("/usr/bin/unshare")
                    .args([
                        "--user",
                        "--map-root-user",
                        "--pid",
                        "--fork",
                        "--mount-proc",
                    ])
                    .arg(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        TEST,
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(MARKER, scenario)
                    .env(
                        PARENT_NAMESPACE,
                        std::fs::read_link("/proc/self/ns/pid").unwrap(),
                    )
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stdout.contains("owned LSP PID-reuse scheduler fixture started"),
                    "the exact namespace test must run: {stdout}; {stderr}"
                );
                assert!(
                    output.status.success(),
                    "owned LSP scheduler-window audit failed: {stdout}; {stderr}"
                );
            }
            return;
        }
        let already_reaped = std::env::var(MARKER).unwrap() == "already-reaped";
        println!("owned LSP PID-reuse scheduler fixture started");
        assert_eq!(
            std::process::id(),
            1,
            "only owned namespace init may reset PIDs"
        );

        assert_ne!(
            std::fs::read_link("/proc/self/ns/pid").unwrap().as_os_str(),
            std::env::var_os(PARENT_NAMESPACE).expect("outer namespace identity"),
            "PID allocation must be changed only inside a new owned namespace"
        );

        struct Replacement {
            child: std::process::Child,
            pidfd: OwnedFd,
        }
        impl Drop for Replacement {
            fn drop(&mut self) {
                // SAFETY: the descriptor identifies only our replacement child,
                // including after the numeric PID has become available again.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                let _ = self.child.wait();
            }
        }
        block_on(async {
            // Runtime initialization precedes PID allocation. It cannot create
            // another worker thread while the synchronous hook runs.
            std::fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
            let child = tokio::process::Command::new("/bin/true")
                .process_group(0)
                .spawn()
                .unwrap();
            let group = child.id().unwrap();
            assert_eq!(group, 200);
            let mut child = ChildProcess::new(child).unwrap();
            let published = published_group(&child);
            let replacement = Rc::new(RefCell::new(None::<Replacement>));
            let captured = Rc::clone(&replacement);
            let replace = move || {
                assert!(
                    !Path::new(&format!("/proc/{group}")).exists(),
                    "the actual grace wait must have reaped the original leader"
                );
                std::fs::write("/proc/sys/kernel/ns_last_pid", (group - 1).to_string()).unwrap();
                let child = std::process::Command::new("/bin/sleep")
                    .arg("30")
                    .process_group(0)
                    .spawn()
                    .unwrap();
                assert_eq!(child.id(), group, "kernel must really reuse the PID");
                assert_eq!(unsafe { libc::getpgid(group as i32) }, group as i32);
                let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, group, 0) } as i32;
                assert!(raw >= 0);
                *captured.borrow_mut() = Some(Replacement {
                    child,
                    // SAFETY: pidfd_open returned a new owned descriptor.
                    pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
                });
            };
            if already_reaped {
                child.child.wait().await.unwrap();
                replace();
            } else {
                LSP_AFTER_REAP.with(|hook| *hook.borrow_mut() = Some(Box::new(replace)));
            }
            assert_eq!(
                child.stop(&published).await,
                if already_reaped {
                    ChildStop::Unconfirmed
                } else {
                    ChildStop::Exited
                }
            );
            assert_eq!(published.current(), 0);
            let mut replacement = replacement.borrow_mut();
            let replacement = replacement.as_mut().expect("after-reap hook ran");
            let mut poll = libc::pollfd {
                fd: replacement.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let exited = unsafe { libc::poll(&mut poll, 1, 250) };
            let status = replacement.child.try_wait().unwrap();
            println!(
                "actual stop scenario already_reaped={already_reaped}; reused PID/PGID={group}; replacement readiness={exited}; status={status:?}"
            );
            assert_eq!(
                exited, 0,
                "actual stop killed the unrelated replacement group"
            );
            assert!(status.is_none());
        });
    }

    #[test]
    fn retired_lsp_authority_refuses_late_publication_and_restart_uses_a_new_slot() {
        let retired = PublishedGroup::default();
        retired.kill();
        assert!(!retired.publish(123));
        let replacement = PublishedGroup::default();
        assert!(replacement.publish(456));
        assert!(!replacement.publish(789));
        assert_eq!(replacement.claim(), Some(456));
        assert!(!replacement.publish(789));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn dropping_an_lsp_child_disarms_its_published_group_before_tokio_reaps() {
        block_on(async {
            let mut child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                true,
            );
            let group = child.process_group;
            let published = Arc::new(PublishedGroup::default());
            assert!(child.publish(Arc::clone(&published)));
            drop(child);
            assert_eq!(published.current(), 0);
            assert!(!published.publish(group));
            assert!(wait_until_gone(group).await);
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn losing_lsp_shutdown_claim_waits_for_the_fallback_signal_before_reaping() {
        block_on(async {
            let mut child = spawn_test_child(&["--list"], Helper::None, true);
            assert!(child.settled(WAIT).await.unwrap());
            let group = child.process_group;
            let published = Arc::new(PublishedGroup::default());
            assert!(child.publish(Arc::clone(&published)));
            let completed = Arc::new(AtomicBool::new(false));
            let check = Arc::clone(&completed);
            LSP_AFTER_REAP.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    assert!(
                        check.load(Ordering::Acquire),
                        "fallback signal must precede final reap"
                    );
                }));
            });
            let fallback = Arc::clone(&published);
            let (claimed_tx, claimed_rx) = std::sync::mpsc::channel();
            let (starting_tx, starting_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                // Model preemption between the fallback's claim and syscall,
                // retaining the exact production authority mutex throughout.
                let mut state = fallback.0.lock().unwrap();
                assert!(
                    matches!(std::mem::replace(&mut *state, GroupPublication::Closed), GroupPublication::Live(id) if id == group)
                );
                claimed_tx.send(()).unwrap();
                starting_rx.recv_timeout(WAIT).unwrap();
                signal_process_group(group, SIGKILL);
                completed.store(true, Ordering::Release);
            });
            claimed_rx.recv_timeout(WAIT).unwrap();
            assert!(
                published.0.try_lock().is_err(),
                "pending claimed signal owns the authority lock"
            );
            starting_tx.send(()).unwrap();
            assert_eq!(child.stop(&published).await, ChildStop::Exited);
            thread.join().unwrap();
            assert!(wait_until_gone(group).await);
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn observing_lsp_exit_preserves_the_original_leader_until_group_cleanup() {
        block_on(async {
            let child = spawn_test_child(&["--list"], Helper::None, true);
            let group = child.process_group;
            assert!(child.settled(WAIT).await.unwrap());
            assert!(child.observed_exit().unwrap());
            assert_eq!(child.child.id(), Some(group));
            let stat = std::fs::read_to_string(format!("/proc/{group}/stat")).unwrap();
            assert_eq!(
                stat.rsplit_once(") ").unwrap().1.split_whitespace().next(),
                Some("Z")
            );
            let published = published_group(&child);
            assert_eq!(child.stop(&published).await, ChildStop::Exited);
            assert!(
                wait_until_gone(group).await,
                "final shutdown must reap the leader"
            );
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn reaped_lsp_identity_refuses_group_shutdown() {
        block_on(async {
            let mut child = spawn_test_child(&["--list"], Helper::None, true);
            let published = published_group(&child);
            child.child.wait().await.unwrap();
            assert_eq!(
                child.observed_exit().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            assert_eq!(child.stop(&published).await, ChildStop::Unconfirmed);
            assert_eq!(published.current(), 0);
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cancelling_lsp_shutdown_preserves_tokio_drop_cleanup() {
        block_on(async {
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                true,
            );
            let group = child.process_group;
            let published = published_group(&child);
            assert!(
                tokio::time::timeout(TERMINATION_GRACE / 2, child.stop(&published))
                    .await
                    .is_err()
            );
            assert_eq!(published.current(), 0);
            assert!(
                wait_until_gone(group).await,
                "Tokio must still kill and reap a cancelled shutdown child"
            );
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_child_that_exits_on_its_own_is_reaped_without_being_terminated() {
        block_on(async {
            // `--list` makes libtest print its test names and exit at once.
            let child = spawn_test_child(&["--list"], Helper::None, true);
            let published = published_group(&child);
            assert_eq!(child.stop(&published).await, ChildStop::Exited);
        });
    }

    #[test]
    #[cfg(unix)]
    fn a_live_child_is_group_terminated_and_reaped() {
        block_on(async {
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                true,
            );
            let published = published_group(&child);
            assert_eq!(
                child.stop(&published).await,
                ChildStop::Terminated,
                "a server that ignores `exit` is SIGTERMed by process group and reaped"
            );
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_published_group_is_withdrawn_before_its_leader_can_be_reaped() {
        block_on(async {
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                true,
            );
            let group = child.process_group;
            let published = PublishedGroup(Mutex::new(GroupPublication::Live(group)));
            let mut stop = std::pin::pin!(child.stop(&published));
            // This child ignores everything, so `stop` is still inside its
            // first grace period here — and the id must already be gone from
            // the cell, because the moment it is reaped is the moment the
            // kernel may hand the id to something else. A reader that finds
            // the id after that has no way to tell.
            let observed = tokio::select! {
                _ = &mut stop => panic!("a sleeping child cannot be stopped within the grace period"),
                _ = tokio::time::sleep(TERMINATION_GRACE / 2) => published.current(),
            };
            assert!(
                PathBuf::from(format!("/proc/{group}")).exists(),
                "the leader is still running, so its id is still its own"
            );
            assert_eq!(
                observed, 0,
                "the id must be withdrawn before the reap, not after it"
            );
            stop.await;
        });
    }

    /// Waits for a helper child to publish its grandchild's pid. Its absence
    /// means the helper body never ran, which a `libtest` filter that selects
    /// nothing would otherwise make indistinguishable from success.
    #[cfg(target_os = "linux")]
    async fn published_pid(path: &Path) -> u32 {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                return text.trim().parse().expect("a published pid");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no grandchild pid was published, so the helper never ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A leader that exits promptly is the common case — `rust-analyzer` quits
    /// on `exit` — and its group is exactly what must still be ended: nothing
    /// else can reach the children it left behind.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_prompt_exit_still_ends_the_server_process_group() {
        block_on(async {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("grandchild-pid");
            let child = spawn_test_child(
                &["--exact", GRANDCHILD_TEST, "--test-threads=1"],
                Helper::LeavesAGrandchild(&path),
                true,
            );
            let group = child.process_group;
            let grandchild = published_pid(&path).await;
            assert_ne!(
                grandchild, group,
                "the grandchild is a process of its own, in the leader's group"
            );

            // Nothing has reaped the leader, so it is now a zombie in the
            // group it still leads, and the grace period below sees it exit
            // at once: the case this test is about, rather than a race with
            // libtest's own shutdown.
            assert!(
                wait_until_not_running(group).await,
                "the helper publishes its grandchild's pid and returns"
            );
            let published = published_group(&child);
            assert_eq!(
                child.stop(&published).await,
                ChildStop::Exited,
                "the helper exits on its own, so no signal is needed to end it"
            );
            let gone = wait_until_not_running(grandchild).await;
            if !gone {
                // Only reached when the group was never ended, in which case
                // it still holds this process and its id is therefore still
                // reserved: this is the signal `stop` owed it.
                signal_process_group(group, SIGKILL);
            }
            assert!(
                gone,
                "a server's descendants must go with the process group it led"
            );
        });
    }

    /// Whether the process is gone entirely. A terminated but unreaped child
    /// keeps its `/proc` entry as a zombie, so this also proves reaping.
    #[cfg(target_os = "linux")]
    async fn wait_until_gone(pid: u32) -> bool {
        let entry = PathBuf::from(format!("/proc/{pid}"));
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline {
            if !entry.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_graceful_shutdown_terminates_and_reaps_the_server_process() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            // No `kill_on_drop`: this child outlives every handle unless the
            // session task terminates its process group on purpose, so a
            // forgotten teardown path fails this test instead of hiding.
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                false,
            );
            let pid = child.process_group;
            let mut manager = rust_manager();
            let slot = install_fake_with_child(
                &mut manager,
                incremental_capabilities(),
                |_| Vec::new(),
                Arc::new(Mutex::new(Some(child))),
            );
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            assert!(
                PathBuf::from(format!("/proc/{pid}")).exists(),
                "the child must still be running before shutdown"
            );

            manager.shutdown();
            handle.wait_for("exit").await;
            assert!(
                wait_until_gone(pid).await,
                "the server process must be group-terminated and reaped by the session"
            );
        });
    }

    /// Installs a fake server that keeps one handle per discovered root, so a
    /// test can address each session separately.
    fn install_fake_registry(
        manager: &mut LspManager,
        capabilities: Value,
        handler: impl Fn(&Incoming) -> Vec<Value> + Send + Sync + 'static,
    ) -> Arc<Mutex<Vec<(PathBuf, FakeHandle)>>> {
        let registry: Arc<Mutex<Vec<(PathBuf, FakeHandle)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&registry);
        let handler = Arc::new(handler);
        manager.set_transport(Arc::new(move |spec: &LaunchSpec| {
            let capabilities = capabilities.clone();
            let handler = Arc::clone(&handler);
            let (transport, handle) =
                fake_transport(move |incoming| match incoming.method.as_str() {
                    "initialize" => vec![json!({
                        "jsonrpc": "2.0",
                        "id": incoming.id,
                        "result": { "capabilities": capabilities },
                    })],
                    "shutdown" => vec![json!({
                        "jsonrpc": "2.0",
                        "id": incoming.id,
                        "result": Value::Null,
                    })],
                    _ => handler(incoming),
                });
            sink.lock()
                .expect("registry")
                .push((spec.root.clone(), handle));
            Ok(transport)
        }));
        registry
    }

    async fn handle_for(
        registry: &Arc<Mutex<Vec<(PathBuf, FakeHandle)>>>,
        root: &Path,
    ) -> FakeHandle {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some((_, handle)) = registry
                .lock()
                .expect("registry")
                .iter()
                .find(|(at, _)| at == root)
            {
                return handle.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no transport was created for {}",
                root.display()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[test]
    fn two_files_in_one_root_share_one_session_and_debounce_independently() {
        block_on(async {
            let root = tempfile::tempdir().expect("tempdir");
            let worktree = root.path().to_path_buf();
            std::fs::write(worktree.join("Cargo.toml"), "").expect("marker");
            let first = worktree.join("first.rs");
            let second = worktree.join("second.rs");
            std::fs::write(&first, "a\n").expect("write");
            std::fs::write(&second, "b\n").expect("write");

            let mut manager = rust_manager();
            let registry =
                install_fake_registry(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let one = LspDocumentId(1);
            let two = LspDocumentId(2);
            manager.open_for_test(one, &first, &worktree, rust_language(), 1, "a\n");
            manager.open_for_test(two, &second, &worktree, rust_language(), 1, "b\n");
            let handle = handle_for(&registry, &worktree).await;
            await_ready(&events, one).await;
            await_ready(&events, two).await;

            assert_eq!(
                registry.lock().expect("registry").len(),
                1,
                "two files under one root must share one process"
            );
            assert_eq!(manager.sessions.len(), 1);
            handle.wait_for_count("textDocument/didOpen", 2).await;

            // One edit to `first`, then a stream of edits to `second`. A
            // session-wide debounce timer would let `second` postpone
            // `first`'s notification indefinitely.
            manager.change(
                one,
                2,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 1), LspPosition::new(0, 1)),
                    new_text: "x".to_owned(),
                }],
                "ax\n",
            );
            let flood = tokio::time::Instant::now() + CHANGE_DEBOUNCE * 6;
            let mut text = String::from("b\n");
            let mut version = 2;
            while tokio::time::Instant::now() < flood {
                text.insert(1, 'y');
                version += 1;
                manager.change(two, version, &[], &text);
                tokio::time::sleep(CHANGE_DEBOUNCE / 3).await;
            }

            let first_uri = Url::from_file_path(&first).expect("uri").to_string();
            let changes = handle.entries("textDocument/didChange");
            assert!(
                changes
                    .iter()
                    .any(|entry| entry.params["textDocument"]["uri"] == json!(first_uri)),
                "the quiet file's change must be flushed on its own deadline, saw {:?}",
                changes
                    .iter()
                    .map(|entry| entry.params["textDocument"]["uri"].clone())
                    .collect::<Vec<Value>>()
            );
            manager.shutdown();
        });
    }

    #[test]
    fn progress_reaches_only_the_documents_of_the_emitting_session() {
        block_on(async {
            let first_root = tempfile::tempdir().expect("tempdir");
            let second_root = tempfile::tempdir().expect("tempdir");
            let first = first_root.path().join("one.rs");
            let second = second_root.path().join("two.rs");
            std::fs::write(&first, "a\n").expect("write");
            std::fs::write(&second, "b\n").expect("write");

            let mut manager = rust_manager();
            let registry =
                install_fake_registry(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let one = LspDocumentId(1);
            let two = LspDocumentId(2);
            manager.open_for_test(one, &first, first_root.path(), rust_language(), 1, "a\n");
            manager.open_for_test(two, &second, second_root.path(), rust_language(), 1, "b\n");
            let first_handle = handle_for(&registry, first_root.path()).await;
            let _second_handle = handle_for(&registry, second_root.path()).await;
            await_ready(&events, one).await;
            await_ready(&events, two).await;
            assert_eq!(
                manager.sessions.len(),
                2,
                "distinct roots must not share a session"
            );

            first_handle.notify(
                "$/progress",
                json!({
                    "token": "indexing",
                    "value": { "kind": "begin", "title": "Indexing", "percentage": 10 },
                }),
            );
            let doc = next_matching(&events, |event| match event {
                LspEvent::Progress { doc, progress } if progress.token == "indexing" => Some(*doc),
                _ => None,
            })
            .await;
            assert_eq!(doc, one, "progress belongs to its own session's documents");
            // Nothing else may arrive for the unrelated root.
            tokio::time::sleep(Duration::from_millis(50)).await;
            while let Ok(event) = events.try_recv() {
                if let LspEvent::Progress { doc, .. } = event {
                    assert_ne!(doc, two, "an unrelated server's progress leaked");
                }
            }
            manager.shutdown();
        });
    }

    #[test]
    fn a_second_binding_on_one_uri_resynchronises_and_survives_the_first_close() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let one = LspDocumentId(1);
            let two = LspDocumentId(2);
            manager.open_for_test(one, &file, &worktree, rust_language(), 1, "first\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, one).await;
            handle.wait_for("textDocument/didOpen").await;

            // A second buffer on the same file with different text: the server
            // holds the first text, so it must be resynchronised.
            manager.open_for_test(two, &file, &worktree, rust_language(), 1, "second\n");
            let resync = handle.wait_for("textDocument/didChange").await;
            assert_eq!(
                resync.params["contentChanges"][0]["text"],
                json!("second\n"),
                "a repeated open must resend the authoritative text"
            );
            assert_eq!(
                handle.entries("textDocument/didOpen").len(),
                1,
                "one URI produces exactly one didOpen"
            );

            // A pending edit from the surviving binding must not be discarded
            // by the other binding closing.
            manager.change(
                one,
                5,
                &[LspTextEdit {
                    range: LspRange::new(LspPosition::new(0, 6), LspPosition::new(0, 6)),
                    new_text: "!".to_owned(),
                }],
                "second!\n",
            );
            manager.close(two);
            let changes = handle.wait_for_count("textDocument/didChange", 2).await;
            assert_eq!(
                changes.last().expect("second change").params["textDocument"]["version"],
                json!(5)
            );
            assert!(
                handle.entries("textDocument/didClose").is_empty(),
                "the file is still open in another tab"
            );

            manager.close(one);
            handle.wait_for("textDocument/didClose").await;
            manager.shutdown();
        });
    }

    #[test]
    fn a_diagnostic_batch_for_an_older_version_is_ignored() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let slot = install_fake(&mut manager, incremental_capabilities(), |_| Vec::new());
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let handle = handle_of(&slot).await;
            await_ready(&events, doc).await;
            manager.change(doc, 2, &[], "abcd\n");
            let uri = Url::from_file_path(&file).expect("uri").to_string();

            let stale_range = json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 },
            });
            handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": uri,
                    "version": 1,
                    "diagnostics": [{ "range": stale_range, "message": "computed for version 1" }],
                }),
            );
            handle.notify(
                "textDocument/publishDiagnostics",
                json!({
                    "uri": uri,
                    "version": 2,
                    "diagnostics": [{ "range": stale_range, "message": "computed for version 2" }],
                }),
            );
            let items = next_matching(&events, |event| match event {
                LspEvent::Diagnostics { doc: at, items, .. } if *at == doc => Some(items.clone()),
                _ => None,
            })
            .await;
            assert_eq!(
                items.first().map(|item| item.message.as_str()),
                Some("computed for version 2"),
                "a batch computed for a superseded version must not be published"
            );
            manager.shutdown();
        });
    }

    #[test]
    fn a_dead_session_is_replaced_and_every_open_document_is_reopened() {
        block_on(async {
            let root = tempfile::tempdir().expect("tempdir");
            let worktree = root.path().to_path_buf();
            std::fs::write(worktree.join("Cargo.toml"), "").expect("marker");
            let first = worktree.join("first.rs");
            let second = worktree.join("second.rs");
            std::fs::write(&first, "a\n").expect("write");
            std::fs::write(&second, "b\n").expect("write");

            let mut manager = rust_manager();
            // The first process dies right after `initialized`; the second
            // behaves normally.
            let launches = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&launches);
            let registry =
                install_fake_registry(&mut manager, incremental_capabilities(), move |incoming| {
                    if incoming.method == "initialized"
                        && counter.fetch_add(1, Ordering::AcqRel) == 0
                    {
                        return vec![json!({ "jsonrpc": "2.0", "method": 42 })];
                    }
                    Vec::new()
                });
            let events = manager.events();
            let one = LspDocumentId(1);
            manager.open_for_test(one, &first, &worktree, rust_language(), 1, "a\n");
            let _ = handle_for(&registry, &worktree).await;
            let status = next_matching(&events, |event| match event {
                LspEvent::Status { doc, status } if *doc == one => match status {
                    LspStatus::Failed { .. } => Some(status.clone()),
                    _ => None,
                },
                _ => None,
            })
            .await;
            assert!(status.can_restart());

            // Opening an unrelated file in the same root replaces the dead
            // session; the already-open document must be reopened on it, or
            // the new server would know nothing about it.
            let two = LspDocumentId(2);
            manager.open_for_test(two, &second, &worktree, rust_language(), 1, "b\n");
            let replacement = {
                let deadline = tokio::time::Instant::now() + WAIT;
                loop {
                    let entries = registry.lock().expect("registry").len();
                    if entries >= 2 {
                        break registry.lock().expect("registry")[1].1.clone();
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the dead session was never replaced"
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            };
            let opens = replacement.wait_for_count("textDocument/didOpen", 2).await;
            let uris: Vec<String> = opens
                .iter()
                .map(|entry| {
                    entry.params["textDocument"]["uri"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect();
            for path in [&first, &second] {
                let expected = Url::from_file_path(path).expect("uri").to_string();
                assert!(
                    uris.contains(&expected),
                    "{expected} must be opened on the replacement, saw {uris:?}"
                );
            }
            manager.shutdown();
        });
    }

    #[test]
    fn wait_for_shutdown_reaps_every_session_process() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                false,
            );
            let pid = child.process_group;
            let mut manager = rust_manager();
            let slot = install_fake_with_child(
                &mut manager,
                incremental_capabilities(),
                |_| Vec::new(),
                Arc::new(Mutex::new(Some(child))),
            );
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let _handle = handle_of(&slot).await;
            await_ready(&events, doc).await;

            manager.shutdown();
            manager.wait_for_shutdown().await;
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "wait_for_shutdown must return only after the process is reaped"
            );
        });
    }

    /// Setup shared by the tests that stop a live server: a session serving
    /// one document, with a real process behind it that nothing but the
    /// session's own teardown can end.
    #[cfg(target_os = "linux")]
    async fn session_over_a_live_process(
        manager: &mut LspManager,
        worktree: &Path,
        file: &Path,
        doc: LspDocumentId,
    ) -> u32 {
        let child = spawn_test_child(
            &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
            Helper::Sleeps,
            false,
        );
        let pid = child.process_group;
        let slot = install_fake_with_child(
            manager,
            incremental_capabilities(),
            |_| Vec::new(),
            Arc::new(Mutex::new(Some(child))),
        );
        let events = manager.events();
        manager.open_for_test(doc, file, worktree, rust_language(), 1, "abc\n");
        let _handle = handle_of(&slot).await;
        await_ready(&events, doc).await;
        assert!(
            PathBuf::from(format!("/proc/{pid}")).exists(),
            "the server must be running before anything stops it"
        );
        pid
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn wait_for_shutdown_leaves_a_session_nobody_retired_alone() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let doc = LspDocumentId(1);
            let pid = session_over_a_live_process(&mut manager, &worktree, &file, doc).await;

            // Nothing has been retired, so there is nothing to wait for. A
            // wait that drained the installed session instead would spend
            // `SESSION_REAP_TIMEOUT` on a server it never sent `shutdown` to,
            // and then kill the process group of a healthy one.
            let started = std::time::Instant::now();
            manager.wait_for_shutdown().await;
            assert!(
                started.elapsed() < SESSION_REAP_TIMEOUT,
                "waiting for nothing took {:?}",
                started.elapsed()
            );
            assert!(
                PathBuf::from(format!("/proc/{pid}")).exists(),
                "a server that was never told to stop must still be running"
            );
            assert_eq!(manager.status(doc), LspStatus::Ready);

            // And the session is still the manager's to stop, rather than one
            // whose task slot was emptied and can never be awaited again.
            manager.shutdown();
            manager.wait_for_shutdown().await;
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "the session must still be waitable after the wait that skipped it"
            );
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn begin_shutdown_reaps_the_server_it_stops() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            let mut manager = rust_manager();
            let doc = LspDocumentId(1);
            let pid = session_over_a_live_process(&mut manager, &worktree, &file, doc).await;

            manager.begin_shutdown().await;
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "the future begin_shutdown returns must not finish before the reap"
            );
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn begin_shutdown_reaps_the_server_with_no_runtime_in_scope() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (_guard, worktree, file) = workspace();
        let mut manager = rust_manager();
        let doc = LspDocumentId(1);
        let pid = runtime.block_on(session_over_a_live_process(
            &mut manager,
            &worktree,
            &file,
            doc,
        ));

        // The application's exit path calls this from its update thread, which
        // has no runtime entered. The future handed back is then the only
        // thing that can reap, so it must be the thing that does.
        let wait = manager.begin_shutdown();
        runtime.block_on(wait);
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "the wait must reap even when it could not be spawned"
        );
    }

    /// Whether the process has stopped executing, whether or not anything has
    /// collected it yet. A test that deliberately keeps Tokio from reaping
    /// its child can only observe the zombie it leaves behind.
    #[cfg(target_os = "linux")]
    async fn wait_until_not_running(pid: u32) -> bool {
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return true;
            };
            // "pid (comm) state ...", and `comm` may itself contain
            // parentheses, so the state is what follows the last one.
            if stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_session_that_cannot_finish_is_still_reaped_within_the_wait() {
        block_on(async {
            let (_guard, worktree, file) = workspace();
            // No `kill_on_drop`: nothing except this session's own teardown
            // can end this process, so a wait that gives up without killing
            // the group leaves the server running and fails here.
            let child = spawn_test_child(
                &["--exact", CHILD_HELPER_TEST, "--test-threads=1"],
                Helper::Sleeps,
                false,
            );
            let pid = child.process_group;
            let mut manager = rust_manager();
            let slot = install_fake_with_child(
                &mut manager,
                incremental_capabilities(),
                |_| Vec::new(),
                Arc::new(Mutex::new(Some(child))),
            );
            let events = manager.events();
            let doc = LspDocumentId(1);
            manager.open_for_test(doc, &file, &worktree, rust_language(), 1, "abc\n");
            let _handle = handle_of(&slot).await;
            await_ready(&events, doc).await;

            // An application on its way out stops draining events, and a
            // session task that is publishing one then never reaches the
            // shutdown it was sent. Superseding a request makes it publish
            // the cancellation, against a channel nobody is emptying.
            while manager
                .events
                .tx
                .try_send(LspEvent::RequestFailed {
                    request: LspRequestId(0),
                    doc,
                    error: LspError::Cancelled,
                })
                .is_ok()
            {}
            manager.request_hover(doc, LspPosition::new(0, 0));
            manager.request_hover(doc, LspPosition::new(0, 1));

            manager.shutdown();
            assert!(
                tokio::time::timeout(WAIT, manager.wait_for_shutdown())
                    .await
                    .is_ok(),
                "an exit path cannot wait on a session task that will never finish"
            );
            assert!(
                wait_until_not_running(pid).await,
                "giving up on the session must still kill the server's process group"
            );
        });
    }

    /// A worktree, a sibling directory outside it that also carries a root
    /// marker, and a symlink inside the worktree pointing at that sibling.
    fn escaping_symlink() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let worktree = root.path().join("worktree");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(worktree.join("src")).expect("worktree");
        std::fs::create_dir_all(outside.join("src")).expect("outside");
        std::fs::write(worktree.join("Cargo.toml"), "").expect("worktree marker");
        std::fs::write(outside.join("Cargo.toml"), "").expect("outside marker");
        std::fs::write(outside.join("src/lib.rs"), "fn f() {}\n").expect("outside file");
        std::os::unix::fs::symlink(&outside, worktree.join("link")).expect("symlink");
        (root, worktree, outside)
    }

    #[test]
    fn a_symlinked_directory_that_leaves_the_worktree_is_never_a_server_root() {
        let (_root, worktree, outside) = escaping_symlink();
        let markers = vec!["Cargo.toml".to_owned()];
        // Lexically `<worktree>/link/src` is inside the worktree and its parent
        // holds a marker, but it resolves to a directory the user never opened.
        assert_eq!(
            discover_root(&worktree.join("link/src/lib.rs"), &worktree, &markers),
            worktree,
            "a root reached through a symlink out of the worktree must be refused"
        );
        assert_ne!(
            discover_root(&worktree.join("link/src/lib.rs"), &worktree, &markers),
            outside,
            "the escaping directory must never be selected"
        );
        // The genuine nested case still finds the nearest marker.
        std::fs::create_dir_all(worktree.join("inner/src")).expect("inner");
        std::fs::write(worktree.join("inner/Cargo.toml"), "").expect("inner marker");
        assert_eq!(
            discover_root(&worktree.join("inner/src/lib.rs"), &worktree, &markers),
            worktree.join("inner"),
            "a real nested marker still wins"
        );
    }

    fn location_request(worktree: &Path, uri: &Arc<Url>) -> PendingRequest {
        PendingRequest {
            id: LspRequestId(1),
            doc: LspDocumentId(1),
            uri: Arc::clone(uri),
            worktree: Arc::from(worktree.to_path_buf()),
            kind: RequestKind::Definition,
            position: lsp_types::Position::new(0, 0),
            version: 1,
                args: RequestArgs::None, timeout: Duration::from_secs(15), observed_versions: HashMap::new(),
        }
    }

    fn classify(worktree: &Path, target_path: &Path) -> LspTarget {
        let snapshot = Snapshot::new(1, Arc::from("fn main() {}\n"));
        let request_uri = Arc::new(Url::from_file_path(worktree.join("main.rs")).expect("uri"));
        let request = location_request(worktree, &request_uri);
        let location = lsp_types::Location {
            uri: Url::from_file_path(target_path).expect("target uri"),
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 1),
            ),
        };
        let converted = convert_locations(&snapshot, &request, &[location]);
        assert_eq!(converted.len(), 1, "a file location is always reported");
        converted.into_iter().next().expect("one location").target
    }

    #[test]
    fn a_symlinked_path_resolving_outside_the_worktree_classifies_as_external() {
        let (_root, worktree, _outside) = escaping_symlink();
        assert!(
            matches!(
                classify(&worktree, &worktree.join("link/src/lib.rs")),
                LspTarget::External { .. }
            ),
            "a path that resolves outside the worktree must open read-only"
        );
    }

    #[test]
    fn a_nested_worktree_file_is_workspace_and_an_unresolvable_one_is_external() {
        let (_root, worktree, _outside) = escaping_symlink();
        let nested = worktree.join("src/deep.rs");
        std::fs::write(&nested, "fn deep() {}\n").expect("nested file");
        assert!(
            matches!(classify(&worktree, &nested), LspTarget::Workspace { .. }),
            "an ordinary nested file stays editable"
        );
        // Canonicalisation cannot resolve a path that does not exist, so
        // containment is unproven and the conservative answer is read-only.
        assert!(
            matches!(
                classify(&worktree, &worktree.join("src/vanished.rs")),
                LspTarget::External { .. }
            ),
            "an unresolvable path must never be reported as writable workspace"
        );
    }

    #[test]
    fn an_escaped_comma_stays_inside_a_snippet_choice() {
        let snippet = parse_snippet(r"m(${1|a\,b,c|})");
        assert_eq!(
            snippet.text, "m(a,b)",
            "the first choice ends at the first unescaped comma"
        );
        assert_eq!(snippet.stops.len(), 1);
        assert_eq!(
            snippet.stops[0].range,
            2..5,
            "the tab stop spans the inserted choice"
        );
    }

    #[test]
    fn an_escaped_dollar_is_literal_text_and_not_a_tab_stop() {
        let snippet = parse_snippet(r#"println!("\${}", $1)"#);
        assert_eq!(
            snippet.text, "println!(\"${}\", )",
            "an escaped dollar contributes text, not grammar"
        );
        assert_eq!(snippet.stops.len(), 1, "only the real `$1` is a tab stop");
        // A choice list whose bar is escaped is not a choice list at all.
        assert_eq!(parse_snippet(r"${1:a\|b}").text, r"a\|b");
    }

    #[test]
    fn a_nested_placeholder_expands_and_keeps_both_tab_stops() {
        let snippet = parse_snippet("for ${1:item} in ${2:${3:iter}} {\n\t$0\n}");
        assert_eq!(snippet.text, "for item in iter {\n\t\n}");
        let indexes: Vec<u32> = snippet.stops.iter().map(|stop| stop.index).collect();
        assert_eq!(
            indexes,
            vec![1, 2, 3, 0],
            "visit order is ascending with `$0` last, and nesting is preserved"
        );
        let stop_two = &snippet.stops[1];
        let stop_three = &snippet.stops[2];
        assert_eq!(
            snippet.text[stop_two.range.clone()].to_owned(),
            "iter",
            "the outer placeholder spans its expanded child"
        );
        assert_eq!(stop_two.range, stop_three.range);
    }

    #[test]
    fn a_completion_with_an_unconvertible_additional_edit_is_refused_whole() {
        let text: Arc<str> = Arc::from("fn main() {}\n");
        let snapshot = Snapshot::new(1, Arc::clone(&text));
        let good_range = lsp_types::Range::new(
            lsp_types::Position::new(0, 3),
            lsp_types::Position::new(0, 7),
        );
        let item = |label: &str, additional: Option<lsp_types::Range>| lsp_types::CompletionItem {
            label: label.to_owned(),
            text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                range: good_range,
                new_text: label.to_owned(),
            })),
            additional_text_edits: additional.map(|range| {
                vec![lsp_types::TextEdit {
                    range,
                    new_text: "use std::io;\n".to_owned(),
                }]
            }),
            ..lsp_types::CompletionItem::default()
        };
        // The import edit points at a line the document does not have.
        let broken_range = lsp_types::Range::new(
            lsp_types::Position::new(99, 0),
            lsp_types::Position::new(99, 0),
        );
        let error = convert_completion(
            &snapshot,
            lsp_types::Position::new(0, 3),
            &item("broken", Some(broken_range)),
        )
        .expect_err("an unconvertible import edit must refuse the item");
        assert!(
            matches!(error, LspError::PositionOutOfRange { line: 99, .. }),
            "the reason is reported, not swallowed: {error:?}"
        );
        assert!(
            convert_completion(
                &snapshot,
                lsp_types::Position::new(0, 3),
                &item("fine", Some(good_range))
            )
            .expect("a convertible item survives")
            .additional_edits
            .len()
                == 1,
            "a convertible additional edit is still carried"
        );

        // End to end: the refused item is absent from the event while its
        // healthy sibling is offered.
        let store = SnapshotStore::default();
        let uri = Arc::new(Url::parse("file:///tmp/main.rs").expect("uri"));
        let key = SessionKey {
            server_id: Arc::from("rust-analyzer"),
            root: Arc::from(PathBuf::from("/tmp")),
        };
        store
            .register(LspDocumentId(1), &uri, &key, 1, text)
            .expect("register");
        let request = PendingRequest {
            id: LspRequestId(7),
            doc: LspDocumentId(1),
            uri: Arc::clone(&uri),
            worktree: Arc::from(PathBuf::from("/tmp")),
            kind: RequestKind::Completion,
            position: lsp_types::Position::new(0, 3),
            version: 1,
                args: RequestArgs::None, timeout: Duration::from_secs(15), observed_versions: HashMap::new(),
        };
        let event = convert_result(
            &store,
            &request,
            RawResult::Completion(Some(lsp_types::CompletionResponse::Array(vec![
                item("broken", Some(broken_range)),
                item("fine", None),
            ]))),
        );
        let LspEvent::Completion { items, .. } = event else {
            panic!("expected a completion event, got {event:?}");
        };
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["fine"],
            "the refused item is omitted and the rest are still offered"
        );
    }
    #[test]
    fn editor_feature_requests_round_trip_with_exact_configuration_and_coordinates() {
        block_on(async {
            let (_guard,worktree,file)=workspace();let other=file.with_file_name("other.rs");
            let uri=Url::from_file_path(&file).unwrap();let other_uri=Url::from_file_path(&other).unwrap();
            let mut manager=rust_manager();manager.servers[0].initialization_options=Some(json!({"check":{"command":"clippy"}}));
            manager.servers[0].configuration=json!({"rust-analyzer":{"check":{"command":"clippy"}}});
            let edit=json!({"documentChanges":[{"textDocument":{"uri":uri,"version":1},"edits":[{"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"newText":"bar"}]},{"textDocument":{"uri":other_uri,"version":null},"edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"}]}]});
            let response_edit=edit.clone();
            let slot=install_fake(&mut manager,json!({"textDocumentSync":2,"renameProvider":{"prepareProvider":true},"documentFormattingProvider":true,"documentRangeFormattingProvider":true,"codeActionProvider":{"resolveProvider":true},"executeCommandProvider":{"commands":["fixture.fix"]},"signatureHelpProvider":{"triggerCharacters":["(",","],"retriggerCharacters":[","]}}),move |request|{
                let result=match request.method.as_str(){
                    "textDocument/prepareRename"=>json!({"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"placeholder":"foo"}),
                    "textDocument/rename"=>response_edit.clone(),
                    "textDocument/formatting"|"textDocument/rangeFormatting"=>json!([{"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"newText":"foo "}]),
                    "textDocument/codeAction"=>json!([{"title":"Import fixture","kind":"quickfix","data":{"opaque":7}}]),
                    "codeAction/resolve"=>json!({"title":"Import fixture","edit":response_edit,"command":{"title":"Apply","command":"fixture.fix","arguments":[{"opaque":7}]}}),
                    "workspace/executeCommand"=>Value::Null,
                    "textDocument/signatureHelp"=>json!({"signatures":[{"label":"f(𝄞, value)","parameters":[{"label":[2,4]},{"label":[6,11]}]}],"activeSignature":0,"activeParameter":1}),
                    _=>return vec![],
                };vec![json!({"jsonrpc":"2.0","id":request.id,"result":result})]
            });
            let doc=LspDocumentId(1);let events=manager.events();manager.open_for_test(doc,&file,&worktree,rust_language(),1,"a𝄞foo\n");
            let handle=handle_of(&slot).await;await_ready(&events,doc).await;
            assert_eq!(handle.wait_for("initialize").await.params["initializationOptions"],json!({"check":{"command":"clippy"}}));
            assert_eq!(handle.wait_for("workspace/didChangeConfiguration").await.params["settings"]["rust-analyzer"]["check"]["command"],json!("clippy"));
            assert!(manager.capabilities(doc).rename);assert!(manager.signature_trigger(doc,'('));
            let request=manager.request_prepare_rename(doc,LspPosition::new(0,2));
            let range=next_matching(&events,|e|match e{LspEvent::PrepareRename{request:r,range,..}if *r==request=>Some(*range),_=>None}).await.unwrap();assert_eq!(range.start.character,2);
            let request=manager.request_rename(doc,LspPosition::new(0,2),"bar".into());
            let edit=next_matching(&events,|e|match e{LspEvent::WorkspaceEdit{request:r,edit,..}if *r==request=>Some(edit.clone()),_=>None}).await;
            assert_eq!(edit.documents.len(),2);assert_eq!(edit.documents[0].edits[0].range.start.character,2);assert_eq!(edit.documents[0].observed_version,Some(1));assert_eq!(edit.documents[1].coordinates,LspCoordinates::Utf16);
            assert_eq!(handle.wait_for("textDocument/rename").await.params["position"]["character"],json!(3));
            for selection in [false,true] {
                let request=if selection{manager.request_range_formatting(doc,range,LspFormattingOptions::default(),Duration::from_millis(1000))}else{manager.request_formatting(doc,LspFormattingOptions::default(),Duration::from_millis(1000))};
                next_matching(&events,|e|match e{LspEvent::WorkspaceEdit{request:r,..}if *r==request=>Some(()),_=>None}).await;
            }
            handle.notify("textDocument/publishDiagnostics",json!({"uri":uri,"version":1,"diagnostics":[{"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"message":"missing import","code":2304,"data":{"opaque_fix":7}}]}));
            let diagnostics=next_matching(&events,|e|match e{LspEvent::Diagnostics{items,..}=>Some(items.clone()),_=>None}).await;
            let request=manager.request_code_actions(doc,range,&diagnostics);
            let item=next_matching(&events,|e|match e{LspEvent::CodeActions{request:r,items,..}if *r==request=>items.first().cloned(),_=>None}).await;assert!(item.needs_resolve);
            let context=handle.wait_for("textDocument/codeAction").await.params;
            assert_eq!(context["context"]["diagnostics"][0]["code"],json!(2304));assert_eq!(context["context"]["diagnostics"][0]["data"]["opaque_fix"],json!(7));
            let request=manager.request_resolve_code_action(doc,item);
            let item=next_matching(&events,|e|match e{LspEvent::CodeActionResolved{request:r,item,..}if *r==request=>Some(item.clone()),_=>None}).await;assert_eq!(item.edit.as_ref().unwrap().documents.len(),2);
            assert_eq!(handle.wait_for("codeAction/resolve").await.params["data"]["opaque"],json!(7));
            let request=manager.execute_command(doc,item.command.unwrap());next_matching(&events,|e|match e{LspEvent::CommandExecuted{request:r,..}if *r==request=>Some(()),_=>None}).await;
            let request=manager.request_signature_help(doc,LspPosition::new(0,2),Some("(".into()));let help=next_matching(&events,|e|match e{LspEvent::SignatureHelp{request:r,help,..}if *r==request=>Some(help.clone()),_=>None}).await.unwrap();
            assert_eq!(help.active_parameter,Some(1));assert_eq!(help.signatures[0].parameters[0].range,Some(2..6));assert_eq!(help.signatures[0].parameters[1].label,"value");
            handle.notify("window/showMessage",json!({"type":1,"message":"workspace failed to load"}));
            let message=next_matching(&events,|e|match e{LspEvent::ServerMessage{level:LspMessageLevel::Error,message,..}=>Some(message.clone()),_=>None}).await;assert_eq!(message,"workspace failed to load");
            manager.begin_shutdown().await;
        });
    }

    #[test]
    fn editor_feature_timeout_cancels_wire_and_reports_terminal_failure(){block_on(async{
        let (_guard,worktree,file)=workspace();let mut manager=rust_manager();let slot=install_fake(&mut manager,json!({"textDocumentSync":2,"documentFormattingProvider":true}), |_|vec![]);let events=manager.events();let doc=LspDocumentId(1);
        manager.open_for_test(doc,&file,&worktree,rust_language(),1,"foo\n");let handle=handle_of(&slot).await;await_ready(&events,doc).await;
        let request=manager.request_formatting(doc,LspFormattingOptions::default(),Duration::from_millis(25));
        next_matching(&events,|e|match e{LspEvent::RequestFailed{request:r,error:LspError::TimedOut{milliseconds:25},..}if *r==request=>Some(()),_=>None}).await;
        let wire=handle.wait_for("textDocument/formatting").await.id.unwrap();assert_eq!(handle.wait_for("$/cancelRequest").await.params["id"],json!(wire));manager.begin_shutdown().await;
    });}

    #[test]
    fn workspace_edit_conversion_refuses_stale_targets_and_preserves_document_changes_precedence(){
        let store=SnapshotStore::default();let uri=Arc::new(Url::parse("file:///repo/a.rs").unwrap());let key=SessionKey{root:Arc::from(Path::new("/repo")),server_id:Arc::from("rust")};
        store.register(LspDocumentId(1),&uri,&key,2,Arc::from("a𝄞foo"));
        let edit: lsp_types::WorkspaceEdit=serde_json::from_value(json!({"changes":{"file:///ignored.rs":[]},"documentChanges":[{"textDocument":{"uri":uri.as_str(),"version":2},"edits":[{"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"newText":"bar"}]}]})).unwrap();
        let converted=features::convert_workspace_edit(&store,edit.clone(),Some(&HashMap::from([((*uri).clone(),2)]))).unwrap();assert_eq!(converted.documents.len(),1);assert_eq!(converted.documents[0].edits[0].range.start.character,2);
        assert!(matches!(features::convert_workspace_edit(&store,edit,Some(&HashMap::from([((*uri).clone(),1)]))),Err(LspError::StaleVersion{..})));
        let resource: lsp_types::WorkspaceEdit=serde_json::from_value(json!({"documentChanges":[{"kind":"create","uri":"file:///repo/new.rs"}]})).unwrap();assert!(matches!(features::convert_workspace_edit(&store,resource,None),Err(LspError::Unsupported(_))));
        assert_eq!(features::configuration(&json!({"rust-analyzer":{"check":{"command":"clippy"}}}),Some("rust-analyzer.check.command")),json!("clippy"));
    }

    #[test]
    fn server_workspace_edits_wait_for_application_ack_and_configuration_sections(){block_on(async{
        let (_guard,worktree,file)=workspace();let uri=Url::from_file_path(&file).unwrap();let mut manager=rust_manager();
        manager.servers[0].configuration=json!({"rust-analyzer":{"check":{"command":"clippy"}}});
        let slot=install_fake(&mut manager,incremental_capabilities(), |_|vec![]);let events=manager.events();let doc=LspDocumentId(1);
        manager.open_for_test(doc,&file,&worktree,rust_language(),1,"foo\n");let handle=handle_of(&slot).await;await_ready(&events,doc).await;
        handle.push.send(Outgoing::Message(json!({"jsonrpc":"2.0","id":600,"method":"workspace/applyEdit","params":{"label":"Quick fix","edit":{"changes":{uri.as_str():[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"bar"}]}}}}))).unwrap();
        let id=next_matching(&events,|e|match e{LspEvent::ApplyWorkspaceEdit{id,edit,label,..}=>{assert_eq!(edit.documents.len(),1);assert_eq!(label.as_deref(),Some("Quick fix"));Some(*id)},_=>None}).await;
        assert!(handle.entries("").iter().all(|e|e.id!=Some(600)),"server must wait for preview approval");
        manager.respond_workspace_edit(id,true,None);
        let deadline=tokio::time::Instant::now()+WAIT;
        loop {if let Some(response)=handle.entries("").into_iter().find(|e|e.id==Some(600)){assert_eq!(response.result.unwrap()["applied"],json!(true));break;}assert!(tokio::time::Instant::now()<deadline);tokio::time::sleep(Duration::from_millis(2)).await;}
        handle.push.send(Outgoing::Message(json!({"jsonrpc":"2.0","id":601,"method":"workspace/configuration","params":{"items":[{"section":"rust-analyzer.check"},{"section":"missing"}]}}))).unwrap();
        let deadline=tokio::time::Instant::now()+WAIT;
        loop {if let Some(response)=handle.entries("").into_iter().find(|e|e.id==Some(601)){assert_eq!(response.result.unwrap(),json!([{"command":"clippy"},null]));break;}assert!(tokio::time::Instant::now()<deadline);tokio::time::sleep(Duration::from_millis(2)).await;}
        manager.begin_shutdown().await;
    });}

    #[test]
    fn cross_file_rename_flushes_other_buffers_inside_their_debounce_window(){block_on(async{
        let (_guard,worktree,file)=workspace();let other=file.with_file_name("other.rs");let other_uri=Url::from_file_path(&other).unwrap();
        let mut manager=rust_manager();let slot=install_fake(&mut manager,json!({"textDocumentSync":2,"renameProvider":true}),|request|if request.method=="textDocument/rename"{vec![json!({"jsonrpc":"2.0","id":request.id,"result":null})]}else{vec![]});
        let events=manager.events();let doc=LspDocumentId(1);let second=LspDocumentId(2);manager.open_for_test(doc,&file,&worktree,rust_language(),1,"foo\n");let handle=handle_of(&slot).await;await_ready(&events,doc).await;
        manager.open_for_test(second,&other,&worktree,rust_language(),1,"foo\n");handle.wait_for_count("textDocument/didOpen",2).await;
        manager.change(second,2,&[],"changed foo\n");let request=manager.request_rename(doc,LspPosition::new(0,1),"bar".into());
        next_matching(&events,|e|match e{LspEvent::WorkspaceEdit{request:r,..}if *r==request=>Some(()),_=>None}).await;
        let entries=handle.log.lock().unwrap();let change=entries.iter().position(|e|e.method=="textDocument/didChange"&&e.params["textDocument"]["uri"]==json!(other_uri)).unwrap();let rename=entries.iter().position(|e|e.method=="textDocument/rename").unwrap();assert!(change<rename);drop(entries);manager.begin_shutdown().await;
    });}

    #[test]
    fn oversized_code_action_lists_and_titles_report_terminal_failure(){block_on(async{
        let (_guard,worktree,file)=workspace();let mut manager=rust_manager();let request_count=Arc::new(AtomicUsize::new(0));let count=Arc::clone(&request_count);
        let slot=install_fake(&mut manager,json!({"textDocumentSync":2,"codeActionProvider":true}),move|request|{
            if request.method!="textDocument/codeAction"{return vec![];}
            let result=if count.fetch_add(1,Ordering::Relaxed)==0{json!(vec![json!({"title":"Fix"});513])}else{json!([{"title":"x".repeat(4097)}])};
            vec![json!({"jsonrpc":"2.0","id":request.id,"result":result})]
        });let events=manager.events();let doc=LspDocumentId(1);manager.open_for_test(doc,&file,&worktree,rust_language(),1,"foo\n");let _handle=handle_of(&slot).await;await_ready(&events,doc).await;
        for _ in 0..2 {let request=manager.request_code_actions(doc,LspRange::default(),&[]);next_matching(&events,|e|match e{LspEvent::RequestFailed{request:r,error:LspError::Unsupported(_),..}if *r==request=>Some(()),_=>None}).await;}
        manager.begin_shutdown().await;
    });}
}
