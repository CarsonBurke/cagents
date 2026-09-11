//! Native repository-card state, asynchronous file review tasks, and focused UI.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::Duration,
};

#[path = "editor_widget.rs"]
mod text_editor;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::{
    Element,
    iced::{self, Alignment, Font, Length, Task, highlighter},
    widget::{self, markdown},
};

use crate::{
    diff_view::{
        DIFF_FONT_SIZE, DIFF_LINE_HEIGHT, DiffMode, DiffModel, DiffRowKind, DiffSyntax,
        InlineDiffRow, RenderedDiff, SplitDiffRow, SyntaxSpan, highlighter_settings,
        load_repository_diff,
    },
    editor::{
        FileContent, FileIoError, ReadOnlyReason, SaveDurability, SaveOutcome, load_file,
        save_file, validate_repo_path,
    },
    lsp::{LspCompletionEdit, LspCompletionItem, LspDocumentId, LspPosition},
    model::{CardId, RepositoryView},
    repository::{
        CancellationToken, DiffBasis, DiffContent, DiffSide, HistoryRequest, HistoryRevision,
        MAX_DIFF_SIDE_BYTES, RepositoryService, RepositorySnapshot, ValidatedRefName,
    },
    ui_controls::{icon_control, segmented_controls},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryFile {
    pub path: PathBuf,
    pub staged_old_path: Option<PathBuf>,
    pub unstaged_old_path: Option<PathBuf>,
    pub staged: bool,
    pub unstaged: bool,
    pub status: Option<String>,
}

impl RepositoryFile {
    fn changed(&self) -> bool {
        self.staged || self.unstaged
    }

    /// Whether this comparison has anything to show for this file.
    ///
    /// Only the two working bases are answerable from a file's status: a
    /// committed comparison reads two trees and can differ for a file with no
    /// local change at all, so it is always offered.
    fn has_basis(&self, basis: &DiffBasis) -> bool {
        match basis {
            DiffBasis::HeadToIndex => self.staged,
            DiffBasis::IndexToWorktree => self.unstaged,
            DiffBasis::MergeBaseToHead { .. } | DiffBasis::CommitToParent { .. } => true,
        }
    }

    /// The rename source the old side of this comparison must read.
    ///
    /// `git status` reports a rename source per area, which is where both
    /// working bases get theirs. A committed comparison's rename detection
    /// belongs to the range that produced the file list, and the card's list
    /// comes from the working tree, so nothing is invented here.
    fn previous_path(&self, basis: &DiffBasis) -> Option<PathBuf> {
        match basis {
            DiffBasis::HeadToIndex => self.staged_old_path.clone(),
            DiffBasis::IndexToWorktree => self.unstaged_old_path.clone(),
            DiffBasis::MergeBaseToHead { .. } | DiffBasis::CommitToParent { .. } => None,
        }
    }
}

fn status_with_unstaged_change(status: Option<&str>, already_unstaged: bool) -> String {
    if already_unstaged {
        return status.unwrap_or(" M").into();
    }
    match status {
        None => " M".into(),
        Some(status) => {
            let staged = status.chars().next().filter(char::is_ascii).unwrap_or(' ');
            format!("{staged}M")
        }
    }
}

/// One choice in the card's comparison-basis control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BasisChoice {
    /// `HEAD` to index.
    Staged,
    /// Index to working tree.
    Unstaged,
    /// A comparison against committed history.
    Committed(CommittedChoice),
}

/// A comparison the card cannot name without asking Git: a merge base's
/// default ref comes from the remote's symbolic `HEAD`, and a commit's parent
/// comes from the commit itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommittedChoice {
    /// Merge base of the default base ref with `HEAD`.
    Branch,
    /// `HEAD`'s own commit against its first parent.
    LastCommit,
}

impl CommittedChoice {
    const fn label(self) -> &'static str {
        match self {
            Self::Branch => "Branch",
            Self::LastCommit => "Commit",
        }
    }
}

/// What resolving a committed comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedBasis {
    /// A comparison ready to be read.
    Ready(DiffBasis),
    /// A branch comparison has no default base, because this repository has no
    /// `refs/remotes/<remote>/HEAD` to read one from. The refs that could
    /// serve as one are offered; a name is never guessed.
    NeedsBase { candidates: Vec<ValidatedRefName> },
    /// An unborn branch has no commit to compare against anything.
    NoCommit,
}

/// Where an outstanding committed-comparison resolution got to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BasisResolution {
    Resolving(CommittedChoice),
    /// The user must choose the base ref, because nothing resolved one.
    NeedsBase(Vec<ValidatedRefName>),
    Unavailable(String),
}

/// The comparison a basis names, for the diff surface's own chrome.
///
/// A committed comparison names its ref or its commit: the whole point of a
/// fourth basis is that a user looking at one commit's diff must not see the
/// same heading as a user looking at unstaged work.
fn basis_label(basis: &DiffBasis) -> String {
    match basis {
        DiffBasis::IndexToWorktree => "Unstaged changes".into(),
        DiffBasis::HeadToIndex => "Staged changes".into(),
        DiffBasis::MergeBaseToHead { base_ref } => {
            format!("Branch vs {}", base_ref.short_name())
        }
        DiffBasis::CommitToParent {
            oid,
            parent: Some(_),
        } => format!("Commit {}", oid.short()),
        DiffBasis::CommitToParent { oid, parent: None } => {
            format!("Root commit {}", oid.short())
        }
    }
}

/// The basis in one static word, for the inspector projection, which holds
/// `&'static str` and so cannot carry a ref name or a commit id.
fn basis_choice_label(basis: &DiffBasis) -> &'static str {
    match basis_choice(basis) {
        BasisChoice::Staged => "Staged",
        BasisChoice::Unstaged => "Unstaged",
        BasisChoice::Committed(choice) => choice.label(),
    }
}

/// Which control choice a basis was made from, so the widget can never show a
/// choice the runtime has not made.
fn basis_choice(basis: &DiffBasis) -> BasisChoice {
    match basis {
        DiffBasis::HeadToIndex => BasisChoice::Staged,
        DiffBasis::IndexToWorktree => BasisChoice::Unstaged,
        DiffBasis::MergeBaseToHead { .. } => BasisChoice::Committed(CommittedChoice::Branch),
        DiffBasis::CommitToParent { .. } => BasisChoice::Committed(CommittedChoice::LastCommit),
    }
}

/// What one side of a comparison is, in one word, for the inspector.
fn side_kind(side: &DiffSide) -> &'static str {
    match side {
        DiffSide::Absent => "Absent",
        DiffSide::Text { .. } => "Text",
        DiffSide::Binary { .. } => "Binary",
        DiffSide::InvalidUtf8 { .. } => "Invalid UTF-8",
        DiffSide::Oversize { .. } => "Too large",
        DiffSide::Submodule { .. } => "Submodule",
        DiffSide::Symlink { .. } => "Symlink",
        DiffSide::NotRegularFile => "Not a regular file",
        DiffSide::Unreadable { .. } => "Unreadable",
    }
}

/// The one-word state a comparison with no text rows reports.
fn no_text_kind(content: &DiffContent) -> &'static str {
    match content {
        DiffContent::Unmerged { .. } => "Conflicted",
        // At least one side is undiffable, or there would be rows; the first
        // such side is the one the reader has to be told about.
        DiffContent::Comparison { old, new, .. } => [old, new]
            .into_iter()
            .find(|side| !side.is_diffable())
            .map_or("Unavailable", side_kind),
    }
}

/// What one side of a comparison holds, in the diff surface's own words.
fn side_detail(side: &DiffSide) -> String {
    match side {
        DiffSide::Absent => "not present".into(),
        DiffSide::Text { bytes, .. } => format!("{bytes} bytes of text"),
        DiffSide::Binary { bytes } => format!("binary, {bytes} bytes"),
        DiffSide::InvalidUtf8 { bytes } => format!("not valid UTF-8, {bytes} bytes"),
        DiffSide::Oversize { bytes } => {
            format!("{bytes} bytes, over the {MAX_DIFF_SIDE_BYTES}-byte limit for one side")
        }
        DiffSide::Submodule { oid: Some(oid) } => {
            format!("a submodule recorded at {}", oid.short())
        }
        DiffSide::Submodule { oid: None } => "a submodule with no recorded commit".into(),
        DiffSide::Symlink { target } => format!("a symlink to {}", target.display()),
        DiffSide::NotRegularFile => "not a regular file".into(),
        DiffSide::Unreadable { reason } => format!("unreadable: {reason}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncIdentity {
    pub card: CardId,
    pub worktree: PathBuf,
    pub path: PathBuf,
    pub generation: u64,
}

/// One thing the language server must be told about the open document.
///
/// The runtime records these; the application drains them with
/// [`RepositoryRuntime::take_lsp_intents`] and performs them, because the
/// runtime owns no [`crate::lsp::LspManager`] and does no I/O.
#[derive(Debug, Clone, PartialEq)]
pub enum LspIntent {
    /// The buffer became the card's open document. `absolute_path` is
    /// resolvable on disk; [`AsyncIdentity::path`] is worktree-relative and a
    /// document URI cannot be built from it alone.
    Open {
        absolute_path: PathBuf,
        worktree: PathBuf,
        version: i32,
        text: String,
    },
    /// The buffer changed. `text` is the whole document, because
    /// [`text_editor::Action`] and [`text_editor::Edit`] carry no ranges: the
    /// widget reports *what* was pressed, never *where* the resulting text
    /// differs. Incremental synchronisation is therefore unavailable by
    /// construction, not by omission, and the application must call
    /// [`crate::lsp::LspManager::change`] with an empty edit slice so the
    /// manager sends the full document rather than trying to reconcile edits
    /// it was never given.
    Change { version: i32, text: String },
    /// The buffer reached disk. `text` is what was written.
    Save { text: String },
    /// The buffer is gone: another file was opened over it, or the card
    /// returned to its file list.
    Close,
}

#[derive(Debug, Clone)]
pub enum RepositoryAction {
    RefreshFiles {
        card: CardId,
    },
    NewTerminal {
        card: CardId,
    },
    FilesLoaded {
        identity: AsyncIdentity,
        result: Result<Vec<RepositoryFile>, String>,
    },
    OpenFile {
        card: CardId,
        path: PathBuf,
    },
    FileLoaded {
        identity: AsyncIdentity,
        result: Result<FileContent, FileIoError>,
    },
    DiffLoaded {
        identity: AsyncIdentity,
        basis: DiffBasis,
        request: u64,
        content: RenderedDiff,
    },
    EditorAction {
        card: CardId,
        action: text_editor::Action,
    },
    Save {
        card: CardId,
        overwrite: bool,
    },
    AutoSaveDue {
        identity: AsyncIdentity,
        revision: u64,
        ticket: u64,
    },
    SaveFinished {
        identity: AsyncIdentity,
        ticket: u64,
        revision: u64,
        saved_text: String,
        result: Result<SaveOutcome, FileIoError>,
    },
    Reload {
        card: CardId,
    },
    DiscardEdits {
        card: CardId,
    },
    BackToFiles {
        card: CardId,
    },
    OpenExternal {
        card: CardId,
        editor_id: String,
    },
    StageSelected {
        card: CardId,
        stage: bool,
    },
    ApplySourceSurface {
        card: CardId,
        diff: bool,
    },
    ApplyMarkdownMode {
        card: CardId,
        preview: bool,
    },
    ApplyDiffMode {
        card: CardId,
        split: bool,
    },
    SelectCommittedBasis {
        card: CardId,
        choice: CommittedChoice,
    },
    /// Installs a fully-formed comparison, however it was chosen: the basis
    /// control, the base-ref picker, or any other producer of one.
    ApplyDiffBasis {
        card: CardId,
        basis: DiffBasis,
    },
    DiffBasisResolved {
        identity: AsyncIdentity,
        choice: CommittedChoice,
        result: Result<ResolvedBasis, String>,
    },
    RetryDiff {
        card: CardId,
    },
    FileViewport {
        card: CardId,
        offset: f32,
        height: f32,
    },
    DiffViewport {
        card: CardId,
        offset: f32,
        height: f32,
        horizontal: f32,
    },
    PreviousChange {
        card: CardId,
    },
    NextChange {
        card: CardId,
    },
    MarkdownLink {
        card: CardId,
        uri: markdown::Uri,
    },
}

impl RepositoryAction {
    pub fn card(&self) -> CardId {
        match self {
            Self::RefreshFiles { card }
            | Self::NewTerminal { card }
            | Self::OpenFile { card, .. }
            | Self::EditorAction { card, .. }
            | Self::Save { card, .. }
            | Self::Reload { card }
            | Self::DiscardEdits { card }
            | Self::BackToFiles { card }
            | Self::OpenExternal { card, .. }
            | Self::StageSelected { card, .. }
            | Self::ApplySourceSurface { card, .. }
            | Self::ApplyMarkdownMode { card, .. }
            | Self::ApplyDiffMode { card, .. }
            | Self::SelectCommittedBasis { card, .. }
            | Self::ApplyDiffBasis { card, .. }
            | Self::RetryDiff { card }
            | Self::FileViewport { card, .. }
            | Self::DiffViewport { card, .. }
            | Self::PreviousChange { card }
            | Self::NextChange { card }
            | Self::MarkdownLink { card, .. } => *card,
            Self::FilesLoaded { identity, .. }
            | Self::FileLoaded { identity, .. }
            | Self::DiffLoaded { identity, .. }
            | Self::DiffBasisResolved { identity, .. }
            | Self::AutoSaveDue { identity, .. }
            | Self::SaveFinished { identity, .. } => identity.card,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceSurface {
    Editor,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownMode {
    Edit,
    Preview,
}

struct EditorDocument {
    path: PathBuf,
    content: text_editor::Content<cosmic::Renderer>,
    saved_text: String,
    disk_hash: blake3::Hash,
    markdown: bool,
    markdown_items: Vec<markdown::Item>,
    dirty: bool,
    revision: u64,
    /// Version of this buffer as the language server counts it: monotonic and
    /// starting at the `1` sent with `textDocument/didOpen`.
    ///
    /// Deliberately not [`EditorDocument::revision`], which is a `u64` that
    /// *wraps* — a wrapped version would silently rewind the server's idea of
    /// the document, while [`crate::lsp::LspManager::change`] takes an `i32`
    /// and reports exhaustion as [`crate::lsp::LspError::VersionExhausted`].
    /// This one saturates instead, and a saturated document simply stops
    /// reporting changes.
    lsp_version: i32,
    active_save: Option<SaveTicket>,
    conflict: bool,
    error: Option<String>,
    durability_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveTicket {
    id: u64,
    identity: AsyncIdentity,
    revision: u64,
    snapshot: String,
}

impl EditorDocument {
    fn new(path: PathBuf, text: String, disk_hash: blake3::Hash, markdown: bool) -> Self {
        let markdown_items = if markdown {
            markdown::parse(&text).collect()
        } else {
            Vec::new()
        };
        Self {
            path,
            content: text_editor::Content::with_text(&text),
            saved_text: text,
            disk_hash,
            markdown,
            markdown_items,
            dirty: false,
            revision: 0,
            lsp_version: 1,
            active_save: None,
            conflict: false,
            error: None,
            durability_warning: None,
        }
    }

    fn apply(&mut self, action: text_editor::Action) {
        let is_edit = action.is_edit();
        self.content.perform(action);
        if is_edit {
            self.revision = self.revision.wrapping_add(1).max(1);
            let text = self.content.text();
            self.dirty = text != self.saved_text;
            if self.markdown {
                self.markdown_items = markdown::parse(&text).collect();
            }
        }
    }

    fn discard_edits(&mut self) {
        self.content = text_editor::Content::with_text(&self.saved_text);
        if self.markdown {
            self.markdown_items = markdown::parse(&self.saved_text).collect();
        }
        self.dirty = false;
        debug_assert!(self.active_save.is_none());
        self.conflict = false;
        self.error = None;
    }

    /// Advances the version the language server will be told about, or reports
    /// `None` once the `i32` version space is spent.
    ///
    /// Exhaustion is not a wrap: a document that has been through two billion
    /// edits stops being synchronised rather than being resynchronised
    /// backwards, which a server would reject as a stale version.
    fn bump_lsp_version(&mut self) -> Option<i32> {
        if self.lsp_version == i32::MAX {
            return None;
        }
        self.lsp_version += 1;
        Some(self.lsp_version)
    }

    /// Installs a whole new buffer text and brings the derived editor state
    /// with it.
    ///
    /// Used where the result is computed as text rather than as editor
    /// actions — applying a completion — which the widget's `Content` has no
    /// other way to express.
    fn replace_text(&mut self, text: &str) {
        self.content = text_editor::Content::with_text(text);
        if self.markdown {
            self.markdown_items = markdown::parse(text).collect();
        }
        self.dirty = text != self.saved_text;
        self.revision = self.revision.wrapping_add(1).max(1);
    }
}

// Caret coordinates.
//
// The two sides of the editor boundary count a column differently and neither
// says so in its types. `text_editor::Position::column` is a **byte** index
// into the line: libcosmic re-exports iced's type, and `iced_graphics`'
// editor builds it straight from a `cosmic_text::Cursor::index`, which is a
// byte offset. [`LspPosition::character`] counts **Unicode scalar values**.
// On any line that is pure ASCII the two agree, which is exactly why getting
// this wrong survives casual testing and then misplaces every diagnostic in a
// file with an accent or an emoji in it.

/// Converts a byte column inside `line` into the scalar count the protocol
/// uses.
///
/// A byte index that does not land on a character boundary is rounded down to
/// the boundary before it rather than slicing: a caret can only sit between
/// scalars, and panicking would throw away a position the editor considers
/// perfectly valid.
fn scalar_column(line: &str, byte_column: usize) -> u32 {
    let mut boundary = byte_column.min(line.len());
    while !line.is_char_boundary(boundary) {
        boundary -= 1;
    }
    u32::try_from(line[..boundary].chars().count()).unwrap_or(u32::MAX)
}

/// Converts a scalar count into the byte column the caret uses.
///
/// A count past the end of the line clamps to its end, which is what the
/// protocol asks a client to do with an out-of-range character.
fn byte_column(line: &str, character: u32) -> usize {
    let character = usize::try_from(character).unwrap_or(usize::MAX);
    line.char_indices()
        .nth(character)
        .map_or(line.len(), |(at, _)| at)
}

/// Byte range of the given zero-based line inside `text`, excluding its
/// terminator.
///
/// Deliberately identical to `Snapshot::line_span` in [`crate::lsp`]: lines
/// break on `\n` alone and a `\r` immediately before one is its partner, not
/// content. Every [`LspPosition`] crossing this boundary was produced in that
/// numbering, so anything else here would address the wrong line.
fn line_span(text: &str, line: u32) -> Option<Range<usize>> {
    let line = usize::try_from(line).unwrap_or(usize::MAX);
    let mut start = 0;
    let mut index = 0;
    loop {
        let end = text[start..].find('\n').map_or(text.len(), |at| start + at);
        if index == line {
            let content_end = if end > start && text.as_bytes()[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            return Some(start..content_end);
        }
        if end == text.len() {
            return None;
        }
        start = end + 1;
        index += 1;
    }
}

/// Byte offset of a protocol position inside `text`, or `None` when it names a
/// line the buffer does not have.
fn byte_offset(text: &str, at: LspPosition) -> Option<usize> {
    let span = line_span(text, at.line)?;
    Some(span.start + byte_column(&text[span.clone()], at.character))
}

/// Protocol position of a byte offset into `text`.
fn position_for_offset(text: &str, offset: usize) -> LspPosition {
    let offset = char_boundary(text, offset);
    let line = u32::try_from(text[..offset].matches('\n').count()).unwrap_or(u32::MAX);
    // Clamped to the line's *content*, not merely to the offset: [`line_span`]
    // drops a `\r` that the editor treats as a terminator of its own and will
    // happily park a caret after. A column that counted such a `\r` would name
    // a character the protocol's line does not have, and
    // `Snapshot::to_utf16` refuses that outright — silently failing every
    // hover, completion and definition made from that caret.
    let span = line_span(text, line).unwrap_or(0..offset);
    let content = &text[span.start..offset.min(span.end)];
    LspPosition::new(line, scalar_column(content, content.len()))
}

/// Largest character boundary of `text` at or before `offset`, clamped to its
/// length.
fn char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

// Editor lines against protocol lines.
//
// These two do not agree, and no amount of column arithmetic hides it:
// `cosmic_text`'s `LineIter` breaks a buffer on a bare `\r` and on `\n\r` as
// well as on `\n`, while [`crate::lsp`] counts `\n` alone. A file carrying a
// stray carriage return therefore has two different line numberings at once.
// Byte offsets into [`text_editor::Content::text`] are the one space both
// sides provably share, so every conversion below goes through an offset
// rather than passing a line index off as if it meant the same thing on both
// sides of the boundary.

/// What [`text_editor::Content::text`] writes after this line: nothing after
/// the last one, and a newline for an unterminated line in the middle.
fn line_terminator(line: &text_editor::Line<'_>, last: bool) -> &'static str {
    if last {
        ""
    } else if line.ending == text_editor::LineEnding::None {
        text_editor::LineEnding::default().as_str()
    } else {
        line.ending.as_str()
    }
}

/// Byte offset, into the text the buffer would produce, of a caret expressed
/// in the editor's own line and byte column.
fn caret_offset(
    content: &text_editor::Content<cosmic::Renderer>,
    caret: text_editor::Position,
) -> Option<usize> {
    let lines = content.line_count();
    let mut offset = 0;
    for index in 0..lines {
        let line = content.line(index)?;
        if index == caret.line {
            return Some(offset + char_boundary(&line.text, caret.column));
        }
        offset += line.text.len() + line_terminator(&line, index + 1 == lines).len();
    }
    None
}

/// The editor-native caret for a byte offset into the buffer's text.
///
/// An offset past the end lands at the end of the last line, because a caret
/// has to be somewhere.
fn caret_for_offset(
    content: &text_editor::Content<cosmic::Renderer>,
    offset: usize,
) -> text_editor::Position {
    let lines = content.line_count();
    let mut start = 0;
    let mut caret = text_editor::Position { line: 0, column: 0 };
    for index in 0..lines {
        let Some(line) = content.line(index) else {
            break;
        };
        let end = start + line.text.len();
        caret = text_editor::Position {
            line: index,
            column: char_boundary(&line.text, offset.saturating_sub(start)),
        };
        if offset <= end {
            break;
        }
        start = end + line_terminator(&line, index + 1 == lines).len();
    }
    caret
}

/// Byte range one completion edit replaces, or `None` when it cannot be
/// addressed against `text` at all.
///
/// [`LspCompletionEdit::replace_range`] is deliberately ignored: choosing the
/// wider replace range over the insert range is a user preference nothing in
/// this application exposes yet, and guessing it would silently eat the
/// identifier the caret sits inside.
fn completion_span(text: &str, edit: &LspCompletionEdit) -> Option<Range<usize>> {
    let start = byte_offset(text, edit.range.start)?;
    let end = byte_offset(text, edit.range.end)?;
    (start <= end).then_some(start..end)
}

enum FileListState {
    Loading,
    Ready(Vec<RepositoryFile>),
    Failed(String),
}

enum DocumentState {
    None,
    Loading(PathBuf),
    Editable(EditorDocument),
    Image {
        handle: iced::widget::image::Handle,
        width: u32,
        height: u32,
    },
    ReadOnly {
        path: PathBuf,
        reason: ReadOnlyReason,
    },
    Failed {
        path: PathBuf,
        message: String,
    },
}

enum DiffState {
    Loading {
        request: u64,
        cancellation: CancellationToken,
    },
    Ready(RenderedDiff),
}

impl Drop for DiffState {
    fn drop(&mut self) {
        if let Self::Loading { cancellation, .. } = self {
            cancellation.cancel();
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ListViewport {
    offset: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct VisibleRows {
    start: usize,
    end: usize,
    top_space: f32,
    bottom_space: f32,
}

const FILE_ROW_HEIGHT: f32 = 36.0;
const DIFF_ROW_HEIGHT: f32 = 30.0;
const VISIBLE_ROW_OVERSCAN: usize = 6;
const INITIAL_VIEWPORT_HEIGHT: f32 = 900.0;
/// How many committed comparisons one open file keeps.
///
/// Viewed working bases stay cached until a save or mutation invalidates them.
/// A committed basis is named by a ref or a commit
/// id, so its keyspace is open - stepping through history would otherwise
/// retain one whole diff model per commit visited for as long as the file
/// stays open, and a side is allowed to be megabytes. Four keeps stepping back
/// instant and the ceiling bounded.
const MAX_RETAINED_COMMITTED_DIFFS: usize = 4;

fn visible_rows(total: usize, viewport: ListViewport, row_height: f32) -> VisibleRows {
    let height = if viewport.height.is_finite() && viewport.height > 0.0 {
        viewport.height
    } else {
        INITIAL_VIEWPORT_HEIGHT
    };
    let offset = if viewport.offset.is_finite() {
        viewport.offset.max(0.0)
    } else {
        0.0
    };
    let first = (offset / row_height).floor() as usize;
    let visible = (height / row_height).ceil() as usize + 1;
    let start = first.saturating_sub(VISIBLE_ROW_OVERSCAN).min(total);
    let end = first
        .saturating_add(visible)
        .saturating_add(VISIBLE_ROW_OVERSCAN)
        .min(total);
    VisibleRows {
        start,
        end,
        top_space: start as f32 * row_height,
        bottom_space: total.saturating_sub(end) as f32 * row_height,
    }
}

pub struct RepositoryRuntime {
    pub descriptor: RepositoryView,
    directory_only: bool,
    standalone: bool,
    external_editors: Vec<crate::sidebar::SidebarEditor>,
    /// Epoch for selected-document, diff, and save work. File-list refreshes
    /// deliberately use a separate epoch so they cannot obsolete an active
    /// document operation.
    generation: u64,
    file_list_generation: u64,
    mutation_refresh_pending: bool,
    files: FileListState,
    selected: Option<RepositoryFile>,
    document: DocumentState,
    /// One entry per comparison this selection has been asked for.
    ///
    /// A list rather than a map: at most six bases are live at once, and
    /// [`DiffBasis`] is deliberately neither `Ord` nor `Hash` — it owns a ref
    /// name and an object id.
    diffs: Vec<(DiffBasis, DiffState)>,
    source_surface: SourceSurface,
    markdown_mode: MarkdownMode,
    diff_mode: DiffMode,
    diff_basis: DiffBasis,
    /// The committed comparison being resolved, or the reason one could not
    /// be, while that is the whole story of the diff surface.
    basis_resolution: Option<BasisResolution>,
    auto_save: bool,
    auto_save_pending: Option<(u64, u64)>,
    next_auto_save_ticket: u64,
    diff_change: usize,
    diff_scroll_id: widget::Id,
    file_viewport: ListViewport,
    diff_viewport: ListViewport,
    diff_horizontal: f32,
    next_save_ticket: u64,
    next_diff_request: u64,
    /// Identity the application gave the open buffer at the LSP boundary, or
    /// `None` while nothing is open there. Stored rather than derived: only
    /// the application, which owns the manager, can mint one.
    lsp_document: Option<LspDocumentId>,
    /// Protocol notifications this buffer has earned but nobody has performed
    /// yet, oldest first.
    ///
    /// An outbox rather than direct calls, because a widget that reached a
    /// language server would have to own one, and because the ordering of
    /// `didOpen`/`didChange`/`didClose` is exactly what a server cannot
    /// tolerate being wrong. Every buffer mutation appends here, so a mutation
    /// that forgets is a desynchronised server rather than a silent nothing.
    lsp_intents: Vec<LspIntent>,
}

/// Owned, renderer-free projection of the active repository source surface.
///
/// The contextual inspector is rebuilt independently from the repository card
/// widget tree, so it consumes a snapshot rather than reaching into editor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInspectorContext {
    pub file_list: &'static str,
    pub file_count: Option<usize>,
    pub file_error: Option<String>,
    pub selected_path: Option<PathBuf>,
    pub selected_status: Option<String>,
    pub selected_changed: bool,
    pub has_staged: bool,
    pub has_unstaged: bool,
    pub surface: &'static str,
    pub document: &'static str,
    pub document_error: Option<String>,
    pub conflict: bool,
    pub markdown_mode: Option<&'static str>,
    pub diff_basis: Option<&'static str>,
    pub diff_mode: Option<&'static str>,
    pub diff_state: Option<&'static str>,
    pub diff_error: Option<String>,
    pub diff_additions: Option<u32>,
    pub diff_deletions: Option<u32>,
    pub diff_change: Option<(usize, usize)>,
    pub dirty: bool,
    pub saving: bool,
    pub can_save: bool,
    pub can_discard: bool,
    pub can_reload: bool,
    pub can_retry_diff: bool,
    pub can_show_diff: bool,
    pub can_change_diff_layout: bool,
    pub can_select_staged_diff: bool,
    pub can_select_unstaged_diff: bool,
    pub can_back: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRuntimeStatus {
    Loading,
    Ready,
    Unavailable,
}

impl RepositoryRuntime {
    pub fn new(descriptor: RepositoryView) -> Self {
        let identity = descriptor.worktree.display().to_string();
        Self {
            descriptor,
            directory_only: false,
            standalone: false,
            external_editors: Vec::new(),
            generation: 0,
            file_list_generation: 0,
            mutation_refresh_pending: false,
            files: FileListState::Loading,
            selected: None,
            document: DocumentState::None,
            diffs: Vec::new(),
            next_diff_request: 0,
            source_surface: SourceSurface::Editor,
            markdown_mode: MarkdownMode::Edit,
            diff_mode: DiffMode::Inline,
            diff_basis: DiffBasis::IndexToWorktree,
            basis_resolution: None,
            auto_save: true,
            auto_save_pending: None,
            next_auto_save_ticket: 0,
            diff_change: 0,
            diff_scroll_id: widget::Id::new(format!("repository.diff.{identity}")),
            file_viewport: ListViewport::default(),
            diff_viewport: ListViewport::default(),
            diff_horizontal: 0.0,
            next_save_ticket: 0,
            lsp_document: None,
            lsp_intents: Vec::new(),
        }
    }

    /// Shares the safe editor with ordinary folders without inventing Git state.
    pub fn new_directory(descriptor: RepositoryView) -> Self {
        let mut runtime = Self::new(descriptor);
        runtime.directory_only = true;
        runtime
    }

    /// Hides workbench launch controls in an independent editor window.
    pub fn set_standalone(&mut self, standalone: bool) {
        self.standalone = standalone;
    }

    pub fn set_external_editors(&mut self, editors: Vec<crate::sidebar::SidebarEditor>) {
        self.external_editors = editors;
    }

    #[cfg(test)]
    pub(crate) fn post_persist_failure_fixture(card: CardId) -> (Self, RepositoryAction) {
        let path = PathBuf::from("notes.txt");
        let descriptor = RepositoryView {
            project: crate::model::ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = Self::new(descriptor);
        runtime.generation = 1;
        runtime.files = FileListState::Ready(vec![RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        }]);
        runtime.selected = runtime.file(&path).cloned();
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 1,
        };
        let mut document = EditorDocument::new(path, "A\n".into(), blake3::hash(b"A\n"), false);
        document.content = text_editor::Content::with_text("A\n");
        document.dirty = false;
        document.revision = 2;
        document.active_save = Some(SaveTicket {
            id: 1,
            identity: identity.clone(),
            revision: 1,
            snapshot: "B\n".into(),
        });
        runtime.document = DocumentState::Editable(document);
        let completion = RepositoryAction::SaveFinished {
            identity,
            ticket: 1,
            revision: 1,
            saved_text: "B\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"B\n"),
                durability: SaveDurability::Unconfirmed {
                    error: "injected parent sync failure".into(),
                },
            }),
        };
        (runtime, completion)
    }

    #[cfg(test)]
    pub(crate) fn post_persist_warning_fixture(card: CardId) -> Self {
        let (mut runtime, completion) = Self::post_persist_failure_fixture(card);
        let _ = runtime.update(completion);
        runtime
    }

    /// A runtime whose file list is `Ready` with exactly `paths` and nothing
    /// else: no selection, no open document, no save in flight.
    ///
    /// For callers that need [`Self::can_open_file`] to answer truthfully
    /// without also inheriting a fixture's document state.
    #[cfg(test)]
    pub(crate) fn file_list_fixture(descriptor: RepositoryView, paths: &[&str]) -> Self {
        let mut runtime = Self::new(descriptor);
        runtime.files = FileListState::Ready(
            paths
                .iter()
                .map(|path| RepositoryFile {
                    path: PathBuf::from(path),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: true,
                    status: Some(" M".into()),
                })
                .collect(),
        );
        runtime
    }

    #[cfg(test)]
    pub(crate) fn dirty_fixture(card: CardId) -> Self {
        let (mut runtime, _) = Self::post_persist_failure_fixture(card);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!("post-persist fixture has an editable document");
        };
        document.content = text_editor::Content::with_text("unsaved\n");
        document.dirty = true;
        document.revision = 1;
        document.active_save = None;
        runtime
    }

    #[cfg(test)]
    pub(crate) fn durable_active_save_completion(&self) -> RepositoryAction {
        let DocumentState::Editable(document) = &self.document else {
            panic!("test fixture requires an editable document");
        };
        let ticket = document
            .active_save
            .as_ref()
            .expect("test fixture requires an active save");
        RepositoryAction::SaveFinished {
            identity: ticket.identity.clone(),
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot.clone(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(ticket.snapshot.as_bytes()),
                durability: SaveDurability::Durable,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn failed_active_save_completion(&self) -> RepositoryAction {
        let DocumentState::Editable(document) = &self.document else {
            panic!("test fixture requires an editable document");
        };
        let ticket = document
            .active_save
            .as_ref()
            .expect("test fixture requires an active save");
        RepositoryAction::SaveFinished {
            identity: ticket.identity.clone(),
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot.clone(),
            result: Err(FileIoError::Io(
                "injected retry failure before persist".into(),
            )),
        }
    }

    /// A card whose file list is loaded and whose `notes.txt` selection is
    /// still in flight, plus the identity that selection's load will carry.
    ///
    /// `other.txt` is listed too, so a test can open a second file over the
    /// first the way the file list does.
    #[cfg(test)]
    pub(crate) fn lsp_selection_fixture(card: CardId) -> (Self, AsyncIdentity) {
        Self::selection_fixture(card, "notes.txt")
    }

    /// The same fixture under a chosen file name, so a test can vary the one
    /// thing a selection tells the highlighter: the extension it was opened
    /// under.
    #[cfg(test)]
    pub(crate) fn selection_fixture(card: CardId, name: &str) -> (Self, AsyncIdentity) {
        let path = PathBuf::from(name);
        let descriptor = RepositoryView {
            project: crate::model::ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = Self::new(descriptor);
        runtime.files = FileListState::Ready(
            [name, "other.txt"]
                .into_iter()
                .map(|path| RepositoryFile {
                    path: PathBuf::from(path),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: true,
                    status: Some(" M".into()),
                })
                .collect(),
        );
        // Drives the real selection path so the generation, the selection and
        // the loading document agree the way they do in the application.
        drop(runtime.open_file(card, path.clone()));
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path,
            generation: runtime.generation,
        };
        (runtime, identity)
    }

    /// The same card once `notes.txt` has loaded as an editable buffer holding
    /// `text`, with the resulting intents still queued.
    #[cfg(test)]
    pub(crate) fn lsp_open_fixture(card: CardId, text: &str) -> (Self, AsyncIdentity) {
        Self::open_fixture(card, "notes.txt", text)
    }

    /// The same, under a chosen file name.
    #[cfg(test)]
    pub(crate) fn open_fixture(card: CardId, name: &str, text: &str) -> (Self, AsyncIdentity) {
        let (mut runtime, identity) = Self::selection_fixture(card, name);
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: identity.clone(),
            result: Ok(FileContent::Editable {
                text: text.into(),
                content_hash: blake3::hash(text.as_bytes()),
                markdown: false,
            }),
        }));
        (runtime, identity)
    }

    /// Completion that reports the in-flight save found the file changed
    /// underneath it.
    #[cfg(test)]
    pub(crate) fn conflicted_active_save_completion(&self) -> RepositoryAction {
        let DocumentState::Editable(document) = &self.document else {
            panic!("test fixture requires an editable document");
        };
        let ticket = document
            .active_save
            .as_ref()
            .expect("test fixture requires an active save");
        RepositoryAction::SaveFinished {
            identity: ticket.identity.clone(),
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot.clone(),
            result: Ok(SaveOutcome::Conflict {
                disk_hash: blake3::hash(b"changed underneath the editor"),
            }),
        }
    }

    pub fn initial_load(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.files = FileListState::Loading;
        self.file_viewport = ListViewport::default();
        self.file_list_generation = self.file_list_generation.wrapping_add(1).max(1);
        let identity = AsyncIdentity {
            card,
            worktree: self.descriptor.worktree.clone(),
            path: PathBuf::from("."),
            generation: self.file_list_generation,
        };
        files_task(identity, self.directory_only)
    }

    /// Refreshes Git's status and mutable comparisons without replacing the
    /// open buffer or changing the identity of its loads and saves.
    pub fn refresh_after_mutation(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.mutation_refresh_pending = true;
        // Only object-id comparisons are immutable. A branch comparison also
        // reads HEAD, which may have moved in a commit.
        self.diffs
            .retain(|(basis, _)| matches!(basis, DiffBasis::CommitToParent { .. }));
        self.initial_load(card)
    }

    fn refresh_selected_status(&mut self, card: CardId) -> Task<RepositoryAction> {
        let Some(path) = self.selected.as_ref().map(|file| file.path.clone()) else {
            return Task::none();
        };
        // A discarded untracked file or committed deletion can disappear from
        // the list entirely. Keep its buffer, but remove all obsolete Git facts.
        let file = self.file(&path).cloned().unwrap_or_else(|| RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: false,
            status: None,
        });
        if !file.has_basis(&self.diff_basis) && self.basis_resolution.is_none() {
            self.diff_basis = if file.unstaged {
                DiffBasis::IndexToWorktree
            } else {
                DiffBasis::HeadToIndex
            };
            if !file.changed() {
                self.source_surface = SourceSurface::Editor;
            }
            self.diff_change = 0;
            self.diff_viewport = ListViewport::default();
        }
        self.selected = Some(file);
        self.ensure_current_diff(card)
    }

    /// Removes and returns every protocol notification the open buffer has
    /// earned since the last drain, oldest first.
    ///
    /// The application performs them in order against its
    /// [`crate::lsp::LspManager`]. Draining is destructive because a
    /// notification performed twice is worse than one performed late.
    pub fn take_lsp_intents(&mut self) -> Vec<LspIntent> {
        std::mem::take(&mut self.lsp_intents)
    }

    /// Identity of the open buffer at the LSP boundary, if the application
    /// gave it one.
    #[must_use]
    pub fn lsp_document(&self) -> Option<LspDocumentId> {
        self.lsp_document
    }

    /// Records the identity the application minted for the open buffer, or
    /// clears it once the buffer has been closed at the boundary.
    pub fn set_lsp_document(&mut self, doc: Option<LspDocumentId>) {
        self.lsp_document = doc;
    }

    /// The caret as the protocol counts it: Unicode scalars, not bytes, and
    /// lines as [`crate::lsp`] breaks them rather than as the editor does.
    ///
    /// `None` when no editable buffer is open, which is also every case where
    /// a hover or completion request would have nothing to ask about.
    #[must_use]
    pub fn cursor_position(&self) -> Option<LspPosition> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        let offset = caret_offset(&document.content, document.content.cursor().position)?;
        Some(position_for_offset(&document.content.text(), offset))
    }

    /// Moves the caret to a protocol position, for jump-to-location.
    ///
    /// Reports whether the position named a line this buffer has; a character
    /// past the end of that line clamps to its end. The caller is responsible
    /// for only passing [`crate::lsp::LspCoordinates::Scalar`] positions: a
    /// location whose target file was not open at the LSP boundary still
    /// carries UTF-16 code units, and this runtime has no way to tell the two
    /// apart from an [`LspPosition`] alone.
    pub fn move_cursor_to(&mut self, at: LspPosition) -> bool {
        let DocumentState::Editable(document) = &mut self.document else {
            return false;
        };
        let Some(offset) = byte_offset(&document.content.text(), at) else {
            return false;
        };
        let position = caret_for_offset(&document.content, offset);
        // `move_to` leaves an existing selection installed, which would turn a
        // jump into a selection stretching from wherever the user last was.
        document
            .content
            .perform(text_editor::Action::ClearSelection);
        document.content.move_to(text_editor::Cursor {
            position,
            selection: None,
        });
        true
    }

    /// The focus target of the editable buffer while that buffer is the
    /// surface actually on screen, and `None` while it is not.
    ///
    /// A card showing the diff, a Markdown preview, or a file that never
    /// became editable builds no editor, so there is nothing there to focus
    /// and a focus operation aimed at it would be delivered to no widget.
    #[must_use]
    pub fn editor_focus_target(&self, card: CardId) -> Option<widget::Id> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        let editing = self.source_surface == SourceSurface::Editor
            && !(document.markdown && self.markdown_mode == MarkdownMode::Preview);
        editing.then(|| editor_focus_id(card))
    }

    /// Gives the keyboard to the editable buffer.
    ///
    /// Moving the caret is not enough on its own to *show* a jump: an
    /// unfocused [`text_editor`] draws no caret at all, so a jump into a
    /// buffer the user has not clicked into lands invisibly. Callers that
    /// move the caret programmatically chain this after it.
    #[must_use]
    pub fn focus_editor<Message: 'static>(&self, card: CardId) -> Task<Message> {
        self.editor_focus_target(card)
            .map_or_else(Task::none, iced::widget::operation::focus)
    }

    /// Applies a completion's edits to the open buffer.
    ///
    /// Reports whether it was applied. A completion is refused rather than
    /// half-applied when any of its edits cannot be addressed against the
    /// current buffer or when two of them overlap, because a partially applied
    /// completion is a corrupted file the user did not ask for.
    ///
    /// A snippet is inserted as its literal
    /// [`crate::lsp::LspSnippet::text`] — equal to
    /// [`LspCompletionEdit::new_text`], so the text is right — and its tab
    /// stops are not entered. `text_editor` has no multi-caret or placeholder
    /// concept to enter them with; a following pass that adds one can consume
    /// [`crate::lsp::LspSnippet::stops`], which is why they are carried this
    /// far rather than dropped at the protocol boundary.
    ///
    /// Unlike an [`crate::lsp::LspLocation`], a completion edit carries no
    /// [`crate::lsp::LspCoordinates`] to check: the manager converts every
    /// completion range against the snapshot of *this* document, so scalar
    /// coordinates are the only kind that can arrive here. There is
    /// consequently nothing to refuse on that ground, and pretending to check
    /// would only suggest the field exists.
    pub fn apply_completion(&mut self, item: &LspCompletionItem) -> bool {
        let DocumentState::Editable(document) = &mut self.document else {
            return false;
        };
        let text = document.content.text();
        // Every range is resolved against the buffer as it stands *now*,
        // before anything is applied, and the edits are then applied
        // last-first. Resolving as we go would let an earlier insertion shift
        // a later edit's range out from under it — which is how "add the `use`
        // line and complete the call" quietly writes the call into the middle
        // of the import.
        let mut edits = Vec::with_capacity(1 + item.additional_edits.len());
        for edit in std::iter::once(&item.edit).chain(&item.additional_edits) {
            let Some(span) = completion_span(&text, edit) else {
                return false;
            };
            edits.push((span, edit.new_text.as_str()));
        }
        let primary = edits[0].0.clone();
        edits.sort_by(|left, right| left.0.start.cmp(&right.0.start));
        if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
            // Overlapping edits have no defined joint meaning; applying them
            // in either order would invent one.
            return false;
        }

        let mut updated = text;
        for (span, new_text) in edits.iter().rev() {
            updated.replace_range(span.clone(), new_text);
        }
        // The caret belongs after the text the user chose. Edits that sit
        // before the primary one displace it by however much they inserted
        // less however much they removed; none of them can reach past
        // `primary.start`, having just been proven disjoint, so this cannot
        // underflow.
        let (inserted, removed) = edits
            .iter()
            .filter(|(span, _)| span.start < primary.start)
            .fold((0, 0), |(inserted, removed), (span, new_text)| {
                (inserted + new_text.len(), removed + (span.end - span.start))
            });
        let caret = primary.start + item.edit.new_text.len() + inserted - removed;

        // One version for the whole application: the server is told about a
        // completion as the single document transition the user performed, not
        // as one transition per edit it happened to decompose into.
        let version = document.bump_lsp_version();
        document.replace_text(&updated);
        // Resolved against the rebuilt buffer, so the caret lands where the
        // editor now believes that offset to be.
        let position = caret_for_offset(&document.content, caret);
        document
            .content
            .perform(text_editor::Action::ClearSelection);
        document.content.move_to(text_editor::Cursor {
            position,
            selection: None,
        });
        if let Some(version) = version {
            self.lsp_intents.push(LspIntent::Change {
                version,
                text: updated,
            });
        }
        true
    }

    /// Records that the open buffer became the card's LSP document, and does
    /// nothing at all when what loaded was read-only or unreadable.
    ///
    /// The path comes from the buffer rather than from the caller, so the
    /// intent cannot name a file other than the one that was opened. It is
    /// worktree-relative, as every [`AsyncIdentity`] carries it, and is joined
    /// here because a document URI cannot be built from a relative path.
    fn push_lsp_open(&mut self, worktree: &Path) {
        let DocumentState::Editable(document) = &self.document else {
            return;
        };
        self.lsp_intents.push(LspIntent::Open {
            absolute_path: worktree.join(&document.path),
            worktree: worktree.to_path_buf(),
            version: document.lsp_version,
            text: document.content.text(),
        });
    }

    /// Whether `path` names a row this card's file list can open.
    ///
    /// A location can resolve into a file the card has no row for - a
    /// gitignored or generated source a build script produced - or arrive
    /// while the list is still loading, and [`Self::update`]'s `OpenFile`
    /// answers both by doing nothing. A caller about to park work on that open
    /// has to know that before it waits for something that will never happen.
    #[must_use]
    pub fn can_open_file(&self, path: &Path) -> bool {
        self.file(path).is_some()
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.selected.as_ref().map(|file| file.path.as_path())
    }

    pub fn selected_staging_selection(
        &self,
        stage: bool,
    ) -> Option<crate::repository::PathSelection> {
        use crate::repository::{PathSelection, RepoPath};
        let file = self.selected.as_ref()?;
        if self.mutation_refresh_pending
            || self.blocks_navigation()
            || !(if stage { file.unstaged } else { file.staged })
        {
            return None;
        }
        let old_path = if stage {
            &file.unstaged_old_path
        } else {
            &file.staged_old_path
        };
        Some(PathSelection {
            path: RepoPath::parse(file.path.as_os_str()).ok()?,
            old_path: old_path
                .as_ref()
                .map(|path| RepoPath::parse(path.as_os_str()))
                .transpose()
                .ok()?,
        })
    }

    /// Re-announces the open buffer to a replaced language-server manager.
    ///
    /// A manager's server set is fixed at construction, so a changed
    /// configuration is a new manager holding no documents. Without this the
    /// buffer stays open on screen while the servers it is checked against know
    /// nothing about it, and no diagnostic arrives until the next keystroke.
    pub fn resync_lsp(&mut self) {
        let worktree = self.descriptor.worktree.clone();
        self.push_lsp_open(&worktree);
    }

    /// Records that the open buffer's text changed, whatever changed it.
    ///
    /// Silent once the version space is spent: the buffer keeps working and
    /// the server keeps its last coherent version rather than being rewound.
    fn push_lsp_change(&mut self) {
        let DocumentState::Editable(document) = &mut self.document else {
            return;
        };
        let Some(version) = document.bump_lsp_version() else {
            return;
        };
        let text = document.content.text();
        self.lsp_intents.push(LspIntent::Change { version, text });
    }

    /// Records that the open buffer reached disk.
    fn push_lsp_save(&mut self) {
        let DocumentState::Editable(document) = &self.document else {
            return;
        };
        self.lsp_intents.push(LspIntent::Save {
            text: document.saved_text.clone(),
        });
    }

    /// Records that the open buffer is about to stop existing.
    ///
    /// Conditioned on there being an editable buffer rather than on
    /// [`RepositoryRuntime::lsp_document`], which is only set once the
    /// application has drained the matching [`LspIntent::Open`]: gating on it
    /// would drop the close of a document opened and abandoned between two
    /// drains, and leak that session.
    fn push_lsp_close(&mut self) {
        if matches!(&self.document, DocumentState::Editable(_)) {
            self.lsp_intents.push(LspIntent::Close);
        }
    }

    fn can_show_diff(&self) -> bool {
        !self.is_saving()
            && !self.directory_only
            && self.selected.as_ref().is_some_and(|file| {
                file.has_basis(&self.diff_basis) || self.basis_resolution.is_some()
            })
    }

    fn can_apply_markdown_mode(&self, preview: bool) -> bool {
        !self.is_saving()
            && match &self.document {
                DocumentState::Editable(document) => !preview || document.markdown,
                DocumentState::Image { .. } => preview,
                _ => false,
            }
    }

    fn can_select_diff_basis(&self, basis: &DiffBasis) -> bool {
        !self.is_saving()
            && !self.directory_only
            && (!self.mutation_refresh_pending || matches!(basis, DiffBasis::CommitToParent { .. }))
            && self
                .selected
                .as_ref()
                .is_some_and(|file| file.has_basis(basis))
    }

    fn can_select_committed_basis(&self, choice: CommittedChoice) -> bool {
        !self.is_saving()
            && !self.directory_only
            && self.selected.is_some()
            && self.basis_resolution != Some(BasisResolution::Resolving(choice))
    }

    fn can_change_diff_layout(&self) -> bool {
        !self.is_saving()
            && self.source_surface == SourceSurface::Diff
            && self.diff_model().is_some()
    }

    fn can_navigate_diff(&self) -> bool {
        self.can_change_diff_layout() && self.change_count() != 0
    }

    fn can_retry_diff(&self) -> bool {
        self.source_surface == SourceSurface::Diff
            && self.basis_resolution.is_none()
            && self.can_select_diff_basis(&self.diff_basis)
            && matches!(
                self.diff_state(&self.diff_basis),
                Some(DiffState::Ready(RenderedDiff::Error(_)))
            )
    }

    pub fn is_dirty(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.dirty)
    }

    pub fn can_save(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if (document.dirty || document.durability_warning.is_some()) && document.active_save.is_none())
    }

    fn can_discard_edits(&self) -> bool {
        self.is_dirty() && !self.is_saving()
    }

    pub fn can_open_external(&self) -> bool {
        !self.standalone && self.selected.is_some() && !self.is_saving()
    }

    pub fn is_saving(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.active_save.is_some())
    }

    pub fn blocks_navigation(&self) -> bool {
        self.is_dirty()
            || self.is_saving()
            || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning.is_some())
    }

    /// Whether abandoning the editor buffer is sufficient to release every
    /// repository navigation fence.
    ///
    /// A post-persist durability warning describes unresolved disk state, not
    /// merely unsaved editor contents. Discarding the buffer cannot resolve it.
    pub fn can_abandon_unsaved_changes(&self) -> bool {
        matches!(
            &self.document,
            DocumentState::Editable(document)
                if document.dirty
                    && document.active_save.is_none()
                    && document.durability_warning.is_none()
        )
    }

    pub fn has_durability_warning(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.durability_warning.is_some())
    }

    pub fn inspector_context(&self) -> RepositoryInspectorContext {
        let (file_list, file_count, file_error) = match &self.files {
            FileListState::Loading => ("Loading", None, None),
            FileListState::Ready(files) => ("Ready", Some(files.len()), None),
            FileListState::Failed(error) => ("Unavailable", None, Some(error.clone())),
        };
        let selected_path = self.selected.as_ref().map(|file| file.path.clone());
        let selected_status = self.selected.as_ref().and_then(|file| file.status.clone());
        let document = match &self.document {
            DocumentState::None => "No file selected",
            DocumentState::Loading(_) => "Loading",
            DocumentState::Editable(_) if self.is_saving() => "Saving",
            DocumentState::Editable(document) if document.conflict => "Save conflict",
            DocumentState::Editable(document) if document.durability_warning.is_some() => {
                "Durability warning"
            }
            DocumentState::Editable(document) if document.error.is_some() => "Save failed",
            DocumentState::Editable(_) if self.is_dirty() => "Unsaved changes",
            DocumentState::Editable(_) => "Editable",
            DocumentState::Image { .. } => "Image preview",
            DocumentState::ReadOnly { .. } => "Read-only",
            DocumentState::Failed { .. } => "Unavailable",
        };
        let (document_error, conflict) = match &self.document {
            DocumentState::Editable(document) => (
                document
                    .error
                    .clone()
                    .or_else(|| document.durability_warning.clone()),
                document.conflict,
            ),
            DocumentState::Failed { message, .. } => (Some(message.clone()), false),
            _ => (None, false),
        };
        let selected = self.selected.is_some();
        let selected_changed = self.selected.as_ref().is_some_and(RepositoryFile::changed);
        let has_staged = self.selected.as_ref().is_some_and(|file| file.staged);
        let has_unstaged = self.selected.as_ref().is_some_and(|file| file.unstaged);
        let editor = selected && self.source_surface == SourceSurface::Editor;
        let diff = selected && self.source_surface == SourceSurface::Diff;
        let markdown_mode = editor.then_some(match self.markdown_mode {
            MarkdownMode::Edit => "Edit",
            MarkdownMode::Preview => "Preview",
        });
        let markdown_mode =
            matches!(&self.document, DocumentState::Editable(document) if document.markdown)
                .then_some(markdown_mode)
                .flatten();
        let (diff_state, diff_error, diff_additions, diff_deletions, diff_change) = if !diff {
            (None, None, None, None, None)
        } else if let Some(resolution) = &self.basis_resolution {
            // An unresolved comparison is the whole story of the surface, so
            // no stale diff is reported behind it.
            match resolution {
                BasisResolution::Resolving(_) => (Some("Loading"), None, None, None, None),
                BasisResolution::NeedsBase(_) => (Some("Choose a base"), None, None, None, None),
                BasisResolution::Unavailable(message) => {
                    (Some("Unavailable"), Some(message.clone()), None, None, None)
                }
            }
        } else {
            match self.diff_state(&self.diff_basis) {
                None => (Some("No changes"), None, Some(0), Some(0), None),
                Some(DiffState::Loading { .. }) => (Some("Loading"), None, None, None, None),
                Some(DiffState::Ready(RenderedDiff::Text { model, .. })) => {
                    let changes = model.change_starts(self.diff_mode).len();
                    (
                        Some(if changes == 0 { "No changes" } else { "Ready" }),
                        None,
                        Some(model.additions),
                        Some(model.deletions),
                        (changes != 0).then_some((self.diff_change + 1, changes)),
                    )
                }
                Some(DiffState::Ready(RenderedDiff::Error(error))) => {
                    (Some("Unavailable"), Some(error.clone()), None, None, None)
                }
                Some(DiffState::Ready(RenderedDiff::NoText(content))) => {
                    (Some(no_text_kind(content)), None, None, None, None)
                }
            }
        };
        // A retry re-reads the comparison. A basis that never resolved has no
        // comparison to re-read, and its own surface offers the base choice,
        // so the retry control stays unarmed for it.
        let can_retry_diff = self.can_retry_diff();
        RepositoryInspectorContext {
            file_list,
            file_count,
            file_error,
            selected_path,
            selected_status,
            selected_changed,
            has_staged,
            has_unstaged,
            surface: if !selected {
                "Files"
            } else if diff {
                "Diff"
            } else {
                "Editor"
            },
            document,
            document_error,
            conflict,
            markdown_mode,
            diff_basis: diff.then(|| basis_choice_label(&self.diff_basis)),
            diff_mode: diff.then_some(match self.diff_mode {
                DiffMode::Inline => "Inline",
                DiffMode::Split => "Split",
            }),
            diff_state,
            diff_error,
            diff_additions,
            diff_deletions,
            diff_change,
            dirty: self.is_dirty(),
            saving: self.is_saving(),
            can_save: self.can_save(),
            // Discarding the local buffer is still a valid editor action when
            // a durability warning remains; it just cannot authorize closing
            // or navigation, because that disk-state fence stays installed.
            can_discard: self.can_discard_edits(),
            can_reload: conflict
                || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning.is_some())
                || matches!(&self.document, DocumentState::Failed { .. }),
            can_retry_diff,
            can_show_diff: self.can_show_diff(),
            can_change_diff_layout: self.can_change_diff_layout(),
            can_select_staged_diff: self.can_select_diff_basis(&DiffBasis::HeadToIndex),
            can_select_unstaged_diff: self.can_select_diff_basis(&DiffBasis::IndexToWorktree),
            can_back: selected && !self.blocks_navigation(),
        }
    }

    pub fn inspector_runtime_status(&self) -> RepositoryRuntimeStatus {
        match &self.files {
            FileListState::Loading => return RepositoryRuntimeStatus::Loading,
            FileListState::Failed(_) => return RepositoryRuntimeStatus::Unavailable,
            FileListState::Ready(_) => {}
        }
        match &self.document {
            DocumentState::Loading(_) => RepositoryRuntimeStatus::Loading,
            DocumentState::Failed { .. } => RepositoryRuntimeStatus::Unavailable,
            _ if self.source_surface == SourceSurface::Diff => match &self.basis_resolution {
                Some(BasisResolution::Resolving(_)) => RepositoryRuntimeStatus::Loading,
                Some(BasisResolution::NeedsBase(_) | BasisResolution::Unavailable(_)) => {
                    RepositoryRuntimeStatus::Unavailable
                }
                None => match self.diff_state(&self.diff_basis) {
                    Some(DiffState::Loading { .. }) => RepositoryRuntimeStatus::Loading,
                    Some(DiffState::Ready(RenderedDiff::Error(_))) => {
                        RepositoryRuntimeStatus::Unavailable
                    }
                    _ => RepositoryRuntimeStatus::Ready,
                },
            },
            _ => RepositoryRuntimeStatus::Ready,
        }
    }

    pub fn abandon_unsaved_changes(&mut self) -> bool {
        if !self.can_abandon_unsaved_changes() {
            return false;
        }
        let DocumentState::Editable(document) = &mut self.document else {
            unreachable!("discard eligibility requires an editable document");
        };
        document.discard_edits();
        // Reached from the application's dirty-close dialog, which throws the
        // buffer away without the editor ever seeing an action for it.
        self.push_lsp_change();
        true
    }

    pub fn save_error(&self) -> Option<&str> {
        match &self.document {
            DocumentState::Editable(document) => document
                .error
                .as_deref()
                .or(document.durability_warning.as_deref()),
            _ => None,
        }
    }

    pub fn has_save_conflict(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.conflict)
    }

    pub fn set_auto_save(&mut self, enabled: bool) {
        if self.auto_save != enabled {
            self.auto_save = enabled;
            self.auto_save_pending = None;
            self.next_auto_save_ticket = self.next_auto_save_ticket.wrapping_add(1);
        }
    }

    fn auto_save_revision(&self) -> Option<u64> {
        match &self.document {
            DocumentState::Editable(document)
                if self.auto_save
                    && document.dirty
                    && document.active_save.is_none()
                    && !document.conflict
                    && document.error.is_none()
                    && document.durability_warning.is_none() =>
            {
                Some(document.revision)
            }
            _ => None,
        }
    }

    /// Called after buffer changes (including LSP completions) and save results.
    /// Unchanged revisions do not create timers; stale timers cannot write.
    pub fn auto_save_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        let Some(revision) = self.auto_save_revision() else {
            self.auto_save_pending = None;
            return Task::none();
        };
        let pending = (self.generation, revision);
        if self.auto_save_pending == Some(pending) {
            return Task::none();
        }
        let Some(selected) = self.selected.as_ref() else {
            return Task::none();
        };
        let identity = self.current_identity(card, selected.path.clone());
        self.auto_save_pending = Some(pending);
        self.next_auto_save_ticket = self.next_auto_save_ticket.wrapping_add(1);
        let ticket = self.next_auto_save_ticket;
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            RepositoryAction::AutoSaveDue {
                identity,
                revision,
                ticket,
            }
        })
    }

    pub fn request_save(&mut self, card: CardId, overwrite: bool) -> Task<RepositoryAction> {
        self.request_save_inner(card, overwrite, true)
    }

    fn request_save_inner(
        &mut self,
        card: CardId,
        overwrite: bool,
        reveal_editor: bool,
    ) -> Task<RepositoryAction> {
        let (path, text, disk_hash, revision) = match &self.document {
            DocumentState::Editable(document)
                if (document.dirty || document.durability_warning.is_some())
                    && document.active_save.is_none() =>
            {
                (
                    document.path.clone(),
                    document.content.text(),
                    document.disk_hash,
                    document.revision,
                )
            }
            _ => return Task::none(),
        };
        let identity = self.current_identity(card, path);
        self.next_save_ticket = self.next_save_ticket.wrapping_add(1).max(1);
        let ticket = SaveTicket {
            id: self.next_save_ticket,
            identity: identity.clone(),
            revision,
            snapshot: text.clone(),
        };
        if let DocumentState::Editable(document) = &mut self.document {
            document.active_save = Some(ticket.clone());
            document.error = None;
        }
        // Explicit saves expose recovery immediately; autosave must not steal
        // the user's Markdown preview or diff while writing in the background.
        if reveal_editor {
            self.source_surface = SourceSurface::Editor;
            self.markdown_mode = MarkdownMode::Edit;
        }
        let worktree = identity.worktree.clone();
        let relative_path = identity.path.clone();
        let saved_text = text.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                save_file(&worktree, &relative_path, &text, disk_hash, overwrite)
            })
            .await
            .map_err(|error| FileIoError::Io(format!("save worker failed: {error}")))
            .and_then(|result| result);
            RepositoryAction::SaveFinished {
                identity,
                ticket: ticket.id,
                revision,
                saved_text,
                result,
            }
        })
    }

    pub fn update(&mut self, action: RepositoryAction) -> Task<RepositoryAction> {
        match action {
            RepositoryAction::RefreshFiles { card } => self.initial_load(card),
            RepositoryAction::NewTerminal { .. } => Task::none(),
            RepositoryAction::StageSelected { .. } => Task::none(),
            RepositoryAction::FilesLoaded { identity, result } => {
                if !self.accepts_file_list(&identity) || identity.path != Path::new(".") {
                    return Task::none();
                }
                // Ordinary list refreshes leave the selection's snapshot alone.
                // Mutation refreshes explicitly reconcile it after this newest
                // authoritative status read, without replacing the document.
                let refresh_selected = self.mutation_refresh_pending && result.is_ok();
                self.files = result.map_or_else(FileListState::Failed, FileListState::Ready);
                if refresh_selected {
                    self.mutation_refresh_pending = false;
                    self.refresh_selected_status(identity.card)
                } else {
                    Task::none()
                }
            }
            RepositoryAction::OpenFile { card, path } => {
                if self.blocks_navigation() {
                    return Task::none();
                }
                self.open_file(card, path)
            }
            RepositoryAction::FileLoaded { identity, result } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                self.document = match result {
                    Ok(FileContent::Editable {
                        text,
                        content_hash,
                        markdown,
                    }) => DocumentState::Editable(EditorDocument::new(
                        identity.path,
                        text,
                        content_hash,
                        markdown,
                    )),
                    Ok(FileContent::Image {
                        width,
                        height,
                        rgba,
                    }) => DocumentState::Image {
                        handle: iced::widget::image::Handle::from_rgba(
                            width,
                            height,
                            iced::advanced::graphics::core::Bytes::from_owner(rgba),
                        ),
                        width,
                        height,
                    },
                    Ok(FileContent::ReadOnly(reason)) => DocumentState::ReadOnly {
                        path: identity.path,
                        reason,
                    },
                    Err(error) => DocumentState::Failed {
                        path: identity.path,
                        message: error.to_string(),
                    },
                };
                // A read-only or unreadable file is not a document a language
                // server can serve, and `push_lsp_open` reports nothing for
                // either of them.
                self.push_lsp_open(&identity.worktree);
                Task::none()
            }
            RepositoryAction::DiffLoaded {
                identity,
                basis,
                request,
                content,
            } => {
                // Three independent fences: the generation and path in the
                // identity, the per-basis slot the result is keyed by, and the
                // request counter within that slot. A result for a basis the
                // user has left lands in its own slot and is displayed by
                // nothing; a superseded result for the same basis is dropped.
                let current_request = matches!(
                    self.diff_state(&basis),
                    Some(DiffState::Loading { request: active, .. }) if *active == request
                );
                if self.accepts_selected(&identity) && current_request {
                    let current = basis == self.diff_basis;
                    self.set_diff_state(basis, DiffState::Ready(content));
                    if current {
                        self.diff_change = 0;
                        self.diff_viewport = ListViewport::default();
                    }
                }
                Task::none()
            }
            RepositoryAction::EditorAction { action, .. } => {
                let is_edit = action.is_edit();
                if let DocumentState::Editable(document) = &mut self.document {
                    document.apply(action);
                }
                // A caret movement is not a document change. Bumping the
                // version for one would leave every request the user makes
                // next quoting a version the server has never been sent.
                if is_edit {
                    self.push_lsp_change();
                }
                Task::none()
            }
            RepositoryAction::Save { card, overwrite } => self.request_save(card, overwrite),
            RepositoryAction::AutoSaveDue {
                identity,
                revision,
                ticket,
            } => {
                if ticket != self.next_auto_save_ticket
                    || !self.accepts_selected(&identity)
                    || self.auto_save_pending != Some((identity.generation, revision))
                    || self.auto_save_revision() != Some(revision)
                {
                    return Task::none();
                }
                self.auto_save_pending = None;
                self.request_save_inner(identity.card, false, false)
            }
            RepositoryAction::SaveFinished {
                identity,
                ticket,
                revision,
                saved_text,
                result,
            } => {
                let active_matches = matches!(
                    &self.document,
                    DocumentState::Editable(document)
                        if document.active_save.as_ref().is_some_and(|active| {
                            active.id == ticket
                                && active.identity == identity
                                && active.revision == revision
                                && active.snapshot == saved_text
                        })
                );
                if !active_matches {
                    if let DocumentState::Editable(document) = &mut self.document
                        && document.active_save.is_some()
                    {
                        document.active_save = None;
                        document.dirty = true;
                        document.conflict = true;
                        document.error = Some(
                            "The save result did not match the active document snapshot; the disk state is unknown. Reload or overwrite before leaving."
                                .into(),
                        );
                    }
                    return Task::none();
                }
                let mut saved = false;
                if let DocumentState::Editable(document) = &mut self.document {
                    document.active_save = None;
                    match result {
                        Ok(SaveOutcome::Saved {
                            content_hash,
                            durability,
                        }) => {
                            document.saved_text = saved_text;
                            document.disk_hash = content_hash;
                            document.dirty = document.content.text() != document.saved_text;
                            document.conflict = false;
                            document.error = None;
                            document.durability_warning = match durability {
                                SaveDurability::Durable => None,
                                SaveDurability::Unconfirmed { error } => Some(format!(
                                    "The file was replaced on disk, but its durability could not be confirmed: {error}. Reload from disk or retry the save before leaving."
                                )),
                            };
                            saved = true;
                        }
                        Ok(SaveOutcome::Conflict { .. }) => {
                            document.conflict = true;
                            document.error =
                                Some("The file changed on disk after it was opened.".into());
                        }
                        Err(error) => {
                            let error = error.to_string();
                            document.error = Some(
                                document
                                    .durability_warning
                                    .as_ref()
                                    .map_or(error.clone(), |warning| format!("{error} {warning}")),
                            );
                        }
                    }
                }
                if self.save_error().is_some() {
                    self.source_surface = SourceSurface::Editor;
                    self.markdown_mode = MarkdownMode::Edit;
                }
                if saved {
                    // Only here: not when the write was submitted, not on a
                    // conflict, and not on a failure — in none of those does
                    // the file on disk match this buffer.
                    self.push_lsp_save();
                    if self.mutation_refresh_pending {
                        // Status may have been read before this save reached
                        // disk. Supersede that read rather than let it erase the
                        // newly saved worktree change.
                        return self.refresh_after_mutation(identity.card);
                    }
                    let Some(selected) = self.selected.as_mut() else {
                        return Task::none();
                    };
                    selected.status = Some(status_with_unstaged_change(
                        selected.status.as_deref(),
                        selected.unstaged,
                    ));
                    selected.unstaged = true;
                    if !selected.has_basis(&self.diff_basis) && self.basis_resolution.is_none() {
                        self.diff_basis = DiffBasis::IndexToWorktree;
                    }
                    self.diffs
                        .retain(|(basis, _)| *basis != DiffBasis::IndexToWorktree);
                    self.ensure_current_diff(identity.card)
                } else {
                    Task::none()
                }
            }
            RepositoryAction::Reload { card } => {
                if self.is_saving() {
                    return Task::none();
                }
                let Some(path) = self.selected.as_ref().map(|file| file.path.clone()) else {
                    return Task::none();
                };
                self.open_file(card, path)
            }
            RepositoryAction::DiscardEdits { .. } => {
                if !self.can_discard_edits() {
                    return Task::none();
                }
                if let DocumentState::Editable(document) = &mut self.document {
                    document.discard_edits();
                    // `discard_edits` rebuilds the buffer without going
                    // through an editor action, so nothing else here would
                    // notice that the server's copy just became wrong.
                    self.push_lsp_change();
                }
                Task::none()
            }
            RepositoryAction::BackToFiles { card } => {
                if self.blocks_navigation() {
                    return Task::none();
                }
                self.push_lsp_close();
                self.selected = None;
                self.document = DocumentState::None;
                self.diffs.clear();
                self.basis_resolution = None;
                self.source_surface = SourceSurface::Editor;
                self.initial_load(card)
            }
            RepositoryAction::ApplySourceSurface { card, diff } => {
                let surface = if diff {
                    SourceSurface::Diff
                } else {
                    SourceSurface::Editor
                };
                if self.is_saving()
                    || self.selected.is_none()
                    || (surface == SourceSurface::Diff && !self.can_show_diff())
                {
                    return Task::none();
                }
                self.source_surface = surface;
                self.ensure_current_diff(card)
            }
            RepositoryAction::ApplyMarkdownMode { preview, .. } => {
                if !self.can_apply_markdown_mode(preview) {
                    return Task::none();
                }
                self.source_surface = SourceSurface::Editor;
                self.diffs
                    .retain(|(_, state)| !matches!(state, DiffState::Loading { .. }));
                self.markdown_mode = if preview {
                    MarkdownMode::Preview
                } else {
                    MarkdownMode::Edit
                };
                Task::none()
            }
            RepositoryAction::ApplyDiffMode { split, .. } => {
                if !self.can_change_diff_layout() {
                    return Task::none();
                }
                self.diff_mode = if split {
                    DiffMode::Split
                } else {
                    DiffMode::Inline
                };
                self.diff_change = 0;
                self.diff_viewport = ListViewport::default();
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::SelectCommittedBasis { card, choice } => {
                if !self.can_select_committed_basis(choice) {
                    return Task::none();
                }
                let Some(selected) = self.selected.as_ref() else {
                    return Task::none();
                };
                let identity = self.current_identity(card, selected.path.clone());
                if self.is_saving() {
                    return Task::none();
                }
                self.source_surface = SourceSurface::Diff;
                self.basis_resolution = Some(BasisResolution::Resolving(choice));
                self.diffs
                    .retain(|(_, state)| !matches!(state, DiffState::Loading { .. }));
                basis_task(identity, choice)
            }
            RepositoryAction::DiffBasisResolved {
                identity,
                choice,
                result,
            } => {
                // A resolution nobody is waiting for must not install a basis:
                // the selection, or the choice, has moved on since it started.
                if !self.accepts_selected(&identity)
                    || self.basis_resolution != Some(BasisResolution::Resolving(choice))
                {
                    return Task::none();
                }
                match result {
                    Ok(ResolvedBasis::Ready(basis)) => {
                        self.basis_resolution = None;
                        return cosmic::task::message(RepositoryAction::ApplyDiffBasis {
                            card: identity.card,
                            basis,
                        });
                    }
                    Ok(ResolvedBasis::NeedsBase { candidates }) => {
                        self.basis_resolution = Some(BasisResolution::NeedsBase(candidates));
                    }
                    Ok(ResolvedBasis::NoCommit) => {
                        self.basis_resolution = Some(BasisResolution::Unavailable(
                            "This branch has no commits yet, so there is nothing to compare."
                                .into(),
                        ));
                    }
                    Err(error) => {
                        self.basis_resolution = Some(BasisResolution::Unavailable(error));
                    }
                }
                Task::none()
            }
            RepositoryAction::ApplyDiffBasis { card, basis } => {
                if !self.can_select_diff_basis(&basis) {
                    return Task::none();
                }
                self.basis_resolution = None;
                self.source_surface = SourceSurface::Diff;
                self.diff_basis = basis;
                self.diff_change = 0;
                self.diff_viewport = ListViewport::default();
                let load = self.ensure_current_diff(card);
                self.realize_current_change();
                Task::batch([load, self.scroll_to_change()])
            }
            RepositoryAction::RetryDiff { card } => {
                if !self.can_retry_diff() {
                    return Task::none();
                }
                let Some(selected) = self.selected.as_ref() else {
                    return Task::none();
                };
                if !selected.has_basis(&self.diff_basis) {
                    return Task::none();
                }
                let previous_path = selected.previous_path(&self.diff_basis);
                let identity = self.current_identity(card, selected.path.clone());
                let basis = self.diff_basis.clone();
                self.request_diff(identity, basis, previous_path)
            }
            RepositoryAction::FileViewport { offset, height, .. } => {
                self.file_viewport = ListViewport { offset, height };
                Task::none()
            }
            RepositoryAction::DiffViewport {
                offset,
                height,
                horizontal,
                ..
            } => {
                self.diff_viewport = ListViewport { offset, height };
                self.diff_horizontal = horizontal;
                Task::none()
            }
            RepositoryAction::PreviousChange { .. } => {
                if !self.can_navigate_diff() {
                    return Task::none();
                }
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + changes - 1) % changes;
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::NextChange { .. } => {
                if !self.can_navigate_diff() {
                    return Task::none();
                }
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + 1) % changes;
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::MarkdownLink { .. } | RepositoryAction::OpenExternal { .. } => {
                Task::none()
            }
        }
    }

    /// The state of one comparison, or `None` when it has never been asked for.
    fn diff_state(&self, basis: &DiffBasis) -> Option<&DiffState> {
        self.diffs
            .iter()
            .find(|(candidate, _)| candidate == basis)
            .map(|(_, state)| state)
    }

    fn set_diff_state(&mut self, basis: DiffBasis, state: DiffState) {
        match self
            .diffs
            .iter_mut()
            .find(|(candidate, _)| *candidate == basis)
        {
            Some(entry) => entry.1 = state,
            None => {
                self.diffs.push((basis, state));
                self.prune_committed_diffs();
            }
        }
    }

    /// Drops the committed comparisons added longest ago, beyond the cap.
    ///
    /// Never the one on screen, and never a working basis. Dropping a loading
    /// slot cancels its worker; the completion fence also prevents an already
    /// queued answer from recreating that abandoned slot.
    fn prune_committed_diffs(&mut self) {
        let committed =
            |basis: &DiffBasis| matches!(basis_choice(basis), BasisChoice::Committed(_));
        let mut excess = self
            .diffs
            .iter()
            .filter(|(basis, _)| committed(basis))
            .count()
            .saturating_sub(MAX_RETAINED_COMMITTED_DIFFS);
        if excess == 0 {
            return;
        }
        let current = self.diff_basis.clone();
        // `retain` visits in insertion order, so the oldest go first.
        self.diffs.retain(|(basis, _)| {
            if excess == 0 || *basis == current || !committed(basis) {
                return true;
            }
            excess -= 1;
            false
        });
    }

    /// Only the visible comparison earns a worker. Completed comparisons remain
    /// cached, but navigating away cancels unfinished work rather than merely
    /// discarding its expensive result later.
    fn ensure_current_diff(&mut self, card: CardId) -> Task<RepositoryAction> {
        let showing_diff =
            self.source_surface == SourceSurface::Diff && self.basis_resolution.is_none();
        self.diffs.retain(|(basis, state)| {
            !matches!(state, DiffState::Loading { .. })
                || (showing_diff && *basis == self.diff_basis)
        });
        if !showing_diff || self.diff_state(&self.diff_basis).is_some() {
            return Task::none();
        }
        let Some(selected) = self.selected.as_ref() else {
            return Task::none();
        };
        if !selected.has_basis(&self.diff_basis) {
            return Task::none();
        }
        let old_path = selected.previous_path(&self.diff_basis);
        let identity = self.current_identity(card, selected.path.clone());
        self.request_diff(identity, self.diff_basis.clone(), old_path)
    }

    fn request_diff(
        &mut self,
        identity: AsyncIdentity,
        basis: DiffBasis,
        old_path: Option<PathBuf>,
    ) -> Task<RepositoryAction> {
        if self.mutation_refresh_pending && !matches!(basis, DiffBasis::CommitToParent { .. }) {
            return Task::none();
        }
        self.next_diff_request = self.next_diff_request.wrapping_add(1).max(1);
        let request = self.next_diff_request;
        let cancellation = CancellationToken::new();
        self.set_diff_state(
            basis.clone(),
            DiffState::Loading {
                request,
                cancellation: cancellation.clone(),
            },
        );
        diff_task(identity, basis, request, old_path, cancellation)
    }

    fn open_file(&mut self, card: CardId, path: PathBuf) -> Task<RepositoryAction> {
        let file = match self.file(&path).cloned() {
            Some(file) => file,
            None if self.standalone && crate::repository::RepoPath::parse(&path).is_ok() => {
                RepositoryFile {
                    path: path.clone(),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: false,
                    status: None,
                }
            }
            None => return Task::none(),
        };
        // Whatever was open is about to be replaced — including by a reload of
        // this very path, which is a different document however identical its
        // name. Emitted before the replacement, while the old buffer still
        // exists to be closed.
        self.push_lsp_close();
        self.selected = Some(file.clone());
        self.document = DocumentState::Loading(path.clone());
        self.diffs.clear();
        self.basis_resolution = None;
        self.diff_viewport = ListViewport::default();
        self.diff_basis = if file.unstaged {
            DiffBasis::IndexToWorktree
        } else {
            DiffBasis::HeadToIndex
        };
        self.source_surface = SourceSurface::Editor;
        self.markdown_mode = MarkdownMode::Edit;
        self.diff_change = 0;
        let identity = self.next_identity(card, path);
        file_task(identity)
    }

    fn file(&self, path: &Path) -> Option<&RepositoryFile> {
        match &self.files {
            FileListState::Ready(files) => files.iter().find(|file| file.path == path),
            _ => None,
        }
    }

    fn next_identity(&mut self, card: CardId, path: PathBuf) -> AsyncIdentity {
        self.generation = self.generation.wrapping_add(1).max(1);
        AsyncIdentity {
            card,
            worktree: self.descriptor.worktree.clone(),
            path,
            generation: self.generation,
        }
    }

    fn current_identity(&self, card: CardId, path: PathBuf) -> AsyncIdentity {
        AsyncIdentity {
            card,
            worktree: self.descriptor.worktree.clone(),
            path,
            generation: self.generation,
        }
    }

    fn accepts(&self, identity: &AsyncIdentity) -> bool {
        identity.worktree == self.descriptor.worktree && identity.generation == self.generation
    }

    fn accepts_file_list(&self, identity: &AsyncIdentity) -> bool {
        identity.worktree == self.descriptor.worktree
            && identity.generation == self.file_list_generation
    }

    fn accepts_selected(&self, identity: &AsyncIdentity) -> bool {
        self.accepts(identity)
            && self
                .selected
                .as_ref()
                .is_some_and(|file| file.path == identity.path)
    }

    fn diff_model(&self) -> Option<&DiffModel> {
        match self.diff_state(&self.diff_basis) {
            Some(DiffState::Ready(RenderedDiff::Text { model, .. })) => Some(model),
            _ => None,
        }
    }

    fn change_count(&self) -> usize {
        self.diff_model()
            .map_or(0, |model| model.change_starts(self.diff_mode).len())
    }

    fn realize_current_change(&mut self) {
        let row = self
            .diff_model()
            .and_then(|model| model.change_starts(self.diff_mode).get(self.diff_change))
            .copied();
        if let Some(row) = row {
            self.diff_viewport.offset = row as f32 * DIFF_ROW_HEIGHT;
        }
    }

    fn scroll_to_change(&self) -> Task<RepositoryAction> {
        let Some(model) = self.diff_model() else {
            return Task::none();
        };
        let starts = model.change_starts(self.diff_mode);
        let Some(row) = starts.get(self.diff_change) else {
            return Task::none();
        };
        let denominator = model.row_count(self.diff_mode).saturating_sub(1).max(1);
        let y = *row as f32 / denominator as f32;
        iced::widget::scrollable::snap_to(
            self.diff_scroll_id.clone(),
            iced::widget::scrollable::RelativeOffset {
                x: None,
                y: Some(y),
            },
        )
    }
}

fn files_task(identity: AsyncIdentity, directory_only: bool) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if directory_only {
                load_directory_files(&worktree)
            } else {
                load_repository_files(&worktree)
            }
        })
        .await
        .map_err(|error| format!("file-list worker failed: {error}"))
        .and_then(|result| result);
        RepositoryAction::FilesLoaded { identity, result }
    })
}

fn file_task(identity: AsyncIdentity) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || load_file(&worktree, &path))
            .await
            .map_err(|error| FileIoError::Io(format!("file worker failed: {error}")))
            .and_then(|result| result);
        RepositoryAction::FileLoaded { identity, result }
    })
}

fn diff_task(
    identity: AsyncIdentity,
    basis: DiffBasis,
    request: u64,
    old_path: Option<PathBuf>,
    cancellation: CancellationToken,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    let requested = basis.clone();
    cosmic::task::future(async move {
        let content = tokio::task::spawn_blocking(move || {
            load_repository_diff(
                &worktree,
                &path,
                &requested,
                old_path.as_deref(),
                &cancellation,
            )
        })
        .await
        .unwrap_or_else(|error| RenderedDiff::Error(format!("diff worker failed: {error}")));
        RepositoryAction::DiffLoaded {
            identity,
            basis,
            request,
            content,
        }
    })
}

fn basis_task(identity: AsyncIdentity, choice: CommittedChoice) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || resolve_basis(&worktree, choice))
            .await
            .unwrap_or_else(|error| Err(format!("basis worker failed: {error}")));
        RepositoryAction::DiffBasisResolved {
            identity,
            choice,
            result,
        }
    })
}

/// Names the comparison a committed choice stands for.
///
/// Both halves come from Git rather than from a guess: the branch base is the
/// remote's own symbolic `HEAD`, and the commit's parent is the commit's own
/// first parent — `None` for a root commit, which the service compares against
/// the empty tree.
fn resolve_basis(worktree: &Path, choice: CommittedChoice) -> Result<ResolvedBasis, String> {
    let service = RepositoryService::system("git");
    let cancellation = CancellationToken::new();
    match choice {
        CommittedChoice::Branch => {
            // A merge base is taken against `HEAD`, so an unborn branch has
            // nothing to compare here either. Without this the base ref
            // resolves, `MergeBaseToHead` is installed, and the comparison
            // dies in Git with "Not a valid object name HEAD".
            if service
                .resolve_head(worktree, &cancellation)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(ResolvedBasis::NoCommit);
            }
            let snapshot = service
                .snapshot(worktree, &cancellation)
                .map_err(|error| error.to_string())?;
            Ok(match snapshot.default_branch_base() {
                Some(base_ref) => ResolvedBasis::Ready(DiffBasis::MergeBaseToHead {
                    base_ref: base_ref.clone(),
                }),
                None => ResolvedBasis::NeedsBase {
                    candidates: base_candidates(&snapshot),
                },
            })
        }
        CommittedChoice::LastCommit => {
            let page = service
                .history(
                    &HistoryRequest {
                        worktree: worktree.to_path_buf(),
                        revision: HistoryRevision::Head,
                        page: 0,
                        path: None,
                    },
                    &cancellation,
                )
                .map_err(|error| error.to_string())?;
            // History is newest first, so `HEAD`'s own commit is the first
            // entry and its first parent is what it was written against.
            Ok(match page.first() {
                Some(head) => ResolvedBasis::Ready(DiffBasis::CommitToParent {
                    oid: head.oid.clone(),
                    parent: head.parents.first().cloned(),
                }),
                None => ResolvedBasis::NoCommit,
            })
        }
    }
}

/// The refs a branch comparison could take its merge base from, offered when
/// no default resolves.
///
/// Remote-tracking branches first, because a review base is normally one, and
/// the checked-out branch is left out: its merge base with `HEAD` is `HEAD`.
fn base_candidates(snapshot: &RepositorySnapshot) -> Vec<ValidatedRefName> {
    snapshot
        .remote_refs
        .iter()
        .map(|record| record.name.clone())
        .chain(
            snapshot
                .local_refs
                .iter()
                .filter(|record| !record.head)
                .map(|record| record.name.clone()),
        )
        .collect()
}

/// Lists a plain folder without following symlinked directories. Traversal
/// uses the same descriptor-relative resolver as file loading and saving.
pub fn load_directory_files(directory: &Path) -> Result<Vec<RepositoryFile>, String> {
    use std::os::fd::AsRawFd;
    let root = crate::fs_safety::Root::open(directory).map_err(|error| error.to_string())?;
    let mut pending = vec![(PathBuf::new(), 0usize)];
    let mut files = Vec::new();
    let mut entries = 0usize;
    while let Some((relative, depth)) = pending.pop() {
        if depth > 128 {
            return Err("Folder nesting exceeds 128 levels; open a narrower folder.".into());
        }
        let handle = if relative.as_os_str().is_empty() {
            std::fs::File::open(directory).map_err(|error| error.to_string())?
        } else {
            crate::fs_safety::open_beneath(&root, &relative).map_err(|error| error.to_string())?
        };
        let descriptor_path = format!("/proc/self/fd/{}", handle.as_raw_fd());
        for entry in std::fs::read_dir(descriptor_path).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            entries += 1;
            if entries > 100_000 {
                return Err("Folder exceeds 100,000 entries; open a narrower folder.".into());
            }
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let path = relative.join(name);
            let metadata = crate::fs_safety::metadata_beneath(&root, &path)
                .map_err(|error| error.to_string())?;
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else {
                files.push(RepositoryFile {
                    path,
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: false,
                    status: None,
                });
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub fn load_repository_files(worktree: &Path) -> Result<Vec<RepositoryFile>, String> {
    let tracked = git_output(worktree, ["ls-files", "--cached", "-z"])?;
    let untracked = git_output(
        worktree,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let status = git_output(
        worktree,
        ["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )?;
    assemble_repository_files(&tracked, &untracked, &status)
}

fn assemble_repository_files(
    tracked: &[u8],
    untracked: &[u8],
    status: &[u8],
) -> Result<Vec<RepositoryFile>, String> {
    let statuses = parse_status_paths(status)?;
    let mut files = BTreeMap::<PathBuf, bool>::new();
    for path in parse_paths(tracked, "git ls-files")? {
        files.insert(path, true);
    }
    for path in parse_paths(untracked, "git ls-files --others")? {
        files.entry(path).or_insert(false);
    }
    for status in statuses.values() {
        for old_path in [
            status.staged_old_path.as_ref(),
            status.unstaged_old_path.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            files.remove(old_path);
        }
    }
    for path in statuses.keys() {
        files.entry(path.clone()).or_insert(false);
    }
    Ok(files
        .into_iter()
        .map(|(path, _tracked)| {
            let status = statuses.get(&path);
            let (staged, unstaged) = status.map_or((false, false), |status| {
                (status.staged.is_some(), status.unstaged.is_some())
            });
            RepositoryFile {
                staged_old_path: status.and_then(|status| status.staged_old_path.clone()),
                unstaged_old_path: status.and_then(|status| status.unstaged_old_path.clone()),
                path,
                staged,
                unstaged,
                status: status.map(ParsedStatus::display_code),
            }
        })
        .collect())
}

fn git_output<const N: usize>(worktree: &Path, arguments: [&str; N]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(worktree)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "Git failed while listing repository files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn parse_paths(output: &[u8], format: &str) -> Result<Vec<PathBuf>, String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err(format!("truncated NUL-delimited {format} output"));
    }
    output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            let path = bytes_to_path(field);
            validate_repo_path(&path).map_err(|error| error.to_string())?;
            Ok(path)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedStatus {
    staged: Option<u8>,
    unstaged: Option<u8>,
    staged_old_path: Option<PathBuf>,
    unstaged_old_path: Option<PathBuf>,
}

impl ParsedStatus {
    fn display_code(&self) -> String {
        match (self.staged, self.unstaged) {
            (None, Some(b'?')) => "??".into(),
            (staged, unstaged) => {
                String::from_utf8(vec![staged.unwrap_or(b' '), unstaged.unwrap_or(b' ')])
                    .expect("porcelain status codes are ASCII")
            }
        }
    }

    fn merge(&mut self, other: Self) -> Result<(), String> {
        merge_status_fact(&mut self.staged, other.staged, "staged")?;
        merge_status_fact(&mut self.unstaged, other.unstaged, "unstaged")?;
        merge_status_path(
            &mut self.staged_old_path,
            other.staged_old_path,
            "staged rename",
        )?;
        merge_status_path(
            &mut self.unstaged_old_path,
            other.unstaged_old_path,
            "unstaged rename",
        )
    }
}

fn merge_status_fact(
    target: &mut Option<u8>,
    incoming: Option<u8>,
    basis: &str,
) -> Result<(), String> {
    if let Some(incoming) = incoming {
        if target.is_some_and(|existing| existing != incoming) {
            return Err(format!("conflicting {basis} status records"));
        }
        *target = Some(incoming);
    }
    Ok(())
}

fn merge_status_path(
    target: &mut Option<PathBuf>,
    incoming: Option<PathBuf>,
    basis: &str,
) -> Result<(), String> {
    if let Some(incoming) = incoming {
        if target
            .as_ref()
            .is_some_and(|existing| existing != &incoming)
        {
            return Err(format!("conflicting {basis} source paths"));
        }
        *target = Some(incoming);
    }
    Ok(())
}

fn parse_status_paths(output: &[u8]) -> Result<HashMap<PathBuf, ParsedStatus>, String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err("truncated NUL-delimited git status output".into());
    }
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut statuses: HashMap<PathBuf, ParsedStatus> = HashMap::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.len() < 4 || field[2] != b' ' {
            return Err(format!("malformed git status record {index}"));
        }
        let code = &field[..2];
        let path = bytes_to_path(&field[3..]);
        validate_repo_path(&path).map_err(|error| error.to_string())?;
        let mut old_path = None;
        if matches!(field[0], b'R' | b'C') || matches!(field[1], b'R' | b'C') {
            index += 1;
            if index >= fields.len() {
                return Err("rename source path is missing from git status".into());
            }
            let source = bytes_to_path(fields[index]);
            validate_repo_path(&source).map_err(|error| error.to_string())?;
            old_path = Some(source);
        }
        let untracked = code == b"??";
        let parsed = ParsedStatus {
            staged: (!untracked && code[0] != b' ').then_some(code[0]),
            unstaged: if untracked {
                Some(b'?')
            } else {
                (code[1] != b' ').then_some(code[1])
            },
            staged_old_path: matches!(code[0], b'R' | b'C')
                .then(|| old_path.clone())
                .flatten(),
            unstaged_old_path: if matches!(code[1], b'R' | b'C') {
                old_path
            } else {
                None
            },
        };
        if let Some(existing) = statuses.get_mut(&path) {
            existing.merge(parsed)?;
        } else {
            statuses.insert(path, parsed);
        }
        index += 1;
    }
    Ok(statuses)
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

pub fn view(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let body = if runtime.selected.is_some() {
        selected_file_view(card, runtime)
    } else {
        file_list_view(card, runtime)
    };
    // Ordinary columns reconcile children through Tree::diff, including tag
    // changes. The pinned keyed column directly diffs equal-length children
    // even when keys change, which reuses incompatible placeholder state.
    widget::column::with_children([body])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The focus target of the card's editable buffer.
///
/// Derived from the card rather than minted, so it is the same id on every
/// rebuild of the view — a focus operation is delivered on the frame *after*
/// the one that asked for it, and an id that changed in between would address
/// nothing — and so two repository cards never contend for one focus target.
pub fn editor_focus_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.editor"))
}

pub fn focus_restore_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.view"))
}

fn file_list_view(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let spacing = cosmic::theme::spacing();
    let title = runtime
        .descriptor
        .worktree
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Repository".into());
    let header = widget::container(
        widget::row::with_children(vec![
            crate::file_icon::folder(true).size(20).into(),
            widget::column::with_children(vec![
                widget::text::title3(title).into(),
                widget::text::caption(runtime.descriptor.worktree.display().to_string()).into(),
            ])
            .width(Length::Fill)
            .into(),
            if runtime.standalone {
                iced::widget::Space::new().into()
            } else {
                widget::button::standard("New terminal here")
                    .leading_icon(widget::icon::from_name("utilities-terminal-symbolic").size(16))
                    .on_press(RepositoryAction::NewTerminal { card })
                    .into()
            },
            widget::button::standard("Refresh")
                .on_press(RepositoryAction::RefreshFiles { card })
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xs),
    )
    .padding([spacing.space_xs, spacing.space_s]);
    let files: Element<'_, RepositoryAction> = match &runtime.files {
        FileListState::Loading => loading("Loading tracked and changed files…"),
        FileListState::Failed(error) => information_state("Could not list files", error),
        FileListState::Ready(files) if files.is_empty() => information_state(
            "No reviewable files",
            "This worktree has no tracked or untracked regular-file paths.",
        ),
        FileListState::Ready(files) => {
            let visible = visible_rows(files.len(), runtime.file_viewport, FILE_ROW_HEIGHT);
            let mut rows = Vec::with_capacity(visible.end - visible.start + 2);
            if visible.top_space > 0.0 {
                rows.push(iced::widget::Space::new().height(visible.top_space).into());
            }
            rows.extend(files[visible.start..visible.end].iter().map(|file| {
                let label = file.path.to_string_lossy();
                let mut children: Vec<Element<'_, RepositoryAction>> = vec![
                    crate::file_icon::icon(&file.path).into(),
                    widget::text::body(label).width(Length::Fill).into(),
                ];
                if let Some(status) = &file.status {
                    children.push(widget::text::caption(status.clone()).into());
                }
                widget::button::custom(
                    widget::row::with_children(children)
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_xxs),
                )
                .class(cosmic::theme::Button::Text)
                .width(Length::Fill)
                .height(Length::Fixed(FILE_ROW_HEIGHT))
                .on_press(RepositoryAction::OpenFile {
                    card,
                    path: file.path.clone(),
                })
                .into()
            }));
            if visible.bottom_space > 0.0 {
                rows.push(
                    iced::widget::Space::new()
                        .height(visible.bottom_space)
                        .into(),
                );
            }
            widget::scrollable(widget::column::with_children(rows))
                .direction(iced::widget::scrollable::Direction::Vertical(
                    iced::widget::scrollable::Scrollbar::default()
                        .width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .scroller_width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .spacing(
                            crate::design::SCROLLBAR_GUTTER - crate::design::SCROLLBAR_THUMB_WIDTH,
                        ),
                ))
                .on_scroll(move |viewport| RepositoryAction::FileViewport {
                    card,
                    offset: viewport.absolute_offset().y,
                    height: viewport.bounds().height,
                })
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        }
    };
    widget::column::with_children(vec![
        header.into(),
        widget::divider::horizontal::default().into(),
        files,
    ])
    .height(Length::Fill)
    .into()
}

fn comparison_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="1"/><path d="M12 4v16M6 10h3M15 10h3M16.5 8.5v3M6 15h3M15 15h3"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}

fn branch_comparison_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round"><circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="5" r="2"/><path d="M6 7v10M18 7c0 7-12 3-12 10"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExternalEditorCommand<'a>(CardId, &'a str);

impl widget::menu::Action for ExternalEditorCommand<'_> {
    type Message = RepositoryAction;

    fn message(&self) -> RepositoryAction {
        RepositoryAction::OpenExternal {
            card: self.0,
            editor_id: self.1.to_owned(),
        }
    }
}

/// Dedicated content toolbar, mounted separately from tabs and the scroll viewport.
pub fn selected_file_toolbar(
    card: CardId,
    runtime: &RepositoryRuntime,
) -> Option<Element<'_, RepositoryAction>> {
    let selected = runtime.selected.as_ref()?;
    let editing = runtime.source_surface == SourceSurface::Editor
        && runtime.markdown_mode == MarkdownMode::Edit;
    let editable = matches!(runtime.document, DocumentState::Editable(_));
    let title = selected
        .path
        .file_name()
        .unwrap_or(selected.path.as_os_str())
        .to_string_lossy();
    let title = crate::interaction::tooltip(
        widget::text::body(title)
            .wrapping(iced::widget::text::Wrapping::None)
            .width(128),
        selected.path.to_string_lossy(),
    );
    // Unsupported text has no preview renderer. Noneditable load/error states
    // still expose file information, so inspecting a binary diff is reversible.
    let file_information = !editable && !matches!(runtime.document, DocumentState::Image { .. });
    let preview = icon_control(
        crate::ui_controls::symbolic_icon("view-reveal-symbolic"),
        if file_information {
            "View file information"
        } else {
            "Preview file"
        },
        runtime.source_surface == SourceSurface::Editor
            && (!editable || !editing)
            && (file_information || runtime.can_apply_markdown_mode(true)),
        if file_information {
            !runtime.is_saving()
        } else {
            runtime.can_apply_markdown_mode(true)
        },
        Some(if file_information {
            RepositoryAction::ApplySourceSurface { card, diff: false }
        } else {
            RepositoryAction::ApplyMarkdownMode {
                card,
                preview: true,
            }
        }),
    );
    let preview = if editable {
        preview
    } else {
        preview.id(focus_restore_id(card))
    };
    let edit = icon_control(
        crate::ui_controls::symbolic_icon("document-edit-symbolic"),
        "Edit file",
        editable && editing && runtime.can_apply_markdown_mode(false),
        runtime.can_apply_markdown_mode(false),
        Some(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }),
    );
    let edit = if editable {
        edit.id(focus_restore_id(card))
    } else {
        edit
    };
    let mut controls: Vec<Element<'_, RepositoryAction>> = vec![
        title.into(),
        segmented_controls(vec![
            preview.into(),
            edit.into(),
            icon_control(
                comparison_icon(),
                "View diff",
                runtime.source_surface == SourceSurface::Diff && runtime.can_show_diff(),
                runtime.can_show_diff(),
                Some(RepositoryAction::ApplySourceSurface { card, diff: true }),
            )
            .into(),
        ]),
    ];
    if !runtime.directory_only {
        controls.push(diff_basis_controls(card, runtime));
    }
    if runtime.source_surface == SourceSurface::Diff {
        controls.push(diff_review_controls(card, runtime));
    }
    if runtime.is_dirty() {
        controls.push(
            icon_control(
                crate::ui_controls::symbolic_icon("edit-undo-symbolic"),
                "Discard unsaved changes",
                false,
                runtime.can_discard_edits(),
                Some(RepositoryAction::DiscardEdits { card }),
            )
            .into(),
        );
    }
    if editable {
        controls.push(
            icon_control(
                crate::ui_controls::symbolic_icon("document-save-symbolic"),
                if runtime.is_saving() {
                    "Saving file…"
                } else if runtime.is_dirty() {
                    "Save unsaved changes"
                } else {
                    "Save file"
                },
                runtime.is_dirty(),
                runtime.can_save(),
                Some(RepositoryAction::Save {
                    card,
                    overwrite: false,
                }),
            )
            .into(),
        );
    }
    for (stage, available, icon, label) in [
        (
            true,
            selected.unstaged,
            "list-add-symbolic",
            "Stage file changes",
        ),
        (
            false,
            selected.staged,
            "list-remove-symbolic",
            "Unstage file changes",
        ),
    ] {
        if available && !runtime.mutation_refresh_pending {
            controls.push(
                icon_control(
                    crate::ui_controls::symbolic_icon(icon),
                    label,
                    false,
                    !runtime.blocks_navigation(),
                    Some(RepositoryAction::StageSelected { card, stage }),
                )
                .into(),
            );
        }
    }
    if !runtime.standalone
        && let Some(editor) = runtime.external_editors.first()
    {
        let icon = |editor: &crate::sidebar::SidebarEditor| {
            if Path::new(&editor.icon).is_absolute() {
                widget::icon::from_path(PathBuf::from(&editor.icon))
            } else {
                widget::icon::from_name(editor.icon.clone()).handle()
            }
        };
        let button = icon_control(
            icon(editor),
            format!("Open in {} · right-click for other editors", editor.name),
            false,
            runtime.can_open_external(),
            Some(RepositoryAction::OpenExternal {
                card,
                editor_id: editor.id.clone(),
            }),
        );
        if runtime.is_saving() {
            controls.push(button.into());
        } else {
            let entries = runtime
                .external_editors
                .iter()
                .map(|editor| {
                    widget::menu::Item::Button(
                        editor.name.clone(),
                        Some(icon(editor)),
                        ExternalEditorCommand(card, editor.id.as_str()),
                    )
                })
                .collect();
            controls.push(crate::interaction::context_menu(
                button,
                widget::menu::items(&HashMap::new(), entries),
            ));
        }
    }
    // Native horizontal scrolling retains every action and keyboard target
    // without a Responsive wrapper hiding controls from accessibility.
    Some(
        widget::scrollable(
            widget::row::with_children(controls)
                .spacing(2)
                .align_y(Alignment::Center),
        )
        .direction(iced::widget::scrollable::Direction::Horizontal(
            iced::widget::scrollable::Scrollbar::default()
                .width(crate::design::SCROLLBAR_THUMB_WIDTH)
                .scroller_width(crate::design::SCROLLBAR_THUMB_WIDTH),
        ))
        .width(Length::Fill)
        .height(40)
        .into(),
    )
}

fn selected_file_view(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    match runtime.source_surface {
        SourceSurface::Editor => editor_surface(card, runtime),
        SourceSurface::Diff => diff_surface(card, runtime),
    }
}

fn editor_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let content = match &runtime.document {
        DocumentState::None => information_state(
            "No file selected",
            "Choose a file from the repository list.",
        ),
        DocumentState::Loading(path) => loading(format!("Loading {}…", path.display())),
        DocumentState::Failed { path, message } => {
            information_state(format!("Could not open {}", path.display()), message)
        }
        DocumentState::ReadOnly { path, reason } => read_only_state(path, reason),
        DocumentState::Editable(document) => editable_document_view(card, runtime, document),
        DocumentState::Image {
            handle,
            width,
            height,
        } => crate::interaction::tooltip(
            widget::container(
                iced::widget::image(handle.clone())
                    .content_fit(iced::ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill),
            format!("{width} × {height} pixels"),
        )
        .into(),
    };
    widget::column::with_children([content])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn editable_document_view<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    document: &'a EditorDocument,
) -> Element<'a, RepositoryAction> {
    let spacing = cosmic::theme::spacing();
    let mut error_rows: Vec<Element<'a, RepositoryAction>> = Vec::new();
    if let Some(error) = document
        .error
        .as_ref()
        .or(document.durability_warning.as_ref())
    {
        let mut actions: Vec<Element<'a, RepositoryAction>> =
            vec![widget::text::body(error).width(Length::Fill).into()];
        if document.conflict || document.durability_warning.is_some() {
            actions.push(
                icon_control(
                    crate::ui_controls::symbolic_icon("view-refresh-symbolic"),
                    "Reload file from disk",
                    false,
                    !runtime.is_saving(),
                    Some(RepositoryAction::Reload { card }),
                )
                .into(),
            );
        }
        if document.conflict {
            actions.push(
                icon_control(
                    crate::ui_controls::symbolic_icon("document-save-as-symbolic"),
                    "Overwrite conflicting file on disk",
                    false,
                    runtime.can_save(),
                    Some(RepositoryAction::Save {
                        card,
                        overwrite: true,
                    }),
                )
                .into(),
            );
        } else if document.durability_warning.is_some() {
            actions.push(
                icon_control(
                    crate::ui_controls::symbolic_icon("document-save-symbolic"),
                    "Retry save",
                    false,
                    runtime.can_save(),
                    Some(RepositoryAction::Save {
                        card,
                        overwrite: false,
                    }),
                )
                .into(),
            );
        }
        error_rows.push(
            widget::container(widget::row::with_children(actions).spacing(spacing.space_xxs))
                .class(cosmic::theme::Container::Secondary)
                .padding(spacing.space_xs)
                .into(),
        );
    }
    let content: Element<'a, RepositoryAction> = if document.markdown
        && runtime.markdown_mode == MarkdownMode::Preview
    {
        widget::scrollable(
            widget::container(
                markdown::view(&document.markdown_items, cosmic::iced::Theme::Dark)
                    .map(move |uri| RepositoryAction::MarkdownLink { card, uri }),
            )
            .padding(spacing.space_m)
            .max_width(900),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            iced::widget::scrollable::Scrollbar::default()
                .width(crate::design::SCROLLBAR_THUMB_WIDTH)
                .scroller_width(crate::design::SCROLLBAR_THUMB_WIDTH)
                .spacing(crate::design::SCROLLBAR_GUTTER - crate::design::SCROLLBAR_THUMB_WIDTH),
        ))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    } else {
        text_editor::TextEditor::new(&document.content)
            // Before `highlight_with`: `id` is only offered by the
            // not-yet-highlighted builder.
            .id(editor_focus_id(card))
            .highlight_with::<highlighter::Highlighter>(
                highlighter_settings(&document.path, cosmic::theme::is_dark()),
                |highlight, _theme: &cosmic::Theme| highlight.to_format(),
            )
            .font(Font::MONOSPACE)
            .padding(spacing.space_xs)
            .height(Length::Fill)
            .style(|theme, status| {
                use iced::widget::text_editor::Catalog;
                let mut style = theme.style(&<cosmic::Theme as Catalog>::default(), status);
                style.border = iced::Border::default();
                style
            })
            .on_action(move |action| RepositoryAction::EditorAction { card, action })
            .into()
    };
    // Keep the named editor in the same slot when save warnings appear.
    let error_slot = widget::column::with_children(error_rows).width(Length::Fill);
    widget::column::with_children([error_slot.into(), content])
        .height(Length::Fill)
        .into()
}

fn diff_basis_controls(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let selected = match &runtime.basis_resolution {
        Some(BasisResolution::Resolving(choice)) => Some(BasisChoice::Committed(*choice)),
        Some(BasisResolution::NeedsBase(_)) => {
            Some(BasisChoice::Committed(CommittedChoice::Branch))
        }
        Some(BasisResolution::Unavailable(_)) => None,
        None => Some(basis_choice(&runtime.diff_basis)),
    };
    let unstaged = runtime.can_select_diff_basis(&DiffBasis::IndexToWorktree);
    let staged = runtime.can_select_diff_basis(&DiffBasis::HeadToIndex);
    segmented_controls(vec![
        icon_control(
            crate::ui_controls::symbolic_icon("document-edit-symbolic"),
            "Unstaged changes",
            unstaged && selected == Some(BasisChoice::Unstaged),
            unstaged,
            Some(RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::IndexToWorktree,
            }),
        )
        .into(),
        icon_control(
            crate::ui_controls::symbolic_icon("object-select-symbolic"),
            "Staged changes",
            staged && selected == Some(BasisChoice::Staged),
            staged,
            Some(RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::HeadToIndex,
            }),
        )
        .into(),
        icon_control(
            branch_comparison_icon(),
            "Compare branch with base",
            runtime.can_select_committed_basis(CommittedChoice::Branch)
                && selected == Some(BasisChoice::Committed(CommittedChoice::Branch)),
            runtime.can_select_committed_basis(CommittedChoice::Branch),
            Some(RepositoryAction::SelectCommittedBasis {
                card,
                choice: CommittedChoice::Branch,
            }),
        )
        .into(),
        icon_control(
            crate::ui_controls::symbolic_icon("document-open-recent-symbolic"),
            "Last commit changes",
            runtime.can_select_committed_basis(CommittedChoice::LastCommit)
                && selected == Some(BasisChoice::Committed(CommittedChoice::LastCommit)),
            runtime.can_select_committed_basis(CommittedChoice::LastCommit),
            Some(RepositoryAction::SelectCommittedBasis {
                card,
                choice: CommittedChoice::LastCommit,
            }),
        )
        .into(),
    ])
}

fn diff_review_controls(
    card: CardId,
    runtime: &RepositoryRuntime,
) -> Element<'_, RepositoryAction> {
    let model = match runtime.diff_state(&runtime.diff_basis) {
        Some(DiffState::Ready(RenderedDiff::Text { model, .. })) => Some(model),
        _ => None,
    };
    let change_count = model.map_or(0, |model| model.change_starts(runtime.diff_mode).len());
    let mut controls = vec![
        segmented_controls(vec![
            icon_control(
                crate::ui_controls::symbolic_icon("view-list-symbolic"),
                "Inline diff",
                runtime.can_change_diff_layout() && runtime.diff_mode == DiffMode::Inline,
                runtime.can_change_diff_layout(),
                Some(RepositoryAction::ApplyDiffMode { card, split: false }),
            )
            .into(),
            icon_control(
                comparison_icon(),
                "Side-by-side diff",
                runtime.can_change_diff_layout() && runtime.diff_mode == DiffMode::Split,
                runtime.can_change_diff_layout(),
                Some(RepositoryAction::ApplyDiffMode { card, split: true }),
            )
            .into(),
        ]),
        icon_control(
            crate::ui_controls::symbolic_icon("go-up-symbolic"),
            "Previous change",
            false,
            runtime.can_navigate_diff(),
            Some(RepositoryAction::PreviousChange { card }),
        )
        .into(),
        crate::interaction::tooltip(
            widget::text::caption(if change_count == 0 {
                "0/0".into()
            } else {
                format!("{}/{change_count}", runtime.diff_change + 1)
            }),
            basis_label(&runtime.diff_basis),
        ),
        icon_control(
            crate::ui_controls::symbolic_icon("go-down-symbolic"),
            "Next change",
            false,
            runtime.can_navigate_diff(),
            Some(RepositoryAction::NextChange { card }),
        )
        .into(),
    ];
    if runtime.can_retry_diff() {
        controls.push(
            icon_control(
                crate::ui_controls::symbolic_icon("view-refresh-symbolic"),
                "Retry diff",
                false,
                true,
                Some(RepositoryAction::RetryDiff { card }),
            )
            .into(),
        );
    }
    if let Some(model) = model
        && let counts = crate::repository::line_change_counts(
            Some(u64::from(model.additions)),
            Some(u64::from(model.deletions)),
        )
        && !counts.is_empty()
    {
        controls.push(crate::interaction::tooltip(
            widget::text::caption(counts),
            match runtime.diff_state(&runtime.diff_basis) {
                Some(DiffState::Ready(RenderedDiff::Text {
                    old_label,
                    new_label,
                    ..
                })) => format!(
                    "{}: {old_label} → {new_label}",
                    basis_label(&runtime.diff_basis)
                ),
                _ => basis_label(&runtime.diff_basis),
            },
        ));
    }
    widget::row::with_children(controls)
        .spacing(2)
        .align_y(Alignment::Center)
        .into()
}

fn diff_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    if runtime.mutation_refresh_pending
        && !matches!(runtime.diff_basis, DiffBasis::CommitToParent { .. })
    {
        return match &runtime.files {
            FileListState::Failed(error) => {
                information_state("Could not refresh file status", error.clone())
            }
            _ => loading("Refreshing file status…"),
        };
    }
    match &runtime.basis_resolution {
        Some(BasisResolution::Resolving(choice)) => {
            loading(format!("Resolving the {} comparison…", choice.label()))
        }
        Some(BasisResolution::NeedsBase(candidates)) => base_choice_view(card, candidates),
        Some(BasisResolution::Unavailable(message)) => {
            information_state("Comparison unavailable", message.clone())
        }
        None => match runtime.diff_state(&runtime.diff_basis) {
            None => information_state(
                "Nothing to compare",
                "This file has nothing to show for the selected comparison.",
            ),
            Some(DiffState::Loading { .. }) => loading(format!(
                "Computing the diff for {} ({})…",
                runtime
                    .selected
                    .as_ref()
                    .map_or_else(String::new, |file| file.path.display().to_string()),
                basis_label(&runtime.diff_basis),
            )),
            Some(DiffState::Ready(RenderedDiff::Text {
                model,
                old_label,
                new_label,
            })) => diff_model_view(card, runtime, model, old_label, new_label),
            Some(DiffState::Ready(RenderedDiff::NoText(content))) => no_text_view(content),
            Some(DiffState::Ready(RenderedDiff::Error(error))) => {
                information_state("Could not compute diff", error.clone())
            }
        },
    }
}

/// A comparison with no text rows, reported as the fact it is.
///
/// A conflicted path is drawn as its stages, never as a two-sided diff: it has
/// no single old side, and pretending otherwise would claim the file had been
/// added.
fn no_text_view(content: &DiffContent) -> Element<'static, RepositoryAction> {
    match content {
        DiffContent::Comparison {
            old,
            new,
            old_label,
            new_label,
        } => information_state(
            format!("No text diff between {old_label} and {new_label}"),
            [(old_label, old), (new_label, new)]
                .into_iter()
                .filter(|(_, side)| !side.is_diffable())
                .map(|(label, side)| format!("{label}: {}", side_detail(side)))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        DiffContent::Unmerged {
            base,
            ours,
            theirs,
            worktree,
        } => information_state(
            "This path is conflicted",
            [
                ("Common ancestor", base),
                ("Ours", ours),
                ("Theirs", theirs),
                ("Working tree", worktree),
            ]
            .into_iter()
            .map(|(label, side)| format!("{label}: {}", side_detail(side)))
            .collect::<Vec<_>>()
            .join("\n"),
        ),
    }
}

/// The base-ref picker a branch comparison falls back to.
///
/// Reached exactly when this repository has no `refs/remotes/<remote>/HEAD`,
/// which is what a remote added by hand rather than by cloning never gets. A
/// name is never guessed: the refs that could serve are offered instead.
fn base_choice_view(
    card: CardId,
    candidates: &[ValidatedRefName],
) -> Element<'static, RepositoryAction> {
    let spacing = cosmic::theme::spacing();
    let mut rows: Vec<Element<'static, RepositoryAction>> = vec![
        widget::text::title3("Choose a branch comparison base").into(),
        widget::text::body(
            "This repository has no remote HEAD to take a default base from, so there is no branch to compare against yet.",
        )
        .wrapping(iced::widget::text::Wrapping::Word)
        .into(),
    ];
    if candidates.is_empty() {
        rows.push(
            widget::text::body(
                "It also has no other ref to offer. Fetch a remote, or point refs/remotes/<remote>/HEAD at its default branch.",
            )
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
        );
    } else {
        let labels: Vec<String> = candidates
            .iter()
            .map(|base_ref| base_ref.short_name().to_owned())
            .collect();
        let refs = candidates.to_vec();
        rows.push(
            widget::dropdown(labels, None, move |index| {
                // The dropdown reports an index into the very list it was given.
                RepositoryAction::ApplyDiffBasis {
                    card,
                    basis: DiffBasis::MergeBaseToHead {
                        base_ref: refs[index].clone(),
                    },
                }
            })
            .into(),
        );
    }
    widget::container(
        widget::container(
            widget::column::with_children(rows)
                .align_x(Alignment::Center)
                .spacing(spacing.space_xs),
        )
        .class(cosmic::theme::Container::Card)
        .padding(spacing.space_m)
        .max_width(620),
    )
    .center(Length::Fill)
    .into()
}

fn diff_model_view<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    model: &'a DiffModel,
    old_label: &'a str,
    new_label: &'a str,
) -> Element<'a, RepositoryAction> {
    let rows: Element<'a, RepositoryAction> = if model.is_empty() {
        information_state(
            "No content changes",
            format!("{old_label} and {new_label} hold identical text."),
        )
    } else {
        iced::widget::responsive(move |size| {
            // One native two-axis scrollable owns both coordinates. The canvas
            // counter-translates the fixed gutters/center; only code moves in X.
            let gutter =
                (model.row_count(DiffMode::Inline).max(1).ilog10() + 1).max(4) as f32 * 8.0 + 12.0;
            let code_width =
                (model.max_line_width[usize::from(cosmic::theme::is_dark())] as f32).max(1.0);
            let geometry = DiffScrollGeometry::new(size.width, gutter, runtime.diff_mode);
            let viewport_width = geometry.viewport_width;
            let overflow = geometry.overflow(code_width);
            let horizontal = runtime.diff_horizontal.clamp(0.0, overflow);
            let total = model.row_count(runtime.diff_mode);
            let visible = visible_rows(total, runtime.diff_viewport, DIFF_ROW_HEIGHT);
            let mut children = Vec::with_capacity(visible.end - visible.start + 2);
            if visible.top_space > 0.0 {
                children.push(iced::widget::Space::new().height(visible.top_space).into());
            }
            match runtime.diff_mode {
                DiffMode::Inline => {
                    children.extend(model.inline_rows[visible.start..visible.end].iter().map(
                        |row| inline_diff_row(row, &model.syntax, gutter, code_width, horizontal),
                    ))
                }
                DiffMode::Split => {
                    children.extend(model.split_rows[visible.start..visible.end].iter().map(
                        |row| split_diff_row(row, &model.syntax, gutter, code_width, horizontal),
                    ))
                }
            };
            if visible.bottom_space > 0.0 {
                children.push(
                    iced::widget::Space::new()
                        .height(visible.bottom_space)
                        .into(),
                );
            }
            // Both-axis scrollbars overlay their content in this iced version.
            // Reserve the bottom rail so the final line remains fully readable.
            children.push(
                iced::widget::Space::new()
                    .height(crate::design::SCROLLBAR_GUTTER)
                    .into(),
            );
            let canvas = Element::new(DiffHorizontalContent {
                content: widget::column::with_children(children)
                    .spacing(0)
                    .width(viewport_width)
                    .into(),
                width: Length::Fixed(size.width + overflow),
                content_width: viewport_width,
                offset: -horizontal,
                accessible_text: None,
                viewport: Some(iced::Size::new(viewport_width, size.height)),
            });
            widget::scrollable(canvas)
                .direction(iced::widget::scrollable::Direction::Both {
                    vertical: iced::widget::scrollable::Scrollbar::default()
                        .width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .scroller_width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .margin(crate::design::SCROLLBAR_MARGIN),
                    horizontal: iced::widget::scrollable::Scrollbar::default()
                        .width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .scroller_width(crate::design::SCROLLBAR_THUMB_WIDTH)
                        .margin(crate::design::SCROLLBAR_MARGIN),
                })
                .id(runtime.diff_scroll_id.clone())
                .on_scroll(move |viewport| RepositoryAction::DiffViewport {
                    card,
                    offset: viewport.absolute_offset().y,
                    height: viewport.bounds().height,
                    horizontal: viewport.absolute_offset().x,
                })
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        })
        .into()
    };
    rows
}

const DIFF_ROW_PADDING: f32 = 4.0;
const DIFF_MARKER_WIDTH: f32 = 16.0;
const DIFF_SPLIT_DIVIDER: f32 = 1.0;

#[derive(Debug, Clone, Copy)]
struct DiffScrollGeometry {
    viewport_width: f32,
    code_viewport: f32,
}

impl DiffScrollGeometry {
    fn new(width: f32, gutter: f32, mode: DiffMode) -> Self {
        let viewport_width = (width - crate::design::SCROLLBAR_GUTTER).max(0.0);
        let code_viewport = match mode {
            DiffMode::Inline => {
                viewport_width - 2.0 * gutter - DIFF_MARKER_WIDTH - 2.0 * DIFF_ROW_PADDING
            }
            DiffMode::Split => {
                (viewport_width - DIFF_SPLIT_DIVIDER) / 2.0 - gutter - 2.0 * DIFF_ROW_PADDING
            }
        }
        .max(0.0);
        Self {
            viewport_width,
            code_viewport,
        }
    }

    fn overflow(self, code_width: f32) -> f32 {
        (code_width - self.code_viewport).max(0.0)
    }
}

fn inline_diff_row<'a>(
    row: &'a InlineDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
) -> Element<'a, RepositoryAction> {
    let marker = match row.kind {
        DiffRowKind::Addition => "+",
        DiffRowKind::Deletion => "−",
        DiffRowKind::Context | DiffRowKind::Modification => " ",
    };
    diff_container(
        widget::row::with_children(vec![
            diff_gutter(row.old_line, gutter),
            diff_gutter(row.new_line, gutter),
            widget::text::body(marker)
                .size(DIFF_FONT_SIZE)
                .font(Font::MONOSPACE)
                .width(DIFF_MARKER_WIDTH)
                .into(),
            diff_text(
                &row.text,
                &row.changed,
                syntax.line(
                    row.new_line.is_none(),
                    row.new_line.or(row.old_line),
                    cosmic::theme::is_dark(),
                ),
                row.kind,
                code_width,
                horizontal,
                (row.old_line, row.new_line),
            ),
        ])
        .align_y(Alignment::Center),
        row.kind,
    )
}

fn split_diff_row<'a>(
    row: &'a SplitDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
) -> Element<'a, RepositoryAction> {
    let old_kind = if row.kind != DiffRowKind::Context && row.old_text.is_some() {
        DiffRowKind::Deletion
    } else {
        DiffRowKind::Context
    };
    let new_kind = if row.kind != DiffRowKind::Context && row.new_text.is_some() {
        DiffRowKind::Addition
    } else {
        DiffRowKind::Context
    };
    widget::row::with_children(vec![
        diff_half(
            (row.old_line, None),
            row.old_text.as_deref().unwrap_or(""),
            &row.old_changed,
            syntax.line(true, row.old_line, cosmic::theme::is_dark()),
            old_kind,
            gutter,
            code_width,
            horizontal,
        ),
        widget::container(iced::widget::Space::new())
            .width(DIFF_SPLIT_DIVIDER)
            .height(DIFF_ROW_HEIGHT)
            .class(cosmic::theme::Container::custom(|theme| {
                let mut color: iced::Color = theme.current_container().on.into();
                color.a = 0.16;
                iced::widget::container::Style {
                    background: Some(iced::Background::Color(color)),
                    ..Default::default()
                }
            }))
            .into(),
        diff_half(
            (None, row.new_line),
            row.new_text.as_deref().unwrap_or(""),
            &row.new_changed,
            syntax.line(false, row.new_line, cosmic::theme::is_dark()),
            new_kind,
            gutter,
            code_width,
            horizontal,
        ),
    ])
    .spacing(0)
    .height(DIFF_ROW_HEIGHT)
    .into()
}

fn diff_half<'a>(
    lines: (Option<u32>, Option<u32>),
    text: &'a str,
    changed: &'a [Range<usize>],
    syntax: &[SyntaxSpan],
    kind: DiffRowKind,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
) -> Element<'a, RepositoryAction> {
    diff_container(
        widget::row::with_children(vec![
            diff_gutter(lines.0.or(lines.1), gutter),
            diff_text(text, changed, syntax, kind, code_width, horizontal, lines),
        ])
        .align_y(Alignment::Center),
        kind,
    )
}

fn diff_gutter(line: Option<u32>, width: f32) -> Element<'static, RepositoryAction> {
    widget::container(
        widget::text::caption(line.map_or_else(String::new, |line| line.to_string()))
            .font(Font::MONOSPACE)
            .size(12)
            .align_x(Alignment::End)
            .width(Length::Fill)
            .class(cosmic::theme::Text::Custom(|theme| {
                let mut color: iced::Color = theme.current_container().on.into();
                color.a = 0.48;
                iced::widget::text::Style {
                    color: Some(color),
                    ..Default::default()
                }
            })),
    )
    .padding([0, 8])
    .width(width)
    .into()
}

fn diff_color(kind: DiffRowKind, alpha: f32) -> iced::Color {
    // Keep additions and deletions distinct without obscuring their text.
    let mut color = match kind {
        DiffRowKind::Addition => iced::Color::from_rgb8(46, 160, 93),
        DiffRowKind::Deletion => iced::Color::from_rgb8(224, 78, 90),
        DiffRowKind::Context | DiffRowKind::Modification => return iced::Color::TRANSPARENT,
    };
    color.a = alpha;
    color
}

fn color_over(foreground: iced::Color, background: iced::Color) -> iced::Color {
    if foreground.a == 1.0 {
        return foreground;
    }
    if foreground.a == 0.0 {
        return background;
    }
    let foreground = foreground.into_linear();
    let background = background.into_linear();
    let alpha = foreground[3];
    iced::Color::from_linear_rgba(
        foreground[0] * alpha + background[0] * (1.0 - alpha),
        foreground[1] * alpha + background[1] * (1.0 - alpha),
        foreground[2] * alpha + background[2] * (1.0 - alpha),
        1.0,
    )
}

fn readable_diff_color(foreground: iced::Color, background: iced::Color) -> iced::Color {
    let foreground = color_over(foreground, background);
    if foreground.relative_contrast(background) >= 4.5 {
        foreground
    } else if iced::Color::BLACK.relative_contrast(background)
        >= iced::Color::WHITE.relative_contrast(background)
    {
        iced::Color::BLACK
    } else {
        iced::Color::WHITE
    }
}

fn diff_row_background(theme: &cosmic::Theme, kind: DiffRowKind) -> iced::Color {
    color_over(
        diff_color(
            kind,
            if theme.theme_type.is_dark() {
                0.055
            } else {
                0.035
            },
        ),
        theme.cosmic().bg_color().into(),
    )
}

fn diff_text<'a>(
    text: &'a str,
    changed: &[Range<usize>],
    syntax: &[SyntaxSpan],
    kind: DiffRowKind,
    code_width: f32,
    horizontal: f32,
    lines: (Option<u32>, Option<u32>),
) -> Element<'a, RepositoryAction> {
    let theme = cosmic::theme::active();
    let row_background = diff_row_background(&theme, kind);
    let mut spans = crate::diff_view::line_spans(
        text,
        changed,
        syntax,
        diff_color(kind, if cosmic::theme::is_dark() { 0.18 } else { 0.12 }),
    );
    for span in &mut spans {
        let background = match span
            .highlight
            .as_mut()
            .map(|highlight| &mut highlight.background)
        {
            Some(iced::Background::Color(color)) => {
                *color = color_over(*color, row_background);
                *color
            }
            _ => row_background,
        };
        let foreground = span
            .color
            .unwrap_or_else(|| theme.cosmic().on_bg_color().into());
        span.color = Some(readable_diff_color(foreground, background));
    }
    Element::new(DiffHorizontalContent {
        content: iced::widget::rich_text(spans)
            .size(DIFF_FONT_SIZE)
            .font(Font::MONOSPACE)
            .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                DIFF_LINE_HEIGHT,
            )))
            .wrapping(iced::widget::text::Wrapping::None)
            .width(code_width)
            .into(),
        width: Length::Fill,
        content_width: code_width,
        offset: horizontal,
        accessible_text: Some(DiffAccessibleText {
            text,
            lines,
            id: iced::advanced::widget::Id::unique(),
        }),
        viewport: None,
    })
}

fn diff_container<'a>(
    content: impl Into<Element<'a, RepositoryAction>>,
    kind: DiffRowKind,
) -> Element<'a, RepositoryAction> {
    widget::container(content)
        .class(cosmic::theme::Container::custom(move |theme| {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(diff_row_background(theme, kind))),
                ..Default::default()
            }
        }))
        .padding([3.0, DIFF_ROW_PADDING])
        .height(DIFF_ROW_HEIGHT)
        .width(Length::FillPortion(1))
        .clip(true)
        .into()
}

/// Borrow the model's exact text; only AccessKit's owned node copies it, when
/// accessibility is queried, not when the view or rich-text spans are rebuilt.
struct DiffAccessibleText<'a> {
    text: &'a str,
    lines: (Option<u32>, Option<u32>),
    id: iced::advanced::widget::Id,
}

/// A clipped native-text viewport, also used in reverse to keep the split
/// canvas fixed inside its single native horizontal scrollable. No text is
/// truncated or reshaped as the shared coordinate changes.
struct DiffHorizontalContent<'a> {
    content: Element<'a, RepositoryAction>,
    width: Length,
    content_width: f32,
    offset: f32,
    accessible_text: Option<DiffAccessibleText<'a>>,
    viewport: Option<iced::Size>,
}

impl Widget<RepositoryAction, cosmic::Theme, cosmic::Renderer> for DiffHorizontalContent<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> iced::Size<Length> {
        iced::Size::new(self.width, Length::Shrink)
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let child = self.content.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(
                iced::Size::ZERO,
                iced::Size::new(self.content_width, limits.max().height),
            ),
        );
        let size = limits.resolve(self.width, Length::Shrink, child.size());
        layout::Node::with_children(
            size,
            vec![child.move_to(iced::Point::new(-self.offset, 0.0))],
        )
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.children().next().expect("diff content"),
            renderer,
            operation,
        );
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, RepositoryAction>,
        viewport: &iced::Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.children().next().expect("diff content"),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        if let Some(clip) = layout.bounds().intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout.children().next().expect("diff content"),
                    cursor,
                    &clip,
                );
            });
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: iced::mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::{
            A11yTree,
            accesskit::{Node, Rect, Role},
        };

        let child_layout = layout.children().next().expect("diff content");
        let mut accessible = if let Some(text) = &self.accessible_text {
            // The pinned native Rich widget draws spans but emits no nodes.
            // Keep the source label separate from its old/new line context.
            let mut node = Node::new(Role::Paragraph);
            node.set_label(text.text);
            match text.lines {
                (Some(old), Some(new)) => {
                    node.set_description(format!("Old line {old}, new line {new}"));
                }
                (Some(old), None) => node.set_description(format!("Old line {old}")),
                (None, Some(new)) => node.set_description(format!("New line {new}")),
                (None, None) => return A11yTree::default(),
            }
            let bounds = child_layout.bounds();
            node.set_bounds(Rect::new(
                f64::from(bounds.x),
                f64::from(bounds.y),
                f64::from(bounds.x + bounds.width),
                f64::from(bounds.y + bounds.height),
            ));
            A11yTree::leaf(node, text.id.clone())
        } else {
            self.content
                .as_widget()
                .a11y_nodes(child_layout, &tree.children[0], cursor)
        };

        // Child layout already includes this wrapper's real X translation.
        // Native text ignores virtual offsets: apply the scrollable's offset
        // once, after traversal, so gutters and nested code nodes agree.
        let offset = layout.virtual_offset();
        let clip = self.viewport.map_or_else(
            || layout.bounds(),
            |size| iced::Rectangle::new(layout.position() + offset, size),
        );
        for roots in [true, false] {
            let nodes = if roots {
                accessible.root_mut()
            } else {
                accessible.children_mut()
            };
            for accessible_node in nodes {
                let node = accessible_node.node_mut();
                if let Some(bounds) = node.bounds() {
                    let x0 = bounds.x0.max(f64::from(clip.x));
                    let y0 = bounds.y0.max(f64::from(clip.y));
                    let x1 = bounds.x1.min(f64::from(clip.x + clip.width));
                    let y1 = bounds.y1.min(f64::from(clip.y + clip.height));
                    if x1 <= x0 || y1 <= y0 {
                        node.set_hidden();
                    } else {
                        node.set_bounds(Rect::new(
                            x0 - f64::from(offset.x),
                            y0 - f64::from(offset.y),
                            x1 - f64::from(offset.x),
                            y1 - f64::from(offset.y),
                        ));
                    }
                }
            }
        }
        accessible
    }
}

fn read_only_state(path: &Path, reason: &ReadOnlyReason) -> Element<'static, RepositoryAction> {
    let detail = match reason {
        ReadOnlyReason::Symlink { target } => format!(
            "{} is a symbolic link to {}.",
            path.display(),
            target.display()
        ),
        ReadOnlyReason::Binary => format!("{} contains binary data.", path.display()),
        ReadOnlyReason::InvalidUtf8 => format!("{} is not valid UTF-8.", path.display()),
        ReadOnlyReason::Image(message) => format!("{}: {message}", path.display()),
        ReadOnlyReason::Oversize { bytes, limit } => format!(
            "{} is {bytes} bytes; editable files are limited to {limit} bytes.",
            path.display()
        ),
        ReadOnlyReason::NotRegularFile => format!("{} is not a regular file.", path.display()),
    };
    information_state(
        if matches!(reason, ReadOnlyReason::Image(_)) {
            "Image preview unavailable"
        } else {
            "Read-only file"
        },
        &detail,
    )
}

fn loading(message: impl Into<String>) -> Element<'static, RepositoryAction> {
    widget::container(
        widget::column::with_children(vec![
            widget::progress_bar::circular::Circular::new()
                .size(24.0)
                .into(),
            widget::text::body(message.into()).into(),
        ])
        .align_x(Alignment::Center)
        .spacing(cosmic::theme::spacing().space_xs),
    )
    .center(Length::Fill)
    .into()
}

fn information_state<'a>(
    title: impl Into<String>,
    detail: impl Into<String>,
) -> Element<'a, RepositoryAction> {
    widget::container(
        widget::container(
            widget::column::with_children(vec![
                widget::icon::from_name("dialog-information-symbolic")
                    .size(28)
                    .into(),
                widget::text::title3(title.into()).into(),
                widget::text::body(detail.into())
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
            ])
            .align_x(Alignment::Center)
            .spacing(cosmic::theme::spacing().space_xs),
        )
        .class(cosmic::theme::Container::Card)
        .padding(cosmic::theme::spacing().space_m)
        .max_width(620),
    )
    .center(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::lsp::{LspRange, LspSnippet, LspSnippetStop};
    use crate::model::ProjectId;

    #[test]
    fn faint_diff_syntax_remains_readable_over_both_highlight_layers() {
        for theme in [cosmic::Theme::light(), cosmic::Theme::dark()] {
            for kind in [DiffRowKind::Addition, DiffRowKind::Deletion] {
                let row = diff_row_background(&theme, kind);
                let highlight = color_over(diff_color(kind, 0.18), row);
                let faint = iced::Color::from_rgba(0.6, 0.6, 0.6, 0.7);
                for background in [row, highlight] {
                    let foreground = readable_diff_color(faint, background);
                    assert!(foreground.relative_contrast(background) >= 4.5);
                    assert_eq!(readable_diff_color(foreground, background), foreground);
                }
            }
        }
    }

    #[test]
    fn preview_actions_require_a_preview_renderer_and_preserve_the_current_surface() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.document = DocumentState::Editable(EditorDocument::new(
            PathBuf::from("plain.rs"),
            "fn main() {}\n".into(),
            blake3::hash(b"fn main() {}\n"),
            false,
        ));
        assert!(!runtime.can_apply_markdown_mode(true));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Edit);
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Editor);

        runtime.document = DocumentState::Editable(EditorDocument::new(
            PathBuf::from("README.md"),
            "# Heading\n".into(),
            blake3::hash(b"# Heading\n"),
            true,
        ));
        assert!(runtime.can_apply_markdown_mode(true));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
        assert!(runtime.editor_focus_target(card).is_none());
    }

    #[test]
    fn diff_actions_share_capabilities_without_blocking_committed_comparisons_or_retry() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.source_surface = SourceSurface::Editor;
        let file = runtime.selected.as_mut().unwrap();
        file.staged = false;
        file.unstaged = false;
        assert!(!runtime.can_show_diff());
        assert!(!runtime.can_select_diff_basis(&DiffBasis::HeadToIndex));
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        drop(runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: DiffBasis::HeadToIndex,
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);

        let basis = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        };
        assert!(runtime.can_select_diff_basis(&basis));
        drop(runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: basis.clone(),
        }));
        assert!(runtime.can_show_diff());
        assert!(!runtime.can_change_diff_layout());
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
        assert_eq!(runtime.diff_mode, DiffMode::Inline);
        runtime.set_diff_state(
            basis.clone(),
            DiffState::Ready(RenderedDiff::Error("read failed".into())),
        );
        assert!(runtime.can_retry_diff());
        drop(runtime.update(RepositoryAction::RetryDiff { card }));
        assert!(!runtime.can_retry_diff());
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
        runtime.set_diff_state(basis, DiffState::Ready(staged_text_diff(model_with(1))));
        assert!(runtime.can_change_diff_layout());
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
        assert_eq!(runtime.diff_mode, DiffMode::Split);
    }

    #[test]
    fn editor_navigation_only_schedules_visible_diffs_and_cancels_hidden_workers() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        drop(runtime.open_file(card, identity.path));
        assert!(
            runtime.diffs.is_empty(),
            "opening text must not compute either hidden diff"
        );
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        let (request, cancelled_on_hide) = match runtime.diff_state(&DiffBasis::IndexToWorktree) {
            Some(DiffState::Loading {
                request,
                cancellation,
            }) => (*request, cancellation.clone()),
            _ => panic!("the visible comparison must start loading"),
        };
        assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_none());
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert!(
            matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Loading { request: current, .. }) if *current == request
            ),
            "redrawing or reselecting the same surface must not restart its worker"
        );
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
        assert!(cancelled_on_hide.is_cancelled());
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());

        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        let cancelled_on_close = match runtime.diff_state(&DiffBasis::IndexToWorktree) {
            Some(DiffState::Loading { cancellation, .. }) => cancellation.clone(),
            _ => panic!("returning to an unfinished comparison must restart it"),
        };
        drop(runtime);
        assert!(cancelled_on_close.is_cancelled());
    }
    use crate::repository::Oid;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn diff_scroll_range_matches_shaped_content_and_real_split_half_layout() {
        let renderer = headless_renderer();
        for source in [
            "short".to_owned(),
            "a very long ASCII line ".repeat(24),
            "\tCafe\u{301} 界 👩‍💻\tffi ".repeat(18),
            "__global__ void kernel() { const char* name = \"CUDA\"; }\t".repeat(12),
        ] {
            let RenderedDiff::Text { model, .. } = RenderedDiff::from_content(
                DiffContent::Comparison {
                    old: DiffSide::Absent,
                    new: DiffSide::Text {
                        text: format!("{source}\n"),
                        bytes: source.len() as u64 + 1,
                    },
                    old_label: "old".into(),
                    new_label: "new".into(),
                },
                Path::new("kernel.cu"),
                &CancellationToken::new(),
            ) else {
                panic!("text comparison must produce rows");
            };
            let dark = cosmic::theme::is_dark();
            let mut widest = 0.0_f32;
            for row in &model.inline_rows {
                let mut text: Element<'_, RepositoryAction> =
                    iced::widget::rich_text(crate::diff_view::line_spans(
                        &row.text,
                        &row.changed,
                        model.syntax.line(
                            row.new_line.is_none(),
                            row.new_line.or(row.old_line),
                            dark,
                        ),
                        iced::Color::TRANSPARENT,
                    ))
                    .size(DIFF_FONT_SIZE)
                    .font(Font::MONOSPACE)
                    .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                        DIFF_LINE_HEIGHT,
                    )))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into();
                let mut tree = Tree::new(text.as_widget());
                let node = text.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(
                        iced::Size::ZERO,
                        iced::Size::new(f32::INFINITY, f32::INFINITY),
                    ),
                );
                widest = widest.max(node.size().width);
            }
            let code_width = model.max_line_width[usize::from(dark)] as f32;
            assert!(code_width >= widest && code_width - widest < 1.0);
            // Reuse the model through narrow/wide/narrow viewports and both
            // modes: no stale range may survive a resize or mode transition.
            for width in [320.0, 1200.0, 480.0] {
                for mode in [DiffMode::Split, DiffMode::Inline, DiffMode::Split] {
                    let geometry = DiffScrollGeometry::new(width, 44.0, mode);
                    let offset = geometry.overflow(code_width);
                    if widest <= geometry.code_viewport {
                        assert_eq!(offset, 0.0, "short lines must not scroll");
                    } else {
                        assert!(
                            (widest - offset - geometry.code_viewport).abs() < 1.0,
                            "the last shaped pixel, not an estimated column, ends the range"
                        );
                    }
                    let mut row = match mode {
                        DiffMode::Inline => inline_diff_row(
                            model.inline_rows.last().unwrap(),
                            &model.syntax,
                            44.0,
                            code_width,
                            offset,
                        ),
                        DiffMode::Split => split_diff_row(
                            model.split_rows.last().unwrap(),
                            &model.syntax,
                            44.0,
                            code_width,
                            offset,
                        ),
                    };
                    let mut tree = Tree::new(row.as_widget());
                    let node = row.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(
                            iced::Size::ZERO,
                            iced::Size::new(geometry.viewport_width, DIFF_ROW_HEIGHT),
                        ),
                    );
                    let root = Layout::new(&node);
                    let halves = match mode {
                        DiffMode::Inline => vec![root],
                        DiffMode::Split => root.children().step_by(2).collect(),
                    };
                    for half in halves {
                        let content = half.children().next().unwrap();
                        let code = content.children().last().unwrap();
                        assert!((code.bounds().width - geometry.code_viewport).abs() < 0.01);
                        let shaped = code.children().next().unwrap();
                        assert!((shaped.bounds().x - code.bounds().x + offset).abs() < 0.01);
                        assert!((shaped.bounds().width - code_width).abs() < 0.01);
                    }
                }
            }
        }
    }

    #[test]
    fn diff_accessibility_retains_source_and_line_context_through_scrolling() {
        use iced_accessibility::accesskit::Role;

        let before = "\tlet café = old_value(); // preserve all source text";
        let after = "\tlet café = new_value(); // preserve all source text";
        let model = crate::diff_view::build_diff(
            &format!("{before}\nunchanged\n"),
            &format!("{after}\nunchanged\n"),
        );
        let renderer = headless_renderer();
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let mut gutter_bounds = None;
            for horizontal in [0.0, 120.0] {
                let mut rows = vec![iced::widget::Space::new().height(30).into()];
                match mode {
                    DiffMode::Inline => {
                        rows.extend(model.inline_rows.iter().map(|row| {
                            inline_diff_row(row, &model.syntax, 44.0, 1024.0, horizontal)
                        }))
                    }
                    DiffMode::Split => {
                        rows.extend(model.split_rows.iter().map(|row| {
                            split_diff_row(row, &model.syntax, 44.0, 1024.0, horizontal)
                        }))
                    }
                }
                let mut element = Element::new(DiffHorizontalContent {
                    content: widget::column::with_children(rows).width(480).into(),
                    width: Length::Fixed(1504.0),
                    content_width: 480.0,
                    offset: -horizontal,
                    accessible_text: None,
                    viewport: Some(iced::Size::new(480.0, 90.0)),
                });
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1504.0, 300.0)),
                );
                let accessible = element.as_widget().a11y_nodes(
                    Layout::new(&node).with_virtual_offset(iced::Vector::new(horizontal, 30.0)),
                    &tree,
                    iced::mouse::Cursor::Unavailable,
                );
                let nodes = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .map(|node| node.node())
                    .filter(|node| !node.is_hidden())
                    .collect::<Vec<_>>();
                for (source, line) in [(before, "Old line 1"), (after, "New line 1")] {
                    let code = nodes
                        .iter()
                        .find(|node| node.label() == Some(source))
                        .expect("screen readers can read the exact old/new source");
                    assert_eq!(code.role(), Role::Paragraph);
                    assert_eq!(code.description(), Some(line));
                    let bounds = code.bounds().expect("visible code bounds");
                    assert!(bounds.x0 >= 0.0 && bounds.x1 <= 480.0);
                    assert!(bounds.y0 >= 0.0 && bounds.y1 <= 90.0);
                }
                let gutters = nodes
                    .iter()
                    .filter(|node| node.label() == Some("1"))
                    .map(|node| node.bounds().expect("visible line number bounds"))
                    .collect::<Vec<_>>();
                assert_eq!(gutters.len(), 2, "both native line-number gutters survive");
                if let Some(previous) = &gutter_bounds {
                    assert_eq!(
                        &gutters, previous,
                        "horizontal scrolling keeps gutters fixed"
                    );
                } else {
                    gutter_bounds = Some(gutters);
                }
                let context = nodes
                    .iter()
                    .filter(|node| node.label() == Some("unchanged"))
                    .map(|node| node.description().expect("context line numbers"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    context,
                    match mode {
                        DiffMode::Inline => vec!["Old line 2, new line 2"],
                        DiffMode::Split => vec!["Old line 2", "New line 2"],
                    }
                );
            }
        }
    }

    #[test]
    fn parses_status_paths_without_losing_rename_boundaries() {
        let parsed =
            parse_status_paths(b" M src/main.rs\0R  renamed.md\0old.md\0?? new.txt\0").unwrap();
        assert_eq!(
            parsed
                .get(Path::new("src/main.rs"))
                .map(ParsedStatus::display_code),
            Some(" M".into())
        );
        assert_eq!(
            parsed
                .get(Path::new("renamed.md"))
                .map(ParsedStatus::display_code),
            Some("R ".into())
        );
        assert_eq!(
            parsed
                .get(Path::new("new.txt"))
                .map(ParsedStatus::display_code),
            Some("??".into())
        );
        assert_eq!(
            parsed
                .get(Path::new("renamed.md"))
                .and_then(|status| status.staged_old_path.as_deref()),
            Some(Path::new("old.md"))
        );
        assert!(!parsed.contains_key(Path::new("old.md")));
    }

    #[test]
    fn rejects_truncated_and_traversing_git_paths() {
        assert!(parse_paths(b"src/main.rs", "fixture").is_err());
        assert!(parse_paths(b"../outside\0", "fixture").is_err());
        assert!(parse_status_paths(b"R  renamed\0").is_err());
    }

    #[test]
    fn merges_staged_deletion_and_untracked_replacement_for_one_path() {
        let parsed = parse_status_paths(b"D  replacement.txt\0?? replacement.txt\0").unwrap();
        let status = parsed.get(Path::new("replacement.txt")).unwrap();
        assert_eq!(status.staged, Some(b'D'));
        assert_eq!(status.unstaged, Some(b'?'));
        assert_eq!(status.display_code(), "D?");

        let root = tempdir().unwrap();
        run_git(root.path(), ["init", "-q"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(root.path().join("replacement.txt"), "same path\n").unwrap();
        run_git(root.path(), ["add", "replacement.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["rm", "--cached", "-q", "replacement.txt"]);
        let files = load_repository_files(root.path()).unwrap();
        let replacements = files
            .iter()
            .filter(|file| file.path == Path::new("replacement.txt"))
            .collect::<Vec<_>>();
        assert_eq!(replacements.len(), 1);
        assert!(replacements[0].staged);
        assert!(replacements[0].unstaged);
        assert_eq!(replacements[0].status.as_deref(), Some("D?"));
    }

    #[test]
    fn worktree_rename_uses_the_old_baseline_and_suppresses_the_source_row() {
        let files = assemble_repository_files(
            b"old-name.md\0",
            b"renamed.md\0",
            b" R renamed.md\0old-name.md\0",
        )
        .unwrap();
        assert_eq!(
            files,
            vec![RepositoryFile {
                path: PathBuf::from("renamed.md"),
                staged_old_path: None,
                unstaged_old_path: Some(PathBuf::from("old-name.md")),
                staged: false,
                unstaged: true,
                status: Some(" R".into()),
            }]
        );
    }

    #[test]
    fn real_git_fixture_lists_tracked_and_changed_files() {
        let root = tempdir().unwrap();
        run_git(root.path(), ["init", "-q"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(root.path().join("tracked.md"), "# Before\n").unwrap();
        std::fs::write(root.path().join("old-name.md"), "unchanged\n").unwrap();
        run_git(root.path(), ["add", "tracked.md", "old-name.md"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["mv", "old-name.md", "renamed.md"]);
        std::fs::write(root.path().join("tracked.md"), "# After\n").unwrap();
        std::fs::write(root.path().join("new.txt"), "new\n").unwrap();

        let files = load_repository_files(root.path()).unwrap();
        assert_eq!(
            files,
            vec![
                RepositoryFile {
                    path: PathBuf::from("new.txt"),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: true,
                    status: Some("??".into())
                },
                RepositoryFile {
                    path: PathBuf::from("renamed.md"),
                    staged_old_path: Some(PathBuf::from("old-name.md")),
                    unstaged_old_path: None,
                    staged: true,
                    unstaged: false,
                    status: Some("R ".into())
                },
                RepositoryFile {
                    path: PathBuf::from("tracked.md"),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: true,
                    status: Some(" M".into())
                },
            ]
        );
    }

    #[test]
    fn real_git_fixture_keeps_status_only_and_staged_plus_unstaged_deletions() {
        let root = tempdir().unwrap();
        run_git(root.path(), ["init", "-q"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        for path in ["staged-delete", "unstaged-delete", "both-delete"] {
            std::fs::write(root.path().join(path), "head\n").unwrap();
        }
        run_git(root.path(), ["add", "."]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["rm", "-q", "staged-delete"]);
        std::fs::remove_file(root.path().join("unstaged-delete")).unwrap();
        std::fs::write(root.path().join("both-delete"), "indexed\n").unwrap();
        run_git(root.path(), ["add", "both-delete"]);
        std::fs::remove_file(root.path().join("both-delete")).unwrap();

        let files = load_repository_files(root.path()).unwrap();
        let staged = files
            .iter()
            .find(|file| file.path == Path::new("staged-delete"))
            .expect("a staged deletion absent from the index must remain listed");
        assert_eq!((staged.staged, staged.unstaged), (true, false));
        assert_eq!(staged.status.as_deref(), Some("D "));
        let unstaged = files
            .iter()
            .find(|file| file.path == Path::new("unstaged-delete"))
            .unwrap();
        assert_eq!((unstaged.staged, unstaged.unstaged), (false, true));
        assert_eq!(unstaged.status.as_deref(), Some(" D"));
        let both = files
            .iter()
            .find(|file| file.path == Path::new("both-delete"))
            .unwrap();
        assert_eq!((both.staged, both.unstaged), (true, true));
        assert_eq!(both.status.as_deref(), Some("MD"));
    }

    #[test]
    fn editor_dirty_state_and_markdown_preview_follow_edits() {
        let mut document = EditorDocument::new(
            PathBuf::from("README.md"),
            "# Title\n".into(),
            blake3::hash(b"# Title\n"),
            true,
        );
        let original_items = document.markdown_items.len();
        document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
            std::sync::Arc::new("More".into()),
        )));
        assert!(document.dirty);
        assert!(!document.markdown_items.is_empty());
        assert!(document.markdown_items.len() >= original_items);
        document.discard_edits();
        assert!(!document.dirty);
        assert!(document.active_save.is_none());
        assert_eq!(document.content.text(), "# Title\n");
    }

    #[test]
    fn save_completion_keeps_edits_made_while_the_write_was_in_flight_dirty() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.txt");
        runtime.generation = 1;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        let mut document = EditorDocument::new(
            path.clone(),
            "before\n".into(),
            blake3::hash(b"before\n"),
            false,
        );
        document.content = text_editor::Content::with_text("typed later\n");
        document.dirty = true;
        document.revision = 2;
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 1,
        };
        document.active_save = Some(SaveTicket {
            id: 7,
            identity: identity.clone(),
            revision: 1,
            snapshot: "submitted\n".into(),
        });
        runtime.document = DocumentState::Editable(document);

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity,
            ticket: 7,
            revision: 1,
            saved_text: "submitted\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"submitted\n"),
                durability: SaveDurability::Durable,
            }),
        });

        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        assert_eq!(document.saved_text, "submitted\n");
        assert_eq!(document.content.text(), "typed later\n");
        assert!(document.dirty);
        assert!(document.active_save.is_none());
    }

    #[test]
    fn post_persist_failure_uses_submitted_snapshot_as_baseline_and_fences_edit_back() {
        let card = CardId::new();
        let runtime = RepositoryRuntime::post_persist_warning_fixture(card);

        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        assert_eq!(document.saved_text, "B\n");
        assert_eq!(document.disk_hash, blake3::hash(b"B\n"));
        assert_eq!(document.content.text(), "A\n");
        assert_eq!(document.revision, 2);
        assert!(document.dirty);
        assert!(document.active_save.is_none());
        assert!(
            document
                .durability_warning
                .as_deref()
                .is_some_and(|warning| {
                    warning.contains("replaced on disk")
                        && warning.contains("Reload from disk or retry")
                })
        );
        assert!(runtime.blocks_navigation());
        assert!(runtime.can_save());
        assert!(runtime.inspector_context().can_reload);
    }

    #[test]
    fn pre_persist_failure_keeps_original_baseline_and_unsaved_buffer_dirty() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!("document should remain editable");
        };
        document.content = text_editor::Content::with_text("B\n");
        document.dirty = true;
        document.revision = 1;
        let ticket = document.active_save.clone().expect("save is in flight");

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity: ticket.identity,
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot,
            result: Err(FileIoError::Io("injected failure before persist".into())),
        });

        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        assert_eq!(document.saved_text, "A\n");
        assert_eq!(document.disk_hash, blake3::hash(b"A\n"));
        assert_eq!(document.content.text(), "B\n");
        assert!(document.dirty);
        assert!(document.durability_warning.is_none());
        assert!(
            document
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failure before persist"))
        );
        assert!(runtime.blocks_navigation());
    }

    #[test]
    fn failed_retry_keeps_post_persist_warning_visible_and_navigation_fenced() {
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::post_persist_warning_fixture(card);
        let _retry = runtime.request_save(card, false);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        let ticket = document.active_save.clone().expect("retry is in flight");

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity: ticket.identity,
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot,
            result: Err(FileIoError::Io(
                "injected retry failure before persist".into(),
            )),
        });

        let message = runtime.save_error().expect("combined warning is visible");
        assert!(message.contains("retry failure before persist"));
        assert!(message.contains("replaced on disk"));
        assert!(message.contains("Reload from disk or retry"));
        assert!(runtime.blocks_navigation());
        assert!(runtime.can_save());
        let context = runtime.inspector_context();
        assert_eq!(context.document, "Durability warning");
        assert!(
            context
                .document_error
                .as_deref()
                .is_some_and(|message| message.contains("retry failure before persist")
                    && message.contains("replaced on disk"))
        );
        assert!(context.can_reload);
        assert!(context.can_save);
    }

    #[test]
    fn reload_and_overwrite_resolve_post_persist_durability_warning_explicitly() {
        let card = CardId::new();
        let mut reloaded = RepositoryRuntime::post_persist_warning_fixture(card);
        let _reload = reloaded.update(RepositoryAction::Reload { card });
        let identity = reloaded.current_identity(card, PathBuf::from("notes.txt"));
        let _ = reloaded.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(FileContent::Editable {
                text: "B\n".into(),
                content_hash: blake3::hash(b"B\n"),
                markdown: false,
            }),
        });
        assert!(!reloaded.blocks_navigation());
        assert!(reloaded.save_error().is_none());

        let mut overwritten = RepositoryRuntime::post_persist_warning_fixture(card);
        let _overwrite = overwritten.request_save(card, true);
        let DocumentState::Editable(document) = &overwritten.document else {
            panic!("document should remain editable");
        };
        let ticket = document.active_save.clone().expect("overwrite started");
        let _ = overwritten.update(RepositoryAction::SaveFinished {
            identity: ticket.identity,
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot,
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"A\n"),
                durability: SaveDurability::Durable,
            }),
        });
        assert!(!overwritten.blocks_navigation());
        assert!(overwritten.save_error().is_none());
    }

    #[test]
    fn obsolete_save_completion_cannot_change_document_or_release_navigation_fence() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.txt");
        runtime.generation = 1;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        let mut document = EditorDocument::new(
            path.clone(),
            "disk\n".into(),
            blake3::hash(b"disk\n"),
            false,
        );
        document.content = text_editor::Content::with_text("newest\n");
        document.dirty = true;
        document.revision = 3;
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 1,
        };
        document.active_save = Some(SaveTicket {
            id: 8,
            identity: identity.clone(),
            revision: 3,
            snapshot: "newest\n".into(),
        });
        runtime.document = DocumentState::Editable(document);

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity,
            ticket: 7,
            revision: 2,
            saved_text: "obsolete\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"obsolete\n"),
                durability: SaveDurability::Durable,
            }),
        });

        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        assert_eq!(document.saved_text, "disk\n");
        assert_eq!(document.content.text(), "newest\n");
        assert!(document.active_save.is_none());
        assert!(document.conflict);
        assert!(
            document
                .error
                .as_deref()
                .is_some_and(|error| error.contains("disk state is unknown"))
        );
        assert!(runtime.blocks_navigation());
    }

    #[test]
    fn save_in_flight_blocks_navigation_even_when_text_temporarily_matches_old_disk() {
        let mut document = EditorDocument::new(
            PathBuf::from("notes.txt"),
            "disk\n".into(),
            blake3::hash(b"disk\n"),
            false,
        );
        document.revision = 2;
        document.active_save = Some(SaveTicket {
            id: 1,
            identity: AsyncIdentity {
                card: CardId::new(),
                worktree: PathBuf::from("/repo"),
                path: PathBuf::from("notes.txt"),
                generation: 1,
            },
            revision: 1,
            snapshot: "submitted\n".into(),
        });
        document.dirty = false;
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.document = DocumentState::Editable(document);
        assert!(!runtime.is_dirty());
        assert!(runtime.blocks_navigation());
    }

    #[test]
    fn active_save_rejects_open_reload_and_discard_transitions() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.txt");
        runtime.generation = 4;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 4,
        };
        let mut document = EditorDocument::new(
            path.clone(),
            "disk\n".into(),
            blake3::hash(b"disk\n"),
            false,
        );
        document.content = text_editor::Content::with_text("submitted\n");
        document.dirty = true;
        document.revision = 1;
        document.active_save = Some(SaveTicket {
            id: 9,
            identity,
            revision: 1,
            snapshot: "submitted\n".into(),
        });
        runtime.document = DocumentState::Editable(document);

        for action in [
            RepositoryAction::DiscardEdits { card },
            RepositoryAction::Reload { card },
            RepositoryAction::OpenFile {
                card,
                path: PathBuf::from("other.txt"),
            },
            RepositoryAction::ApplySourceSurface { card, diff: true },
            RepositoryAction::ApplyMarkdownMode {
                card,
                preview: true,
            },
        ] {
            let _ = runtime.update(action);
        }

        assert_eq!(runtime.generation, 4);
        assert_eq!(runtime.selected.as_ref().unwrap().path, path);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("the active save document must remain installed");
        };
        assert_eq!(document.content.text(), "submitted\n");
        assert_eq!(
            document.active_save.as_ref().map(|ticket| ticket.id),
            Some(9)
        );
        assert!(runtime.blocks_navigation());
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Edit);
    }

    #[test]
    fn mutation_refresh_reconciles_staging_without_obsoleting_document_loads() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
        runtime.source_surface = SourceSurface::Diff;
        let old_request = runtime.next_diff_request;
        let _ = runtime.refresh_after_mutation(card);
        let mut staged = runtime.selected.clone().unwrap();
        staged.staged = true;
        staged.unstaged = false;
        staged.status = Some("M ".into());
        let _ = runtime.update(RepositoryAction::FilesLoaded {
            identity: AsyncIdentity {
                card,
                worktree: identity.worktree.clone(),
                path: PathBuf::from("."),
                generation: runtime.file_list_generation,
            },
            result: Ok(vec![staged]),
        });
        let _ = runtime.update(RepositoryAction::FileLoaded {
            identity: identity.clone(),
            result: Ok(FileContent::Editable {
                text: "loaded\n".into(),
                content_hash: blake3::hash(b"loaded\n"),
                markdown: false,
            }),
        });
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: DiffBasis::IndexToWorktree,
            request: old_request,
            content: RenderedDiff::Error("obsolete diff".into()),
        });
        let context = runtime.inspector_context();
        assert!(context.has_staged);
        assert!(!context.has_unstaged);
        assert_eq!(runtime.diff_basis, DiffBasis::HeadToIndex);
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
        assert!(matches!(
            runtime.diff_state(&DiffBasis::HeadToIndex),
            Some(DiffState::Loading { .. })
        ));
        assert!(matches!(
            &runtime.document,
            DocumentState::Editable(document) if document.content.text() == "loaded\n"
        ));
    }

    #[test]
    fn mutation_refresh_keeps_dirty_buffer_save_and_pinned_commit_when_path_disappears() {
        let card = CardId::new();
        let (mut runtime, completion) = RepositoryRuntime::post_persist_failure_fixture(card);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.replace_text("still editing\n");
        let commit = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        };
        runtime.diff_basis = commit.clone();
        runtime.source_surface = SourceSurface::Diff;
        runtime.set_diff_state(
            commit.clone(),
            DiffState::Ready(staged_text_diff(model_with(4))),
        );
        let _ = runtime.refresh_after_mutation(card);
        let _ = runtime.update(RepositoryAction::FilesLoaded {
            identity: AsyncIdentity {
                card,
                worktree: runtime.descriptor.worktree.clone(),
                path: PathBuf::from("."),
                generation: runtime.file_list_generation,
            },
            result: Ok(Vec::new()),
        });
        let context = runtime.inspector_context();
        assert!(!context.has_staged && !context.has_unstaged);
        assert!(context.dirty && context.saving);
        assert_eq!(runtime.diff_basis, commit);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(context.diff_additions, Some(4));
        let _ = runtime.update(completion);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("mutation refresh must retain the edited buffer");
        };
        assert_eq!(document.content.text(), "still editing\n");
        assert_eq!(document.saved_text, "B\n");
        assert!(document.dirty);
        assert!(document.active_save.is_none());
        assert!(!document.conflict);
    }

    #[test]
    fn save_completion_supersedes_status_read_started_before_it() {
        let card = CardId::new();
        let (mut runtime, completion) = RepositoryRuntime::post_persist_failure_fixture(card);
        let _ = runtime.refresh_after_mutation(card);
        let stale_identity = AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: PathBuf::from("."),
            generation: runtime.file_list_generation,
        };
        let _ = runtime.update(completion);
        let _ = runtime.update(RepositoryAction::FilesLoaded {
            identity: stale_identity,
            result: Ok(Vec::new()),
        });
        assert!(runtime.mutation_refresh_pending);
        let mut changed = runtime.selected.clone().unwrap();
        changed.unstaged = true;
        changed.status = Some(" M".into());
        let _ = runtime.update(RepositoryAction::FilesLoaded {
            identity: AsyncIdentity {
                card,
                worktree: runtime.descriptor.worktree.clone(),
                path: PathBuf::from("."),
                generation: runtime.file_list_generation,
            },
            result: Ok(vec![changed]),
        });
        assert!(runtime.inspector_context().has_unstaged);
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
        assert!(!runtime.is_saving());
    }

    #[test]
    fn file_list_refresh_does_not_obsolete_a_pending_document_load() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.md");
        runtime.generation = 4;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        runtime.document = DocumentState::Loading(path.clone());

        let _refresh = runtime.update(RepositoryAction::RefreshFiles { card });
        assert_eq!(runtime.generation, 4);
        assert_eq!(runtime.file_list_generation, 1);
        let _ = runtime.update(RepositoryAction::FileLoaded {
            identity: AsyncIdentity {
                card,
                worktree: PathBuf::from("/repo"),
                path: path.clone(),
                generation: 4,
            },
            result: Ok(FileContent::Editable {
                text: "loaded\n".into(),
                content_hash: blake3::hash(b"loaded\n"),
                markdown: true,
            }),
        });
        let _ = runtime.update(RepositoryAction::FilesLoaded {
            identity: AsyncIdentity {
                card,
                worktree: PathBuf::from("/repo"),
                path: PathBuf::from("."),
                generation: 1,
            },
            result: Ok(vec![RepositoryFile {
                path: path.clone(),
                staged_old_path: None,
                unstaged_old_path: None,
                staged: true,
                unstaged: false,
                status: Some("M ".into()),
            }]),
        });

        assert!(matches!(
            runtime.document,
            DocumentState::Editable(ref document) if document.path == path
        ));
        assert!(
            runtime
                .selected
                .as_ref()
                .is_some_and(|selected| !selected.staged && selected.unstaged)
        );
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
    }

    #[test]
    fn file_list_refresh_does_not_obsolete_an_active_save() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.txt");
        runtime.generation = 7;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 7,
        };
        let mut document =
            EditorDocument::new(path, "before\n".into(), blake3::hash(b"before\n"), false);
        document.content = text_editor::Content::with_text("after\n");
        document.dirty = true;
        document.revision = 1;
        document.active_save = Some(SaveTicket {
            id: 2,
            identity: identity.clone(),
            revision: 1,
            snapshot: "after\n".into(),
        });
        runtime.document = DocumentState::Editable(document);

        let _refresh = runtime.update(RepositoryAction::RefreshFiles { card });
        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity,
            ticket: 2,
            revision: 1,
            saved_text: "after\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"after\n"),
                durability: SaveDurability::Durable,
            }),
        });

        assert_eq!(runtime.generation, 7);
        assert_eq!(runtime.file_list_generation, 1);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("the saved document must remain installed");
        };
        assert!(!document.dirty);
        assert!(document.active_save.is_none());
        assert!(!document.conflict);
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
    }

    #[test]
    fn starting_a_save_forces_the_result_bearing_editor_surface() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.md");
        runtime.generation = 1;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        let mut document =
            EditorDocument::new(path, "disk\n".into(), blake3::hash(b"disk\n"), true);
        document.content = text_editor::Content::with_text("changed\n");
        document.dirty = true;
        document.revision = 1;
        runtime.document = DocumentState::Editable(document);
        runtime.source_surface = SourceSurface::Diff;
        runtime.markdown_mode = MarkdownMode::Preview;

        let _save = runtime.request_save(card, false);

        assert!(runtime.is_saving());
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Edit);
    }

    #[test]
    fn autosave_ignores_stale_edits_and_preserves_the_current_surface() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::lsp_open_fixture(card, "disk\n");
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.replace_text("first\n");
        let revision = document.revision;
        drop(runtime.auto_save_task(card));
        let obsolete = RepositoryAction::AutoSaveDue {
            identity: identity.clone(),
            revision,
            ticket: runtime.next_auto_save_ticket,
        };
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.replace_text("latest\n");
        let revision = document.revision;
        drop(runtime.auto_save_task(card));
        drop(runtime.update(obsolete));
        assert!(!runtime.is_saving());

        runtime.source_surface = SourceSurface::Diff;
        runtime.markdown_mode = MarkdownMode::Preview;
        drop(runtime.update(RepositoryAction::AutoSaveDue {
            identity,
            revision,
            ticket: runtime.next_auto_save_ticket,
        }));
        assert!(runtime.is_saving());
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
        drop(runtime.update(runtime.durable_active_save_completion()));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(document.saved_text, "latest\n");
        assert!(!runtime.is_dirty());
    }

    #[test]
    fn disabling_autosave_invalidates_an_already_scheduled_save() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::lsp_open_fixture(card, "disk\n");
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.replace_text("unsaved\n");
        let revision = document.revision;
        drop(runtime.auto_save_task(card));
        let due = RepositoryAction::AutoSaveDue {
            identity,
            revision,
            ticket: runtime.next_auto_save_ticket,
        };
        runtime.set_auto_save(false);
        runtime.set_auto_save(true);
        drop(runtime.auto_save_task(card));
        drop(runtime.update(due));
        assert!(!runtime.is_saving());
        assert!(runtime.is_dirty());
    }

    #[test]
    fn editing_after_an_autosave_conflict_keeps_explicit_recovery_required() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::lsp_open_fixture(card, "disk\n");
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.replace_text("unsaved\n");
        let revision = document.revision;
        drop(runtime.auto_save_task(card));
        drop(runtime.update(RepositoryAction::AutoSaveDue {
            identity,
            revision,
            ticket: runtime.next_auto_save_ticket,
        }));
        drop(runtime.update(runtime.conflicted_active_save_completion()));
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.auto_save_task(card));
        assert!(runtime.has_save_conflict());
        assert!(!runtime.is_saving());
        assert!(runtime.is_dirty());
    }

    #[test]
    fn inspector_projection_tracks_loading_conflict_and_active_diff_facts() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        assert_eq!(
            runtime.inspector_runtime_status(),
            RepositoryRuntimeStatus::Loading
        );
        assert_eq!(runtime.inspector_context().file_list, "Loading");

        let file = RepositoryFile {
            path: PathBuf::from("notes.md"),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: true,
            unstaged: true,
            status: Some("MM".into()),
        };
        runtime.files = FileListState::Ready(vec![file.clone()]);
        runtime.selected = Some(file);
        let mut document = EditorDocument::new(
            PathBuf::from("notes.md"),
            "before\n".into(),
            blake3::hash(b"before\n"),
            true,
        );
        document.dirty = true;
        document.conflict = true;
        document.error = Some("changed on disk".into());
        runtime.document = DocumentState::Editable(document);
        runtime.source_surface = SourceSurface::Diff;
        runtime.diff_basis = DiffBasis::HeadToIndex;
        runtime.diff_mode = DiffMode::Split;
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::Ready(staged_text_diff(DiffModel {
                inline_rows: Vec::new(),
                split_rows: Vec::new(),
                inline_change_starts: vec![0, 2],
                split_change_starts: vec![0, 2],
                additions: 3,
                deletions: 2,
                max_line_width: [0; 2],
                syntax: DiffSyntax::default(),
            })),
        );

        let context = runtime.inspector_context();
        assert_eq!(context.file_count, Some(1));
        assert_eq!(context.surface, "Diff");
        assert_eq!(context.document, "Save conflict");
        assert_eq!(context.document_error.as_deref(), Some("changed on disk"));
        assert!(context.conflict);
        assert!(context.has_staged && context.has_unstaged);
        assert_eq!(context.diff_basis, Some("Staged"));
        assert_eq!(context.diff_mode, Some("Split"));
        assert_eq!(context.diff_state, Some("Ready"));
        assert_eq!(context.diff_additions, Some(3));
        assert_eq!(context.diff_deletions, Some(2));
        assert_eq!(context.diff_change, Some((1, 2)));

        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::Loading {
                request: 1,
                cancellation: CancellationToken::new(),
            },
        );
        assert_eq!(
            runtime.inspector_runtime_status(),
            RepositoryRuntimeStatus::Loading
        );
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::Ready(RenderedDiff::Error("diff failed".into())),
        );
        assert_eq!(
            runtime.inspector_runtime_status(),
            RepositoryRuntimeStatus::Unavailable
        );
        assert_eq!(
            runtime.inspector_context().diff_error.as_deref(),
            Some("diff failed")
        );

        runtime.files = FileListState::Failed("status failed".into());
        let context = runtime.inspector_context();
        assert_eq!(context.file_list, "Unavailable");
        assert_eq!(context.file_error.as_deref(), Some("status failed"));
    }

    #[test]
    fn direct_diff_basis_action_cannot_select_an_absent_basis() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.selected = Some(RepositoryFile {
            path: PathBuf::from("only-unstaged.txt"),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        let card = CardId::new();
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: DiffBasis::HeadToIndex,
        });
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
    }

    #[test]
    fn successful_save_invalidates_hidden_diff_then_loads_fresh_changes_on_demand() {
        for tracked in [true, false] {
            let view = RepositoryView {
                project: ProjectId::new(),
                repository: PathBuf::from("/repo/.git"),
                worktree: PathBuf::from("/repo"),
            };
            let mut runtime = RepositoryRuntime::new(view);
            let card = CardId::new();
            let path = PathBuf::from(if tracked { "tracked.txt" } else { "new.txt" });
            runtime.generation = 1;
            runtime.selected = Some(RepositoryFile {
                path: path.clone(),
                staged_old_path: None,
                unstaged_old_path: None,
                staged: false,
                unstaged: !tracked,
                status: (!tracked).then(|| "??".into()),
            });
            let mut document = EditorDocument::new(
                path.clone(),
                "before\n".into(),
                blake3::hash(b"before\n"),
                false,
            );
            document.content = text_editor::Content::with_text("after\n");
            document.dirty = true;
            document.revision = 1;
            let identity = AsyncIdentity {
                card,
                worktree: PathBuf::from("/repo"),
                path: path.clone(),
                generation: 1,
            };
            document.active_save = Some(SaveTicket {
                id: 1,
                identity: identity.clone(),
                revision: 1,
                snapshot: "after\n".into(),
            });
            runtime.document = DocumentState::Editable(document);

            let _ = runtime.update(RepositoryAction::SaveFinished {
                identity,
                ticket: 1,
                revision: 1,
                saved_text: "after\n".into(),
                result: Ok(SaveOutcome::Saved {
                    content_hash: blake3::hash(b"after\n"),
                    durability: SaveDurability::Durable,
                }),
            });

            assert!(
                runtime
                    .selected
                    .as_ref()
                    .is_some_and(RepositoryFile::changed)
            );
            assert_eq!(
                runtime
                    .selected
                    .as_ref()
                    .and_then(|selected| selected.status.as_deref()),
                Some(if tracked { " M" } else { "??" })
            );
            assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
            drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
            assert!(matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Loading { .. })
            ));
        }
    }

    #[test]
    fn successful_save_marks_a_staged_file_as_staged_and_unstaged() {
        assert_eq!(status_with_unstaged_change(Some("M "), false), "MM");
        assert_eq!(status_with_unstaged_change(None, false), " M");
        for status in ["??", "UU", "D?", "AA", " R", " T"] {
            assert_eq!(status_with_unstaged_change(Some(status), true), status);
        }
    }

    #[test]
    fn diff_completions_are_fenced_per_basis_and_preserve_the_active_viewport() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from("notes.txt");
        runtime.generation = 1;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: true,
            unstaged: true,
            status: Some("MM".into()),
        });
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        runtime.diff_change = 2;
        runtime.diff_viewport = ListViewport {
            offset: 600.0,
            height: 300.0,
        };
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::Loading {
                request: 3,
                cancellation: CancellationToken::new(),
            },
        );
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Loading {
                request: 2,
                cancellation: CancellationToken::new(),
            },
        );
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path,
            generation: 1,
        };

        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity: identity.clone(),
            basis: DiffBasis::HeadToIndex,
            request: 3,
            content: staged_text_diff(model_with(3)),
        });
        assert_eq!(runtime.diff_change, 2);
        assert_eq!(runtime.diff_viewport.offset, 600.0);

        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            request: 2,
            content: staged_text_diff(model_with(2)),
        });
        assert_eq!(runtime.diff_change, 0);
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: DiffBasis::IndexToWorktree,
            request: 1,
            content: staged_text_diff(model_with(1)),
        });
        assert!(matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Ready(RenderedDiff::Text { model, .. })) if model.additions == 2
        ));
    }

    #[test]
    fn stale_file_completion_cannot_replace_current_selection() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.generation = 9;
        runtime.selected = Some(RepositoryFile {
            path: PathBuf::from("current.rs"),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        runtime.document = DocumentState::Loading(PathBuf::from("current.rs"));
        let stale = AsyncIdentity {
            card: CardId::new(),
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("older.rs"),
            generation: 8,
        };
        let _ = runtime.update(RepositoryAction::FileLoaded {
            identity: stale,
            result: Ok(FileContent::Editable {
                text: "stale".into(),
                content_hash: blake3::hash(b"stale"),
                markdown: false,
            }),
        });
        assert!(
            matches!(runtime.document, DocumentState::Loading(ref path) if path == Path::new("current.rs"))
        );
    }

    #[test]
    fn visible_row_projection_bounds_widgets_for_large_file_and_diff_fixtures() {
        let top = visible_rows(
            100_000,
            ListViewport {
                offset: 0.0,
                height: 720.0,
            },
            FILE_ROW_HEIGHT,
        );
        let middle = visible_rows(
            1_000_000,
            ListViewport {
                offset: 12_345_678.0,
                height: 900.0,
            },
            DIFF_ROW_HEIGHT,
        );
        assert!(top.end - top.start <= 34);
        assert!(middle.end - middle.start <= 43);
        assert_eq!(
            top.top_space + (top.end - top.start) as f32 * FILE_ROW_HEIGHT + top.bottom_space,
            100_000.0 * FILE_ROW_HEIGHT
        );
        assert_eq!(
            middle.top_space
                + (middle.end - middle.start) as f32 * DIFF_ROW_HEIGHT
                + middle.bottom_space,
            1_000_000.0 * DIFF_ROW_HEIGHT
        );
    }

    #[test]
    fn actual_widget_trees_stay_bounded_at_top_middle_and_end_of_100k_rows() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.files = FileListState::Ready(
            (0..100_000)
                .map(|index| RepositoryFile {
                    path: PathBuf::from(format!("file-{index:06}.txt")),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: false,
                    status: None,
                })
                .collect(),
        );
        let card = CardId::new();
        for offset in [
            0.0,
            50_000.0 * FILE_ROW_HEIGHT,
            100_000.0 * FILE_ROW_HEIGHT - 720.0,
        ] {
            runtime.file_viewport = ListViewport {
                offset,
                height: 720.0,
            };
            let element = file_list_view(card, &runtime);
            assert!(widget_nodes(&element) < 1_000, "file widget tree was eager");
        }

        let inline = InlineDiffRow {
            old_line: Some(1),
            new_line: Some(1),
            text: "line".into(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        };
        let split = SplitDiffRow {
            old_line: Some(1),
            new_line: Some(1),
            old_text: Some("line".into()),
            new_text: Some("line".into()),
            kind: DiffRowKind::Context,
            old_changed: Vec::new(),
            new_changed: Vec::new(),
        };
        runtime.selected = Some(RepositoryFile {
            path: PathBuf::from("large.txt"),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Ready(staged_text_diff(DiffModel {
                inline_rows: vec![inline; 100_000],
                split_rows: vec![split; 100_000],
                inline_change_starts: vec![0, 50_000, 99_999],
                split_change_starts: vec![0, 50_000, 99_999],
                additions: 1,
                deletions: 1,
                max_line_width: [34; 2],
                syntax: DiffSyntax::default(),
            })),
        );
        for offset in [
            0.0,
            50_000.0 * DIFF_ROW_HEIGHT,
            100_000.0 * DIFF_ROW_HEIGHT - 720.0,
        ] {
            runtime.diff_viewport = ListViewport {
                offset,
                height: 720.0,
            };
            let element = diff_surface(card, &runtime);
            assert!(widget_nodes(&element) < 1_500, "diff widget tree was eager");
        }
    }

    #[test]
    fn change_navigation_realizes_far_rows_before_programmatic_scroll() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.diff_viewport.height = 720.0;
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Ready(staged_text_diff(DiffModel {
                inline_rows: vec![
                    InlineDiffRow {
                        old_line: Some(1),
                        new_line: Some(1),
                        text: "line".into(),
                        kind: DiffRowKind::Context,
                        changed: Vec::new(),
                    };
                    100_000
                ],
                split_rows: Vec::new(),
                inline_change_starts: vec![0, 50_000, 99_999],
                split_change_starts: Vec::new(),
                additions: 1,
                deletions: 1,
                max_line_width: [34; 2],
                syntax: DiffSyntax::default(),
            })),
        );
        let _ = runtime.update(RepositoryAction::NextChange { card });
        let middle = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        assert!(middle.start <= 50_000 && middle.end > 50_000);
        let _ = runtime.update(RepositoryAction::NextChange { card });
        let end = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        assert!(end.start <= 99_999 && end.end > 99_999);
    }

    fn completion_edit(range: LspRange, new_text: &str) -> LspCompletionEdit {
        LspCompletionEdit {
            range,
            replace_range: None,
            new_text: new_text.into(),
            snippet: None,
        }
    }

    fn completion_item(
        edit: LspCompletionEdit,
        additional_edits: Vec<LspCompletionEdit>,
    ) -> LspCompletionItem {
        LspCompletionItem {
            label: "println!".into(),
            kind: None,
            detail: None,
            documentation: None,
            sort_text: None,
            filter_text: None,
            preselect: false,
            deprecated: false,
            edit,
            additional_edits,
        }
    }

    fn buffer_text(runtime: &RepositoryRuntime) -> String {
        match &runtime.document {
            DocumentState::Editable(document) => document.content.text(),
            _ => panic!("test expects an editable buffer"),
        }
    }

    fn lsp_version(runtime: &RepositoryRuntime) -> i32 {
        match &runtime.document {
            DocumentState::Editable(document) => document.lsp_version,
            _ => panic!("test expects an editable buffer"),
        }
    }

    /// Normalises the caret without touching the buffer, and proves in passing
    /// that a motion is not a document change.
    fn move_to_start(runtime: &mut RepositoryRuntime, card: CardId) {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
        }));
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "moving the caret changes no text, so the server must hear nothing"
        );
    }

    #[test]
    fn loading_an_editable_file_opens_it_at_the_first_version() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Open {
                // Absolute: `AsyncIdentity::path` is worktree-relative and no
                // document URI can be built from it alone.
                absolute_path: PathBuf::from("/repo/notes.txt"),
                worktree: PathBuf::from("/repo"),
                version: 1,
                text: "fn main() {}\n".into(),
            }]
        );
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "draining is destructive; a notification performed twice is worse than one performed late"
        );
    }

    #[test]
    fn a_read_only_or_unreadable_file_is_never_opened_at_the_boundary() {
        let card = CardId::new();
        for result in [
            Ok(FileContent::ReadOnly(ReadOnlyReason::Binary)),
            Err(FileIoError::Io("injected read failure".into())),
        ] {
            let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
            drop(runtime.update(RepositoryAction::FileLoaded { identity, result }));
            assert_eq!(
                runtime.take_lsp_intents(),
                Vec::new(),
                "only an editable buffer is a document a language server can serve"
            );
        }
    }

    #[test]
    fn image_preview_never_enters_edit_save_or_language_server_pipelines() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
        runtime.set_auto_save(true);
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(FileContent::Image {
                width: 1,
                height: 1,
                rgba: vec![255, 0, 0, 255].into(),
            }),
        }));
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.update(RepositoryAction::Save {
            card,
            overwrite: true,
        }));
        assert!(!runtime.is_dirty());
        assert!(!runtime.can_save());
        assert!(!runtime.is_saving());
        assert!(runtime.take_lsp_intents().is_empty());
        assert_eq!(runtime.inspector_context().document, "Image preview");
        assert!(matches!(runtime.document, DocumentState::Image { .. }));
    }

    #[test]
    fn an_edit_reports_the_whole_document_at_the_next_version() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);

        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version: 2,
                text: "xfn main() {}\n".into(),
            }]
        );
    }

    #[test]
    fn an_exhausted_version_stops_reporting_changes_rather_than_rewinding() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!("the fixture opened an editable buffer");
        };
        document.lsp_version = i32::MAX;

        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "the version space is spent; a wrapped version would rewind the server"
        );
        assert_eq!(
            buffer_text(&runtime),
            "xfn main() {}\n",
            "the buffer stays authoritative even when it can no longer be reported"
        );
        assert_eq!(lsp_version(&runtime), i32::MAX, "saturated, never wrapped");
    }

    #[test]
    fn submitting_a_save_and_hitting_a_conflict_both_tell_the_server_nothing() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.take_lsp_intents());

        drop(runtime.request_save(card, false));
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "a submitted save has not reached disk, so `didSave` would be a lie"
        );

        let conflicted = runtime.conflicted_active_save_completion();
        drop(runtime.update(conflicted));
        assert!(runtime.has_save_conflict());
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "a conflict left the file on disk as it was"
        );
    }

    #[test]
    fn a_completed_save_reports_what_reached_disk() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.take_lsp_intents());
        drop(runtime.request_save(card, false));

        let completion = runtime.durable_active_save_completion();
        drop(runtime.update(completion));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Save {
                text: "xfn main() {}\n".into(),
            }]
        );
    }

    #[test]
    fn discarding_edits_resynchronises_the_server_with_the_saved_text() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.take_lsp_intents());

        drop(runtime.update(RepositoryAction::DiscardEdits { card }));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version: 3,
                text: "fn main() {}\n".into(),
            }],
            "discarding rebuilds the buffer without going through an editor action"
        );
    }

    #[test]
    fn abandoning_unsaved_changes_resynchronises_the_server() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.take_lsp_intents());

        assert!(runtime.abandon_unsaved_changes());
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version: 3,
                text: "fn main() {}\n".into(),
            }],
            "the dirty-close dialog discards the buffer behind the editor's back"
        );
    }

    #[test]
    fn reloading_closes_the_document_before_opening_it_again() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());

        drop(runtime.update(RepositoryAction::Reload { card }));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Close],
            "the reloaded buffer is a different document, however identical its path"
        );

        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("notes.txt"),
            generation: runtime.generation,
        };
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(FileContent::Editable {
                text: "fn main() {}\n".into(),
                content_hash: blake3::hash(b"fn main() {}\n"),
                markdown: false,
            }),
        }));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Open {
                absolute_path: PathBuf::from("/repo/notes.txt"),
                worktree: PathBuf::from("/repo"),
                version: 1,
                text: "fn main() {}\n".into(),
            }]
        );
    }

    #[test]
    fn opening_another_file_closes_the_one_it_replaces() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());

        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: PathBuf::from("other.txt"),
        }));
        assert_eq!(runtime.take_lsp_intents(), vec![LspIntent::Close]);
    }

    #[test]
    fn returning_to_the_file_list_closes_the_document() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        drop(runtime.take_lsp_intents());

        drop(runtime.update(RepositoryAction::BackToFiles { card }));
        assert_eq!(runtime.take_lsp_intents(), vec![LspIntent::Close]);
        assert!(runtime.cursor_position().is_none());
    }

    #[test]
    fn the_document_identity_is_stored_for_the_application_that_minted_it() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        assert_eq!(runtime.lsp_document(), None);
        runtime.set_lsp_document(Some(LspDocumentId(7)));
        assert_eq!(runtime.lsp_document(), Some(LspDocumentId(7)));
        runtime.set_lsp_document(None);
        assert_eq!(runtime.lsp_document(), None);
    }

    #[test]
    fn caret_columns_convert_between_bytes_and_scalars() {
        // Thirteen scalars in seventeen bytes: the accent and the arrow are
        // the whole point.
        let line = "héllo → wörld";
        assert_eq!(line.len(), 17);
        assert_eq!(scalar_column(line, line.len()), 13);
        assert_eq!(byte_column(line, 13), 17);
        assert_eq!(scalar_column(line, 7), 6, "the arrow starts at byte seven");
        assert_eq!(byte_column(line, 6), 7);
        assert_eq!(
            scalar_column(line, 8),
            6,
            "a byte index inside the arrow rounds down instead of panicking"
        );
        assert_eq!(scalar_column(line, 9), 6);
        assert_eq!(
            byte_column(line, 99),
            17,
            "a character past the end clamps to the end of the line"
        );
        assert_eq!(scalar_column(line, 99), 13);
    }

    #[test]
    fn the_caret_round_trips_through_protocol_coordinates_on_a_non_ascii_line() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "héllo → wörld\nplain\n");
        drop(runtime.take_lsp_intents());

        for at in [
            LspPosition::new(0, 0),
            LspPosition::new(0, 6),
            LspPosition::new(0, 9),
            LspPosition::new(0, 13),
            LspPosition::new(1, 5),
        ] {
            assert!(runtime.move_cursor_to(at), "line {} exists", at.line);
            assert_eq!(
                runtime.cursor_position(),
                Some(at),
                "a caret placed at {at:?} must be reported there, not at its byte offset"
            );
        }
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "jumping the caret changes no text"
        );
        assert!(
            !runtime.move_cursor_to(LspPosition::new(9, 0)),
            "a line the buffer does not have is refused rather than clamped"
        );
    }

    #[test]
    fn the_caret_round_trips_across_windows_line_endings() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::lsp_open_fixture(card, "let a = 1;\r\nlet b = 2;\r\n");
        drop(runtime.take_lsp_intents());

        for at in [LspPosition::new(0, 10), LspPosition::new(1, 4)] {
            assert!(runtime.move_cursor_to(at));
            assert_eq!(
                runtime.cursor_position(),
                Some(at),
                "a `\\r\\n` is one terminator and belongs to neither line's content"
            );
        }
    }

    #[test]
    fn a_bare_carriage_return_is_a_line_break_to_the_editor_and_not_to_the_protocol() {
        // The editor's `LineIter` breaks on a lone `\r`; `crate::lsp` counts
        // `\n` alone. This buffer therefore has three lines to the editor and
        // two to the protocol, and a caret that survives the trip proves the
        // conversion goes through byte offsets rather than through a line
        // index that means different things on each side.
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "old\rmac\n");
        drop(runtime.take_lsp_intents());

        let inside_the_second_editor_line = LspPosition::new(0, 5);
        assert!(runtime.move_cursor_to(inside_the_second_editor_line));
        assert_eq!(
            runtime.cursor_position(),
            Some(inside_the_second_editor_line)
        );
        assert!(
            !runtime.move_cursor_to(LspPosition::new(2, 0)),
            "the protocol sees two lines here, whatever the editor does"
        );
    }

    #[test]
    fn a_caret_after_a_stripped_carriage_return_reports_a_position_the_protocol_can_hold() {
        // `crate::lsp` drops a `\r` sitting immediately before a line's
        // terminating `\n`, or at the end of the buffer, from that line's
        // content — while the editor treats the same `\r` as a terminator of
        // its own and will park a caret after it. A column that counted the
        // `\r` would name a character the protocol's line does not have, which
        // `Snapshot::to_utf16` refuses with `PositionOutOfRange`: every hover,
        // completion and definition asked from that caret would silently fail.
        assert_eq!(
            position_for_offset("a\r", 2),
            LspPosition::new(0, 1),
            "the buffer's trailing bare `\\r` is not a character of line zero"
        );
        assert_eq!(
            position_for_offset("x\n\r\ny", 3),
            LspPosition::new(1, 0),
            "the editor reads `\\n\\r` as one terminator, the protocol reads `\\r\\n`; \
             either way line one is empty"
        );

        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "a\r");
        drop(runtime.take_lsp_intents());
        // The end of this buffer is a place `move_cursor_to` cannot name, so
        // the caret is driven there the way a keypress would.
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::DocumentEnd),
        }));
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 1)));
        assert_eq!(runtime.take_lsp_intents(), Vec::new());
    }

    #[test]
    fn every_reported_position_lands_inside_its_own_protocol_line() {
        // Exhaustive over the shapes that pull the editor's line numbering
        // apart from the protocol's: every terminator the editor recognises, a
        // multi-byte scalar, and an unterminated final line, in every order up
        // to four pieces.
        let atoms = ["", "a", "\n", "\r", "\r\n", "\n\r", "é"];
        let mut texts = vec![String::new()];
        for _ in 0..4 {
            texts = texts
                .iter()
                .flat_map(|text| atoms.iter().map(move |atom| format!("{text}{atom}")))
                .collect();
        }

        for text in &texts {
            for offset in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
                let at = position_for_offset(text, offset);
                let span =
                    line_span(text, at.line).expect("the line was counted out of this very text");
                let scalars = text[span].chars().count();
                assert!(
                    usize::try_from(at.character).unwrap() <= scalars,
                    "{text:?} offset {offset} reported {at:?}, but that line holds \
                     {scalars} scalars — `Snapshot::to_utf16` would refuse it"
                );
                let addressed =
                    byte_offset(text, at).expect("a reported position addresses a real line");
                assert_eq!(
                    position_for_offset(text, addressed),
                    at,
                    "{text:?} offset {offset} must survive a second conversion unchanged"
                );
            }
        }
    }

    #[test]
    fn a_completion_applies_every_edit_and_reports_one_version() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(
            card,
            "use std::fmt;\n\nfn main() {\n    prin\n}\n",
        );
        drop(runtime.take_lsp_intents());

        let item = completion_item(
            completion_edit(
                LspRange::new(LspPosition::new(3, 4), LspPosition::new(3, 8)),
                "println!()",
            ),
            vec![completion_edit(
                LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 0)),
                "use std::io;\n",
            )],
        );
        assert!(runtime.apply_completion(&item));

        let expected = "use std::fmt;\nuse std::io;\n\nfn main() {\n    println!()\n}\n";
        assert_eq!(buffer_text(&runtime), expected);
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version: 2,
                text: expected.into(),
            }],
            "one completion is one document transition, not one per edit"
        );
        assert_eq!(lsp_version(&runtime), 2);
        assert_eq!(
            runtime.cursor_position(),
            Some(LspPosition::new(4, 14)),
            "the caret follows the text the user chose, past the edit inserted above it"
        );
        assert!(runtime.is_dirty());
    }

    #[test]
    fn a_completion_whose_edits_overlap_is_refused_rather_than_half_applied() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "let value = 1;\n");
        drop(runtime.take_lsp_intents());

        let overlapping = completion_item(
            completion_edit(
                LspRange::new(LspPosition::new(0, 4), LspPosition::new(0, 9)),
                "other",
            ),
            vec![completion_edit(
                LspRange::new(LspPosition::new(0, 6), LspPosition::new(0, 11)),
                "clash",
            )],
        );
        assert!(!runtime.apply_completion(&overlapping));

        let unaddressable = completion_item(
            completion_edit(
                LspRange::new(LspPosition::new(40, 0), LspPosition::new(40, 1)),
                "nowhere",
            ),
            Vec::new(),
        );
        assert!(!runtime.apply_completion(&unaddressable));

        assert_eq!(buffer_text(&runtime), "let value = 1;\n");
        assert_eq!(lsp_version(&runtime), 1);
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "a refused completion is not a document change"
        );
        assert!(!runtime.is_dirty());
    }

    #[test]
    fn a_snippet_completion_inserts_its_literal_text() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "    prin\n");
        drop(runtime.take_lsp_intents());

        let mut edit = completion_edit(
            LspRange::new(LspPosition::new(0, 4), LspPosition::new(0, 8)),
            "println!()",
        );
        edit.snippet = Some(LspSnippet {
            text: "println!()".into(),
            stops: vec![LspSnippetStop {
                index: 1,
                range: 9..9,
            }],
        });
        assert!(runtime.apply_completion(&completion_item(edit, Vec::new())));
        assert_eq!(
            buffer_text(&runtime),
            "    println!()\n",
            "tab stops are carried but not entered in this pass"
        );
    }

    #[test]
    fn nothing_is_reported_for_a_card_with_no_editable_buffer() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_selection_fixture(card);
        assert_eq!(runtime.cursor_position(), None);
        assert!(!runtime.move_cursor_to(LspPosition::new(0, 0)));
        assert!(!runtime.apply_completion(&completion_item(
            completion_edit(LspRange::default(), "x"),
            Vec::new()
        )));
        assert_eq!(runtime.take_lsp_intents(), Vec::new());
    }

    /// A basis switch is a new request. The diff still in flight for the basis
    /// the user left must not be able to answer for the one they moved to, and
    /// a request the counter has already superseded must not answer at all.
    #[test]
    fn a_diff_for_a_superseded_basis_cannot_answer_for_the_current_one() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        let commit = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        };

        // The unstaged diff is in flight when the user switches to the commit
        // comparison, which starts a request of its own.
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Loading {
                request: 1,
                cancellation: CancellationToken::new(),
            },
        );
        runtime.next_diff_request = 1;
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: commit.clone(),
        });
        assert_eq!(runtime.diff_basis, commit);
        assert!(matches!(
            runtime.diff_state(&commit),
            Some(DiffState::Loading { request: 2, .. })
        ));

        // A cancelled answer may still arrive. It must neither recreate its
        // slot nor reset the viewport/change cursor of the active comparison.
        runtime.diff_viewport = ListViewport {
            offset: 480.0,
            height: 300.0,
        };
        runtime.diff_change = 3;
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            request: 1,
            content: staged_text_diff(model_with(9)),
        });
        assert!(
            matches!(
                runtime.diff_state(&commit),
                Some(DiffState::Loading { request: 2, .. })
            ),
            "the superseded basis answered for the current one"
        );
        assert!(runtime.diff_model().is_none());
        assert_eq!(runtime.diff_viewport.offset, 480.0);
        assert_eq!(runtime.diff_change, 3);
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());

        // A superseded request for the *current* basis is dropped outright.
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: commit.clone(),
            request: 1,
            content: staged_text_diff(model_with(7)),
        });
        assert!(matches!(
            runtime.diff_state(&commit),
            Some(DiffState::Loading { request: 2, .. })
        ));
    }

    /// A resolution nobody is waiting for cannot install a basis: the
    /// selection, or the choice, has moved on since it started.
    #[test]
    fn a_stale_basis_resolution_cannot_install_a_basis() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_basis = DiffBasis::IndexToWorktree;

        // Nothing outstanding at all.
        let _ = runtime.update(RepositoryAction::DiffBasisResolved {
            identity: identity.clone(),
            choice: CommittedChoice::Branch,
            result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
        });
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
        assert!(runtime.basis_resolution.is_none());

        // Outstanding, but for the other committed choice.
        runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::LastCommit));
        let _ = runtime.update(RepositoryAction::DiffBasisResolved {
            identity: identity.clone(),
            choice: CommittedChoice::Branch,
            result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
        });
        assert_eq!(
            runtime.basis_resolution,
            Some(BasisResolution::Resolving(CommittedChoice::LastCommit))
        );

        // Outstanding for this choice, but from a selection the card has left.
        runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::Branch));
        let _ = runtime.update(RepositoryAction::DiffBasisResolved {
            identity: AsyncIdentity {
                generation: 99,
                ..identity.clone()
            },
            choice: CommittedChoice::Branch,
            result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
        });
        assert_eq!(
            runtime.basis_resolution,
            Some(BasisResolution::Resolving(CommittedChoice::Branch))
        );

        // The one that is waited for is accepted, and clears the wait.
        let _ = runtime.update(RepositoryAction::DiffBasisResolved {
            identity,
            choice: CommittedChoice::Branch,
            result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
        });
        assert!(runtime.basis_resolution.is_none());
    }

    /// The branch comparison's default base is the remote's own symbolic
    /// `HEAD`. Without one, nothing is guessed: the refs that could serve as a
    /// base are offered instead.
    #[test]
    fn a_branch_basis_reports_a_missing_default_base_and_offers_the_refs() {
        let root = tempdir().unwrap();
        git_fixture(root.path());
        std::fs::write(root.path().join("file.txt"), "one\n").unwrap();
        run_git(root.path(), ["add", "file.txt"]);
        run_git(root.path(), ["commit", "-qm", "base"]);
        run_git(root.path(), ["branch", "release"]);

        let resolved = resolve_basis(root.path(), CommittedChoice::Branch).unwrap();
        let ResolvedBasis::NeedsBase { candidates } = &resolved else {
            panic!("a repository with no remote HEAD has no default base, got {resolved:?}");
        };
        assert_eq!(
            candidates
                .iter()
                .map(ValidatedRefName::as_str)
                .collect::<Vec<_>>(),
            vec!["refs/heads/release"],
            "the checked-out branch is not a base for itself"
        );

        // A symbolic remote HEAD is the default base, and is used verbatim.
        run_git(
            root.path(),
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        run_git(
            root.path(),
            [
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(
            resolve_basis(root.path(), CommittedChoice::Branch).unwrap(),
            ResolvedBasis::Ready(DiffBasis::MergeBaseToHead {
                base_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
            })
        );
    }

    /// The commit comparison is `HEAD` against its own first parent. A root
    /// commit has none, which the service compares against the empty tree, and
    /// an unborn branch has no commit at all.
    #[test]
    fn the_commit_basis_resolves_head_against_its_own_parent() {
        let root = tempdir().unwrap();
        git_fixture(root.path());
        assert_eq!(
            resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
            ResolvedBasis::NoCommit
        );

        std::fs::write(root.path().join("file.txt"), "one\n").unwrap();
        run_git(root.path(), ["add", "file.txt"]);
        run_git(root.path(), ["commit", "-qm", "root"]);
        let first = head_oid(root.path());
        assert_eq!(
            resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
            ResolvedBasis::Ready(DiffBasis::CommitToParent {
                oid: first.clone(),
                parent: None,
            })
        );

        std::fs::write(root.path().join("file.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "second"]);
        assert_eq!(
            resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
            ResolvedBasis::Ready(DiffBasis::CommitToParent {
                oid: head_oid(root.path()),
                parent: Some(first),
            })
        );
    }

    /// With no default base the card says so and offers the choice; picking one
    /// installs that comparison and clears the outstanding resolution.
    #[test]
    fn an_unresolvable_branch_base_offers_the_choice_instead_of_guessing() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        let candidate = ValidatedRefName::parse("refs/remotes/origin/main").unwrap();
        runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::Branch));

        let _ = runtime.update(RepositoryAction::DiffBasisResolved {
            identity,
            choice: CommittedChoice::Branch,
            result: Ok(ResolvedBasis::NeedsBase {
                candidates: vec![candidate.clone()],
            }),
        });
        assert_eq!(
            runtime.basis_resolution,
            Some(BasisResolution::NeedsBase(vec![candidate.clone()]))
        );
        let context = runtime.inspector_context();
        assert_eq!(context.diff_state, Some("Choose a base"));
        assert_eq!(
            runtime.diff_basis,
            DiffBasis::IndexToWorktree,
            "no base may be guessed"
        );
        assert!(!context.can_retry_diff);
        let _ = diff_surface(card, &runtime);

        // The same surface with nothing to offer still says what is wrong.
        runtime.basis_resolution = Some(BasisResolution::NeedsBase(Vec::new()));
        let _ = diff_surface(card, &runtime);
        assert_eq!(
            runtime.inspector_context().diff_state,
            Some("Choose a base")
        );

        runtime.basis_resolution = Some(BasisResolution::NeedsBase(vec![candidate.clone()]));
        let basis = DiffBasis::MergeBaseToHead {
            base_ref: candidate,
        };
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: basis.clone(),
        });
        assert_eq!(runtime.diff_basis, basis);
        assert!(runtime.basis_resolution.is_none());
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
    }

    /// A committed comparison does not depend on local status: a file with no
    /// staged half can still be compared against committed history, and
    /// choosing that comparison is what starts reading it.
    #[test]
    fn a_committed_basis_is_offered_for_a_file_with_no_staged_half() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let selected = runtime
            .selected
            .as_mut()
            .expect("the fixture selects a file");
        selected.staged = false;
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        let branch = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        };

        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: DiffBasis::HeadToIndex,
        });
        assert_eq!(
            runtime.diff_basis,
            DiffBasis::IndexToWorktree,
            "a file with no staged half has no staged comparison"
        );

        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: branch.clone(),
        });
        assert_eq!(runtime.diff_basis, branch);
        assert!(matches!(
            runtime.diff_state(&branch),
            Some(DiffState::Loading { .. })
        ));
    }

    /// A file with no local change at all still has a diff surface, and still
    /// reaches the committed comparisons — which is the only way to review a
    /// file the branch changed and the worktree has not.
    #[test]
    fn a_file_with_no_local_change_still_reaches_a_committed_comparison() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let selected = runtime
            .selected
            .as_mut()
            .expect("the fixture selects a file");
        selected.staged = false;
        selected.unstaged = false;
        selected.status = None;
        runtime.source_surface = SourceSurface::Editor;
        runtime.diff_basis = DiffBasis::HeadToIndex;

        let _ = runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true });
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
        assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_none());
        assert!(!runtime.can_show_diff());

        let commit = DiffBasis::CommitToParent {
            oid: fixture_oid(4),
            parent: Some(fixture_oid(5)),
        };
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: commit.clone(),
        });
        assert_eq!(runtime.diff_basis, commit);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert!(runtime.can_show_diff());
        assert!(matches!(
            runtime.diff_state(&commit),
            Some(DiffState::Loading { .. })
        ));

        // With no file selected there is no diff surface to open.
        runtime.selected = None;
        runtime.source_surface = SourceSurface::Editor;
        let _ = runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true });
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
    }

    /// A committed comparison that does not resolve replaces the body of the
    /// diff surface. The control that chose it must survive that, or the user
    /// is stranded in a basis with their own diff still cached behind it.
    #[test]
    fn an_unresolved_comparison_still_offers_the_way_back() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Ready(staged_text_diff(model_with(1))),
        );
        runtime.source_surface = SourceSurface::Diff;

        for resolution in [
            BasisResolution::Resolving(CommittedChoice::Branch),
            // No candidates at all, so even the ref selector offers nothing.
            BasisResolution::NeedsBase(Vec::new()),
            BasisResolution::Unavailable("This branch has no commits yet".into()),
        ] {
            runtime.basis_resolution = Some(resolution.clone());
            // The diff the user was reading is still cached, and the basis
            // never moved - so changing basis back is a real escape.
            assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
            assert!(matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Ready(RenderedDiff::Text { .. }))
            ));
            let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::IndexToWorktree,
            });
            assert!(runtime.basis_resolution.is_none());
        }
    }

    /// A committed basis is named by a commit id, so the per-basis diff cache
    /// has an open keyspace where the two working bases were a closed one.
    /// Stepping through history must not retain one diff model per commit
    /// visited, and the eviction must never take the comparison on screen, a
    /// working basis, or resurrect an abandoned slot.
    #[test]
    fn stepping_through_commits_does_not_retain_a_diff_per_commit() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        // Previously viewed working comparisons remain cached.
        for basis in [DiffBasis::HeadToIndex, DiffBasis::IndexToWorktree] {
            runtime.set_diff_state(basis, DiffState::Ready(staged_text_diff(model_with(1))));
        }
        let commit = |step: u8| DiffBasis::CommitToParent {
            oid: fixture_oid(step * 2),
            parent: Some(fixture_oid(step * 2 + 1)),
        };

        const STEPS: u8 = 12;
        for step in 0..STEPS {
            let basis = commit(step);
            let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
                card,
                basis: basis.clone(),
            });
            drop(runtime.update(RepositoryAction::DiffLoaded {
                identity: identity.clone(),
                basis: basis.clone(),
                request: runtime.next_diff_request,
                content: staged_text_diff(model_with(1)),
            }));
            assert_eq!(runtime.diff_basis, basis, "step {step} was not applied");
            assert!(
                runtime
                    .diffs
                    .iter()
                    .filter(|(basis, _)| matches!(basis_choice(basis), BasisChoice::Committed(_)))
                    .count()
                    <= MAX_RETAINED_COMMITTED_DIFFS,
                "the cache exceeded its cap at step {step}"
            );
        }

        // Neither working basis was evicted to make room.
        assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_some());
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_some());
        // The comparison on screen survived, and so did the ones just before
        // it, which is what makes stepping back instant.
        let kept = usize::from(STEPS) - MAX_RETAINED_COMMITTED_DIFFS;
        for step in 0..STEPS {
            let present = runtime.diff_state(&commit(step)).is_some();
            assert_eq!(
                present,
                usize::from(step) >= kept,
                "step {step} retention is wrong"
            );
        }

        // A duplicate answer for an evicted slot must not recreate it.
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: commit(0),
            request: 1,
            content: staged_text_diff(model_with(9)),
        });
        assert!(runtime.diff_state(&commit(0)).is_none());
        assert_eq!(runtime.diff_basis, commit(STEPS - 1));
    }

    /// A commit diff and unstaged work must not read the same. The heading
    /// names the comparison, and the inspector has its own static word for it.
    #[test]
    fn every_basis_names_itself_in_the_chrome() {
        let commit = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        };
        let root_commit = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: None,
        };
        let branch = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
        };
        let labels = [
            &DiffBasis::IndexToWorktree,
            &DiffBasis::HeadToIndex,
            &branch,
            &commit,
            &root_commit,
        ]
        .map(basis_label);
        assert_eq!(
            labels,
            [
                "Unstaged changes".to_owned(),
                "Staged changes".to_owned(),
                "Branch vs origin/main".to_owned(),
                format!("Commit {}", fixture_oid(1).short()),
                format!("Root commit {}", fixture_oid(1).short()),
            ]
        );
        let mut distinct = labels.to_vec();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), labels.len(), "two bases share their chrome");

        assert_eq!(basis_choice_label(&DiffBasis::IndexToWorktree), "Unstaged");
        assert_eq!(basis_choice_label(&DiffBasis::HeadToIndex), "Staged");
        assert_eq!(basis_choice_label(&branch), "Branch");
        assert_eq!(basis_choice_label(&commit), "Commit");
        assert_eq!(basis_choice_label(&root_commit), "Commit");
    }

    /// A comparison with no text rows reports what it actually is, including
    /// the states #45's loader could not produce. A conflicted path reports as
    /// conflicted and has no two-sided model at all.
    #[test]
    fn a_comparison_without_rows_reports_what_it_is() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.diff_basis = DiffBasis::IndexToWorktree;
        let text = || DiffSide::Text {
            text: "a\n".into(),
            bytes: 2,
        };

        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Ready(RenderedDiff::NoText(DiffContent::Unmerged {
                base: text(),
                ours: text(),
                theirs: text(),
                worktree: DiffSide::Absent,
            })),
        );
        assert_eq!(runtime.inspector_context().diff_state, Some("Conflicted"));
        assert!(
            runtime.diff_model().is_none(),
            "a conflict has no two-sided model"
        );
        let _ = diff_surface(card, &runtime);

        for (new, expected) in [
            (DiffSide::Binary { bytes: 3 }, "Binary"),
            (DiffSide::InvalidUtf8 { bytes: 1 }, "Invalid UTF-8"),
            (DiffSide::Oversize { bytes: 1 << 30 }, "Too large"),
            (DiffSide::NotRegularFile, "Not a regular file"),
            (DiffSide::Submodule { oid: None }, "Submodule"),
            (
                DiffSide::Submodule {
                    oid: Some(fixture_oid(3)),
                },
                "Submodule",
            ),
            (
                DiffSide::Unreadable {
                    reason: "permission denied".into(),
                },
                "Unreadable",
            ),
            (
                DiffSide::Symlink {
                    target: PathBuf::from("elsewhere"),
                },
                "Symlink",
            ),
        ] {
            runtime.set_diff_state(
                DiffBasis::IndexToWorktree,
                DiffState::Ready(RenderedDiff::NoText(DiffContent::Comparison {
                    old: text(),
                    new,
                    old_label: "Index".into(),
                    new_label: "Working tree".into(),
                })),
            );
            assert_eq!(runtime.inspector_context().diff_state, Some(expected));
            assert!(runtime.diff_model().is_none());
            // The surface renders the fact instead of panicking on a state
            // #45's reduced content type had no variant for.
            let _ = diff_surface(card, &runtime);
        }
    }

    /// A runtime with one selected file that has both working halves and the
    /// diff surface showing, which is what every basis fence needs to be
    /// observable.
    fn diff_runtime(card: CardId) -> (RepositoryRuntime, AsyncIdentity) {
        let mut runtime = RepositoryRuntime::new(RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        });
        let path = PathBuf::from("notes.txt");
        let file = RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: true,
            unstaged: true,
            status: Some("MM".into()),
        };
        runtime.generation = 1;
        runtime.files = FileListState::Ready(vec![file.clone()]);
        runtime.selected = Some(file);
        runtime.source_surface = SourceSurface::Diff;
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path,
            generation: 1,
        };
        (runtime, identity)
    }

    /// A ready text diff carrying the labels the staged basis produces.
    fn staged_text_diff(model: DiffModel) -> RenderedDiff {
        RenderedDiff::Text {
            model,
            old_label: "HEAD".into(),
            new_label: "Index".into(),
        }
    }

    fn model_with(additions: u32) -> DiffModel {
        DiffModel {
            inline_rows: Vec::new(),
            split_rows: Vec::new(),
            inline_change_starts: Vec::new(),
            split_change_starts: Vec::new(),
            additions,
            deletions: 0,
            max_line_width: [0; 2],
            syntax: DiffSyntax::default(),
        }
    }

    /// Every node in the widget tree an element actually builds.
    fn widget_nodes(element: &Element<'_, RepositoryAction>) -> usize {
        fn count(tree: &iced::advanced::widget::Tree) -> usize {
            1 + tree.children.iter().map(count).sum::<usize>()
        }
        count(&iced::advanced::widget::Tree::new(element))
    }

    fn fixture_oid(seed: u8) -> Oid {
        Oid::parse_object(format!("{}{seed:02x}", "ab".repeat(19))).expect("a valid object id")
    }

    fn head_oid(root: &Path) -> Oid {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success(), "git rev-parse HEAD failed");
        Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    /// An unborn branch has no `HEAD` for a merge base to be taken against —
    /// even when the remote's own `HEAD` names a base ref perfectly well, which
    /// is what makes this different from the commit basis's own empty case.
    #[test]
    fn an_unborn_branch_reports_no_commit_for_the_branch_basis_too() {
        let origin = tempdir().unwrap();
        git_fixture(origin.path());
        std::fs::write(origin.path().join("seed.txt"), "seed\n").unwrap();
        run_git(origin.path(), ["add", "seed.txt"]);
        run_git(origin.path(), ["commit", "-qm", "seed"]);

        let fetched = tempdir().unwrap();
        git_fixture(fetched.path());
        run_git(
            fetched.path(),
            [
                "remote",
                "add",
                "origin",
                origin.path().to_str().expect("a UTF-8 fixture path"),
            ],
        );
        run_git(fetched.path(), ["fetch", "-q", "origin"]);
        run_git(fetched.path(), ["remote", "set-head", "origin", "-a"]);

        // The base ref resolves: this repository really does know what the
        // remote's default branch is.
        let snapshot = RepositoryService::system("git")
            .snapshot(fetched.path(), &CancellationToken::new())
            .expect("snapshot");
        assert!(snapshot.default_branch_base().is_some());
        // Nothing has been committed here, so there is no HEAD to compare that
        // base with, and both committed choices must say so rather than
        // installing a comparison that dies in Git.
        assert_eq!(
            resolve_basis(fetched.path(), CommittedChoice::Branch),
            Ok(ResolvedBasis::NoCommit)
        );
        assert_eq!(
            resolve_basis(fetched.path(), CommittedChoice::LastCommit),
            Ok(ResolvedBasis::NoCommit)
        );
    }

    /// An empty repository on a named initial branch, because the branch tests
    /// name `main` and Git's default depends on the host's configuration.
    fn git_fixture(root: &Path) {
        run_git(root, ["init", "-q", "-b", "main"]);
        run_git(root, ["config", "user.name", "Fixture"]);
        run_git(root, ["config", "user.email", "fixture@example.invalid"]);
    }

    fn run_git<const N: usize>(root: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// A buffer whose tokens are exactly the ones the smoke run found
    /// indistinguishable: a comment, a keyword, a string, and a macro call.
    const HIGHLIGHTABLE: &str = "// what main does\nfn main() {\n    let hello = \"world\";\n    println!(\"{hello}\");\n}\n";

    /// A headless `cosmic::Renderer`. With no graphics device it falls back to
    /// `tiny-skia`, which needs neither a device nor a surface and which can
    /// hand back the pixels it drew.
    ///
    /// One per render: the renderer accumulates the layers it is drawn into.
    fn headless_renderer() -> cosmic::Renderer {
        use cosmic::iced::Pixels;
        use cosmic::iced::advanced::renderer::Headless;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("executor")
            .block_on(cosmic::Renderer::new(
                Font::MONOSPACE,
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .expect("a headless renderer")
    }

    /// The RGBA pixels the card's own `view` rasterises, at `size` logical
    /// pixels on the dark COSMIC palette.
    fn rendered_card(card: CardId, runtime: &RepositoryRuntime, size: (u32, u32)) -> Vec<u8> {
        use cosmic::iced::advanced::renderer::{Headless, Style};
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        use cosmic::iced::{Color, Rectangle, Size};

        let (width, height) = size;
        let mut renderer = headless_renderer();
        let mut element = view(card, runtime);
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(width as f32, height as f32));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        renderer.screenshot(Size::new(width, height), 1.0, Color::BLACK)
    }

    #[test]
    fn opening_an_image_reconciles_the_mounted_file_list_and_renders_pixels() {
        use cosmic::iced::advanced::renderer::{Headless, Style};
        use cosmic::iced::{Color, Rectangle, Size};
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::new(RepositoryView {
            project: ProjectId::new(),
            repository: "/repo/.git".into(),
            worktree: "/repo".into(),
        });
        let file = RepositoryFile {
            path: "picture.png".into(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some("??".into()),
        };
        runtime.files = FileListState::Ready(vec![file.clone()]);
        let mut tree = Tree::new(view(card, &runtime).as_widget());
        let mut renderer = headless_renderer();
        let viewport = Rectangle::with_size(Size::new(320.0, 240.0));
        let _ = runtime.update(RepositoryAction::OpenFile {
            card,
            path: file.path,
        });
        {
            let mut element = view(card, &runtime);
            tree.diff(element.as_widget_mut());
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
        }
        let _ = runtime.update(RepositoryAction::FileLoaded {
            identity: AsyncIdentity {
                card,
                worktree: runtime.descriptor.worktree.clone(),
                path: "picture.png".into(),
                generation: runtime.generation,
            },
            result: Ok(FileContent::Image {
                width: 1,
                height: 1,
                rgba: vec![200, 20, 40, 255].into(),
            }),
        });
        let mut element = view(card, &runtime);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &Style::default(),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &viewport,
        );
        let pixels = renderer.screenshot(Size::new(320, 240), 1.0, Color::BLACK);
        let center = (120 * 320 + 160) * 4;
        assert_eq!(&pixels[center..center + 4], &[200, 20, 40, 255]);
    }

    /// The colour families a render puts on screen, as (brightest channel,
    /// dimmest channel) pairs.
    ///
    /// Counting *distinct colours* would count anti-aliasing: a single
    /// coloured glyph fades into the background over a dozen shades. Which
    /// channel leads is what survives that fade, so this counts token colours
    /// rather than blend steps. Achromatic pixels — the background, the
    /// chrome's greys, and unhighlighted text — are excluded outright, and a
    /// family needs a real population to count so a stray fringe pixel cannot
    /// invent one.
    fn colour_families(pixels: &[u8]) -> BTreeSet<(usize, usize)> {
        const MIN_CHROMA: u8 = 32;
        const MIN_PIXELS: usize = 16;

        let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
        for pixel in pixels.chunks_exact(4) {
            let rgb = [pixel[0], pixel[1], pixel[2]];
            let high = rgb.iter().copied().max().expect("three channels");
            let low = rgb.iter().copied().min().expect("three channels");
            if high - low < MIN_CHROMA {
                continue;
            }
            let brightest = rgb
                .iter()
                .enumerate()
                .max_by_key(|&(_, v)| v)
                .expect("max")
                .0;
            let dimmest = rgb
                .iter()
                .enumerate()
                .min_by_key(|&(_, v)| v)
                .expect("min")
                .0;
            *counts.entry((brightest, dimmest)).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|&(_, count)| count >= MIN_PIXELS)
            .map(|(family, _)| family)
            .collect()
    }

    /// GitHub issue #1 asks for editable *syntax-highlighted* buffers, and the
    /// editor that first landed drew every token in one colour. This renders
    /// the same bytes twice, changing nothing but the name they were opened
    /// under, and reads the pixels back.
    #[test]
    fn the_editable_buffer_is_highlighted_by_the_grammar_its_name_names() {
        let card = CardId::new();
        let render = |name: &str| {
            let (runtime, _) = RepositoryRuntime::open_fixture(card, name, HIGHLIGHTABLE);
            rendered_card(card, &runtime, (520, 200))
        };
        // `.txt` is plain text to every grammar set, so this is the same card
        // with the highlighting switched off by the only input that chooses it.
        let rust = render("main.rs");
        let plain = render("main.txt");

        assert_eq!(rust.len(), plain.len(), "both renders are one size");
        assert!(
            rust.iter().any(|&channel| channel != 0),
            "the harness drew nothing at all, so every comparison below would pass vacuously"
        );

        // Not a comparison of the two frames byte for byte: the header prints
        // the file name, so they differ on a row that has nothing to do with
        // highlighting, and such a check would pass with the highlighter
        // deleted. Colour is what separates them. Every one of this card's own
        // greys and its single chromatic accent is drawn identically in both,
        // so a colour family the Rust render has and the plain-text one lacks
        // can only have come from a grammar.
        let highlighted = colour_families(&rust);
        let unhighlighted = colour_families(&plain);
        assert!(
            unhighlighted.is_subset(&highlighted),
            "the plain render has a colour the highlighted one lacks, so these two renders differ \
             in more than the grammar and the comparison below proves nothing: {unhighlighted:?} \
             against {highlighted:?}"
        );
        assert!(
            highlighted.len() >= unhighlighted.len() + 2,
            "a keyword, a string and a comment must not all read the same: the highlighted render \
             put {} colour families on screen against the plain render's {} — {highlighted:?} \
             against {unhighlighted:?}",
            highlighted.len(),
            unhighlighted.len()
        );
    }

    /// Both inputs the highlighter takes, checked where they are decided.
    #[test]
    fn the_grammar_follows_the_file_name_and_the_scheme_follows_the_palette() {
        use cosmic::iced::advanced::text::Highlighter as _;

        /// Whether a token resolves to a grammar that actually colours
        /// `line`, rather than to the plain-text fallback that colours nothing.
        fn colours(token: &str, line: &str) -> bool {
            let settings = highlighter::Settings {
                theme: highlighter::Theme::Base16Ocean,
                token: token.to_owned(),
            };
            highlighter::Highlighter::new(&settings)
                .highlight_line(line)
                .any(|(_, highlight)| highlight.color().is_some())
        }

        for (path, line) in [
            ("src/main.rs", "fn main() {}"),
            ("README.MD", "# heading"),
            ("Makefile", "all: build"),
            ("deploy/Dockerfile", "FROM scratch"),
            ("deploy/Dockerfile.dev", "FROM scratch"),
            ("src/kernel.cu", "__global__ void kernel() { return; }"),
        ] {
            let settings = highlighter_settings(Path::new(path), true);
            assert!(
                colours(&settings.token, line),
                "{path} must resolve to a grammar rather than render as flat plain text"
            );
        }

        assert!(
            !colours("no-such-grammar", "fn main() {}"),
            "an unrecognised token must fall back to plain text rather than colour arbitrarily; \
             without that the check above cannot tell a grammar from the fallback"
        );

        // A scheme picks its foregrounds for a background of its own kind, so
        // reading a dark one off a light surface is what makes highlighted
        // text illegible.
        assert!(
            highlighter_settings(Path::new("main.rs"), true)
                .theme
                .is_dark(),
            "a dark application must highlight with a dark scheme"
        );
        assert!(
            !highlighter_settings(Path::new("main.rs"), false)
                .theme
                .is_dark(),
            "a light application must highlight with a light scheme"
        );
    }

    /// Moving the caret is not enough to *show* a jump: an unfocused
    /// `text_editor` draws no caret at all, which is what made
    /// jump-to-location look like it did nothing. The affordance the
    /// application chains after a caret move has to name a target that the
    /// view really carries, on the frame after the one that asked for it.
    #[test]
    fn the_editable_buffer_offers_a_stable_focus_target_the_view_really_carries() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::widget::Operation as _;
        use cosmic::iced::advanced::widget::operation::{self, Outcome};
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};

        let card = CardId::new();
        let expected = editor_focus_id(card).to_string();
        assert_eq!(
            editor_focus_id(card).to_string(),
            expected,
            "the id must be derived from the card rather than minted: a focus operation is \
             delivered on a later frame than the one that asked for it, and an id that changed in \
             between would address nothing"
        );
        assert_ne!(
            editor_focus_id(CardId::new()).to_string(),
            expected,
            "two repository cards must not contend for one focus target"
        );

        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "main.rs", HIGHLIGHTABLE);
        assert_eq!(
            runtime.editor_focus_target(card).map(|id| id.to_string()),
            Some(expected.clone()),
            "an open editable buffer is exactly the surface a jump lands in"
        );

        // The id is only worth having if the widget tree answers to it, so ask
        // the real `view` to focus it and then ask it back what is focused.
        let landed = {
            let renderer = headless_renderer();
            let mut element = view(card, &runtime);
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(520.0, 200.0)),
            );
            let mut focus = operation::focusable::focus::<()>(editor_focus_id(card));
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
            // `Widget::operate` only takes an `Operation<()>`; `black_box` is
            // how iced runs an operation that reports an outcome through one.
            let mut focused = operation::focusable::find_focused();
            let mut probe = operation::black_box::<_, ()>(&mut focused);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
            drop(probe);
            match focused.finish() {
                Outcome::Some(id) => Some(id.to_string()),
                Outcome::None | Outcome::Chain(_) => None,
            }
        };
        assert_eq!(
            landed,
            Some(expected.clone()),
            "the focus operation reached no focusable widget under that id: the editor the view \
             builds does not carry it"
        );

        // A surface that builds no editor has nothing to focus, and a focus
        // operation aimed at it would be delivered to no widget at all.
        if let DocumentState::Editable(document) = &mut runtime.document {
            document.markdown = true;
        }
        runtime.markdown_mode = MarkdownMode::Preview;
        assert!(
            runtime.editor_focus_target(card).is_none(),
            "a Markdown preview renders the buffer, it does not edit it"
        );

        runtime.markdown_mode = MarkdownMode::Edit;
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert!(
            runtime.editor_focus_target(card).is_none(),
            "the diff surface replaces the editor rather than sitting beside it"
        );

        drop(runtime.update(RepositoryAction::BackToFiles { card }));
        assert!(
            runtime.editor_focus_target(card).is_none(),
            "the file list has no buffer for a caret to be in"
        );
    }
}
