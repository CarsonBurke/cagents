//! Native repository-card state, asynchronous file review tasks, and focused UI.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

#[path = "editor_widget.rs"]
pub(crate) mod text_editor;

mod diff_actions;
mod diff_layout;
mod preview;
use diff_layout::{DiffLayout, DisplayRow, RowOffsets};

pub use text_editor::{Action as EditorAction, Edit as EditorEdit};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::{
    Element,
    iced::{self, Alignment, Font, Length, Task, highlighter},
    widget::{self, markdown},
};

#[cfg(test)]
use crate::diff_view::load_repository_diff;

use crate::{
    design::space,
    diff_view::{
        DIFF_FONT_SIZE, DIFF_LINE_HEIGHT, DiffMode, DiffModel, DiffRowKind, DiffSyntax,
        InlineDiffRow, RenderedDiff, SplitDiffRow, SyntaxSpan,
        build_document_diff_sources_with_font, build_line_changes, highlighter_settings,
        load_repository_diff_with_options_and_font,
    },
    editor::{
        FileContent, FileIoError, LinkRefusal, LoadedFile, OversizeKind, ReadOnlyReason,
        SaveDurability, SaveOutcome, load_file, save_file, validate_repo_path,
    },
    editor_history::{EditBuffer, EditKind, History, Replay, TextChange},
    lsp::{
        LspCompletionEdit, LspCompletionItem, LspDocumentId, LspPosition, LspRange, LspTextEdit,
    },
    model::{CardId, RepositoryView},
    repository::{
        CancellationToken, DiffBasis, DiffContent, DiffSide, HistoryRequest, HistoryRevision,
        MAX_DIFF_SIDE_BYTES, RepoPath, RepositoryService, RepositorySnapshot, ValidatedRefName,
    },
    ui_controls::{Icon, icon_button},
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepositoryFile {
    pub path: PathBuf,
    pub staged_old_path: Option<PathBuf>,
    pub unstaged_old_path: Option<PathBuf>,
    pub staged: bool,
    pub unstaged: bool,
    pub status: Option<String>,
    pub directory: bool,
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
            // A pull request comparison is installed by a review surface with
            // a synthetic file, and it must survive every status refresh the
            // way the other immutable bases do.
            DiffBasis::MergeBaseToHead { .. }
            | DiffBasis::CommitToParent { .. }
            | DiffBasis::PullRequest { .. } => true,
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
            DiffBasis::MergeBaseToHead { .. }
            | DiffBasis::CommitToParent { .. }
            | DiffBasis::PullRequest { .. } => None,
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
    /// The branch has no commits, so no committed comparison can resolve.
    NoCommits,
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
        DiffBasis::MergeBaseToHead { base_ref, .. } => {
            format!("Branch vs {}", base_ref.short_name())
        }
        DiffBasis::CommitToParent {
            oid,
            parent: Some(_),
        } => format!("Commit {}", oid.short()),
        DiffBasis::CommitToParent { oid, parent: None } => {
            format!("Root commit {}", oid.short())
        }
        DiffBasis::PullRequest {
            number, base_ref, ..
        } => format!("#{number} vs {}", base_ref.short_name()),
    }
}

/// The basis in one static word, for the inspector projection, which holds
/// `&'static str` and so cannot carry a ref name or a commit id.
fn basis_choice_label(basis: &DiffBasis) -> &'static str {
    match basis_choice(basis) {
        Some(BasisChoice::Staged) => "Staged",
        Some(BasisChoice::Unstaged) => "Unstaged",
        Some(BasisChoice::Committed(choice)) => choice.label(),
        None => "Pull request",
    }
}

/// Which control choice a basis was made from, so the widget can never show a
/// choice the runtime has not made. A pull request comparison is installed by
/// a review surface, not chosen here, so no control claims it.
fn basis_choice(basis: &DiffBasis) -> Option<BasisChoice> {
    match basis {
        DiffBasis::HeadToIndex => Some(BasisChoice::Staged),
        DiffBasis::IndexToWorktree => Some(BasisChoice::Unstaged),
        DiffBasis::MergeBaseToHead { .. } => Some(BasisChoice::Committed(CommittedChoice::Branch)),
        DiffBasis::CommitToParent { .. } => {
            Some(BasisChoice::Committed(CommittedChoice::LastCommit))
        }
        DiffBasis::PullRequest { .. } => None,
    }
}

/// Whether a basis lives in the retained committed-diff cache: everything
/// immutable, whichever control (if any) made it.
fn is_committed_choice(basis: &DiffBasis) -> bool {
    basis.is_immutable()
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
pub enum EditorLspAction {
    Complete,
    CompleteAt {
        doc: LspDocumentId,
        version: i32,
        at: LspPosition,
    },
    Hover(Option<LspPosition>),
    Definition(Option<LspPosition>),
    References,
    Diagnostics,
    Diagnostic(bool),
    Choose {
        index: usize,
        label: String,
    },
    Next(bool),
    Dismiss,
}

#[derive(Default)]
struct EditorLanguageState {
    diagnostics: Vec<crate::lsp::LspDiagnostic>,
    selected_diagnostic: Option<usize>,
    marks: Vec<text_editor::Diagnostic>,
    completions: Vec<LspCompletionItem>,
    selected: usize,
    hover: Vec<markdown::Item>,
    hover_at: Option<text_editor::Position>,
}

#[derive(Debug, Clone)]
pub enum RepositoryAction {
    Review {
        card: CardId,
        action: crate::pull_request_review::Action,
    },
    ReviewLine {
        identity: AsyncIdentity,
        basis: DiffBasis,
        action: crate::pull_request_review::Action,
        extend: bool,
    },
    DiffAnnotations {
        identity: AsyncIdentity,
        basis: DiffBasis,
        mode: DiffMode,
        layout_revision: u64,
        review_revision: (u64, u64),
        leading: f32,
        rows: Vec<(usize, f32)>,
    },
    RefreshFiles {
        card: CardId,
    },
    ListDirectory {
        card: CardId,
        path: PathBuf,
    },
    DirectoryListed {
        identity: AsyncIdentity,
        result: Result<Vec<RepositoryFile>, String>,
    },
    SearchDirectory {
        card: CardId,
        query: String,
    },
    DirectorySearchLoaded {
        card: CardId,
        generation: u64,
        query: String,
        result: Result<(Vec<RepositoryFile>, usize), String>,
    },
    NewTerminal {
        card: CardId,
    },
    FilesLoaded {
        identity: AsyncIdentity,
        result: Result<Vec<RepositoryFile>, String>,
    },
    /// A file-list read that found the worktree exactly as this card already
    /// has it.
    ///
    /// Carries no list on purpose: rebuilding one would allocate a `PathBuf`
    /// per file and invalidate the card's view for nothing. A variant of its
    /// own rather than an empty [`RepositoryAction::FilesLoaded`], because
    /// an empty list legitimately means "this repository has no files".
    FileListUnchanged {
        identity: AsyncIdentity,
    },
    /// The Markdown preview's debounce elapsed; re-parse if an edit
    /// invalidated the items.
    MarkdownDue {
        card: CardId,
        ticket: u64,
    },
    DiagramsRendered {
        identity: AsyncIdentity,
        revision: u64,
        tone_generation: u64,
        result: Vec<(blake3::Hash, preview::Diagram)>,
    },
    OpenFile {
        card: CardId,
        path: PathBuf,
    },
    /// A validated path selected in the browser, including ignored files
    /// absent from the repository's source-control listing.
    OpenFileFromBrowser {
        card: CardId,
        path: RepoPath,
    },
    OpenWorkingDiff {
        card: CardId,
        file: RepositoryFile,
        basis: DiffBasis,
    },
    OpenCommittedFile {
        card: CardId,
        path: RepoPath,
        old_path: Option<RepoPath>,
        basis: DiffBasis,
    },
    SelectBranchBase {
        card: CardId,
        base_ref: ValidatedRefName,
    },
    FileLoaded {
        identity: AsyncIdentity,
        result: Result<LoadedFile, FileIoError>,
    },
    /// Turn to a page of the open document, or re-render the current one after
    /// the reader changes how its colors are adapted or how large it is drawn.
    PdfPage {
        card: CardId,
        page: usize,
        tone: crate::editor::PdfTone,
        zoom: crate::editor::PdfZoom,
    },
    /// The reader chose a tone for the open document, which stops the shell's
    /// theme deciding it. Separate from `PdfPage` so the state can tell a
    /// deliberate choice from the tone a page turn or a theme change carries.
    PdfTone {
        card: CardId,
        tone: crate::editor::PdfTone,
    },
    /// The reader changed how large the page is drawn.
    PdfZoom {
        card: CardId,
        zoom: crate::editor::PdfZoom,
    },
    PdfPageRendered {
        card: CardId,
        request: u64,
        result: Result<crate::editor::DocumentPage, String>,
    },
    DiffLoaded {
        identity: AsyncIdentity,
        basis: DiffBasis,
        request: u64,
        content: RenderedDiff,
    },
    WorkingDiffDue {
        card: CardId,
        ticket: u64,
    },
    WorkingDiffLoaded {
        identity: AsyncIdentity,
        revision: u64,
        ticket: u64,
        before: Arc<str>,
        result: Result<Option<WorkingDiffOutput>, String>,
    },
    EditorAction {
        card: CardId,
        action: text_editor::Action,
    },
    Language {
        card: CardId,
        action: EditorLspAction,
    },
    Clipboard {
        card: CardId,
        op: ClipboardOp,
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
    /// Reverts the newest step of the open buffer's edit history.
    Undo {
        card: CardId,
    },
    /// Re-applies the newest undone step.
    Redo {
        card: CardId,
    },
    BackToFiles {
        card: CardId,
    },
    OpenExternal {
        card: CardId,
        editor_id: String,
    },
    OpenWorkingFileLocation {
        identity: AsyncIdentity,
        line: usize,
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
    RevertFile {
        card: CardId,
    },
    ToggleDiffWhitespace {
        card: CardId,
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
    DiffViewport {
        card: CardId,
        offset: f32,
        height: f32,
        horizontal: f32,
        user_initiated: bool,
    },
    PreviewViewport {
        card: CardId,
        offset: f32,
    },
    PreviousChange {
        card: CardId,
    },
    NextChange {
        card: CardId,
    },
    /// Scrolls the diff to its first row, where file-level annotations sit.
    RevealDiffTop {
        card: CardId,
    },
    /// Reveals rows of a folded run of unchanged lines. `run` is the rows
    /// that run held in `mode` when the fold was drawn: a fold index names a
    /// run of one model, and a rebuilt model may number its runs differently.
    /// Anything else may change in between; a second click lands as a second
    /// reveal, as a reader clicking twice expects.
    ExpandDiffFold {
        identity: AsyncIdentity,
        basis: DiffBasis,
        mode: DiffMode,
        fold: usize,
        run: Range<usize>,
        expansion: FoldExpansion,
    },
    MarkdownLink {
        card: CardId,
        uri: markdown::Uri,
    },
    /// Mounts the find bar over the open buffer and hands it the keyboard.
    ///
    /// Find keeps the buffer editable; clicks return keyboard focus to it.
    OpenFind {
        card: CardId,
    },
    CloseFind {
        card: CardId,
    },
    FindQueryChanged {
        card: CardId,
        query: String,
    },
    FindOptionsChanged {
        card: CardId,
        option: crate::ui_controls::FindOption,
        enabled: bool,
    },
    FindSelectionChanged {
        card: CardId,
        enabled: bool,
    },
    PreserveCaseChanged {
        card: CardId,
        enabled: bool,
    },
    FindNext {
        card: CardId,
    },
    FindPrevious {
        card: CardId,
    },
    FindInputFocus {
        card: CardId,
        focused: bool,
    },
    FindLoaded {
        card: CardId,
        ticket: uuid::Uuid,
        result: Result<Option<Vec<(text_editor::Position, text_editor::Position)>>, String>,
    },
    ReplaceQueryChanged {
        card: CardId,
        query: String,
    },
    ToggleReplace {
        card: CardId,
    },
    ReplaceMatch {
        card: CardId,
        all: bool,
    },
    ToggleToolbarOverflow {
        card: CardId,
    },
    CloseToolbarOverflow {
        card: CardId,
    },
    SelectDiffRow {
        identity: AsyncIdentity,
        basis: DiffBasis,
        mode: DiffMode,
        revision: u64,
        row: usize,
        side: crate::diff_interaction::Side,
        extend: bool,
        context: bool,
        drag: bool,
    },
    EndDiffSelection {
        card: CardId,
    },
    MoveDiffSelection {
        card: CardId,
        delta: i32,
        extend: bool,
    },
    ClearDiffSelection {
        card: CardId,
    },
    DiffCommand {
        card: CardId,
        generation: u64,
        revision: u64,
        mode: DiffMode,
        sources: [(usize, usize); 2],
        command: crate::diff_interaction::Command,
    },
    DiffFindLoaded {
        card: CardId,
        ticket: uuid::Uuid,
        result: Result<Option<Vec<crate::diff_interaction::Match>>, String>,
    },
    ZoomFont {
        card: CardId,
        delta: i8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOp {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl RepositoryAction {
    pub fn card(&self) -> CardId {
        match self {
            Self::SelectDiffRow { identity, .. }
            | Self::OpenWorkingFileLocation { identity, .. } => identity.card,
            Self::EndDiffSelection { card }
            | Self::MoveDiffSelection { card, .. }
            | Self::ClearDiffSelection { card }
            | Self::DiffCommand { card, .. }
            | Self::DiffFindLoaded { card, .. } => *card,
            Self::Review { card, .. }
            | Self::RefreshFiles { card }
            | Self::ListDirectory { card, .. }
            | Self::SearchDirectory { card, .. }
            | Self::DirectorySearchLoaded { card, .. }
            | Self::NewTerminal { card }
            | Self::OpenFile { card, .. }
            | Self::OpenFileFromBrowser { card, .. }
            | Self::OpenWorkingDiff { card, .. }
            | Self::OpenCommittedFile { card, .. }
            | Self::SelectBranchBase { card, .. }
            | Self::EditorAction { card, .. }
            | Self::Language { card, .. }
            | Self::Clipboard { card, .. }
            | Self::Save { card, .. }
            | Self::Reload { card }
            | Self::DiscardEdits { card }
            | Self::Undo { card }
            | Self::Redo { card }
            | Self::BackToFiles { card }
            | Self::OpenExternal { card, .. }
            | Self::StageSelected { card, .. }
            | Self::ApplySourceSurface { card, .. }
            | Self::ApplyMarkdownMode { card, .. }
            | Self::ApplyDiffMode { card, .. }
            | Self::ToggleDiffWhitespace { card }
            | Self::RevertFile { card }
            | Self::SelectCommittedBasis { card, .. }
            | Self::ApplyDiffBasis { card, .. }
            | Self::RetryDiff { card }
            | Self::DiffViewport { card, .. }
            | Self::PreviewViewport { card, .. }
            | Self::PreviousChange { card }
            | Self::NextChange { card }
            | Self::RevealDiffTop { card }
            | Self::WorkingDiffDue { card, .. }
            | Self::MarkdownDue { card, .. }
            | Self::MarkdownLink { card, .. }
            | Self::OpenFind { card }
            | Self::CloseFind { card }
            | Self::FindQueryChanged { card, .. }
            | Self::FindOptionsChanged { card, .. }
            | Self::FindSelectionChanged { card, .. }
            | Self::PreserveCaseChanged { card, .. }
            | Self::FindNext { card }
            | Self::FindPrevious { card }
            | Self::FindInputFocus { card, .. }
            | Self::FindLoaded { card, .. }
            | Self::ReplaceQueryChanged { card, .. }
            | Self::ToggleReplace { card }
            | Self::ReplaceMatch { card, .. }
            | Self::ToggleToolbarOverflow { card }
            | Self::CloseToolbarOverflow { card }
            | Self::ZoomFont { card, .. }
            | Self::PdfPage { card, .. }
            | Self::PdfTone { card, .. }
            | Self::PdfZoom { card, .. }
            | Self::PdfPageRendered { card, .. } => *card,
            Self::ReviewLine { identity, .. }
            | Self::DiffAnnotations { identity, .. }
            | Self::ExpandDiffFold { identity, .. }
            | Self::FilesLoaded { identity, .. }
            | Self::FileListUnchanged { identity }
            | Self::DirectoryListed { identity, .. }
            | Self::FileLoaded { identity, .. }
            | Self::DiagramsRendered { identity, .. }
            | Self::DiffLoaded { identity, .. }
            | Self::WorkingDiffLoaded { identity, .. }
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
    /// The selected path: the name the reader opened and the file list rows.
    path: PathBuf,
    /// The file the bytes were read from and are saved to. Differs from
    /// [`EditorDocument::path`] when that is a symbolic link, and everything
    /// that is about the bytes rather than the row — saving, the language
    /// server's document, the syntax the buffer is highlighted with — goes by
    /// this one. See [`LoadedFile::source`].
    source: PathBuf,
    content: text_editor::Content<cosmic::Renderer>,
    saved_text: String,
    saved_history_revision: u64,
    saved_snapshot: text_editor::TextSnapshot,
    disk_hash: blake3::Hash,
    markdown: bool,
    markdown_items: Vec<markdown::Item>,
    /// Whether [`EditorDocument::markdown_items`] still describes the buffer.
    ///
    /// Parsing is the whole document's work and only the preview surface
    /// reads the result, so an edit records staleness and the parse happens
    /// at the debounce boundary — or at the moment the preview is mounted,
    /// whichever comes first.
    markdown_stale: bool,
    preview_text: String,
    diagrams: HashMap<blake3::Hash, preview::Diagram>,
    diagram_pending: Option<u64>,
    diagram_cancel: Option<preview::CancellationGuard>,
    diagram_dark: bool,
    diagram_tone_generation: u64,
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
    /// Undo and redo for this buffer. It lives exactly as long as the buffer:
    /// a document loaded afresh starts a history of its own.
    history: History,
    #[cfg(test)]
    history_time: Option<Instant>,
}

/// What one editor action did to the buffer's text.
#[derive(Debug, Clone, PartialEq)]
enum Applied {
    Unchanged,
    /// The text changed; the protocol edit describing it, or `None` where it
    /// has no protocol range and the server needs the whole document.
    Changed(Option<LspTextEdit>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveTicket {
    id: u64,
    identity: AsyncIdentity,
    revision: u64,
    history_revision: u64,
    snapshot_source: text_editor::TextSnapshot,
    snapshot: String,
}

impl EditorDocument {
    fn new(
        path: PathBuf,
        source: PathBuf,
        text: String,
        disk_hash: blake3::Hash,
        markdown: bool,
    ) -> Self {
        let content = text_editor::Content::with_text(&text);
        let saved_snapshot = content.text_snapshot();
        Self {
            path,
            source,
            content,
            saved_snapshot,
            saved_text: text,
            saved_history_revision: 0,
            disk_hash,
            markdown,
            // Source editing does not consume the preview AST. Parse only
            // when preview is first mounted, alongside its text snapshot.
            markdown_items: Vec::new(),
            markdown_stale: true,
            preview_text: String::new(),
            diagrams: HashMap::new(),
            diagram_pending: None,
            diagram_cancel: None,
            diagram_dark: cosmic::theme::is_dark(),
            diagram_tone_generation: 0,
            dirty: false,
            revision: 0,
            lsp_version: 1,
            active_save: None,
            conflict: false,
            error: None,
            durability_warning: None,
            history: History::default(),
            #[cfg(test)]
            history_time: None,
        }
    }

    /// A document that is its own file, which is every fixture that is not
    /// about links.
    #[cfg(test)]
    fn unlinked(path: PathBuf, text: String, disk_hash: blake3::Hash, markdown: bool) -> Self {
        Self::new(path.clone(), path, text, disk_hash, markdown)
    }

    /// Applies one editor action and brings the derived state with it,
    /// answering whether the text changed and how to tell the server.
    ///
    /// Deliberately does *not* serialize the buffer.
    /// [`text_editor::Content::text`] walks every line into a fresh `String`,
    /// and a file is allowed to be 8 MiB, so answering "is this dirty?" that
    /// way copies megabytes per keystroke on the COSMIC update thread.
    /// Dirtiness is decided from the byte length first and Markdown is
    /// re-parsed at a debounce boundary instead.
    fn apply(&mut self, action: text_editor::Action) -> Applied {
        match action {
            text_editor::Action::Edit(edit) => {
                let kind = edit_kind(&edit, self.content.cursor());
                let trailing_cr = trailing_carriage_return(&self.content).is_some();
                let Some(recorded) = self.content.perform_edit(edit) else {
                    return Applied::Unchanged;
                };
                // A final bare `\r` is a byte the protocol does not count, and
                // `protocol_edit` measures against the edited buffer only; while
                // either side ends in one, the server gets the whole document.
                let protocol = if trailing_cr || trailing_carriage_return(&self.content).is_some() {
                    None
                } else {
                    protocol_edit(&self.content, &recorded.change)
                };
                let now = Instant::now();
                #[cfg(test)]
                let now = self.history_time.unwrap_or(now);
                self.history.record(
                    recorded.change,
                    kind,
                    recorded.before,
                    self.content.cursor(),
                    now,
                );
                self.text_changed();
                Applied::Changed(protocol)
            }
            // Scrolling is only looking; everything else moves the caret or
            // the selection, and typing resumed from there is a new step.
            text_editor::Action::Scroll { .. } => {
                self.content.perform(action);
                Applied::Unchanged
            }
            action => {
                self.history.seal();
                self.content.perform(action);
                Applied::Unchanged
            }
        }
    }

    /// Brings the derived state up to date after the text changed.
    fn text_changed(&mut self) {
        self.revision = self.revision.wrapping_add(1).max(1);
        self.refresh_dirty();
        self.markdown_stale = true;
        if let Some(cancel) = &self.diagram_cancel {
            cancel.0.cancel();
        }
    }

    fn refresh_dirty(&mut self) {
        self.dirty = self.history.revision() != self.saved_history_revision;
        if self.dirty && self.content.text_snapshot().same_text(&self.saved_snapshot) {
            // Signature inequality is constant-time on ordinary edits. Only
            // a possible return to the saved bytes pays for exact equality.
            self.history.adopt_revision(self.saved_history_revision);
            self.dirty = false;
        }
    }

    /// Steps through the edit history, reporting what changed for the
    /// language server, or `None` when there was nothing to step to.
    ///
    /// Each entry describes one replacement in the order it was applied; an
    /// entry is `None` where the replacement had no protocol range.
    fn step_history(&mut self, redo: bool) -> Option<Vec<Option<LspTextEdit>>> {
        let mut buffer = ProtocolTracked {
            content: &mut self.content,
            edits: Vec::new(),
        };
        let replay = if redo {
            self.history.redo(&mut buffer)
        } else {
            self.history.undo(&mut buffer)
        };
        let edits = buffer.edits;
        match replay {
            Replay::Empty => return None,
            Replay::Applied => {}
            Replay::Diverged => {
                tracing::warn!("editor history no longer matched its buffer and was cleared");
            }
        }
        if !edits.is_empty() {
            self.text_changed();
        }
        Some(edits)
    }

    /// Returns the buffer to the text last read from or written to disk, as
    /// one step the user can undo.
    ///
    /// Reported like [`EditorDocument::step_history`]. A buffer whose lines
    /// cannot be addressed the way the history needs is rebuilt instead,
    /// which cannot be undone and forgets the history.
    fn discard_edits(&mut self) -> Vec<Option<LspTextEdit>> {
        // The reader keeps their place: the caret and the viewport stay where
        // they were rather than following the replaced text.
        let scroll = self.content.scroll_position();
        let cursor = self.content.cursor();
        let change = self.content.replacement_to_text(&self.saved_text);
        let mut edits = Vec::new();
        if let Some(change) = change {
            let mut buffer = ProtocolTracked {
                content: &mut self.content,
                edits: Vec::new(),
            };
            // Verified afterwards as well as before: the change was measured
            // on the text re-split into lines, and a buffer whose own lines
            // disagree with that split is rebuilt rather than trusted.
            let replaced = buffer.replace(
                change.start,
                &change.removed,
                &change.inserted,
                &change.inserted_breaks,
            );
            let replayed = buffer.edits;
            if replaced && content_matches(&self.content, &self.saved_text) {
                edits = replayed;
                self.content.set_cursor(cursor);
                self.history.record_group(
                    vec![change],
                    cursor,
                    self.content.cursor(),
                    Instant::now(),
                );
            } else {
                tracing::warn!(
                    "discarded edits by rebuilding the editor buffer; they cannot be undone"
                );
                self.content = text_editor::Content::with_text(&self.saved_text);
                self.history.clear();
                edits.push(None);
            }
        }
        self.content.restore_scroll_position(scroll);
        self.revision = self.revision.wrapping_add(1).max(1);
        self.markdown_stale = true;
        if let Some(cancel) = &self.diagram_cancel {
            cancel.0.cancel();
        }
        self.history.adopt_revision(self.saved_history_revision);
        self.dirty = false;
        debug_assert!(self.active_save.is_none());
        self.conflict = false;
        self.error = None;
        edits
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
        // The positions the history recorded address the buffer that was
        // just thrown away.
        self.history.clear();
        self.markdown_stale = true;
        if let Some(cancel) = &self.diagram_cancel {
            cancel.0.cancel();
        }
        self.dirty = text != self.saved_text;
        if !self.dirty { self.history.adopt_revision(self.saved_history_revision); }
        self.revision = self.revision.wrapping_add(1).max(1);
    }

    /// Applies replacements given as byte spans of `text`, the buffer's
    /// current text, in place and in the order given, returning the changes
    /// the history needs to undo them.
    ///
    /// Each span must still address the buffer when its turn comes, which
    /// holds for spans applied highest-first. A span the buffer cannot
    /// address — one splitting a line break — stops the splice and answers
    /// `None`; the caller then replaces the text wholesale, which is what
    /// the partly spliced buffer needs anyway.
    fn splice<'t>(
        &mut self,
        text: &str,
        spans: impl Iterator<Item = (Range<usize>, &'t str)>,
    ) -> Option<Vec<TextChange>> {
        let mut changes = Vec::new();
        for (span, inserted) in spans {
            let removed = text.get(span.clone())?;
            if removed == inserted {
                continue;
            }
            let start = caret_for_offset(&self.content, span.start);
            let end = self.content.range_end(start, removed)?;
            if let Some(change) = self.content.replace_range_recorded(start, end, inserted) {
                changes.push(change);
            }
        }
        Some(changes)
    }

    /// Re-parses Markdown when an edit has invalidated the preview's items.
    ///
    /// Called from the preview debounce and from the moment the preview is
    /// mounted, never from [`EditorDocument::apply`]: this is the one place
    /// that pays for a full document walk, and nothing reads
    /// [`EditorDocument::markdown_items`] until the preview is on screen.
    fn sync_markdown(&mut self) {
        if !std::mem::take(&mut self.markdown_stale) {
            return;
        }
        self.preview_text = self.content.text();
        self.markdown_items = if self.markdown {
            markdown::parse(&self.preview_text).collect()
        } else {
            Vec::new()
        };
    }
}

/// How an edit may coalesce with the undo step before it, judged from the
/// caret it was made at.
fn edit_kind(edit: &text_editor::Edit, before: text_editor::Cursor) -> EditKind {
    // Any selection counts, even one reported as anchored where the caret
    // is: a double-clicked word selects its span from a single point.
    let selected = before.selection.is_some();
    match edit {
        text_editor::Edit::Insert(_) | text_editor::Edit::Enter => EditKind::Insert,
        text_editor::Edit::Backspace if !selected => EditKind::DeleteBackward,
        text_editor::Edit::Delete if !selected => EditKind::DeleteForward,
        text_editor::Edit::Backspace
        | text_editor::Edit::Delete
        | text_editor::Edit::Paste(_)
        | text_editor::Edit::Indent
        | text_editor::Edit::Unindent => EditKind::Other,
    }
}

/// The editor buffer as the history replays into it, recording each
/// replacement as the protocol edit that describes it.
struct ProtocolTracked<'a> {
    content: &'a mut text_editor::Content<cosmic::Renderer>,
    edits: Vec<Option<LspTextEdit>>,
}

impl EditBuffer for ProtocolTracked<'_> {
    fn replace(
        &mut self,
        start: text_editor::Position,
        expected: &str,
        replacement: &str,
        replacement_breaks: &[usize],
    ) -> bool {
        let Some(end) = self.content.range_end(start, expected) else {
            return false;
        };
        // Measured before the replacement lands, like every other edit the
        // server is told about.
        let range = caret_position(self.content, start)
            .zip(caret_position(self.content, end))
            .map(|(start, end)| LspRange::new(start, end));
        let trailing_cr = trailing_carriage_return(self.content).is_some();
        self.content
            .replace_range_with_breaks(start, end, replacement, replacement_breaks);
        // A final bare carriage return has no protocol column. Replaying
        // its removal would otherwise report an empty range and leave the
        // server holding a byte that the editor has removed.
        self.edits.push(
            if trailing_cr || trailing_carriage_return(self.content).is_some() {
                None
            } else {
                range.map(|range| LspTextEdit {
                    range,
                    new_text: replacement.to_owned(),
                })
            },
        );
        true
    }

    fn set_cursor(&mut self, cursor: text_editor::Cursor) {
        self.content.set_cursor(cursor);
    }
}

/// Whether the buffer's text equals `saved`, decided without building it.
///
/// [`text_editor::Content::text`] allocates the whole document, so comparing
/// a keystroke against the saved text that way copies megabytes per
/// character. The byte length settles almost every case in constant time,
/// and the rare length-preserving edit falls through to a streaming
/// comparison that allocates nothing and stops at the first differing byte.
fn content_matches(content: &text_editor::Content<cosmic::Renderer>, saved: &str) -> bool {
    if content.text_len() != saved.len() {
        return false;
    }
    let lines = content.line_count();
    let mut rest = saved;
    for index in 0..lines {
        let here = rest;
        let matched = content.with_line(index, |text, ending| {
            let tail = here.strip_prefix(text)?;
            tail.strip_prefix(terminator_str(ending, index + 1 == lines))
        });
        let Some(Some(tail)) = matched else {
            return false;
        };
        rest = tail;
    }
    rest.is_empty()
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
///
/// Production code reads the caret straight off the buffer with
/// [`caret_position`], which does the same arithmetic without materialising
/// the document; this is the obvious-but-allocating reference the tests
/// check that one against.
#[cfg(test)]
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
///
/// The reference implementation [`caret_position`] is checked against; see
/// [`scalar_column`].
#[cfg(test)]
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
fn terminator_str(ending: text_editor::LineEnding, last: bool) -> &'static str {
    if last {
        ""
    } else if ending == text_editor::LineEnding::None {
        text_editor::LineEnding::default().as_str()
    } else {
        ending.as_str()
    }
}

/// The editor-native caret for a byte offset into the buffer's text.
///
/// An offset past the end lands at the end of the last line, because a caret
/// has to be somewhere.
///
/// Reads each line in place. The obvious loop over
/// [`text_editor::Content::line`] allocates and frees one `String` per line
/// of the document for every conversion, which is what a single hover used
/// to cost on a hundred-thousand-line file.
fn caret_for_offset(
    content: &text_editor::Content<cosmic::Renderer>,
    offset: usize,
) -> text_editor::Position {
    let lines = content.line_count();
    let mut start = 0;
    let mut caret = text_editor::Position { line: 0, column: 0 };
    for index in 0..lines {
        let Some((position, next, reached)) = content.with_line(index, |text, ending| {
            let end = start + text.len();
            (
                text_editor::Position {
                    line: index,
                    column: char_boundary(text, offset.saturating_sub(start)),
                },
                end + terminator_str(ending, index + 1 == lines).len(),
                offset <= end,
            )
        }) else {
            break;
        };
        caret = position;
        if reached {
            break;
        }
        start = next;
    }
    caret
}

/// Index of the editor line whose bare `\r` terminator is the buffer's very
/// last byte, if there is one.
///
/// [`line_span`] ends a protocol line before a `\r` that sits at its end,
/// and the last protocol line ends at the end of the buffer — so a buffer
/// finishing in a bare `\r` has one byte the protocol does not count. The
/// editor disagrees: it breaks on that `\r` and will park a caret after it.
/// This is the one place the two numberings have to be reconciled by hand.
fn trailing_carriage_return(content: &text_editor::Content<cosmic::Renderer>) -> Option<usize> {
    let last = content.line_count().checked_sub(1)?;
    let previous = last.checked_sub(1)?;
    if !content.with_line(last, |text, _| text.is_empty())? {
        return None;
    }
    content
        .with_line(previous, |_, ending| {
            !terminator_str(ending, false).contains('\n')
        })?
        .then_some(previous)
}

/// Protocol position of the editor's caret, without serializing the buffer.
///
/// Equivalent to `position_for_offset(&content.text(), caret_offset(..))`,
/// and deliberately so: a caret always sits inside a line's *content* and
/// never on the `\r` of a `\r\n` that [`line_span`] excludes, so that clamp
/// is a no-op here and the walk can accumulate scalars as it goes. Protocol
/// lines break on `\n` alone while the editor also breaks on a bare `\r`,
/// so one protocol line can span several editor lines and the scalar count
/// only resets at a terminator containing `\n`.
fn caret_position(
    content: &text_editor::Content<cosmic::Renderer>,
    caret: text_editor::Position,
) -> Option<LspPosition> {
    let snapshot = content.text_snapshot();
    if snapshot.protocol_compatible_lines() {
        let line = snapshot.line(caret.line)?;
        let column = line.text[..char_boundary(&line.text, caret.column)].chars().count();
        return Some(LspPosition::new(u32::try_from(caret.line).ok()?, u32::try_from(column).ok()?));
    }
    let lines = content.line_count();
    if caret.line >= lines {
        return None;
    }
    let trailing_cr = trailing_carriage_return(content);
    let mut line = 0u32;
    let mut scalars = 0usize;
    for index in 0..caret.line {
        let terminator = content.with_line(index, |text, ending| {
            scalars += text.chars().count();
            terminator_str(ending, index + 1 == lines)
        })?;
        if terminator.contains('\n') {
            line = line.saturating_add(1);
            scalars = 0;
        } else if trailing_cr != Some(index) {
            scalars += terminator.chars().count();
        }
    }
    content.with_line(caret.line, |text, _| {
        let column = scalars + text[..char_boundary(text, caret.column)].chars().count();
        LspPosition::new(line, u32::try_from(column).unwrap_or(u32::MAX))
    })
}

/// The editor-native caret for a protocol position, or `None` when it names a
/// line the buffer does not have.
///
/// The inverse of [`caret_position`], and allocation-free for the same
/// reason. A character past the end of the protocol line clamps to its end,
/// which is what the protocol asks a client to do.
fn caret_for_position(
    content: &text_editor::Content<cosmic::Renderer>,
    at: LspPosition,
) -> Option<text_editor::Position> {
    let snapshot = content.text_snapshot();
    if snapshot.protocol_compatible_lines() {
        let line_index = usize::try_from(at.line).ok()?;
        let line = snapshot.line(line_index)?;
        let column = line.text.char_indices().nth(usize::try_from(at.character).ok()?).map_or(line.text.len(), |(byte,_)| byte);
        return Some(text_editor::Position {line:line_index,column});
    }
    let lines = content.line_count();
    let trailing_cr = trailing_carriage_return(content);
    let target = usize::try_from(at.character).unwrap_or(usize::MAX);
    let mut line = 0u32;
    let mut consumed = 0usize;
    let mut found = None;
    for index in 0..lines {
        if line > at.line {
            break;
        }
        let Some((position, scalars, terminator)) = content.with_line(index, |text, ending| {
            let remaining = target.saturating_sub(consumed);
            (
                text_editor::Position {
                    line: index,
                    column: text
                        .char_indices()
                        .nth(remaining)
                        .map_or(text.len(), |(byte, _)| byte),
                },
                text.chars().count(),
                terminator_str(ending, index + 1 == lines),
            )
        }) else {
            break;
        };
        let on_target = line == at.line;
        if on_target {
            found = Some(position);
            if consumed + scalars >= target {
                break;
            }
        }
        if trailing_cr == Some(index) {
            // The protocol line ends at this `\r`; the empty editor line
            // after it is not a place a protocol position can name.
            break;
        }
        if terminator.contains('\n') {
            if on_target {
                break;
            }
            line = line.saturating_add(1);
            consumed = 0;
        } else {
            consumed += scalars + terminator.chars().count();
        }
    }
    found
}

/// The protocol edit that describes a change the buffer has just made.
///
/// Built from the measured change rather than guessed from the keystroke: a
/// double-clicked word reports its selection as a single point, so a guess
/// would tell the server one character went where a whole word did. The
/// start is read off the edited buffer, where nothing before it moved; the
/// end is the start advanced over the removed text in protocol terms, where
/// lines break on `\n` alone.
///
/// `None` where the start has no protocol position; the caller then falls
/// back to the full document.
fn protocol_edit(
    content: &text_editor::Content<cosmic::Renderer>,
    change: &TextChange,
) -> Option<LspTextEdit> {
    let start = caret_position(content, change.start)?;
    let mut end = start;
    for character in change.removed.chars() {
        if character == '\n' {
            end = LspPosition::new(end.line.checked_add(1)?, 0);
        } else {
            end = LspPosition::new(end.line, end.character.checked_add(1)?);
        }
    }
    Some(LspTextEdit {
        range: LspRange::new(start, end),
        new_text: change.inserted.clone(),
    })
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
    Loading,
    Editable(EditorDocument),
    Image {
        /// The worktree-relative file the pixels came from, which is not the
        /// selected row when that row is a symbolic link. See
        /// [`LoadedFile::source`].
        source: PathBuf,
        handle: iced::widget::image::Handle,
        width: u32,
        height: u32,
    },
    /// A paged document. `path` is the file the pages are read from, because
    /// no document bytes are kept in this process: each page is rendered by a
    /// child process that re-reads the file, which is what keeps a damaged or
    /// hostile document from taking the window down with it.
    Pdf {
        /// The same file as `path`, worktree-relative, kept so the chrome can
        /// say which file a row that is a symbolic link resolved to.
        source: PathBuf,
        path: PathBuf,
        pages: usize,
        page: usize,
        tone: crate::editor::PdfTone,
        /// How large the page is drawn: the pane's width at
        /// `PdfZoom::Fit`, and a fraction of the page's printed size
        /// otherwise. Not part of the raster's identity by accident: a page
        /// rendered at one size and drawn at another is either soft or
        /// needlessly large, so a change here re-renders exactly as a page
        /// turn does.
        zoom: crate::editor::PdfZoom,
        handle: iced::widget::image::Handle,
        width: u32,
        height: u32,
        /// The drawn page's printed width in points, which is what a zoom
        /// step is a fraction of.
        points_wide: f32,
        /// The page turn being rendered now, if any. A page is rasterized in
        /// another process, and the reader can turn faster than that, so only
        /// one render runs at a time and the newest turn waits in it.
        rendering: Option<PdfRender>,
        message: Option<String>,
        /// The tone the reader asked for with the toolbar toggle, which the
        /// shell's theme then stops deciding. Following the theme is the right
        /// default and the wrong override: a reader who turned a figure back
        /// to its own colours did so to read the figure, and a theme change
        /// is no reason to take that away.
        chosen_tone: Option<crate::editor::PdfTone>,
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

/// Everything about the open document that decides which raster belongs on
/// screen.
///
/// One value rather than three fields compared pairwise, because every place
/// that asks "is this the page the reader is looking at" has to ask about all
/// three, and a comparison that forgets one silently accepts a stale raster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PdfView {
    page: usize,
    tone: crate::editor::PdfTone,
    zoom: crate::editor::PdfZoom,
}

/// The page turn a child process is rendering now.
struct PdfRender {
    /// Which request this is, so a result the reader has already turned away
    /// from cannot replace the page they are looking at now.
    request: u64,
    /// The view being rendered, which becomes the drawn one once the render
    /// succeeds.
    requested: PdfView,
    /// The view actually on screen. A render can fail - a document the helper
    /// cannot parse does - and the counter, the tooltip, the tone toggle and
    /// the zoom controls then have to describe the page that is still drawn
    /// rather than the one that never arrived.
    drawn: PdfView,
    /// The newest view the reader asked for while this render was in flight.
    ///
    /// Turning twenty pages must cost the two renders the reader can see, not
    /// twenty that each reparse the document, so intermediate turns replace
    /// this one and only the last of them is dispatched.
    pending: Option<PdfView>,
}

enum DiffState {
    Loading {
        request: u64,
        cancellation: CancellationToken,
    },
    Ready(RenderedDiff, DiffLayout),
}

impl DiffState {
    fn ready(content: RenderedDiff) -> Self {
        let layout = DiffLayout::new(
            match &content {
                RenderedDiff::Text { model, .. } => Some(model),
                _ => None,
            },
            DIFF_ROW_HEIGHT,
        );
        Self::Ready(content, layout)
    }
}

impl Drop for DiffState {
    fn drop(&mut self) {
        if let Self::Loading { cancellation, .. } = self {
            cancellation.cancel();
        }
    }
}
/// A debounce result carries gutter marks in both lanes; a full comparison is
/// built only when the reader can see the diff surface.
#[derive(Debug, Clone)]
pub struct WorkingDiffOutput {
    model: Option<DiffModel>,
    additions: Vec<usize>,
}

/// One immutable snapshot is in flight; further edits only advance the desired
/// revision/deadline. Cancellation does not release the slot until it returns.
struct WorkingDiffJob {
    identity: AsyncIdentity,
    revision: u64,
    ticket: u64,
    before: Arc<str>,
    rich: bool,
    cancellation: CancellationToken,
}

impl Drop for WorkingDiffJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Default)]
struct WorkingDiffRefresh {
    timer: Option<u64>,
    next_ticket: u64,
    desired: Option<(u64, u64, Arc<str>)>,
    applied: Option<(u64, u64, Arc<str>)>,
    desired_rich: bool,
    applied_rich: bool,
    deadline: Option<Instant>,
    active: Option<WorkingDiffJob>,
    error: Option<String>,
    additions: Vec<usize>,
}

/// Coalescing timer for the Markdown preview's parse, shaped exactly like
/// [`WorkingDiffRefresh`]: at most one timer is ever in flight and it always
/// observes the latest deadline, so continuous typing parses once when the
/// typing stops rather than once per character.
#[derive(Debug, Default)]
struct MarkdownDebounce {
    timer: Option<u64>,
    next_ticket: u64,
    deadline: Option<Instant>,
}

/// How long the Markdown preview waits for typing to stop before re-parsing.
/// The same window [`RepositoryRuntime::working_diff_task`] uses, for the
/// same reason: a parse is the whole document's work.
const MARKDOWN_DEBOUNCE: Duration = Duration::from_millis(180);

/// A file-list read still outstanding after this long is presumed lost.
const LOST_FILE_LIST_READ: Duration = Duration::from_secs(60);

fn same_diff_revision(
    left: &Option<(u64, u64, Arc<str>)>,
    right: &Option<(u64, u64, Arc<str>)>,
) -> bool {
    match (left, right) {
        (Some((lg, lr, lb)), Some((rg, rr, rb))) => lg == rg && lr == rr && Arc::ptr_eq(lb, rb),
        (None, None) => true,
        _ => false,
    }
}

/// The source lines an unsaved buffer has added, for the editor to tint.
///
/// Deleted lines are not in the buffer at all, so they have no source line to
/// name: the comparison surface is where a reader sees them, with their text.
fn added_source_lines(model: &DiffModel) -> Vec<usize> {
    let mut additions = Vec::new();
    for row in &model.inline_rows {
        if let Some(line) = row.new_line
            && row.kind == DiffRowKind::Addition
        {
            additions.push((line as usize).saturating_sub(1));
        }
    }
    additions
}

fn native_added_lines(snapshot: &text_editor::TextSnapshot, additions: Vec<usize>) -> Vec<usize> {
    if snapshot.protocol_compatible_lines() {
        return additions;
    }
    let count = snapshot.line_count();
    let mut source = 0;
    let mut output = Vec::new();
    for native in 0..count {
        if additions.binary_search(&source).is_ok() {
            output.push(native);
        }
        if native + 1 == count
            || snapshot
                .line(native)
                .is_some_and(|line| line.ending.as_str().contains('\n'))
        {
            source += 1;
        }
    }
    output
}

#[derive(Debug, Clone, Copy, Default)]
struct ListViewport {
    offset: f32,
    height: f32,
}

/// Which rows of a folded run of unchanged lines to reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldExpansion {
    /// The run's first rows, continuing the content above the fold.
    Down,
    /// The run's last rows, leading into the content below the fold.
    Up,
    All,
}

/// A logical diff position. `row` is in model rows, not display rows, so
/// neither expanding a fold nor rebuilding the model can move it.
#[derive(Debug, Clone, Copy, Default)]
struct DiffViewportAnchor {
    offset: f32,
    row: Option<diff_layout::Anchor>,
    horizontal: f32,
    change: usize,
    /// A comparison opened for the first time: show its first change, with
    /// context, as soon as its rows exist.
    first_change: bool,
}

impl DiffViewportAnchor {
    const FIRST_CHANGE: Self = Self {
        offset: 0.0,
        row: None,
        horizontal: 0.0,
        change: 0,
        first_change: true,
    };
}

/// Scroll coordinates only: the selected document remains the sole live buffer.
#[derive(Debug, Clone, Copy, Default)]
struct FileViewportAnchors {
    editor: text_editor::ScrollPosition,
    preview: f32,
    diff: [Option<DiffViewportAnchor>; 2],
}

const MAX_RETAINED_FILE_VIEWPORTS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
struct VisibleRows {
    start: usize,
    end: usize,
    top_space: f32,
    bottom_space: f32,
}

const DIFF_ROW_HEIGHT: f32 = 21.0;
const VISIBLE_ROW_OVERSCAN: usize = 6;
const INITIAL_VIEWPORT_HEIGHT: f32 = 900.0;
/// Room for the widest zoom step the viewer's label can hold, so the controls
/// either side of it do not move when the step changes.
const ZOOM_LABEL_WIDTH: f32 = 48.0;
/// How many committed comparisons one open file keeps.
///
/// Viewed working bases stay cached until a save or mutation invalidates them.
/// A committed basis is named by a ref or a commit
/// id, so its keyspace is open - stepping through history would otherwise
/// retain one whole diff model per commit visited for as long as the file
/// stays open, and a side is allowed to be megabytes. Four keeps stepping back
/// instant and the ceiling bounded.
const MAX_RETAINED_COMMITTED_DIFFS: usize = 4;

#[cfg(test)]
fn visible_rows(total: usize, viewport: ListViewport, row_height: f32) -> VisibleRows {
    visible_diff_rows(&RowOffsets::uniform(total, row_height), viewport)
}

fn visible_diff_rows(rows: &RowOffsets, viewport: ListViewport) -> VisibleRows {
    let height = if viewport.height.is_finite() && viewport.height > 0.0 {
        viewport.height
    } else {
        INITIAL_VIEWPORT_HEIGHT
    };
    let Range { start, end } = rows.visible(viewport.offset, height, VISIBLE_ROW_OVERSCAN);
    VisibleRows {
        start,
        end,
        top_space: rows.offset(start) - rows.leading_height(),
        bottom_space: rows.total_height() - rows.offset(end),
    }
}

type FindScope = ((usize, usize), (usize, usize));

fn scoped_matches(
    matches: &mut Vec<(text_editor::Position, text_editor::Position)>,
    scope: Option<FindScope>,
) {
    if let Some((first, last)) = scope {
        matches.retain(|(start, end)| {
            (start.line, start.column) >= first && (end.line, end.column) <= last
        });
    }
}

#[derive(Clone, PartialEq, Eq)]
struct FindKey {
    options: crate::editor_search::FindOptions,
    selection: Option<FindScope>,
    generation: u64,
    revision: u64,
    query: String,
}

struct FindJob {
    key: FindKey,
    ticket: uuid::Uuid,
    cancellation: CancellationToken,
}

impl Drop for FindJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub struct RepositoryRuntime {
    pub descriptor: RepositoryView,
    worktree_protection: Option<crate::worktree_cleanup::Protection>,
    directory_only: bool,
    standalone: bool,
    external_editors: Vec<crate::sidebar::SidebarEditor>,
    keyboard_shortcuts: Arc<crate::settings::KeyboardShortcuts>,
    /// Epoch for selected-document, diff, and save work. File-list refreshes
    /// deliberately use a separate epoch so they cannot obsolete an active
    /// document operation.
    generation: u64,
    file_list_generation: u64,
    /// Counter fencing PDF page renders. A page turn supersedes the render in
    /// flight, and the reader can turn faster than a page rasterizes.
    pdf_request: u64,
    /// The tone the open file's load rendered its first document page with.
    ///
    /// Taken when the load is dispatched rather than when it completes,
    /// because a document takes as long to rasterize as it takes and the
    /// reader may change their shell theme while it does. Reading the theme
    /// again at completion would record a tone the pixels do not have, and
    /// the theme-change re-render would then decide there was nothing to do.
    document_tone: crate::editor::PdfTone,
    file_list_loading: bool,
    /// When the outstanding file-list read started.
    ///
    /// A read whose result never comes back — a shell that filtered it away
    /// while a modal was open, a cancelled subscription — would otherwise
    /// leave `file_list_loading` set and stop this card refreshing for the
    /// rest of the session. Being wrong costs one extra Git read.
    file_list_started: Option<Instant>,
    /// Digest of the raw Git output the current file list was built from, so
    /// a refresh tick that finds nothing changed can say so without
    /// rebuilding the list. `None` before the first successful read, and
    /// after any read this card did not install.
    file_list_digest: Option<blake3::Hash>,
    /// Where the worker leaves the digest of the read in flight, tagged with
    /// the generation that asked for it. Committed into
    /// [`RepositoryRuntime::file_list_digest`] only once the matching list
    /// has actually been installed.
    file_list_deposit: Arc<std::sync::Mutex<Option<(u64, blake3::Hash)>>>,
    file_list_refresh_queued: bool,
    background_file_refresh: bool,
    mutation_refresh_pending: bool,
    /// Last comparison stays readable while Git refreshes this file.
    stale_diff: Option<(PathBuf, DiffBasis, DiffState)>,
    diff_hide_unchanged: bool,
    diff_ignore_whitespace: bool,
    files: FileListState,
    selected: Option<RepositoryFile>,
    document: DocumentState,
    search_mode: bool,
    editor_font: Font,
    editor_font_size: f32,
    find_open: bool,
    /// The find bar's query exactly as the user typed it.
    find_query: String,
    find_options: crate::editor_search::FindOptions,
    find_selection: Option<FindScope>,
    find_selection_candidate: Option<FindScope>,
    find_select_on_finish: bool,
    replace_preserve_case: bool,
    /// Which occurrence of [`RepositoryRuntime::find_query`] is selected, and
    /// how many there are.
    ///
    /// The total is recorded by the search that produced it rather than
    /// recounted where it is displayed: counting means materialising the
    /// whole rope and lowercasing a copy of it, which a redraw must not do
    /// once per frame. It is therefore only true of the query and the buffer
    /// revision it was measured against, and every path that changes either
    /// renews it.
    find_index: usize,
    find_total: usize,
    find_input_focused: bool,
    replace_open: bool,
    replace_query: String,
    toolbar_overflow_open: bool,
    find_matches: Vec<(text_editor::Position, text_editor::Position)>,
    find_cache: Option<FindKey>,
    find_snapshot: Option<(u64, u64, Arc<text_editor::SearchSnapshot>)>,
    find_active: Option<FindJob>,
    find_origin: Option<(usize, usize)>,
    find_error: Option<String>,
    /// One entry per comparison this selection has been asked for.
    ///
    /// A list rather than a map: at most six bases are live at once, and
    /// [`DiffBasis`] is deliberately neither `Ord` nor `Hash` — it owns a ref
    /// name and an object id.
    diffs: Vec<(DiffBasis, DiffState)>,
    working_diff: WorkingDiffRefresh,
    diff_surface_visible: bool,
    source_surface: SourceSurface,
    markdown_mode: MarkdownMode,
    pending_fragment: Option<String>,
    diff_interaction: diff_actions::State,
    diff_mode: DiffMode,
    committed_selection: Option<(DiffBasis, Option<PathBuf>)>,
    diff_basis: DiffBasis,
    /// The committed comparison being resolved, or the reason one could not
    /// be, while that is the whole story of the diff surface.
    basis_resolution: Option<BasisResolution>,
    auto_save: bool,
    wrap_lines: bool,
    line_height: f32,
    syntax_theme: crate::settings::EditorSyntaxTheme,
    listed_directories: BTreeSet<PathBuf>,
    file_search_generation: u64,
    file_search_cancel: CancellationToken,
    auto_save_pending: Option<(u64, u64)>,
    next_auto_save_ticket: u64,
    diff_change: usize,
    diff_scroll_id: widget::Id,
    diff_viewport: ListViewport,
    /// Retained logical position while a reopened comparison has no geometry.
    /// Kept through the first uniform layout until measurement or user scroll.
    pending_diff_anchor: Option<DiffViewportAnchor>,
    pending_diff_source: Option<(u32, f64, f32)>,
    diff_layout_revision: u64,
    review_line_anchor: Option<(
        AsyncIdentity,
        DiffBasis,
        crate::pull_request_review::discussion::Anchor,
    )>,
    diff_horizontal: f32,
    preview_scroll_id: widget::Id,
    viewports: FileViewportAnchors,
    retained_viewports: VecDeque<(PathBuf, FileViewportAnchors)>,
    restored_navigation: Option<crate::model::RepositoryNavigation>,
    pending_editor_navigation: Option<(PathBuf, crate::model::EditorNavigation)>,
    pending_source_location: Option<(PathBuf, usize)>,
    pending_editor_focus: Option<AsyncIdentity>,
    next_save_ticket: u64,
    next_diff_request: u64,
    /// Identity the application gave the open buffer at the LSP boundary, or
    /// `None` while nothing is open there. Stored rather than derived: only
    /// the application, which owns the manager, can mint one.
    lsp_document: Option<LspDocumentId>,
    language: EditorLanguageState,
    /// Protocol notifications this buffer has earned but nobody has performed
    /// yet, oldest first.
    ///
    /// An outbox rather than direct calls, because a widget that reached a
    /// language server would have to own one, and because the ordering of
    /// `didOpen`/`didChange`/`didClose` is exactly what a server cannot
    /// tolerate being wrong. Every buffer mutation appends here, so a mutation
    /// that forgets is a desynchronised server rather than a silent nothing.
    lsp_intents: Vec<LspIntent>,
    /// A recorded `didChange` whose text has not been produced yet.
    ///
    /// Serializing the buffer is work proportional to the document, so a
    /// burst of edits inside one drain window materialises the text once, at
    /// the drain, rather than once per keystroke. Any other intent flushes
    /// this first: a `Close` followed by an `Open` names a different
    /// document, and a change recorded before it can no longer be read off
    /// the buffer afterwards.
    pending_lsp_change: Option<i32>,
    /// The edits behind [`RepositoryRuntime::pending_lsp_change`], in the
    /// order they were applied, for incremental synchronisation.
    ///
    /// Empty means "this transition cannot be described as ranges"; the
    /// application must then send the whole document, which is what
    /// [`LspIntent::Change`] carries regardless.
    lsp_edits: Vec<LspTextEdit>,
    /// Whether [`RepositoryRuntime::lsp_edits`] still describes the whole
    /// transition. One edit this runtime cannot express as a range makes the
    /// batch unusable until the next drain.
    lsp_edits_exact: bool,
    markdown_debounce: MarkdownDebounce,
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
    fn protect_editor_worktree(&mut self, directory: &Path) -> std::io::Result<()> {
        if self
            .worktree_protection
            .as_ref()
            .is_some_and(|guard| guard.directory() == directory)
        {
            return Ok(());
        }
        let next = crate::worktree_cleanup::protect(directory)?;
        self.worktree_protection = next;
        Ok(())
    }

    pub fn new(descriptor: RepositoryView) -> Self {
        let identity = descriptor.worktree.display().to_string();
        Self {
            directory_only: descriptor.selected_file.is_some(),
            descriptor,
            worktree_protection: None,
            standalone: false,
            external_editors: Vec::new(),
            keyboard_shortcuts: Arc::new(crate::settings::KeyboardShortcuts::default()),
            generation: 0,
            file_list_generation: 0,
            pdf_request: 0,
            document_tone: crate::editor::PdfTone::Original,
            file_list_loading: false,
            file_list_started: None,
            file_list_digest: None,
            file_list_deposit: Arc::new(std::sync::Mutex::new(None)),
            file_list_refresh_queued: false,
            background_file_refresh: false,
            mutation_refresh_pending: false,
            stale_diff: None,
            diff_hide_unchanged: true,
            diff_ignore_whitespace: false,
            files: FileListState::Loading,
            selected: None,
            search_mode: false,
            editor_font: Font::MONOSPACE,
            editor_font_size: 14.0,
            find_open: false,
            find_query: String::new(),
            find_options: crate::editor_search::FindOptions::default(),
            find_selection: None,
            find_selection_candidate: None,
            find_select_on_finish: false,
            replace_preserve_case: false,
            find_index: 0,
            find_total: 0,
            find_input_focused: false,
            replace_open: false,
            replace_query: String::new(),
            toolbar_overflow_open: false,
            find_matches: Vec::new(),
            find_cache: None,
            find_snapshot: None,
            find_active: None,
            find_origin: None,
            find_error: None,
            document: DocumentState::None,
            diffs: Vec::new(),
            working_diff: WorkingDiffRefresh::default(),
            diff_surface_visible: true,
            next_diff_request: 0,
            source_surface: SourceSurface::Editor,
            markdown_mode: MarkdownMode::Edit,
            pending_fragment: None,
            committed_selection: None,
            diff_interaction: diff_actions::State::default(),
            diff_mode: DiffMode::Inline,
            diff_basis: DiffBasis::IndexToWorktree,
            basis_resolution: None,
            auto_save: true,
            wrap_lines: false,
            line_height: 1.5,
            syntax_theme: crate::settings::EditorSyntaxTheme::Auto,
            listed_directories: BTreeSet::new(),
            file_search_generation: 0,
            file_search_cancel: CancellationToken::new(),
            auto_save_pending: None,
            next_auto_save_ticket: 0,
            diff_change: 0,
            diff_scroll_id: widget::Id::new(format!("repository.diff.{identity}")),
            diff_viewport: ListViewport::default(),
            pending_diff_anchor: None,
            pending_diff_source: None,
            diff_layout_revision: 0,
            review_line_anchor: None,
            diff_horizontal: 0.0,
            preview_scroll_id: widget::Id::new(format!("repository.preview.{identity}")),
            viewports: FileViewportAnchors::default(),
            retained_viewports: VecDeque::new(),
            restored_navigation: None,
            pending_editor_navigation: None,
            pending_source_location: None,
            pending_editor_focus: None,
            next_save_ticket: 0,
            lsp_document: None,
            language: EditorLanguageState::default(),
            lsp_intents: Vec::new(),
            pending_lsp_change: None,
            lsp_edits: Vec::new(),
            lsp_edits_exact: true,
            markdown_debounce: MarkdownDebounce::default(),
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

    pub fn set_keyboard_shortcuts(&mut self, shortcuts: Arc<crate::settings::KeyboardShortcuts>) {
        self.keyboard_shortcuts = shortcuts;
    }

    /// Cached native-buffer capabilities used by menus, palette and shortcuts.
    pub fn editor_command_capabilities(&self) -> u8 {
        if !self.editable_surface_visible() { return 0; }
        let DocumentState::Editable(document) = &self.document else { return 0; };
        let selected = document.content.cursor().selection.is_some_and(|anchor| anchor != document.content.cursor().position);
        let writable = !self.search_mode && document.active_save.is_none();
        u8::from(writable && document.history.can_undo())
            | (u8::from(writable && document.history.can_redo()) << 1)
            | (u8::from(writable && selected) << 2)
            | (u8::from(selected) << 3)
            | (u8::from(writable) << 4)
            | (u8::from(!document.content.is_empty()) << 5)
    }

    /// Changes presentation without replacing the open buffer or its edit history.
    pub fn set_wrap_lines(&mut self, wrap_lines: bool) {
        self.wrap_lines = wrap_lines;
    }

    /// Changes the editor's relative line height without replacing the buffer.
    pub fn set_line_height(&mut self, line_height: f32) {
        if line_height.is_finite() {
            self.line_height = line_height.clamp(1.0, 2.0);
            self.update_diff_typography();
        }
    }

    /// Installs the syntax palette and drops every comparison coloured with
    /// the old one.
    ///
    /// The Edit surface re-derives its highlighter in `view` and repaints at
    /// once; a [`DiffModel`] carries baked spans and would keep showing the
    /// previous palette until the next revision happened to rebuild it.
    pub fn set_syntax_theme(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        if self.syntax_theme == theme {
            return;
        }
        self.syntax_theme = theme;
        self.diffs.clear();
        self.working_diff.applied = None;
        self.working_diff.additions.clear();
    }

    pub fn refresh_diff_preferences(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.ensure_current_diff(card)
    }

    pub fn set_diff_ignore_whitespace(&mut self, enabled: bool) {
        if self.diff_ignore_whitespace != enabled {
            self.diff_ignore_whitespace = enabled;
            self.diffs.clear();
            self.stale_diff = None;
            self.working_diff.applied = None;
            self.working_diff.desired = None;
            self.working_diff.additions.clear();
            if let Some(active) = &self.working_diff.active {
                active.cancellation.cancel();
            }
        }
    }

    pub fn set_diff_hide_unchanged(&mut self, enabled: bool) -> Task<RepositoryAction> {
        if self.diff_hide_unchanged == enabled {
            return Task::none();
        }
        let anchor = self.current_diff_viewport();
        self.diff_hide_unchanged = enabled;
        for (_, state) in &mut self.diffs {
            if let DiffState::Ready(RenderedDiff::Text { model, .. }, layout) = state {
                layout.set_folding(model, enabled);
            }
        }
        if let Some((_, _, DiffState::Ready(RenderedDiff::Text { model, .. }, layout))) =
            &mut self.stale_diff
        {
            layout.set_folding(model, enabled);
        }
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.apply_diff_viewport(anchor);
        self.capture_diff_viewport();
        self.restore_visible_scroll()
    }

    /// Re-renders the open document page for `theme`.
    ///
    /// A page's colours are baked into its pixels when it is rasterized, so a
    /// document opened under a light shell stays a glaring white sheet on a
    /// dark one until the page is rendered again. Anything that is not a
    /// document, and a page whose tone the theme does not change, renders
    /// nothing.
    pub fn retone_document(
        &mut self,
        card: CardId,
        theme: &cosmic::Theme,
    ) -> Task<RepositoryAction> {
        if let DocumentState::Editable(document) = &mut self.document {
            let dark = theme.theme_type.is_dark();
            if document.diagram_dark != dark {
                document.diagram_dark = dark;
                document.diagram_tone_generation = document.diagram_tone_generation.wrapping_add(1);
                document.diagrams.clear();
                document.diagram_cancel = None;
                document.diagram_pending = None;
                return self.diagram_task(card);
            }
            return Task::none();
        }
        let DocumentState::Pdf {
            page,
            zoom,
            chosen_tone,
            ..
        } = &self.document
        else {
            return Task::none();
        };
        // A tone the reader picked is theirs to keep.
        if chosen_tone.is_some() {
            return Task::none();
        }
        let (page, zoom) = (*page, *zoom);
        let tone = pdf_tone(theme);
        // The tone the application's theme now asks for. Recording it is what
        // lets a render that lands afterwards tell whether it was rasterized
        // for the theme in force or for the one on its way out.
        self.document_tone = tone;
        self.update(RepositoryAction::PdfPage {
            card,
            page,
            tone,
            zoom,
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        match &self.document {
            DocumentState::Editable(document) => document.content.selection(),
            _ => None,
        }
    }

    pub fn list_directory(&mut self, card: CardId, path: PathBuf) -> Task<RepositoryAction> {
        if !self.directory_only {
            return Task::none();
        }
        if self.listed_directories.contains(&path) {
            return Task::none();
        }
        self.listed_directories.insert(path.clone());
        self.directory_listing_task(card, path)
    }

    /// Re-lists every directory the user has expanded.
    ///
    /// A refresh replaces the root listing; without this the tree would keep
    /// showing whatever the children looked like when they were first
    /// expanded, and [`RepositoryRuntime::list_directory`]'s memo would never
    /// let them be read again.
    fn relist_expanded_directories(&mut self, card: CardId) -> Task<RepositoryAction> {
        if !self.directory_only {
            return Task::none();
        }
        let expanded: Vec<PathBuf> = self
            .listed_directories
            .iter()
            .filter(|path| !path.as_os_str().is_empty())
            .cloned()
            .collect();
        Task::batch(
            expanded
                .into_iter()
                .map(|path| self.directory_listing_task(card, path)),
        )
    }

    fn directory_listing_task(&self, card: CardId, path: PathBuf) -> Task<RepositoryAction> {
        let identity = AsyncIdentity {
            card,
            worktree: self.descriptor.worktree.clone(),
            path,
            generation: self.file_list_generation,
        };
        let worktree = identity.worktree.clone();
        let relative = identity.path.clone();
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || load_directory_listing(&worktree, &relative))
                    .await
                    .map_err(|error| format!("folder listing failed: {error}"))
                    .and_then(|result| result);
            RepositoryAction::DirectoryListed { identity, result }
        })
    }

    /// Folds a fresh root listing into the folder tree already on screen.
    ///
    /// A folder tab's expanded children exist only as rows appended by
    /// [`RepositoryAction::DirectoryListed`], and
    /// [`load_directory_files`] lists the root level alone, so installing its
    /// result wholesale is what snapped every open folder shut on each
    /// ten-second refresh tick. Rows under a still-expanded directory are
    /// kept; rows whose parent has gone are dropped, and the expansion that
    /// named it is forgotten with them.
    fn merge_directory_listing(&mut self, root: Vec<RepositoryFile>) -> Vec<RepositoryFile> {
        if !self.directory_only || self.listed_directories.len() <= 1 {
            return root;
        }
        let FileListState::Ready(previous) = &self.files else {
            return root;
        };
        let mut merged = root;
        merged.extend(
            previous
                .iter()
                .filter(|file| {
                    file.path
                        .parent()
                        .is_some_and(|parent| !parent.as_os_str().is_empty())
                })
                .cloned(),
        );
        merged.sort_by(|left, right| left.path.cmp(&right.path));
        merged.dedup_by(|left, right| left.path == right.path);
        // A parent sorts before its children, so one pass decides which
        // expansions still have a directory to hang from.
        let listed = std::mem::take(&mut self.listed_directories);
        let mut expanded = BTreeSet::from([PathBuf::new()]);
        merged.retain(|file| {
            if !expanded.contains(file.path.parent().unwrap_or(Path::new(""))) {
                return false;
            }
            if file.directory && listed.contains(&file.path) {
                expanded.insert(file.path.clone());
            }
            true
        });
        self.listed_directories = expanded;
        merged
    }

    /// Performs the editor context menu's clipboard commands.
    ///
    /// Implemented here rather than deferred to the host because both shells
    /// mount the same menu from [`editable_document_view`], and a host that
    /// declines to act leaves four enabled-looking dead menu items. A host
    /// that wants its own behaviour still has one: it can intercept
    /// [`RepositoryAction::Clipboard`] before forwarding it here.
    fn clipboard(&mut self, card: CardId, op: ClipboardOp) -> Task<RepositoryAction> {
        if self.source_surface == SourceSurface::Diff {
            return match op {
                ClipboardOp::Copy => self.copy_diff(card, crate::diff_interaction::Command::Copy),
                ClipboardOp::SelectAll => {
                    self.select_all_diff();
                    Task::none()
                }
                _ => Task::none(),
            };
        }
        match op {
            ClipboardOp::Copy => self
                .selected_text()
                .map_or_else(Task::none, iced::clipboard::write),
            ClipboardOp::Cut => {
                let Some(text) = self.selected_text() else {
                    return Task::none();
                };
                Task::batch([
                    iced::clipboard::write(text),
                    self.update(RepositoryAction::EditorAction {
                        card,
                        action: text_editor::Action::Edit(text_editor::Edit::Delete),
                    }),
                ])
            }
            // `clipboard::read` yields nothing at all for an empty clipboard,
            // so there is no empty paste to guard against here.
            ClipboardOp::Paste => iced::clipboard::read().and_then(move |text: String| {
                cosmic::task::message(RepositoryAction::EditorAction {
                    card,
                    action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text))),
                })
            }),
            ClipboardOp::SelectAll => self.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::SelectAll,
            }),
        }
    }

    pub fn search_directory(&mut self, card: CardId, query: String) -> Task<RepositoryAction> {
        if !self.directory_only {
            return Task::none();
        }
        self.file_search_cancel.cancel();
        if query.trim().is_empty() {
            return Task::none();
        }
        let token = CancellationToken::new();
        self.file_search_cancel = token.clone();
        self.file_search_generation = self.file_search_generation.wrapping_add(1).max(1);
        let generation = self.file_search_generation;
        let worktree = self.descriptor.worktree.clone();
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            if token.is_cancelled() {
                return RepositoryAction::DirectorySearchLoaded {
                    card,
                    generation,
                    query,
                    result: Ok((Vec::new(), 0)),
                };
            }
            let search_token = token.clone();
            let search_query = query.clone();
            let result = tokio::task::spawn_blocking(move || {
                search_directory_files(&worktree, &search_query, &search_token)
            })
            .await
            .map_err(|error| format!("file search failed: {error}"))
            .and_then(|result| result);
            RepositoryAction::DirectorySearchLoaded {
                card,
                generation,
                query,
                result,
            }
        })
    }

    pub fn file_search_generation(&self) -> u64 {
        self.file_search_generation
    }

    pub fn cancel_file_search(&mut self) {
        self.file_search_cancel.cancel();
        self.file_search_generation = self.file_search_generation.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn post_persist_failure_fixture(card: CardId) -> (Self, RepositoryAction) {
        let path = PathBuf::from("notes.txt");
        let descriptor = RepositoryView {
            selected_file: None,
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
            directory: false,
        }]);
        runtime.selected = runtime.file(&path).cloned();
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 1,
        };
        let mut document =
            EditorDocument::unlinked(path, "A\n".into(), blake3::hash(b"A\n"), false);
        document.content = text_editor::Content::with_text("A\n");
        document.dirty = false;
        document.revision = 2;
        document.active_save = Some(SaveTicket {
            id: 1,
            identity: identity.clone(),
            revision: 1,
            history_revision: if content_matches(&document.content, "B\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("B\n").text_snapshot(),
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
                    directory: false,
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
            selected_file: None,
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
                    directory: false,
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
        let (mut runtime, identity) = Self::open_fixture(card, "notes.txt", text);
        runtime.set_lsp_document(Some(LspDocumentId(0)));
        (runtime, identity)
    }

    /// The same, under a chosen file name.
    #[cfg(test)]
    pub(crate) fn open_fixture(card: CardId, name: &str, text: &str) -> (Self, AsyncIdentity) {
        let (mut runtime, identity) = Self::selection_fixture(card, name);
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: identity.clone(),
            result: Ok(LoadedFile {
                source: identity.path.clone(),
                content: FileContent::Editable {
                    text: text.into(),
                    content_hash: blake3::hash(text.as_bytes()),
                    markdown: false,
                },
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

    /// Apply only navigation for this exact pane recipe. Shared projections can
    /// replace the recipe but cannot replace another window's local selection.
    pub(crate) fn restore_navigation(&mut self, navigation: &crate::model::RepositoryNavigation) {
        if navigation.project != self.descriptor.project
            || navigation.worktree != self.descriptor.worktree
            || navigation
                .selected_file
                .as_ref()
                .is_none_or(|path| crate::repository::RepoPath::parse(path).is_err())
            || self
                .descriptor
                .selected_file
                .as_ref()
                .is_some_and(|path| Some(path) != navigation.selected_file.as_ref())
        {
            return;
        }
        self.restored_navigation = Some(navigation.clone());
    }

    pub(crate) fn navigation(&self) -> Option<crate::model::RepositoryNavigation> {
        if self.committed_selection.is_some() {
            return None;
        }
        let file = self.selected.as_ref()?;
        let editor = if let DocumentState::Editable(document) = &self.document {
            let cursor = document.content.cursor();
            let position = |position: text_editor::Position| crate::model::EditorPosition {
                line: position.line,
                column: position.column,
            };
            let scroll = if self.editable_surface_visible() {
                document.content.scroll_position()
            } else {
                self.viewports.editor
            };
            Some(crate::model::EditorNavigation {
                caret: position(cursor.position),
                selection: cursor.selection.map(position),
                scroll_line: scroll.line,
                scroll_vertical: scroll.vertical,
                scroll_horizontal: scroll.horizontal,
            })
        } else {
            self.pending_editor_navigation
                .as_ref()
                .filter(|(path, _)| *path == file.path)
                .map(|(_, editor)| *editor)
        };
        Some(crate::model::RepositoryNavigation {
            project: self.descriptor.project,
            worktree: self.descriptor.worktree.clone(),
            selected_file: Some(file.path.clone()),
            editor,
        })
    }

    pub fn initial_load(&mut self, card: CardId) -> Task<RepositoryAction> {
        if (self.descriptor.selected_file.is_some()
            || self
                .restored_navigation
                .as_ref()
                .is_some_and(|navigation| navigation.selected_file.is_some()))
            && let Err(error) = self.protect_editor_worktree(&self.descriptor.worktree.clone())
        {
            self.files = FileListState::Failed(error.to_string());
            return Task::none();
        }
        if let Some(navigation) = self.restored_navigation.take() {
            let listing = if self.descriptor.selected_file.is_none() {
                // The direct file read can precede the initial Git listing.
                // Install that listing's current status without replacing the buffer.
                self.mutation_refresh_pending = true;
                self.initial_load(card)
            } else {
                self.files = FileListState::Ready(Vec::new());
                Task::none()
            };
            if let Some(path) = navigation.selected_file {
                let load = self.open_file_entry(
                    card,
                    RepositoryFile {
                        path: path.clone(),
                        ..RepositoryFile::default()
                    },
                );
                self.pending_editor_navigation = navigation.editor.map(|editor| (path, editor));
                return Task::batch([listing, load]);
            }
            return listing;
        }
        if let Some(path) = self.descriptor.selected_file.clone() {
            return if self.blocks_navigation() {
                Task::none()
            } else {
                self.files = FileListState::Ready(Vec::new());
                self.open_file(card, path)
            };
        }
        if self.file_list_read_outstanding() {
            self.file_list_refresh_queued = true;
            return Task::none();
        }
        self.files = FileListState::Loading;
        // An explicit load has already thrown the mounted list away, so the
        // "nothing changed" short-circuit would leave the card on `Loading`
        // forever. Only a background refresh, which keeps its list, may take
        // that path.
        self.file_list_digest = None;
        self.background_file_refresh = false;
        self.load_files(card)
    }

    /// Reuses a clean file pane without resetting async generations: an older
    /// read must not win after links switch A → B → A before loading finishes.
    pub(crate) fn retarget_file(
        &mut self,
        card: CardId,
        view: RepositoryView,
    ) -> Task<RepositoryAction> {
        if self.descriptor.selected_file.is_none() || self.blocks_navigation() {
            return Task::none();
        }
        if let Err(error) = self.protect_editor_worktree(&view.worktree) {
            self.files = FileListState::Failed(error.to_string());
            return Task::none();
        }
        self.descriptor = view;
        self.retained_viewports.clear();
        self.initial_load(card)
    }

    /// Poll only visible panes. Keep the mounted list, viewport, and document
    /// intact while discovering external changes, and never overlap a read.
    pub fn refresh_visible(&mut self, card: CardId) -> Task<RepositoryAction> {
        let diff = self.refresh_diff_visibility(card, true);
        if self.descriptor.selected_file.is_some() || self.file_list_read_outstanding() {
            return diff;
        }
        self.background_file_refresh = true;
        let files = self.load_files(card);
        Task::batch([diff, files])
    }

    /// Whether a file-list read is genuinely still in flight.
    ///
    /// A read whose result the shell dropped would otherwise wedge this card
    /// on `file_list_loading` for the rest of the session, and a card that
    /// silently stops refreshing is a far worse failure than one redundant
    /// Git read a minute.
    fn file_list_read_outstanding(&self) -> bool {
        self.file_list_loading
            && self
                .file_list_started
                .is_some_and(|started| started.elapsed() < LOST_FILE_LIST_READ)
    }

    fn load_files(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.file_list_loading = true;
        self.file_list_started = Some(Instant::now());
        self.file_list_generation = self.file_list_generation.wrapping_add(1).max(1);
        // The expanded set is deliberately *not* cleared. In a folder tab the
        // children of an expanded directory exist only as rows this memo
        // admitted, so forgetting them here is what made every open folder
        // snap shut on each ten-second refresh tick. The root listing is
        // merged into the existing tree instead, and every retained
        // expansion is re-listed so external changes still show up.
        self.listed_directories.insert(PathBuf::new());
        let identity = AsyncIdentity {
            card,
            worktree: self.descriptor.worktree.clone(),
            path: PathBuf::from("."),
            generation: self.file_list_generation,
        };
        files_task(
            identity,
            self.directory_only,
            self.file_list_digest,
            Arc::clone(&self.file_list_deposit),
        )
    }

    /// Refreshes Git's status and mutable comparisons without replacing the
    /// open buffer or changing the identity of its loads and saves.
    pub fn refresh_after_mutation(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.mutation_refresh_pending = true;
        if let Some(path) = self.selected.as_ref().map(|file| file.path.clone())
            && let Some(index) = self.diffs.iter().position(|(basis, state)| {
                *basis == self.diff_basis
                    && !basis.is_immutable()
                    && matches!(state, DiffState::Ready(..))
            })
        {
            let (basis, state) = self.diffs.remove(index);
            self.stale_diff = Some((path, basis, state));
        }
        // Resolved branch ranges and commits name immutable objects.
        self.diffs.retain(|(basis, _)| basis.is_immutable());
        self.initial_load(card)
    }

    fn refresh_selected_status(&mut self, card: CardId) -> Task<RepositoryAction> {
        let Some(path) = self.selected.as_ref().map(|file| file.path.clone()) else {
            return Task::none();
        };
        // A discarded untracked file or committed deletion can disappear from
        // the list entirely. Keep its buffer, but remove all obsolete Git facts.
        // The row stays the path that was opened. A link's comparisons follow
        // the target, which is decided again below from the buffer.
        let file = self.file(&path).cloned().unwrap_or_else(|| RepositoryFile {
            path: path.clone(),
            ..RepositoryFile::default()
        });
        self.selected = Some(file);
        let Some(subject) = self.subject_file() else {
            return Task::none();
        };
        if !self.file_has_basis(&subject, &self.diff_basis) && self.basis_resolution.is_none() {
            self.diff_basis = if subject.unstaged {
                DiffBasis::IndexToWorktree
            } else {
                DiffBasis::HeadToIndex
            };
            if !subject.changed() {
                self.source_surface = SourceSurface::Editor;
            }
            self.reset_diff_viewports();
        }
        self.ensure_current_diff(card)
    }

    /// Removes and returns every protocol notification the open buffer has
    /// earned since the last drain, oldest first.
    ///
    /// The application performs them in order against its
    /// [`crate::lsp::LspManager`]. Draining is destructive because a
    /// notification performed twice is worse than one performed late.
    pub fn take_lsp_intents(&mut self) -> Vec<LspIntent> {
        self.flush_lsp_change();
        std::mem::take(&mut self.lsp_intents)
    }

    /// The editor edits behind the `didChange` this drain carries, oldest
    /// first, so the boundary can send content ranges instead of the whole
    /// document.
    ///
    /// Empty means the transition cannot be described as ranges — a buffer
    /// rebuilt wholesale, a keystroke this runtime does not model — and the
    /// caller must send the full text, which [`LspIntent::Change`] carries
    /// regardless.
    ///
    /// Drain *after* [`RepositoryRuntime::take_lsp_intents`]: that call is
    /// what materialises the change these edits describe.
    #[must_use]
    pub fn take_lsp_edits(&mut self) -> Vec<LspTextEdit> {
        if !std::mem::replace(&mut self.lsp_edits_exact, true) {
            self.lsp_edits.clear();
            return Vec::new();
        }
        std::mem::take(&mut self.lsp_edits)
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
        if self.lsp_document != doc {
            self.language = EditorLanguageState::default();
        }
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
        caret_position(&document.content, document.content.cursor().position)
    }

    pub fn diagnostic_navigation_available(&self)->bool {
        self.editable_surface_visible() && !self.language.diagnostics.is_empty()
    }

    pub fn set_editor_diagnostics(&mut self, diagnostics: Vec<crate::lsp::LspDiagnostic>) {
        self.language.marks = if let DocumentState::Editable(document) = &self.document {
            diagnostics
                .iter()
                .filter_map(|diagnostic| {
                    Some(text_editor::Diagnostic {
                        start: caret_for_position(&document.content, diagnostic.range.start)?,
                        end: caret_for_position(&document.content, diagnostic.range.end)?,
                        severity: diagnostic.severity,
                    })
                })
                .collect()
        } else {
            vec![]
        };
        self.language.diagnostics = diagnostics;
        self.language.selected_diagnostic = None;
    }

    fn navigate_diagnostic(&mut self, forward: bool) {
        let Some(at) = self.cursor_position() else { return; };
        let mut order: Vec<_> = self.language.diagnostics.iter().enumerate().map(|(index,item)| (item.range.start,index)).collect();
        order.sort_by_key(|entry| entry.0);
        if order.is_empty() { return; }
        let current = self.language.selected_diagnostic.and_then(|selected| order.iter().position(|(position,index)| *index == selected && *position == at));
        let next = if let Some(current) = current {
            if forward { (current + 1) % order.len() } else { (current + order.len() - 1) % order.len() }
        } else if forward {
            order.iter().position(|(position,_)| *position > at).unwrap_or(0)
        } else {
            order.iter().rposition(|(position,_)| *position < at).unwrap_or(order.len() - 1)
        };
        let (position,index) = order[next];
        let message = self.language.diagnostics[index].message.clone();
        if self.move_cursor_to(position) {
            self.dismiss_editor_results();
            self.set_editor_hover(Some(&message), Some(position));
            self.language.selected_diagnostic = Some(index);
        }
    }

    pub fn set_editor_completions(&mut self, items: Vec<LspCompletionItem>) {
        self.language.selected = items.iter().position(|item| item.preselect).unwrap_or(0);
        self.language.completions = items;
        self.language.hover.clear();
    }

    pub fn set_editor_hover(&mut self, markdown: Option<&str>, at: Option<LspPosition>) {
        self.language.hover = markdown.map_or_else(Vec::new, |raw| markdown::parse(raw).collect());
        self.language.hover_at = at.and_then(|at| match &self.document {
            DocumentState::Editable(document) => caret_for_position(&document.content, at),
            _ => None,
        });
    }

    pub fn dismiss_editor_results(&mut self) {
        self.language.completions.clear();
        self.language.hover.clear();
        self.language.hover_at = None;
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
        let Some(position) = caret_for_position(&document.content, at) else {
            return false;
        };
        // Typing after a jump is a new undo step, like typing after a click.
        document.history.seal();
        document.content.set_cursor(text_editor::Cursor {
            position,
            selection: None,
        });
        true
    }
    /// Keep the search focus policy while allowing normal buffer editing.
    pub fn set_search_mode(&mut self, enabled: bool) {
        self.search_mode = enabled;
    }

    pub fn set_find_preferences(
        &mut self,
        options: crate::editor_search::FindOptions,
        preserve_case: bool,
    ) {
        self.replace_preserve_case = preserve_case;
        if self.find_options != options {
            self.find_options = options;
            self.refresh_find_matches();
        }
    }

    pub fn find_preferences(&self) -> (crate::editor_search::FindOptions, bool) {
        (self.find_options, self.replace_preserve_case)
    }

    fn current_selection_scope(&self) -> Option<FindScope> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        let cursor = document.content.selected_cursor();
        let anchor = cursor.selection?;
        let first = (anchor.line, anchor.column);
        let last = (cursor.position.line, cursor.position.column);
        (first != last).then(|| (first.min(last), first.max(last)))
    }

    /// Selects one case-insensitive occurrence in the open buffer and returns
    /// the total number of occurrences.
    pub fn select_text_match(&mut self, query: &str, occurrence: usize) -> Option<usize> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        let key = FindKey {
            options: crate::editor_search::FindOptions::default(),
            selection: None,
            generation: self.generation,
            revision: document.revision,
            query: query.to_owned(),
        };
        if self.find_cache.as_ref() != Some(&key) {
            self.find_matches = document.content.find_matches(query);
            self.find_cache = Some(key);
        }
        self.select_cached_match(occurrence)
    }

    fn select_cached_match(&mut self, occurrence: usize) -> Option<usize> {
        let DocumentState::Editable(document) = &mut self.document else {
            return None;
        };
        document.history.seal();
        let total = self.find_matches.len();
        if total == 0 {
            document
                .content
                .perform(text_editor::Action::ClearSelection);
            return None;
        }
        let (start, end) = self.find_matches[occurrence % total];
        document.content.move_to(text_editor::Cursor {
            position: end,
            selection: Some(start),
        });
        Some(total)
    }

    /// Selects the first case-insensitive occurrence in the open buffer.
    pub fn select_text(&mut self, query: &str) -> bool {
        self.select_text_match(query, 0).is_some()
    }

    /// The focus target of the editable buffer while that buffer is the
    /// surface actually on screen, and `None` while it is not.
    #[must_use]
    pub fn editor_focus_target(&self, card: CardId) -> Option<widget::Id> {
        self.editable_surface_visible()
            .then(|| editor_focus_id(card))
    }

    pub fn restores_editor_navigation(&self, identity: &AsyncIdentity) -> bool {
        self.accepts_selected(identity)
            && self
                .pending_editor_navigation
                .as_ref()
                .is_some_and(|(path, _)| path == &identity.path)
    }

    /// The accepted restored buffer keeps its focus handoff until the app can
    /// mount it. Reading the file can finish before the first viewport arrives.
    pub fn take_restored_editor_focus(&mut self, card: CardId) -> Option<widget::Id> {
        let identity = self.pending_editor_focus.as_ref()?;
        if identity.card != card || !self.accepts_selected(identity) {
            self.pending_editor_focus = None;
            return None;
        }
        let target = self.editor_focus_target(card)?;
        self.pending_editor_focus = None;
        Some(target)
    }

    /// Whether the editable buffer is the surface actually on screen.
    ///
    /// Only Edit mounts it. The comparison surface renders the diff model, and
    /// Markdown preview renders the parsed document, so neither has a buffer
    /// to focus or search.
    fn editable_surface_visible(&self) -> bool {
        let DocumentState::Editable(_) = &self.document else {
            return false;
        };
        self.source_surface == SourceSurface::Editor && self.markdown_mode != MarkdownMode::Preview
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

    /// Whether the find strip is mounted over the open buffer.
    ///
    /// The shell consults this to route Ctrl+F and Escape: a card already
    /// showing find owns them, and one that is not leaves them to the
    /// project-wide meaning.
    #[must_use]
    pub fn find_is_open(&self) -> bool {
        self.find_open
    }

    pub fn find_input_is_focused(&self) -> bool {
        self.find_open && self.find_input_focused
    }

    /// Mounts the find strip and selects the occurrence the user is looking
    /// at.
    ///
    /// Seeding from the selection is why Ctrl+F feels instant, and starting
    /// from the caret rather than from occurrence zero is why it does not
    /// throw a reader a thousand lines back up the file. A selection
    /// spanning lines seeds nothing: this search is a substring search, and
    /// a paragraph is not a term anyone meant to look for.
    fn open_find(&mut self, card: CardId) -> Task<RepositoryAction> {
        if self.source_surface == SourceSurface::Diff {
            return self.open_diff_find(card);
        }
        if !self.editable_surface_visible() {
            return Task::none();
        }
        if self.find_open {
            return iced::widget::operation::focus(find_focus_id(card));
        }
        if let Some(selection) = self.selected_text()
            && !selection.trim().is_empty()
            && !selection.contains('\n')
        {
            self.find_query = selection;
        }
        self.find_selection_candidate = self.current_selection_scope();
        self.find_open = true;
        self.set_search_mode(true);
        if let DocumentState::Editable(document) = &self.document {
            let cursor = document.content.cursor();
            let origin = cursor.selection.map_or(cursor.position, |anchor| {
                if (anchor.line, anchor.column) < (cursor.position.line, cursor.position.column) {
                    anchor
                } else {
                    cursor.position
                }
            });
            self.find_origin = Some((origin.line, origin.column));
        }
        self.prepare_find_matches();
        let occurrence = self.occurrence_from_caret();
        self.select_find_match(occurrence);
        if self.find_cache.as_ref() == self.wanted_find().as_ref() {
            self.find_origin = None;
        }
        iced::widget::operation::focus(find_focus_id(card))
    }

    /// Dismisses the find strip and gives the keyboard back to the buffer.
    fn close_find(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.clear_find();
        self.focus_editor(card)
    }

    /// Drops the strip and its transient match/selection state.
    fn clear_find(&mut self) {
        self.clear_diff_find();
        self.find_open = false;
        self.find_query.clear();
        self.find_index = 0;
        self.find_total = 0;
        self.find_input_focused = false;
        self.find_matches.clear();
        self.find_cache = None;
        self.find_snapshot = None;
        self.find_origin = None;
        self.find_error = None;
        self.find_selection = None;
        self.find_selection_candidate = None;
        self.find_select_on_finish = false;
        if let Some(active) = &self.find_active {
            active.cancellation.cancel();
        }
        self.set_search_mode(false);
    }

    /// Selects one occurrence and records the count that search reported,
    /// which is the only place the count is ever computed.
    fn select_find_match(&mut self, occurrence: usize) {
        if self.source_surface == SourceSurface::Diff {
            self.select_diff_match(occurrence);
            return;
        }
        self.prepare_find_matches();
        if self.find_cache.as_ref() != self.wanted_find().as_ref() {
            // Keep the requested occurrence while a revision is re-searched;
            // an empty pending result is not a completed zero-match search.
            self.find_index = occurrence;
            self.find_select_on_finish = true;
            return;
        }
        let total = self.select_cached_match(occurrence);
        match total {
            Some(total) => {
                self.find_index = occurrence % total;
                self.find_total = total;
            }
            // No query, or none of it in the buffer: the search already
            // cleared the selection, and there is no count to show.
            None => {
                self.find_index = 0;
                self.find_total = 0;
            }
        }
    }

    fn wanted_find(&self) -> Option<FindKey> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        self.find_open.then(|| FindKey {
            options: self.find_options,
            selection: self.find_selection,
            generation: self.generation,
            revision: document.revision,
            query: self.find_query.clone(),
        })
    }

    /// Small files search immediately. Larger files clear stale highlights and
    /// let `find_task` scan a reusable immutable snapshot off the update thread.
    fn prepare_find_matches(&mut self) {
        if self.source_surface == SourceSurface::Diff {
            self.prepare_diff_find();
            return;
        }
        let Some(wanted) = self.wanted_find() else {
            return;
        };
        if let Some(active) = &self.find_active
            && active.key != wanted
        {
            active.cancellation.cancel();
        }
        if self.find_cache.as_ref() == Some(&wanted) {
            return;
        }
        self.find_matches.clear();
        self.find_total = 0;
        self.find_error = None;
        let DocumentState::Editable(document) = &mut self.document else {
            return;
        };
        if wanted.query.is_empty()
            || (document.content.line_count() <= 2048 && document.content.text_len() <= 64 * 1024)
        {
            match document
                .content
                .find_matches_with(&wanted.query, wanted.options)
            {
                Ok(mut matches) => {
                    scoped_matches(&mut matches, wanted.selection);
                    self.find_matches = matches;
                }
                Err(error) => self.find_error = Some(error),
            }
            self.find_cache = Some(wanted);
        }
    }

    /// Only one CPU worker is outstanding, including after cancellation. A
    /// completed superseded query releases that slot before the latest starts.
    fn find_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        if self.source_surface == SourceSurface::Diff {
            return self.diff_find_task(card);
        }
        self.prepare_find_matches();
        let Some(key) = self.wanted_find() else {
            return Task::none();
        };
        if self.find_cache.as_ref() == Some(&key) || self.find_active.is_some() {
            return Task::none();
        }
        let DocumentState::Editable(document) = &self.document else {
            return Task::none();
        };
        if self
            .find_snapshot
            .as_ref()
            .is_none_or(|(generation, revision, _)| {
                *generation != key.generation || *revision != key.revision
            })
        {
            self.find_snapshot = Some((
                key.generation,
                key.revision,
                Arc::new(document.content.search_snapshot()),
            ));
        }
        let snapshot = Arc::clone(&self.find_snapshot.as_ref().unwrap().2);
        // A card can replace its runtime while an already-completed task is
        // waiting for delivery. A fresh identity fences that old callback too.
        let ticket = uuid::Uuid::new_v4();
        let cancellation = CancellationToken::new();
        self.find_active = Some(FindJob {
            key: key.clone(),
            ticket,
            cancellation: cancellation.clone(),
        });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                snapshot
                    .find_matches_with(&key.query, key.options, || cancellation.is_cancelled())
                    .map(|result| {
                        result.map(|mut matches| {
                            scoped_matches(&mut matches, key.selection);
                            matches
                        })
                    })
            })
            .await
            .map_err(|error| format!("Find worker failed: {error}"))
            .and_then(std::convert::identity);
            RepositoryAction::FindLoaded {
                card,
                ticket,
                result,
            }
        })
    }

    fn finish_find(
        &mut self,
        ticket: uuid::Uuid,
        result: Result<Option<Vec<(text_editor::Position, text_editor::Position)>>, String>,
    ) {
        if self
            .find_active
            .as_ref()
            .is_none_or(|active| active.ticket != ticket)
        {
            return;
        }
        let active = self.find_active.take().unwrap();
        if active.cancellation.is_cancelled() || self.wanted_find().as_ref() != Some(&active.key) {
            return;
        }
        match result {
            Ok(Some(matches)) => self.find_matches = matches,
            Ok(None) => return,
            Err(error) => self.find_error = Some(error),
        }
        self.find_cache = Some(active.key.clone());
        let occurrence = if self.find_origin.is_some() {
            self.occurrence_from_caret()
        } else {
            self.find_index
        };
        self.find_origin = None;
        if std::mem::take(&mut self.find_select_on_finish) {
            self.select_find_match(occurrence);
        } else {
            self.find_total = self.find_matches.len();
            self.find_index = self.find_index.min(self.find_total.saturating_sub(1));
        }
    }

    /// Renews the cached count after something changed the buffer under an
    /// open find strip, clamping the selected occurrence into the new total.
    ///
    /// Without this the strip keeps claiming "3 of 7" over text that no
    /// longer contains seven of anything.
    fn refresh_find_matches(&mut self) {
        if self.source_surface == SourceSurface::Diff {
            self.select_diff_match(self.find_index);
            return;
        }
        if !self.find_open {
            return;
        }
        self.find_select_on_finish = false;
        self.prepare_find_matches();
        self.find_total = self.find_matches.len();
        if self.find_cache.as_ref() == self.wanted_find().as_ref() {
            self.find_index = self.find_index.min(self.find_total.saturating_sub(1));
        }
    }

    /// Steps to the next or previous occurrence.
    ///
    /// Going backwards is written against the known total rather than as
    /// `wrapping_sub`: `usize::MAX % total` is `total - 1` only when `total`
    /// divides 2^64, so a wrapped index re-selects occurrence zero for a
    /// total of three or five and Previous appears to do nothing at all.
    fn step_find(&mut self, forward: bool) {
        if self.find_total == 0 {
            return;
        }
        let occurrence = if forward {
            (self.find_index + 1) % self.find_total
        } else {
            self.find_index
                .checked_sub(1)
                .unwrap_or(self.find_total.saturating_sub(1))
        };
        self.select_find_match(occurrence);
    }

    /// Which occurrence of the current query the caret is sitting at or
    /// before, and zero when the caret is past the last one — a search that
    /// runs off the end of the buffer wraps to the top.
    ///
    /// A selection is measured from its own start, so opening find on a
    /// selected word keeps that very word selected instead of jumping to the
    /// next copy of it.
    fn occurrence_from_caret(&self) -> usize {
        let DocumentState::Editable(document) = &self.document else {
            return 0;
        };
        let query = &self.find_query;
        if query.is_empty() {
            return 0;
        }
        let cursor = document.content.cursor();
        let mut origin = (cursor.position.line, cursor.position.column);
        if let Some(anchor) = cursor.selection {
            origin = origin.min((anchor.line, anchor.column));
        }
        origin = self.find_origin.unwrap_or(origin);
        self.find_matches
            .iter()
            .take_while(|(start, _)| (start.line, start.column) < origin)
            .count()
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
        if self.source_surface != SourceSurface::Editor {
            return false;
        }
        // A keystroke recorded but not yet materialised describes the buffer
        // as it was before this completion; it has to be said first.
        self.flush_lsp_change();
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
            edits.push((span, edit.new_text.as_str(), edit.range));
        }
        let primary = edits[0].0.clone();
        edits.sort_by(|left, right| left.0.start.cmp(&right.0.start));
        if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
            // Overlapping edits have no defined joint meaning; applying them
            // in either order would invent one.
            return false;
        }

        let mut updated = text.clone();
        for (span, new_text, _) in edits.iter().rev() {
            updated.replace_range(span.clone(), new_text);
        }
        // The caret belongs after the text the user chose. Edits that sit
        // before the primary one displace it by however much they inserted
        // less however much they removed; none of them can reach past
        // `primary.start`, having just been proven disjoint, so this cannot
        // underflow.
        let (inserted, removed) = edits
            .iter()
            .filter(|(span, _, _)| span.start < primary.start)
            .fold((0, 0), |(inserted, removed), (span, new_text, _)| {
                (inserted + new_text.len(), removed + (span.end - span.start))
            });
        let caret = primary.start + item.edit.new_text.len() + inserted - removed;

        // One version for the whole application: the server is told about a
        // completion as the single document transition the user performed, not
        // as one transition per edit it happened to decompose into.
        let version = document.bump_lsp_version();
        let before = document.content.cursor();
        let changes = document.splice(
            &text,
            edits
                .iter()
                .rev()
                .map(|(span, new_text, _)| (span.clone(), *new_text)),
        );
        if changes.is_none() {
            document.replace_text(&updated);
        }
        debug_assert_eq!(document.content.text(), updated);
        // Resolved against the edited buffer, so the caret lands where the
        // editor now believes that offset to be.
        let position = caret_for_offset(&document.content, caret);
        document.content.set_cursor(text_editor::Cursor {
            position,
            selection: None,
        });
        if let Some(changes) = changes {
            // One step, like the one choice the user made.
            document.history.record_group(
                changes,
                before,
                document.content.cursor(),
                Instant::now(),
            );
            document.text_changed();
        }
        if let Some(version) = version {
            // Recorded highest-range-first, the order they were applied in:
            // each range then still addresses the document the one before it
            // produced.
            self.lsp_edits
                .extend(edits.iter().rev().map(|(_, new_text, range)| LspTextEdit {
                    range: *range,
                    new_text: (*new_text).to_owned(),
                }));
            self.lsp_intents.push(LspIntent::Change {
                version,
                text: updated,
            });
        }
        // The one buffer mutation that does not pass through
        // `push_lsp_change`, because it owns its own version bookkeeping —
        // and therefore the one that has to renew the find count by hand.
        self.refresh_find_matches();
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
        self.flush_lsp_change();
        // A new baseline: ranges measured against the previous document
        // address nothing here.
        self.lsp_edits.clear();
        self.lsp_edits_exact = true;
        let DocumentState::Editable(document) = &self.document else {
            return;
        };
        self.lsp_intents.push(LspIntent::Open {
            absolute_path: worktree.join(&document.source),
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
        self.file(path).map_or_else(
            || self.directory_only && crate::repository::RepoPath::parse(path).is_ok(),
            |file| !file.directory,
        )
    }

    pub fn listed_files(&self) -> &[RepositoryFile] {
        match &self.files {
            FileListState::Ready(files) => files,
            _ => &[],
        }
    }
    #[must_use]
    pub fn is_directory_only(&self) -> bool {
        self.directory_only
    }

    /// Whether the runtime compares a fetched pull request range. The
    /// working tree plays no part in that comparison, so staging and the
    /// basis switch are left out of the toolbar.
    pub fn reviews_pull_request(&self) -> bool {
        matches!(self.diff_basis, DiffBasis::PullRequest { .. })
    }

    pub fn shows_diff(&self) -> bool {
        self.source_surface == SourceSurface::Diff
    }

    pub fn selected_diff_basis(&self) -> &DiffBasis {
        &self.diff_basis
    }

    pub fn selected_path(&self) -> Option<&Path> {
        self.selected.as_ref().map(|file| file.path.as_path())
    }

    /// The file the open buffer really is, when the selected path is a
    /// symbolic link to it; `None` for a buffer that is its own file.
    ///
    /// The chrome shows this so a reader editing `AGENTS.md` can see that the
    /// keystrokes land in `CLAUDE.md`, which is also where a save will be
    /// reported by the file list.
    #[must_use]
    pub fn linked_source(&self) -> Option<&Path> {
        let source = match &self.document {
            DocumentState::Editable(document) => &document.source,
            DocumentState::Image { source, .. }
            | DocumentState::Pdf { source, .. }
            | DocumentState::ReadOnly { path: source, .. } => source,
            DocumentState::None | DocumentState::Loading | DocumentState::Failed { .. } => {
                return None;
            }
        };
        let selected = self.selected.as_ref()?;
        (*source != selected.path).then_some(source.as_path())
    }

    /// The file the open buffer's bytes belong to, when that is not the row.
    ///
    /// A refused link stays on the row: its source names the link that failed,
    /// not a file Git can compare. An editable, image, or document buffer
    /// names the target the bytes were read from.
    fn followed_source(&self) -> Option<&Path> {
        let selected = self.selected.as_ref()?;
        let source = match &self.document {
            DocumentState::Editable(document) => &document.source,
            DocumentState::Image { source, .. } | DocumentState::Pdf { source, .. } => source,
            _ => return None,
        };
        (source != &selected.path).then_some(source.as_path())
    }

    /// Git's facts for the open buffer.
    ///
    /// A link aliases the target's row while it is open, so a diff, a gutter
    /// baseline, and stage or unstage describe that file. The selected row
    /// stays the link, which is what a result is matched against. A target
    /// the list does not name has no index facts, and staging stays hidden.
    fn subject_file(&self) -> Option<RepositoryFile> {
        let selected = self.selected.as_ref()?;
        let Some(source) = self.followed_source() else {
            return Some(selected.clone());
        };
        Some(
            self.file(source)
                .cloned()
                .unwrap_or_else(|| RepositoryFile {
                    path: source.to_path_buf(),
                    ..RepositoryFile::default()
                }),
        )
    }

    /// The path a comparison of the open buffer asks Git to read.
    fn diff_pathspec(&self) -> Option<PathBuf> {
        self.subject_file().map(|file| file.path)
    }

    /// Whether the open buffer's bytes live in a file other than the selected
    /// row, so that a save changes a row the list has to be asked about.
    fn saves_elsewhere(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.source != document.path)
    }

    pub fn preview_fragment(&mut self, card: CardId, fragment: &str) -> Task<RepositoryAction> {
        if matches!(self.document, DocumentState::Loading) {
            self.pending_fragment = Some(fragment.to_owned());
            return Task::none();
        }
        if let Some(coordinates) = fragment.strip_prefix('L') {
            let (line, column) = coordinates
                .split_once(':')
                .or_else(|| coordinates.split_once('C'))
                .unwrap_or((coordinates, "1"));
            if let (Ok(line), Ok(column)) = (line.parse::<u32>(), column.parse::<u32>()) {
                self.source_surface = SourceSurface::Editor;
                self.markdown_mode = MarkdownMode::Edit;
                self.move_cursor_to(LspPosition::new(
                    line.saturating_sub(1),
                    column.saturating_sub(1),
                ));
                if let DocumentState::Editable(document) = &mut self.document {
                    document
                        .content
                        .restore_scroll_position(text_editor::ScrollPosition {
                            line: line.saturating_sub(1) as usize,
                            vertical: 0.0,
                            horizontal: 0.0,
                        });
                }
                return Task::none();
            }
        }
        let DocumentState::Editable(document) = &mut self.document else {
            return Task::none();
        };
        if !document.markdown {
            return Task::none();
        }
        document.sync_markdown();
        self.source_surface = SourceSurface::Editor;
        self.markdown_mode = MarkdownMode::Preview;
        if fragment.is_empty() {
            return Task::batch([
                self.diagram_task(card),
                iced::widget::scrollable::scroll_to(
                    self.preview_scroll_id.clone(),
                    iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(0.0),
                    },
                ),
            ]);
        }
        let scroll = crate::document_link::heading_index(&document.markdown_items, fragment)
            .map_or_else(Task::none, |index| {
                crate::document_link::scroll_to_heading(
                    self.preview_scroll_id.clone(),
                    crate::document_link::heading_id(card, index),
                )
            });
        Task::batch([scroll, self.diagram_task(card)])
    }

    /// The live buffer, or nothing while no editable document is open.
    pub fn editor_text(&self) -> Option<String> {
        match &self.document {
            DocumentState::Editable(document) => Some(document.content.text()),
            _ => None,
        }
    }

    pub fn selected_staging_selection(
        &self,
        stage: bool,
    ) -> Option<crate::repository::PathSelection> {
        use crate::repository::{PathSelection, RepoPath};
        let file = self.subject_file()?;
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

    pub fn selected_discard_request(&self) -> Option<crate::repository::DiscardRequest> {
        let selection = self.selected_staging_selection(true)?;
        let untracked = self.subject_file()?.status.as_deref() == Some("??");
        Some(crate::repository::DiscardRequest {
            worktree: self.descriptor.worktree.clone(),
            tracked: if untracked {
                Vec::new()
            } else {
                vec![selection.clone()]
            },
            untracked: if untracked {
                vec![selection.path]
            } else {
                Vec::new()
            },
            confirmed: false,
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

    /// Arms the Markdown preview's parse, coalescing a burst of edits into
    /// one parse once typing stops.
    ///
    /// Shaped like [`RepositoryRuntime::working_diff_task`]: at most one
    /// timer exists at a time and it always observes the latest deadline,
    /// so continuous typing in a large Markdown file parses once rather than
    /// once per character.
    fn markdown_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        if self.markdown_mode != MarkdownMode::Preview {
            return Task::none();
        }
        let DocumentState::Editable(document) = &self.document else {
            return Task::none();
        };
        if !document.markdown_stale {
            return Task::none();
        }
        self.markdown_debounce.deadline = Some(Instant::now() + MARKDOWN_DEBOUNCE);
        if self.markdown_debounce.timer.is_some() {
            return Task::none();
        }
        self.markdown_debounce.next_ticket =
            self.markdown_debounce.next_ticket.wrapping_add(1).max(1);
        let ticket = self.markdown_debounce.next_ticket;
        self.markdown_debounce.timer = Some(ticket);
        cosmic::task::future(async move {
            tokio::time::sleep(MARKDOWN_DEBOUNCE).await;
            RepositoryAction::MarkdownDue { card, ticket }
        })
    }

    fn finish_markdown(&mut self, card: CardId, ticket: u64) -> Task<RepositoryAction> {
        if self.markdown_debounce.timer != Some(ticket) {
            return Task::none();
        }
        self.markdown_debounce.timer = None;
        if self
            .markdown_debounce
            .deadline
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            // Typing continued while this timer ran; wait out the newer
            // deadline instead of parsing a document about to change again.
            return self.markdown_task(card);
        }
        self.markdown_debounce.deadline = None;
        if let DocumentState::Editable(document) = &mut self.document {
            document.sync_markdown();
        }
        self.diagram_task(card)
    }

    fn diagram_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        if self.markdown_mode != MarkdownMode::Preview {
            return Task::none();
        }
        let DocumentState::Editable(document) = &mut self.document else {
            return Task::none();
        };
        if document.diagram_pending.is_some() {
            return Task::none();
        }
        document.sync_markdown();
        let sources = preview::sources(
            &document.path,
            &document.preview_text,
            &document.markdown_items,
        );
        let keys: std::collections::HashSet<_> = sources
            .iter()
            .map(|source| blake3::hash(source.as_bytes()))
            .collect();
        document.diagrams.retain(|key, _| keys.contains(key));
        let sources: Vec<_> = sources
            .into_iter()
            .filter(|source| {
                !document
                    .diagrams
                    .contains_key(&blake3::hash(source.as_bytes()))
            })
            .collect();
        if sources.is_empty() {
            return Task::none();
        }
        let available = preview::available_budget(&document.diagrams);
        let path = document.path.clone();
        let revision = document.revision;
        document.diagram_pending = Some(revision);
        let cancel = tokio_util::sync::CancellationToken::new();
        document.diagram_cancel = Some(preview::CancellationGuard(cancel.clone()));
        let dark = document.diagram_dark;
        let tone_generation = document.diagram_tone_generation;
        let identity = self.current_identity(card, path);
        cosmic::task::future(async move {
            let result = preview::render(sources, dark, available, cancel).await;
            RepositoryAction::DiagramsRendered {
                identity,
                revision,
                tone_generation,
                result,
            }
        })
    }

    /// Records that the open buffer's text changed, whatever changed it, and
    /// how — `None` where the change cannot be described as one range.
    ///
    /// Silent once the version space is spent: the buffer keeps working and
    /// the server keeps its last coherent version rather than being rewound.
    ///
    /// The text is deliberately *not* produced here. Serializing the buffer
    /// is work proportional to the document and a keystroke arrives on the
    /// COSMIC update thread, so the whole burst of edits inside one drain
    /// window collapses onto one serialization at
    /// [`RepositoryRuntime::take_lsp_intents`]. Intermediate versions are
    /// never sent, which is exactly what a full-text `didChange` allows.
    fn push_lsp_change(&mut self, edit: Option<LspTextEdit>) {
        // Every path that changes the buffer's text passes through here, so
        // this is where the find strip's cached count is renewed rather than
        // recounted per frame in `view`. It runs before the version
        // bookkeeping because an exhausted version space stops that
        // bookkeeping early and must not leave the strip counting text that
        // is no longer there.
        self.refresh_find_matches();
        let DocumentState::Editable(document) = &mut self.document else {
            return;
        };
        let Some(version) = document.bump_lsp_version() else {
            return;
        };
        self.pending_lsp_change = Some(version);
        match edit {
            Some(edit) => self.lsp_edits.push(edit),
            // One indescribable edit poisons the batch: every later range is
            // measured against a document the server was never told about.
            None => {
                self.lsp_edits.clear();
                self.lsp_edits_exact = false;
            }
        }
    }

    /// Undoes or redoes one step in the open buffer.
    fn step_history(&mut self, card: CardId, redo: bool) -> Task<RepositoryAction> {
        // A history step edits the mounted source buffer, including while Find is open.
        if self.source_surface != SourceSurface::Editor {
            return Task::none();
        }
        let DocumentState::Editable(document) = &mut self.document else {
            return Task::none();
        };
        let Some(edits) = document.step_history(redo) else {
            return Task::none();
        };
        self.push_lsp_edits(edits);
        // Undo from the context menu leaves the keyboard on the menu; the
        // restored caret is only visible in a focused buffer.
        Task::batch([self.markdown_task(card), self.focus_editor(card)])
    }

    /// Reports a run of replacements, in the order they were applied, and
    /// nothing at all for an empty run.
    fn push_lsp_edits(&mut self, edits: Vec<Option<LspTextEdit>>) {
        for edit in edits {
            self.push_lsp_change(edit);
        }
    }

    /// Materialises the text of a deferred `didChange`, if one is pending.
    fn flush_lsp_change(&mut self) {
        let Some(version) = self.pending_lsp_change.take() else {
            return;
        };
        if self.lsp_document.is_none() {
            self.lsp_edits.clear();
            self.lsp_edits_exact = true;
            return;
        }
        let DocumentState::Editable(document) = &self.document else {
            // The buffer stopped existing before anyone drained; the close
            // that replaced it is the only thing left worth saying.
            self.lsp_edits.clear();
            self.lsp_edits_exact = true;
            return;
        };
        let text = document.content.text();
        self.lsp_intents.push(LspIntent::Change { version, text });
    }

    /// Records that the open buffer reached disk.
    fn push_lsp_save(&mut self) {
        self.flush_lsp_change();
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
        self.pending_diff_source = None;
        self.flush_lsp_change();
        self.lsp_edits.clear();
        self.lsp_edits_exact = true;
        if matches!(&self.document, DocumentState::Editable(_)) {
            self.lsp_intents.push(LspIntent::Close);
        }
        // The buffer the strip was searching is going away, and a strip left
        // mounted over the next one would hold it in search mode while
        // counting occurrences of a query nobody typed for it.
        self.clear_find();
    }

    /// Whether the working-tree comparison is taken from the live buffer
    /// rather than from the file on disk.
    ///
    /// It is taken from the buffer whenever the index baseline is already
    /// known, on either surface: the comparison then keeps step with unsaved
    /// edits, and the editor can tint the lines the author added while they
    /// are still typing them. Without a loaded comparison there is no baseline
    /// to diff against, and loading one is the comparison surface's own job.
    fn editable_working_diff(&self) -> bool {
        self.diff_basis == DiffBasis::IndexToWorktree
            && self.basis_resolution.is_none()
            && matches!(self.document, DocumentState::Editable(_))
            && self.diff_model().is_some()
    }

    fn wanted_working_diff(&self) -> Option<(u64, u64, Arc<str>)> {
        if !self.editable_working_diff() || self.mutation_refresh_pending {
            return None;
        }
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        Some((
            self.generation,
            document.revision,
            Arc::clone(&self.diff_model()?.before),
        ))
    }

    fn working_annotations_current(&self) -> bool {
        self.editable_working_diff()
            && same_diff_revision(&self.working_diff.applied, &self.wanted_working_diff())
            && self.working_diff.error.is_none()
    }

    /// The live-buffer comparison's failure, while that comparison is the one
    /// the surface would show.
    fn working_diff_failure(&self) -> Option<&str> {
        (self.diff_basis == DiffBasis::IndexToWorktree
            && self.basis_resolution.is_none()
            && matches!(self.document, DocumentState::Editable(_)))
        .then_some(self.working_diff.error.as_deref())
        .flatten()
    }

    pub fn set_diff_surface_visible(&mut self, visible: bool) {
        self.diff_surface_visible = visible;
        if !visible {
            if let Some(active) = &self.working_diff.active
                && active.rich
            {
                active.cancellation.cancel();
            }
            // Dropping the loading state cancels an initial repository diff as
            // well as live-buffer work. Completed models remain reusable.
            self.diffs
                .retain(|(_, state)| !matches!(state, DiffState::Loading { .. }));
        }
    }

    /// Visibility transitions schedule work themselves: revealing a pane must
    /// not wait for another edit, and overscan must not retain rich CPU work.
    pub fn refresh_diff_visibility(
        &mut self,
        card: CardId,
        visible: bool,
    ) -> Task<RepositoryAction> {
        self.set_diff_surface_visible(visible);
        Task::batch([self.ensure_current_diff(card), self.working_diff_task(card)])
    }

    #[cfg(test)]
    pub(crate) fn diff_surface_is_visible(&self) -> bool {
        self.diff_surface_visible
    }

    fn wants_rich_working_diff(&self) -> bool {
        self.diff_surface_visible && self.source_surface == SourceSurface::Diff
    }

    fn working_diff_current(&self) -> bool {
        same_diff_revision(&self.working_diff.desired, &self.working_diff.applied)
            && (!self.working_diff.desired_rich || self.working_diff.applied_rich)
    }

    /// Call after repository actions and LSP edits, alongside autosave. At most
    /// one timer or one immutable CPU snapshot exists, even under continuous
    /// typing or selection/basis churn. The timer observes the latest deadline.
    pub fn working_diff_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        let wanted = self.wanted_working_diff();
        let revision_changed = !same_diff_revision(&wanted, &self.working_diff.desired);
        let rich = self.wants_rich_working_diff();
        if revision_changed || rich != self.working_diff.desired_rich {
            self.working_diff.desired = wanted;
            self.working_diff.desired_rich = rich;
            self.working_diff.deadline = Some(
                Instant::now()
                    + if revision_changed {
                        Duration::from_millis(180)
                    } else {
                        Duration::ZERO
                    },
            );
            if self.working_diff.error.take().is_some() {
                self.working_diff.applied = None;
            }
            if let Some(active) = &self.working_diff.active {
                active.cancellation.cancel();
            }
        }
        if self.working_diff.active.is_some()
            || self.working_diff.timer.is_some()
            || self.working_diff.desired.is_none()
            || self.working_diff_current()
        {
            return Task::none();
        }
        self.working_diff.next_ticket = self.working_diff.next_ticket.wrapping_add(1).max(1);
        let ticket = self.working_diff.next_ticket;
        self.working_diff.timer = Some(ticket);
        let delay = self
            .working_diff
            .deadline
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(Instant::now());
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            RepositoryAction::WorkingDiffDue { card, ticket }
        })
    }

    fn start_working_diff(&mut self, card: CardId, ticket: u64) -> Task<RepositoryAction> {
        if self.working_diff.timer != Some(ticket) {
            return Task::none();
        }
        self.working_diff.timer = None;
        if !same_diff_revision(&self.wanted_working_diff(), &self.working_diff.desired)
            || self
                .working_diff
                .deadline
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return self.working_diff_task(card);
        }
        let Some((_, revision, before)) = self.wanted_working_diff() else {
            return Task::none();
        };
        if self.working_diff.active.is_some() || self.working_diff_current() {
            return Task::none();
        }
        let DocumentState::Editable(document) = &self.document else {
            return Task::none();
        };
        let identity = self.current_identity(card, document.path.clone());
        // Highlighting follows the bytes, which is the source; the identity
        // stays the row, which is what the result is matched against.
        let source = document.source.clone();
        // Capturing a persistent revision is constant time. Materialization
        // belongs to the CPU worker, never the native event loop.
        let snapshot_started = Instant::now();
        let snapshot = document.content.text_snapshot();
        let rich = self.wants_rich_working_diff();
        tracing::debug!(target: "cagents::diff", snapshot_us = snapshot_started.elapsed().as_micros() as u64,
            revision, rich, bytes = snapshot.len(), "captured diff revision");
        if snapshot.len() as u64 > MAX_DIFF_SIDE_BYTES {
            self.working_diff.applied = self.working_diff.desired.clone();
            self.working_diff.applied_rich = true;
            self.working_diff.error = Some("The unsaved buffer exceeds the diff size limit. Editing and safe saving remain available.".into());
            return Task::none();
        }
        let cancellation = CancellationToken::new();
        let syntax_theme = self.syntax_theme;
        let ignore_whitespace = self.diff_ignore_whitespace;
        let font = self.editor_font;
        let present = [self.diff_model().is_none_or(|model| model.present[0]), true];
        self.working_diff.active = Some(WorkingDiffJob {
            identity: identity.clone(),
            revision,
            ticket,
            before: Arc::clone(&before),
            rich,
            cancellation: cancellation.clone(),
        });
        cosmic::task::future(async move {
            let baseline = Arc::clone(&before);
            let result = tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                if cancellation.is_cancelled() { return None; }
                let after = snapshot.text();
                let serialized_us = started.elapsed().as_micros() as u64;
                let model = if rich {
                    let mut model = build_document_diff_sources_with_font(Arc::clone(&baseline), Arc::clone(&after), &source, syntax_theme, ignore_whitespace, font, &cancellation)?;
                    model.present = present;
                    Some(model)
                } else { None };
                let additions = if let Some(model) = &model { added_source_lines(model) }
                    else { build_line_changes(&baseline, &after, ignore_whitespace, &cancellation)? };
                let additions = native_added_lines(&snapshot, additions);
                tracing::debug!(target: "cagents::diff", worker_us = started.elapsed().as_micros() as u64,
                    serialized_us, revision, rich, "computed diff revision");
                Some(WorkingDiffOutput { model, additions })
            })
            .await
            .map_err(|error| format!("Diff worker failed: {error}"));
            RepositoryAction::WorkingDiffLoaded {
                identity,
                revision,
                ticket,
                before,
                result,
            }
        })
    }

    fn finish_working_diff(
        &mut self,
        identity: AsyncIdentity,
        revision: u64,
        ticket: u64,
        before: Arc<str>,
        result: Result<Option<WorkingDiffOutput>, String>,
    ) -> Task<RepositoryAction> {
        let matches_active = self.working_diff.active.as_ref().is_some_and(|active| {
            active.ticket == ticket
                && active.identity == identity
                && active.revision == revision
                && Arc::ptr_eq(&active.before, &before)
        });
        if !matches_active {
            return Task::none();
        }
        let cancelled = self
            .working_diff
            .active
            .as_ref()
            .is_some_and(|active| active.cancellation.is_cancelled());
        self.working_diff.active = None;
        let mut installed = false;
        let completed = Some((identity.generation, revision, Arc::clone(&before)));
        if !cancelled
            && self.accepts_selected(&identity)
            && same_diff_revision(&completed, &self.wanted_working_diff())
        {
            match result {
                Ok(Some(output)) => {
                    self.working_diff.additions = output.additions;
                    self.working_diff.applied = completed;
                    self.working_diff.applied_rich = output.model.is_some();
                    self.working_diff.error = None;
                    if let Some(mut model) = output.model {
                        model.before = before;
                        self.set_diff_state(
                            DiffBasis::IndexToWorktree,
                            DiffState::ready(RenderedDiff::Text {
                                model: Arc::new(model),
                                old_label: "Index".into(),
                                new_label: "Working buffer".into(),
                            }),
                        );
                        self.pending_diff_source = None;
                        self.diff_change =
                            self.diff_change.min(self.change_count().saturating_sub(1));
                        installed = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.working_diff.applied = completed;
                    self.working_diff.applied_rich = true;
                    self.working_diff.error = Some(error);
                }
            }
        }
        let next = self.working_diff_task(identity.card);
        // A rebuild re-anchors the viewport in rows of the new model; a
        // mounted comparison has to be moved to match.
        if installed && self.source_surface == SourceSurface::Diff {
            return Task::batch([next, self.restore_visible_scroll()]);
        }
        next
    }

    fn file_has_basis(&self, file: &RepositoryFile, basis: &DiffBasis) -> bool {
        file.has_basis(basis) || (*basis == DiffBasis::IndexToWorktree && self.is_dirty())
    }

    fn can_show_diff(&self) -> bool {
        !self.is_saving()
            && !self.directory_only
            && self.subject_file().is_some_and(|file| {
                self.file_has_basis(&file, &self.diff_basis) || self.basis_resolution.is_some()
            })
    }

    /// Whether the diff surface can hold a discussion of the whole file: it
    /// renders one only above a text or no-text comparison, so a failed,
    /// empty, or unresolved comparison would hide a draft begun there. A
    /// comparison still loading does not count: a failure would leave its
    /// draft without a visible Save or Cancel control.
    pub fn can_show_file_discussion(&self) -> bool {
        self.can_show_diff()
            && self.basis_resolution.is_none()
            && self.working_diff_failure().is_none()
            && matches!(
                self.diff_state(&self.diff_basis),
                Some(DiffState::Ready(
                    RenderedDiff::Text { .. } | RenderedDiff::NoText(_),
                    _
                ))
            )
    }

    fn can_apply_markdown_mode(&self, preview: bool) -> bool {
        !self.is_saving()
            && match &self.document {
                DocumentState::Editable(_) => true,
                DocumentState::Image { .. } => preview,
                _ => false,
            }
    }

    fn can_select_diff_basis(&self, basis: &DiffBasis) -> bool {
        !self.is_saving()
            && !self.directory_only
            && (!self.mutation_refresh_pending || basis.is_immutable())
            && self
                .subject_file()
                .is_some_and(|file| self.file_has_basis(&file, basis))
    }

    fn can_select_committed_basis(&self, choice: CommittedChoice) -> bool {
        !self.is_saving()
            && !self.directory_only
            && self.selected.is_some()
            && self.basis_resolution != Some(BasisResolution::Resolving(choice))
            && !matches!(self.basis_resolution, Some(BasisResolution::NoCommits))
    }

    fn can_change_diff_layout(&self) -> bool {
        !self.is_saving()
            && self.source_surface == SourceSurface::Diff
            && self.diff_model().is_some()
    }

    fn can_navigate_diff(&self) -> bool {
        self.can_change_diff_layout()
            && self.change_count() != 0
            && (!self.editable_working_diff() || self.working_annotations_current())
    }

    fn can_retry_diff(&self) -> bool {
        self.source_surface == SourceSurface::Diff
            && self.basis_resolution.is_none()
            && self.can_select_diff_basis(&self.diff_basis)
            && (matches!(
                self.diff_state(&self.diff_basis),
                Some(DiffState::Ready(RenderedDiff::Error(_), _))
            ) || (self.editable_working_diff() && self.working_diff.error.is_some()))
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

    pub fn external_editor_command_available(&self) -> bool {
        self.can_open_external() && !self.external_editors.is_empty()
    }

    pub fn external_editor_command(&self, card: CardId, editor_id: Option<&str>) -> Option<RepositoryAction> {
        if !self.external_editor_command_available() { return None; }
        let editor = match editor_id {
            Some(id) => self.external_editors.iter().find(|editor| editor.id == id)?,
            None => self.external_editors.first()?,
        };
        Some(external_editor_action(card, &editor.id))
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
            DocumentState::Loading => "Loading",
            DocumentState::Editable(_) if self.is_saving() => "Saving",
            DocumentState::Editable(document) if document.conflict => "Save conflict",
            DocumentState::Editable(document) if document.durability_warning.is_some() => {
                "Durability warning"
            }
            DocumentState::Editable(document) if document.error.is_some() => "Save failed",
            DocumentState::Editable(_) if self.is_dirty() => "Unsaved changes",
            DocumentState::Editable(_) => "Editable",
            DocumentState::Image { .. } => "Image preview",
            DocumentState::Pdf { rendering, .. } if rendering.is_some() => "Rendering page",
            DocumentState::Pdf { .. } => "Document preview",
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
        let subject = self.subject_file();
        let has_staged = subject.as_ref().is_some_and(|file| file.staged);
        let has_unstaged = subject.as_ref().is_some_and(|file| file.unstaged);
        let editor = selected && self.source_surface == SourceSurface::Editor;
        let diff = selected && self.source_surface == SourceSurface::Diff;
        let markdown_mode = editor.then_some(match self.markdown_mode {
            MarkdownMode::Edit => "Edit",
            MarkdownMode::Preview => "Preview",
        });
        let markdown_mode = matches!(&self.document, DocumentState::Editable(_))
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
                BasisResolution::NoCommits => (
                    Some("Unavailable"),
                    Some("This branch has no commits yet, so there is nothing to compare.".into()),
                    None,
                    None,
                    None,
                ),
                BasisResolution::Unavailable(message) => {
                    (Some("Unavailable"), Some(message.clone()), None, None, None)
                }
            }
        } else {
            match self.displayed_diff_state() {
                None => (Some("No changes"), None, Some(0), Some(0), None),
                Some(DiffState::Loading { .. }) => (Some("Loading"), None, None, None, None),
                Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) => {
                    let changes = model.change_starts(self.diff_mode).len();
                    (
                        Some(if changes == 0 { "No changes" } else { "Ready" }),
                        None,
                        Some(model.additions),
                        Some(model.deletions),
                        (changes != 0).then_some((self.diff_change + 1, changes)),
                    )
                }
                Some(DiffState::Ready(RenderedDiff::Error(error), _)) => {
                    (Some("Unavailable"), Some(error.clone()), None, None, None)
                }
                Some(DiffState::Ready(RenderedDiff::NoText(content), _)) => {
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
            can_back: selected
                && self.descriptor.selected_file.is_none()
                && !self.blocks_navigation(),
        }
    }

    pub fn inspector_runtime_status(&self) -> RepositoryRuntimeStatus {
        match &self.files {
            FileListState::Loading => return RepositoryRuntimeStatus::Loading,
            FileListState::Failed(_) => return RepositoryRuntimeStatus::Unavailable,
            FileListState::Ready(_) => {}
        }
        match &self.document {
            DocumentState::Loading => RepositoryRuntimeStatus::Loading,
            DocumentState::Failed { .. } => RepositoryRuntimeStatus::Unavailable,
            _ if self.source_surface == SourceSurface::Diff => match &self.basis_resolution {
                Some(BasisResolution::Resolving(_)) => RepositoryRuntimeStatus::Loading,
                Some(
                    BasisResolution::NeedsBase(_)
                    | BasisResolution::NoCommits
                    | BasisResolution::Unavailable(_),
                ) => RepositoryRuntimeStatus::Unavailable,
                None => match self.diff_state(&self.diff_basis) {
                    Some(DiffState::Loading { .. }) => RepositoryRuntimeStatus::Loading,
                    Some(DiffState::Ready(RenderedDiff::Error(_), _)) => {
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
        // Reached from the application's dirty-close dialog, which throws the
        // buffer away without the editor ever seeing an action for it.
        let edits = document.discard_edits();
        self.push_lsp_edits(edits);
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

    fn diff_typography(&self) -> DiffTypography {
        DiffTypography {
            font: self.editor_font,
            size: self.editor_font_size,
            line_height: self.editor_font_size * self.line_height,
        }
    }

    fn update_diff_typography(&mut self) {
        let anchor = self.current_diff_viewport();
        let height = self.diff_typography().line_height;
        for (_, state) in &mut self.diffs {
            if let DiffState::Ready(_, layout) = state {
                layout.set_row_height(height);
            }
        }
        if let Some((_, _, DiffState::Ready(_, layout))) = &mut self.stale_diff {
            layout.set_row_height(height);
        }
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.apply_diff_viewport(anchor);
    }

    pub fn restore_font_viewport(&self) -> Task<RepositoryAction> {
        self.restore_visible_scroll()
    }

    pub fn set_editor_font(&mut self, font: Font, size: f32) {
        if self.editor_font == font && self.editor_font_size == size {
            return;
        }
        let family_changed = self.editor_font != font;
        let anchor = self.current_diff_viewport();
        self.editor_font = font;
        self.editor_font_size = size;
        self.update_diff_typography();
        if family_changed {
            // Widths were measured for the previous family. Completed syntax
            // remains in the shared cache; each visible comparison remeasures
            // on its worker, with stale callbacks fenced by removed requests.
            self.diffs.clear();
            self.stale_diff = None;
            self.pending_diff_anchor = Some(anchor);
            self.working_diff.applied = None;
            self.working_diff.desired = None;
            if let Some(active) = &self.working_diff.active {
                active.cancellation.cancel();
            }
            self.clear_diff_selection();
            self.clear_diff_find();
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
        let (path, destination, text, disk_hash, revision, history_revision, snapshot_source) = match &self.document {
            DocumentState::Editable(document)
                if (document.dirty || document.durability_warning.is_some())
                    && document.active_save.is_none() =>
            {
                (
                    document.path.clone(),
                    document.source.clone(),
                    document.content.text(),
                    document.disk_hash,
                    document.revision,
                    document.history.revision(),
                    document.content.text_snapshot(),
                )
            }
            _ => return Task::none(),
        };
        // The ticket is about the selected row, which is what a result is
        // matched against; the bytes go where they were read from.
        let identity = self.current_identity(card, path);
        self.next_save_ticket = self.next_save_ticket.wrapping_add(1).max(1);
        let ticket = SaveTicket {
            id: self.next_save_ticket,
            identity: identity.clone(),
            revision,
            history_revision,
            snapshot_source,
            snapshot: text.clone(),
        };
        if let DocumentState::Editable(document) = &mut self.document {
            document.history.seal();
            document.active_save = Some(ticket.clone());
            document.error = None;
        }
        // Keep diff review mounted while saving. Only a real save failure or
        // conflict reveals recovery; explicit saves may still leave preview.
        if reveal_editor && self.source_surface != SourceSurface::Diff {
            self.source_surface = SourceSurface::Editor;
            self.markdown_mode = MarkdownMode::Edit;
        }
        let worktree = identity.worktree.clone();
        let saved_text = text.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                save_file(&worktree, &destination, &text, disk_hash, overwrite)
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
        let card = action.card();
        let task = self.update_inner(action);
        Task::batch([task, self.find_task(card)])
    }

    fn update_inner(&mut self, action: RepositoryAction) -> Task<RepositoryAction> {
        if matches!(
            &action,
            RepositoryAction::Save { .. }
                | RepositoryAction::DiscardEdits { .. }
                | RepositoryAction::StageSelected { .. }
                | RepositoryAction::ApplySourceSurface { .. }
                | RepositoryAction::ApplyMarkdownMode { .. }
                | RepositoryAction::ApplyDiffMode { .. }
                | RepositoryAction::ApplyDiffBasis { .. }
                | RepositoryAction::OpenExternal { .. }
        ) {
            self.toolbar_overflow_open = false;
        }
        match action {
            RepositoryAction::RefreshFiles { card } => self.initial_load(card),
            RepositoryAction::ListDirectory { card, path } => self.list_directory(card, path),
            RepositoryAction::SearchDirectory { card, query } => self.search_directory(card, query),
            RepositoryAction::DirectoryListed { identity, result } => {
                if identity.generation != self.file_list_generation {
                    return Task::none();
                }
                let FileListState::Ready(files) = &mut self.files else {
                    return Task::none();
                };
                let Ok(entries) = result else {
                    return Task::none();
                };
                let prefix = &identity.path;
                files
                    .retain(|file| file.path == *prefix || file.path.strip_prefix(prefix).is_err());
                files.extend(entries);
                files.sort_by(|left, right| left.path.cmp(&right.path));
                Task::none()
            }
            RepositoryAction::NewTerminal { .. }
            | RepositoryAction::DirectorySearchLoaded { .. } => Task::none(),
            RepositoryAction::Clipboard { card, op } => self.clipboard(card, op),
            RepositoryAction::StageSelected { .. } => Task::none(),
            RepositoryAction::FileListUnchanged { identity } => {
                if !self.accepts_file_list(&identity) || identity.path != Path::new(".") {
                    return Task::none();
                }
                self.file_list_loading = false;
                self.file_list_started = None;
                self.background_file_refresh = false;
                if std::mem::take(&mut self.file_list_refresh_queued) {
                    return self.initial_load(identity.card);
                }
                // Nothing in the worktree moved, so nothing here moves
                // either: no rebuild, no assignment to `files`, no repaint.
                Task::none()
            }
            RepositoryAction::FilesLoaded { identity, result } => {
                if !self.accepts_file_list(&identity) || identity.path != Path::new(".") {
                    return Task::none();
                }
                self.file_list_loading = false;
                self.file_list_started = None;
                if std::mem::take(&mut self.file_list_refresh_queued) {
                    // A save or mutation happened after this read started.
                    // Discard its status and perform one authoritative follow-up.
                    return self.initial_load(identity.card);
                }
                // Committed only now: a digest recorded for a list this card
                // never installed would make the next refresh believe the
                // worktree it describes is already on screen.
                let deposited = *self
                    .file_list_deposit
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.file_list_digest = deposited
                    .filter(|(generation, _)| *generation == identity.generation)
                    .map(|(_, digest)| digest);
                let result = result.map(|files| self.merge_directory_listing(files));
                let changed = match (&result, &self.selected) {
                    (Ok(after), Some(selected)) => {
                        // A successful save already updates this snapshot.
                        // Comparing the older list would discard its current
                        // native diff after every first autosave.
                        after
                            .iter()
                            .find(|file| file.path == selected.path)
                            .map_or(selected.changed(), |file| file != selected)
                    }
                    _ => false,
                };
                let background_changed =
                    std::mem::take(&mut self.background_file_refresh) && changed;
                if background_changed {
                    self.diffs.retain(|(basis, _)| basis.is_immutable());
                }
                let refresh_selected =
                    (self.mutation_refresh_pending || background_changed) && result.is_ok();
                self.files = result.map_or_else(FileListState::Failed, FileListState::Ready);
                let relist = self.relist_expanded_directories(identity.card);
                if refresh_selected {
                    self.mutation_refresh_pending = false;
                    Task::batch([relist, self.refresh_selected_status(identity.card)])
                } else {
                    relist
                }
            }
            RepositoryAction::OpenFile { card, path } => {
                if self.blocks_navigation() {
                    return Task::none();
                }
                self.open_file(card, path)
            }
            RepositoryAction::OpenFileFromBrowser { card, path } => {
                if self.blocks_navigation() {
                    return Task::none();
                }
                let path = path.as_path().to_path_buf();
                let file = self.file(&path).cloned().unwrap_or(RepositoryFile {
                    path,
                    ..RepositoryFile::default()
                });
                if file.directory {
                    return Task::none();
                }
                self.open_file_entry(card, file)
            }
            RepositoryAction::FileLoaded { identity, result } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                let restored_focus = self
                    .restores_editor_navigation(&identity)
                    .then(|| identity.clone());
                self.document = match result {
                    Ok(LoadedFile {
                        source,
                        content:
                            FileContent::Editable {
                                text,
                                content_hash,
                                markdown,
                            },
                    }) => DocumentState::Editable(EditorDocument::new(
                        identity.path,
                        source,
                        text,
                        content_hash,
                        markdown,
                    )),
                    Ok(LoadedFile {
                        source,
                        content:
                            FileContent::Image {
                                width,
                                height,
                                rgba,
                            },
                    }) => DocumentState::Image {
                        source,
                        handle: iced::widget::image::Handle::from_rgba(
                            width,
                            height,
                            iced::advanced::graphics::core::Bytes::from_owner(rgba),
                        ),
                        width,
                        height,
                    },
                    Ok(LoadedFile {
                        source,
                        content: FileContent::Pdf { pages, page },
                    }) => DocumentState::Pdf {
                        handle: iced::widget::image::Handle::from_rgba(
                            page.width,
                            page.height,
                            iced::advanced::graphics::core::Bytes::from_owner(page.rgba),
                        ),
                        width: page.width,
                        height: page.height,
                        points_wide: page.points_wide,
                        page: page.index,
                        // A freshly opened document is drawn at the pane's
                        // width, which is the zoom the load rendered it for.
                        zoom: crate::editor::PdfZoom::default(),
                        // The tone the load baked into these pixels, not the
                        // theme as it is now: the two differ when the reader
                        // changed their shell while the document rendered.
                        tone: self.document_tone,
                        // The same absolute path the load resolved, because
                        // every later page is read from the file again.
                        path: identity.worktree.join(&source),
                        source,
                        pages,
                        rendering: None,
                        message: None,
                        // A freshly opened document follows the theme until
                        // the reader says otherwise.
                        chosen_tone: None,
                    },
                    // The notice is about the file that was classified: for
                    // a refused link the link that failed, which in a chain
                    // is not the row, and otherwise the file the row led to.
                    Ok(LoadedFile {
                        source,
                        content: FileContent::ReadOnly(reason),
                    }) => DocumentState::ReadOnly {
                        path: source,
                        reason,
                    },
                    Err(error) => DocumentState::Failed {
                        path: identity.path,
                        message: error.to_string(),
                    },
                };
                if let DocumentState::Editable(document) = &mut self.document {
                    if let Some((path, source_line)) = self.pending_source_location.take()
                        && document.path == path
                        && let Some((pending_path, navigation)) = &mut self.pending_editor_navigation
                        && *pending_path == path {
                        let line = document.content.source_line_ranges().get(source_line).map_or_else(|| document.content.line_count().saturating_sub(1), |range| range.start);
                        navigation.caret.line = line;
                        navigation.caret.column = 0;
                        navigation.scroll_line = line.saturating_sub(10);
                    }
                    if let Some((path, editor)) = self.pending_editor_navigation.take()
                        && self.selected.as_ref().is_some_and(|file| file.path == path)
                    {
                        let position =
                            |position: crate::model::EditorPosition| text_editor::Position {
                                line: position.line,
                                column: position.column,
                            };
                        document.content.set_cursor(text_editor::Cursor {
                            position: position(editor.caret),
                            selection: editor.selection.map(position),
                        });
                        self.pending_editor_focus = restored_focus;
                        self.viewports.editor = text_editor::ScrollPosition {
                            line: editor.scroll_line,
                            vertical: editor.scroll_vertical,
                            horizontal: editor.scroll_horizontal,
                        };
                    }
                    document
                        .content
                        .restore_scroll_position(self.viewports.editor);
                }
                // A read-only or unreadable file is not a document a language
                // server can serve, and `push_lsp_open` reports nothing for
                // either of them.
                self.push_lsp_open(&identity.worktree);
                // A theme change while the document was rendering had no page
                // to re-render, so the page reconciles with the shell as it is
                // now. Nothing to reconcile renders nothing.
                let retone = self.retone_document(identity.card, &cosmic::theme::active());
                // The row may already have asked Git about itself, which for a
                // symbolic link is the link. The buffer is the target, so that
                // comparison is discarded and read again from the target.
                let followed = self.followed_source().is_some();
                if followed {
                    self.diffs.retain(|(basis, _)| basis.is_immutable());
                }
                let reload = if followed {
                    self.ensure_current_diff(identity.card)
                } else {
                    Task::none()
                };
                let fragment = self
                    .pending_fragment
                    .take()
                    .map_or_else(Task::none, |fragment| {
                        self.preview_fragment(identity.card, &fragment)
                    });
                Task::batch([retone, self.restore_visible_scroll(), reload, fragment])
            }
            RepositoryAction::PdfTone { card, tone } => {
                let DocumentState::Pdf {
                    page,
                    zoom,
                    chosen_tone,
                    ..
                } = &mut self.document
                else {
                    return Task::none();
                };
                *chosen_tone = Some(tone);
                let (page, zoom) = (*page, *zoom);
                self.update(RepositoryAction::PdfPage {
                    card,
                    page,
                    tone,
                    zoom,
                })
            }
            RepositoryAction::PdfZoom { card, zoom } => {
                let DocumentState::Pdf { page, tone, .. } = &self.document else {
                    return Task::none();
                };
                let (page, tone) = (*page, *tone);
                self.update(RepositoryAction::PdfPage {
                    card,
                    page,
                    tone,
                    zoom,
                })
            }
            RepositoryAction::PdfPage {
                card,
                page,
                tone,
                zoom,
            } => {
                let DocumentState::Pdf {
                    path,
                    pages,
                    page: current,
                    tone: current_tone,
                    zoom: current_zoom,
                    rendering,
                    message,
                    ..
                } = &mut self.document
                else {
                    return Task::none();
                };
                if page >= *pages {
                    return Task::none();
                }
                let wanted = PdfView { page, tone, zoom };
                // What is drawn right now, which is what the display fields
                // hold whenever no render is in flight.
                let drawn = PdfView {
                    page: *current,
                    tone: *current_tone,
                    zoom: *current_zoom,
                };
                if wanted == drawn && rendering.is_none() {
                    return Task::none();
                }
                // The page, tone and zoom move now so the controls reflect
                // where the reader is going, while the image keeps showing the
                // page they can still read until its replacement arrives.
                *current = page;
                *current_tone = tone;
                *current_zoom = zoom;
                *message = None;
                if let Some(active) = rendering {
                    // A render is already running. Starting a second one would
                    // reparse the document and rasterize a page nobody will
                    // see, so this turn waits, and replaces whichever turn was
                    // waiting before it. Asking again for what is already
                    // being rendered queues nothing.
                    active.pending = (active.requested != wanted).then_some(wanted);
                    return Task::none();
                }
                self.pdf_request = self.pdf_request.wrapping_add(1);
                let request = self.pdf_request;
                let path = path.clone();
                *rendering = Some(PdfRender {
                    request,
                    requested: wanted,
                    drawn,
                    pending: None,
                });
                pdf_page_task(card, path, wanted, request)
            }
            RepositoryAction::PdfPageRendered {
                card,
                request,
                result,
            } => {
                // Read before the document is borrowed, because the intent
                // lives beside it rather than in it.
                let intended = self.document_tone;
                let DocumentState::Pdf {
                    source: _,
                    path,
                    pages,
                    page,
                    tone,
                    zoom,
                    handle,
                    width,
                    height,
                    points_wide,
                    rendering,
                    message,
                    chosen_tone,
                } = &mut self.document
                else {
                    return Task::none();
                };
                // One fence: a page the reader has already turned away from
                // must not replace the one they are looking at now.
                let Some(active) = rendering.take_if(|active| active.request == request) else {
                    return Task::none();
                };
                // What the next render, if the reader queued one, will have to
                // put back should it fail.
                let drawn = match result {
                    Ok(rendered) => {
                        // The child re-read the file, so its page count is
                        // newer than the one the open reported and the counter
                        // follows it rather than a count from before.
                        *pages = rendered.pages;
                        let raster = rendered.page;
                        *width = raster.width;
                        *height = raster.height;
                        // A page's printed width can differ from the last
                        // one's, because a document may mix page sizes, and
                        // every zoom step is a fraction of it.
                        *points_wide = raster.points_wide;
                        *handle = iced::widget::image::Handle::from_rgba(
                            raster.width,
                            raster.height,
                            iced::advanced::graphics::core::Bytes::from_owner(raster.rgba),
                        );
                        PdfView {
                            // The index the child rendered, not the one it was
                            // asked for, because the drawn page is what the
                            // counter has to name.
                            page: raster.index,
                            ..active.requested
                        }
                    }
                    Err(reason) => {
                        // The previously rendered page stays on screen, so
                        // everything that describes it goes back too: a
                        // counter, a tooltip, a tone toggle or a zoom control
                        // naming a page nobody drew would also make the next
                        // turn skip one. The message is only worth showing
                        // while the reader is still on the page it is about; a
                        // turn they have already made replaces it.
                        if active.pending.is_none() {
                            *message = Some(reason);
                        }
                        active.drawn
                    }
                };
                // A theme change and the raster it invalidates do not arrive
                // in a defined order, so a page can land rendered for the
                // theme that was on its way out. Comparing what was drawn
                // against the tone the application last asked for converges
                // without assuming either order, and without consulting a
                // global whose own update is a separate command.
                let pending = active.pending.or_else(|| {
                    (chosen_tone.is_none() && drawn.tone != intended).then_some(PdfView {
                        tone: intended,
                        ..drawn
                    })
                });
                let Some(next) = pending else {
                    (*page, *tone, *zoom) = (drawn.page, drawn.tone, drawn.zoom);
                    return Task::none();
                };
                self.pdf_request = self.pdf_request.wrapping_add(1);
                let request = self.pdf_request;
                let path = path.clone();
                *rendering = Some(PdfRender {
                    request,
                    requested: next,
                    drawn,
                    pending: None,
                });
                pdf_page_task(card, path, next, request)
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
                    if current {
                        self.stale_diff = None;
                    }
                    self.set_diff_state(basis, DiffState::ready(content));
                    if current {
                        self.diff_change =
                            self.diff_change.min(self.change_count().saturating_sub(1));
                        self.restore_native_viewport();
                        return self.restore_visible_scroll();
                    }
                }
                Task::none()
            }
            RepositoryAction::EditorAction { card, action } => {
                // No buffer is mounted outside Edit, so an action arriving
                // from any other surface is a stale message about a surface
                // that has already been replaced.
                if self.source_surface != SourceSurface::Editor {
                    return Task::none();
                }
                if !matches!(action, text_editor::Action::Scroll { .. }) {
                    self.dismiss_editor_results();
                }
                let moved = !matches!(
                    action,
                    text_editor::Action::Edit(_) | text_editor::Action::Scroll { .. }
                );
                let applied = match &mut self.document {
                    DocumentState::Editable(document) => document.apply(action),
                    _ => Applied::Unchanged,
                };
                // A caret movement is not a document change, and neither is
                // an edit that found nothing to change. Bumping the version
                // for one would leave every request the user makes next
                // quoting a version the server has never been sent.
                match applied {
                    Applied::Changed(edit) => {
                        self.push_lsp_change(edit);
                        self.markdown_task(card)
                    }
                    Applied::Unchanged => {
                        if self.find_open && moved {
                            self.find_selection_candidate = self.current_selection_scope();
                        }
                        Task::none()
                    }
                }
            }
            RepositoryAction::Language { card, action } => {
                let diagnostic = matches!(action, EditorLspAction::Diagnostic(_));
                match action {
                    EditorLspAction::Next(forward) if !self.language.completions.is_empty() => {
                        let count = self.language.completions.len();
                        self.language.selected = if forward {
                            (self.language.selected + 1) % count
                        } else {
                            (self.language.selected + count - 1) % count
                        };
                    }
                    EditorLspAction::Diagnostic(forward) => self.navigate_diagnostic(forward),
                    EditorLspAction::Dismiss => self.dismiss_editor_results(),
                    _ => {}
                }
                if diagnostic { self.focus_editor(card) } else { Task::none() }
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
                    let ticket = document.active_save.take().expect("matched save ticket");
                    match result {
                        Ok(SaveOutcome::Saved {
                            content_hash,
                            durability,
                        }) => {
                            document.saved_text = saved_text;
                            document.disk_hash = content_hash;
                            document.saved_history_revision = ticket.history_revision;
                            document.saved_snapshot = ticket.snapshot_source;
                            document.refresh_dirty();
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
                    if self.mutation_refresh_pending || self.saves_elsewhere() {
                        // Status may have been read before this save reached
                        // disk. Supersede that read rather than let it erase the
                        // newly saved worktree change. A save through a
                        // symbolic link changed a different row from the
                        // selected one, and only Git knows which state that
                        // row is in now.
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
                    // A save changes the worktree, not its index baseline.
                    // Keep a visible native diff and recompute from the buffer;
                    // hidden disk comparisons are still refreshed on demand.
                    if !self.editable_working_diff() {
                        self.diffs
                            .retain(|(basis, _)| *basis != DiffBasis::IndexToWorktree);
                    }
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
            RepositoryAction::DiscardEdits { card } => {
                if !self.can_discard_edits() {
                    return Task::none();
                }
                // The toolbar promises an immediately undoable discard. Reveal
                // the backing editor, whose mounted key handler owns Ctrl+Z,
                // instead of leaving the keyboard on an immutable comparison.
                if self.source_surface == SourceSurface::Diff {
                    self.capture_native_viewport();
                    self.align_editor_to_diff();
                    self.source_surface = SourceSurface::Editor;
                    self.markdown_mode = MarkdownMode::Edit;
                    self.clear_find();
                    self.restore_native_viewport();
                }
                let DocumentState::Editable(document) = &mut self.document else {
                    return Task::none();
                };
                // `discard_edits` changes the buffer without going through
                // an editor action, so nothing else here would notice that
                // the server's copy just became wrong.
                let edits = document.discard_edits();
                self.push_lsp_edits(edits);
                // Find keeps the keyboard while it is open.
                if self.search_mode {
                    return self.markdown_task(card);
                }
                // The toolbar button took the keyboard; give it back so the
                // Ctrl+Z its tooltip promises reaches the buffer.
                Task::batch([
                    self.markdown_task(card),
                    self.restore_visible_scroll(),
                    self.focus_editor(card),
                ])
            }
            RepositoryAction::Undo { card } => self.step_history(card, false),
            RepositoryAction::Redo { card } => self.step_history(card, true),
            RepositoryAction::BackToFiles { card } => {
                if self.descriptor.selected_file.is_some() || self.blocks_navigation() {
                    return Task::none();
                }
                self.retain_viewports();
                self.push_lsp_close();
                self.selected = None;
                self.committed_selection = None;
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
                self.capture_native_viewport();
                if surface == SourceSurface::Editor {
                    self.align_editor_to_diff();
                } else {
                    self.align_diff_to_editor();
                }
                self.source_surface = surface;
                self.find_cache = None;
                self.clear_diff_find();
                if self.find_open {
                    self.refresh_find_matches();
                }
                self.restore_native_viewport();
                Task::batch([
                    self.ensure_current_diff(card),
                    self.restore_visible_scroll(),
                ])
            }
            RepositoryAction::ApplyMarkdownMode { card, preview } => {
                if !self.can_apply_markdown_mode(preview) {
                    return Task::none();
                }
                self.capture_native_viewport();
                // Preview has its own retained viewport. Only an explicit
                // return to Edit follows the source line currently in Diff.
                if !preview {
                    self.align_editor_to_diff();
                }
                self.source_surface = SourceSurface::Editor;
                self.diffs
                    .retain(|(_, state)| !matches!(state, DiffState::Loading { .. }));
                self.markdown_mode = if preview {
                    MarkdownMode::Preview
                } else {
                    MarkdownMode::Edit
                };
                if preview && let DocumentState::Editable(document) = &mut self.document {
                    // The only reader of the parsed items is about to be
                    // mounted, so this is the deadline the debounce was
                    // deferring to — and the one tick a shell may have
                    // filtered away while a modal was open.
                    document.sync_markdown();
                }
                self.restore_native_viewport();
                Task::batch([self.restore_visible_scroll(), self.diagram_task(card)])
            }
            RepositoryAction::DiagramsRendered {
                identity,
                revision,
                tone_generation,
                result,
            } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                let DocumentState::Editable(document) = &mut self.document else {
                    return Task::none();
                };
                if document.diagram_pending != Some(revision)
                    || document.diagram_tone_generation != tone_generation
                {
                    return Task::none();
                }
                document.diagram_pending = None;
                document.diagram_cancel = None;
                if document.revision == revision {
                    document.diagrams.extend(result);
                    self.diagram_task(identity.card)
                } else {
                    // A superseded worker never bypasses the typing debounce.
                    self.markdown_task(identity.card)
                }
            }
            RepositoryAction::MarkdownDue { card, ticket } => self.finish_markdown(card, ticket),
            RepositoryAction::ApplyDiffMode { split, .. } => {
                if !self.can_change_diff_layout() {
                    return Task::none();
                }
                let mode = if split {
                    DiffMode::Split
                } else {
                    DiffMode::Inline
                };
                if mode == self.diff_mode {
                    return Task::none();
                }
                self.capture_native_viewport();
                self.capture_diff_viewport();
                let mut previous = self.current_diff_viewport();
                // On the first visit to the other layout, keep the same line
                // on screen: the layouts number their rows differently.
                // Subsequent visits restore that layout independently.
                if let Some(model) = self.diff_model()
                    && let Some(diff_layout::Anchor::Row { row, fraction }) = previous.row
                {
                    previous.row = Some(diff_layout::Anchor::Row {
                        row: corresponding_row((model, self.diff_mode), row, (model, mode)),
                        fraction,
                    });
                }
                self.diff_mode = mode;
                self.clear_diff_selection();
                self.clear_diff_find();
                let anchor = self.viewports.diff[self.diff_mode_index()].unwrap_or(previous);
                self.apply_diff_viewport(anchor);
                self.restore_native_viewport();
                self.diff_change = self.diff_change.min(self.change_count().saturating_sub(1));
                self.restore_visible_scroll()
            }
            RepositoryAction::OpenWorkingDiff { card, file, basis } => {
                if self.blocks_navigation()
                    || file.directory
                    || self.directory_only
                    || !matches!(basis, DiffBasis::HeadToIndex | DiffBasis::IndexToWorktree)
                    || RepoPath::parse(&file.path).is_err()
                {
                    return Task::none();
                }
                let task = self.open_file_entry(card, file);
                self.source_surface = SourceSurface::Diff;
                self.diff_basis = basis;
                Task::batch([task, self.ensure_current_diff(card)])
            }
            RepositoryAction::OpenCommittedFile {
                card,
                path,
                old_path,
                basis,
            } => self.open_committed_file(card, path, old_path, basis),
            RepositoryAction::SelectBranchBase { card, base_ref } => {
                let offered = matches!(&self.basis_resolution,
                    Some(BasisResolution::NeedsBase(candidates)) if candidates.contains(&base_ref));
                if !offered || self.is_saving() {
                    return Task::none();
                }
                let Some(selected) = &self.selected else {
                    return Task::none();
                };
                let identity = self.current_identity(card, selected.path.clone());
                self.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::Branch));
                cosmic::task::future(async move {
                    let worktree = identity.worktree.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        resolved_branch_basis(&worktree, base_ref)
                    })
                    .await
                    .map_err(|error| format!("basis worker failed: {error}"))
                    .and_then(|result| result);
                    RepositoryAction::DiffBasisResolved {
                        identity,
                        choice: CommittedChoice::Branch,
                        result,
                    }
                })
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
                self.capture_native_viewport();
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
                        self.basis_resolution = Some(BasisResolution::NoCommits);
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
                self.capture_native_viewport();
                self.basis_resolution = None;
                self.source_surface = SourceSurface::Diff;
                self.diff_basis = basis;
                self.reset_diff_viewports();
                let load = self.ensure_current_diff(card);
                self.restore_native_viewport();
                Task::batch([load, self.restore_visible_scroll()])
            }
            RepositoryAction::RetryDiff { card } => {
                if !self.can_retry_diff() {
                    return Task::none();
                }
                if self.editable_working_diff() && self.working_diff.error.is_some() {
                    self.working_diff.applied = None;
                    self.working_diff.error = None;
                    return self.working_diff_task(card);
                }
                let Some(selected) = self.selected.as_ref() else {
                    return Task::none();
                };
                let Some(subject) = self.subject_file() else {
                    return Task::none();
                };
                if !self.file_has_basis(&subject, &self.diff_basis) {
                    return Task::none();
                }
                let previous_path = self.comparison_old_path();
                let identity = self.current_identity(card, selected.path.clone());
                let basis = self.diff_basis.clone();
                self.request_diff(identity, basis, previous_path)
            }
            RepositoryAction::PreviewViewport { offset, .. } => {
                if self.source_surface == SourceSurface::Editor
                    && self.markdown_mode == MarkdownMode::Preview
                {
                    self.viewports.preview = offset;
                }
                Task::none()
            }
            RepositoryAction::Review { .. } => Task::none(),
            RepositoryAction::ReviewLine {
                identity,
                basis,
                mut action,
                extend,
            } => {
                if !self.accepts_selected(&identity) || self.diff_basis != basis {
                    return Task::none();
                }
                if let Some(anchor) = review_begin_anchor(&mut action) {
                    if extend
                        && let Some((previous_identity, previous_basis, previous)) =
                            &self.review_line_anchor
                        && previous_identity == &identity
                        && previous_basis == &basis
                        && previous.path == anchor.path
                        && previous.side == anchor.side
                    {
                        let origin = previous.start_line;
                        anchor.start_line = origin.min(anchor.line);
                        anchor.line = origin.max(anchor.line);
                    } else {
                        self.review_line_anchor = Some((identity.clone(), basis, anchor.clone()));
                    }
                }
                Task::done(RepositoryAction::Review {
                    card: identity.card,
                    action,
                })
            }
            RepositoryAction::DiffAnnotations {
                identity,
                basis,
                mode,
                layout_revision,
                leading,
                rows,
                ..
            } => {
                if !self.accepts_selected(&identity)
                    || self.diff_basis != basis
                    || self.diff_mode != mode
                    || self.diff_layout_revision != layout_revision
                {
                    return Task::none();
                }
                self.set_diff_annotation_heights(mode, leading, &rows)
            }
            RepositoryAction::DiffViewport {
                offset,
                height,
                horizontal,
                user_initiated,
                ..
            } => {
                if user_initiated {
                    self.pending_diff_anchor = None;
                    self.pending_diff_source = None;
                }
                self.diff_viewport = ListViewport { offset, height };
                self.diff_horizontal = horizontal;
                if user_initiated
                    && let (Some(model), Some(layout)) = (self.diff_model(), self.diff_layout())
                {
                    let rows = layout.rows(self.diff_mode);
                    self.diff_change = model
                        .change_starts(self.diff_mode)
                        .partition_point(|row| {
                            rows.offset(layout.folded(self.diff_mode).display(*row)) <= offset + 0.5
                        })
                        .saturating_sub(1);
                }
                Task::none()
            }
            RepositoryAction::PreviousChange { .. } => {
                self.pending_diff_source = None;
                if !self.can_navigate_diff() {
                    return Task::none();
                }
                if !self.navigate_change(false) {
                    return Task::none();
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::NextChange { .. } => {
                self.pending_diff_source = None;
                if !self.can_navigate_diff() {
                    return Task::none();
                }
                if !self.navigate_change(true) {
                    return Task::none();
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::RevealDiffTop { card } => {
                if self.is_saving() || self.selected.is_none() || !self.can_show_diff() {
                    return Task::none();
                }
                // File discussions exist only above a diff, so a reader in
                // the source view is taken to it.
                let surface = if self.source_surface == SourceSurface::Diff {
                    Task::none()
                } else {
                    self.capture_native_viewport();
                    self.source_surface = SourceSurface::Diff;
                    self.restore_native_viewport();
                    self.ensure_current_diff(card)
                };
                // The block being revealed is usually not measured yet. An
                // anchor at the top of the leading block holds the viewport
                // there when its height arrives, rather than preserving the
                // first code row and scrolling the new block out of view.
                self.apply_diff_viewport(DiffViewportAnchor {
                    offset: 0.0,
                    row: Some(diff_layout::Anchor::Leading(0.0)),
                    first_change: false,
                    horizontal: self.diff_horizontal,
                    change: self.diff_change,
                });
                self.capture_diff_viewport();
                Task::batch([surface, self.restore_visible_scroll()])
            }
            RepositoryAction::ExpandDiffFold {
                identity,
                basis,
                mode,
                fold,
                run,
                expansion,
            } => {
                if !self.accepts_selected(&identity)
                    || self.diff_basis != basis
                    || !self.can_change_diff_layout()
                    || self
                        .diff_model()
                        .is_none_or(|model| model.unchanged_runs(mode).get(fold) != Some(&run))
                {
                    return Task::none();
                }
                self.expand_diff_fold(fold, expansion)
            }
            RepositoryAction::MarkdownLink { .. }
            | RepositoryAction::OpenExternal { .. }
            | RepositoryAction::OpenWorkingFileLocation { .. } => Task::none(),
            RepositoryAction::WorkingDiffDue { card, ticket } => {
                self.start_working_diff(card, ticket)
            }
            RepositoryAction::WorkingDiffLoaded {
                identity,
                revision,
                ticket,
                before,
                result,
            } => self.finish_working_diff(identity, revision, ticket, before, result),
            RepositoryAction::OpenFind { card } => self.open_find(card),
            RepositoryAction::CloseFind { card } => self.close_find(card),
            RepositoryAction::FindQueryChanged { query, .. } => {
                self.find_query = query;
                self.find_origin = None;
                // A new query knows nothing of where the last one had got
                // to, so it starts at its own first occurrence.
                self.find_index = 0;
                self.select_find_match(0);
                Task::none()
            }
            RepositoryAction::FindOptionsChanged {
                option, enabled, ..
            } => {
                match option {
                    crate::ui_controls::FindOption::CaseSensitive => {
                        self.find_options.case_sensitive = enabled
                    }
                    crate::ui_controls::FindOption::Regex => self.find_options.regex = enabled,
                    crate::ui_controls::FindOption::WholeWord => {
                        self.find_options.whole_word = enabled
                    }
                }
                self.select_find_match(0);
                Task::none()
            }
            RepositoryAction::FindSelectionChanged { enabled, .. } => {
                self.find_selection = enabled
                    .then(|| {
                        self.find_selection_candidate
                            .or_else(|| self.current_selection_scope())
                    })
                    .flatten();
                self.select_find_match(0);
                Task::none()
            }
            RepositoryAction::PreserveCaseChanged { enabled, .. } => {
                self.replace_preserve_case = enabled;
                Task::none()
            }
            RepositoryAction::FindNext { .. } => {
                self.step_find(true);
                if self.shows_diff() {
                    self.restore_visible_scroll()
                } else {
                    Task::none()
                }
            }
            RepositoryAction::FindPrevious { .. } => {
                self.step_find(false);
                if self.shows_diff() {
                    self.restore_visible_scroll()
                } else {
                    Task::none()
                }
            }
            RepositoryAction::FindInputFocus { focused, .. } => {
                self.find_input_focused = focused;
                Task::none()
            }
            RepositoryAction::FindLoaded { ticket, result, .. } => {
                self.finish_find(ticket, result);
                Task::none()
            }
            RepositoryAction::ReplaceQueryChanged { query, .. } => {
                self.replace_query = query;
                Task::none()
            }
            RepositoryAction::ToggleReplace { .. } => {
                if self.shows_diff() {
                    return Task::none();
                }
                self.replace_open = !self.replace_open;
                Task::none()
            }
            RepositoryAction::ReplaceMatch { card, all } => {
                if self.is_saving() || self.find_cache.as_ref() != self.wanted_find().as_ref() {
                    return Task::none();
                }
                let DocumentState::Editable(document) = &mut self.document else {
                    return Task::none();
                };
                if self.find_matches.is_empty() {
                    return Task::none();
                }
                if all {
                    let snapshot = document.content.search_snapshot();
                    let text = match snapshot.replace_matches_with(
                        &self.find_matches,
                        &self.find_query,
                        &self.replace_query,
                        self.find_options,
                        self.replace_preserve_case,
                    ) {
                        Ok(text) => text,
                        Err(error) => {
                            self.find_error = Some(error);
                            return Task::none();
                        }
                    };
                    document.content.perform(text_editor::Action::SelectAll);
                    document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
                        Arc::new(text),
                    )));
                } else {
                    let (start, end) = self.find_matches[self.find_index];
                    let replacement = document.content.with_line(start.line, |line, _| {
                        crate::editor_search::replacement(
                            line,
                            start.column,
                            end.column,
                            &self.find_query,
                            &self.replace_query,
                            self.find_options,
                            self.replace_preserve_case,
                        )
                    });
                    let replacement = match replacement {
                        Some(Ok(replacement)) => replacement,
                        _ => {
                            self.find_error =
                                Some("The selected match changed; search again.".into());
                            return Task::none();
                        }
                    };
                    document.content.move_to(text_editor::Cursor {
                        position: end,
                        selection: Some(start),
                    });
                    document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
                        Arc::new(replacement),
                    )));
                }
                self.push_lsp_change(None);
                self.markdown_task(card)
            }
            RepositoryAction::ToggleToolbarOverflow { .. } => {
                self.toolbar_overflow_open = !self.toolbar_overflow_open;
                Task::none()
            }
            RepositoryAction::CloseToolbarOverflow { .. } => {
                self.toolbar_overflow_open = false;
                Task::none()
            }
            RepositoryAction::ToggleDiffWhitespace { card } => {
                self.set_diff_ignore_whitespace(!self.diff_ignore_whitespace);
                self.ensure_current_diff(card)
            }
            RepositoryAction::SelectDiffRow {
                identity,
                basis,
                mode,
                revision,
                row,
                side,
                extend,
                context,
                drag,
            } => {
                self.select_diff_row(
                    identity, basis, mode, revision, row, side, extend, context, drag,
                );
                Task::none()
            }
            RepositoryAction::EndDiffSelection { .. } => {
                self.diff_interaction.dragging = false;
                Task::none()
            }
            RepositoryAction::MoveDiffSelection { delta, extend, .. } => {
                self.move_diff_selection(delta, extend)
            }
            RepositoryAction::ClearDiffSelection { .. } => {
                self.clear_diff_selection();
                Task::none()
            }
            RepositoryAction::DiffCommand {
                card,
                generation,
                revision,
                mode,
                sources,
                command,
            } => self.run_diff_command(card, generation, revision, mode, sources, command),
            RepositoryAction::DiffFindLoaded { ticket, result, .. } => {
                self.finish_diff_find(ticket, result)
            }
            RepositoryAction::ZoomFont { .. } | RepositoryAction::RevertFile { .. } => Task::none(),
        }
    }

    /// The state of one comparison, or `None` when it has never been asked for.
    fn diff_state(&self, basis: &DiffBasis) -> Option<&DiffState> {
        self.diffs
            .iter()
            .find(|(candidate, _)| candidate == basis)
            .map(|(_, state)| state)
    }

    fn set_diff_state(&mut self, basis: DiffBasis, mut state: DiffState) {
        if basis == self.diff_basis {
            self.clear_diff_selection();
            self.clear_diff_find();
        }
        if let DiffState::Ready(_, layout) = &mut state {
            layout.set_row_height(self.diff_typography().line_height);
        }
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        let restores_current = basis == self.diff_basis && matches!(&state, DiffState::Ready(_, _));
        // The rows on screen before a rebuild, which a rebuild must not move:
        // display rows and pixel offsets both change meaning with the model.
        let mut rebuilt = None;
        if restores_current
            && let Some(previous) = self.diff_model()
            && let Some(DiffState::Ready(_, previous_layout)) = self.diff_state(&basis)
            && let DiffState::Ready(RenderedDiff::Text { model, .. }, layout) = &mut state
        {
            // What the reader unfolded survives whatever the viewport awaits.
            layout.carry_reveals(model, (previous, previous_layout));
            // A pending anchor is logical and is applied below as it stands.
            if self.pending_diff_anchor.is_none() {
                let mut anchor = self.current_diff_viewport();
                if let Some(diff_layout::Anchor::Row { row, fraction }) = anchor.row {
                    anchor.row = Some(diff_layout::Anchor::Row {
                        row: corresponding_row(
                            (previous, self.diff_mode),
                            row,
                            (model, self.diff_mode),
                        ),
                        fraction,
                    });
                }
                rebuilt = Some(anchor);
            }
        }
        if let DiffState::Ready(RenderedDiff::Text { model, .. }, layout) = &mut state {
            layout.set_folding(model, self.diff_hide_unchanged);
        }
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
        if let Some(mut anchor) = rebuilt {
            anchor.change = anchor.change.min(self.change_count().saturating_sub(1));
            self.apply_diff_viewport(anchor);
        } else if restores_current && let Some(mut anchor) = self.pending_diff_anchor {
            anchor.change = anchor.change.min(self.change_count().saturating_sub(1));
            self.apply_diff_viewport(anchor);
            // Leading threads may not be measured yet; retain their logical
            // anchor until the first annotation projection or actual scroll.
            // A first visit resolves itself, and is only held while it sits
            // at the top, above which file-level threads may still appear.
            if !anchor.first_change {
                self.pending_diff_anchor = Some(anchor);
            }
        }
        if restores_current && self.shows_diff() && self.apply_pending_diff_source() {
            self.pending_diff_source = None;
        }
    }

    /// Drops the committed comparisons added longest ago, beyond the cap.
    ///
    /// Never the one on screen, and never a working basis. Dropping a loading
    /// slot cancels its worker; the completion fence also prevents an already
    /// queued answer from recreating that abandoned slot.
    fn prune_committed_diffs(&mut self) {
        let committed = is_committed_choice;
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
        let showing_diff = self.diff_surface_visible
            && self.source_surface == SourceSurface::Diff
            && self.basis_resolution.is_none();
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
        let Some(subject) = self.subject_file() else {
            return Task::none();
        };
        if !self.file_has_basis(&subject, &self.diff_basis) {
            return Task::none();
        }
        let old_path = self.comparison_old_path();
        // The result is matched to the row. Git reads the file the buffer is.
        let identity = self.current_identity(card, selected.path.clone());
        self.request_diff(identity, self.diff_basis.clone(), old_path)
    }

    fn request_diff(
        &mut self,
        identity: AsyncIdentity,
        basis: DiffBasis,
        old_path: Option<PathBuf>,
    ) -> Task<RepositoryAction> {
        if !self.diff_surface_visible || (self.mutation_refresh_pending && !basis.is_immutable()) {
            return Task::none();
        }
        self.next_diff_request = self.next_diff_request.wrapping_add(1).max(1);
        let request = self.next_diff_request;
        let cancellation = CancellationToken::new();
        let pathspec = self
            .diff_pathspec()
            .unwrap_or_else(|| identity.path.clone());
        self.set_diff_state(
            basis.clone(),
            DiffState::Loading {
                request,
                cancellation: cancellation.clone(),
            },
        );
        diff_task(
            identity,
            basis,
            request,
            old_path,
            pathspec,
            self.syntax_theme,
            self.diff_ignore_whitespace,
            self.editor_font,
            cancellation,
        )
    }

    /// The rename or copy source the old side of the visible comparison reads.
    pub fn comparison_old_path(&self) -> Option<PathBuf> {
        if let Some((basis, old_path)) = &self.committed_selection
            && *basis == self.diff_basis
        {
            return old_path.clone();
        }
        self.subject_file()?.previous_path(&self.diff_basis)
    }

    fn open_committed_file(
        &mut self,
        card: CardId,
        path: RepoPath,
        old_path: Option<RepoPath>,
        basis: DiffBasis,
    ) -> Task<RepositoryAction> {
        if self.blocks_navigation() || self.directory_only || !basis.is_immutable() {
            return Task::none();
        }
        let path = path.as_path().to_path_buf();
        let old_path = old_path.map(|path| path.as_path().to_path_buf());
        let retained = self
            .retained_viewports
            .iter()
            .position(|(candidate, _)| *candidate == path)
            .and_then(|index| self.retained_viewports.remove(index))
            .map(|(_, anchors)| anchors);
        self.retain_viewports();
        self.push_lsp_close();
        self.viewports = retained.unwrap_or_default();
        self.selected = Some(RepositoryFile {
            path: path.clone(),
            ..RepositoryFile::default()
        });
        self.document = DocumentState::None;
        self.diffs.clear();
        self.basis_resolution = None;
        self.source_surface = SourceSurface::Diff;
        self.diff_basis = basis.clone();
        self.committed_selection = Some((basis.clone(), old_path.clone()));
        self.apply_diff_viewport(
            self.viewports.diff[self.diff_mode_index()].unwrap_or(DiffViewportAnchor::FIRST_CHANGE),
        );
        let identity = self.next_identity(card, path);
        self.request_diff(identity, basis, old_path)
    }

    #[cfg(test)]
    pub(crate) fn diff_snapshot_fixture(
        card: CardId,
        path: &str,
        before: &str,
        after: &str,
    ) -> (Self, AsyncIdentity) {
        let (mut runtime, identity) = Self::open_fixture(card, path, after);
        runtime.document = DocumentState::None;
        runtime.source_surface = SourceSurface::Diff;
        runtime.set_diff_state(
            runtime.diff_basis.clone(),
            DiffState::ready(RenderedDiff::Text {
                model: Arc::new(crate::diff_view::build_diff(before, after)),
                old_label: "Before".into(),
                new_label: "After".into(),
            }),
        );
        (runtime, identity)
    }

    pub(crate) fn diff_working_file_target(
        &self,
        identity: &AsyncIdentity,
    ) -> Option<RepositoryView> {
        (!self.is_saving() && self.accepts_selected(identity) && self.shows_diff())
            .then(|| self.descriptor.clone())
    }

    /// Opens a Git/diff source line counted by LF separators. Native editor
    /// lines also split bare CR, so defer mapping until the actual file loads.
    pub fn open_source_file_location(&mut self, card: CardId, path: PathBuf, source_line: usize) -> Task<RepositoryAction> {
        if self.selected_path() == Some(path.as_path())
            && let DocumentState::Editable(document) = &self.document {
            let line = document.content.source_line_ranges().get(source_line).map_or_else(|| document.content.line_count().saturating_sub(1), |range| range.start);
            return self.open_file_location(card, path, line, 0, 0);
        }
        let task = self.open_file_location(card, path.clone(), 0, 0, 0);
        if self.selected_path() == Some(path.as_path()) {
            match &mut self.document {
                DocumentState::Loading => self.pending_source_location = Some((path, source_line)),
                DocumentState::Editable(document) => {
                    let line = document.content.source_line_ranges().get(source_line).map_or_else(|| document.content.line_count().saturating_sub(1), |range| range.start);
                    document.content.set_cursor(text_editor::Cursor { position: text_editor::Position { line, column: 0 }, selection: None });
                    document.content.restore_scroll_position(text_editor::ScrollPosition { line: line.saturating_sub(10), vertical: 0.0, horizontal: 0.0 });
                }
                _ => {},
            }
        }
        task
    }

    /// Open a repository-relative location, even before the directory listing
    /// arrives. Coordinates are zero-based native line/UTF-8 byte positions.
    /// The requested range is clamped when the asynchronous file read finishes.
    pub fn open_file_location(
        &mut self,
        card: CardId,
        path: PathBuf,
        line: usize,
        column: usize,
        selection_bytes: usize,
    ) -> Task<RepositoryAction> {
        if crate::repository::RepoPath::parse(&path).is_err() {
            return Task::none();
        }
        self.pending_source_location = None;
        let caret = text_editor::Position { line, column };
        let selection = (selection_bytes > 0).then_some(text_editor::Position {
            line,
            column: column.saturating_add(selection_bytes),
        });
        if self.selected_path() == Some(path.as_path())
            && let DocumentState::Editable(document) = &mut self.document
        {
            document.content.set_cursor(text_editor::Cursor {
                position: caret,
                selection,
            });
            document
                .content
                .restore_scroll_position(text_editor::ScrollPosition {
                    line: line.saturating_sub(10),
                    vertical: 0.0,
                    horizontal: 0.0,
                });
            self.source_surface = SourceSurface::Editor;
            self.markdown_mode = MarkdownMode::Edit;
            return self.focus_editor(card);
        }
        if self.blocks_navigation() {
            return Task::none();
        }
        let loading_same = self.selected_path() == Some(path.as_path())
            && matches!(self.document, DocumentState::Loading);
        let task = if loading_same {
            Task::none()
        } else {
            let file = self.file(&path).cloned().unwrap_or(RepositoryFile {
                path: path.clone(),
                ..RepositoryFile::default()
            });
            if file.directory {
                return Task::none();
            }
            self.open_file_entry(card, file)
        };
        self.pending_editor_navigation = Some((
            path,
            crate::model::EditorNavigation {
                caret: crate::model::EditorPosition { line, column },
                selection: selection.map(|position| crate::model::EditorPosition {
                    line: position.line,
                    column: position.column,
                }),
                scroll_line: line.saturating_sub(10),
                scroll_vertical: 0.0,
                scroll_horizontal: 0.0,
            },
        ));
        task
    }

    fn open_file(&mut self, card: CardId, path: PathBuf) -> Task<RepositoryAction> {
        let file = match self.file(&path).cloned() {
            Some(file) => file,
            None if (self.standalone
                || self.directory_only
                || self.descriptor.selected_file.is_some())
                && crate::repository::RepoPath::parse(&path).is_ok() =>
            {
                RepositoryFile {
                    path: path.clone(),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: false,
                    status: None,
                    directory: false,
                }
            }
            None => return Task::none(),
        };
        self.open_file_entry(card, file)
    }

    fn open_file_entry(&mut self, card: CardId, file: RepositoryFile) -> Task<RepositoryAction> {
        if let Err(error) = self.protect_editor_worktree(&self.descriptor.worktree.clone()) {
            self.files = FileListState::Failed(error.to_string());
            return Task::none();
        }
        self.pending_fragment = None;
        self.pending_editor_navigation = None;
        self.pending_source_location = None;
        self.pending_editor_focus = None;
        if self.descriptor.selected_file.is_some() {
            self.descriptor.selected_file = Some(file.path.clone());
        }
        let path = file.path.clone();
        let reopening = self.selected.as_ref().is_some_and(|file| file.path == path);
        // Take the destination before retaining the departing file, which may
        // evict the oldest anchor when the cache is full.
        let retained = self
            .retained_viewports
            .iter()
            .position(|(retained, _)| *retained == path)
            .and_then(|index| self.retained_viewports.remove(index))
            .map(|(_, anchors)| anchors);
        self.retain_viewports();
        if !reopening {
            self.viewports = retained.unwrap_or_default();
        }
        // Whatever was open is about to be replaced — including by a reload of
        // this very path, which is a different document however identical its
        // name. Emitted before the replacement, while the old buffer still
        // exists to be closed.
        self.push_lsp_close();
        self.committed_selection = None;
        self.selected = Some(file.clone());
        self.document = DocumentState::Loading;
        self.diffs.clear();
        self.basis_resolution = None;
        self.apply_diff_viewport(
            self.viewports.diff[self.diff_mode_index()].unwrap_or(DiffViewportAnchor::FIRST_CHANGE),
        );
        self.diff_basis = if file.unstaged {
            DiffBasis::IndexToWorktree
        } else {
            DiffBasis::HeadToIndex
        };
        self.source_surface = SourceSurface::Editor;
        self.markdown_mode = MarkdownMode::Edit;
        self.document_tone = pdf_tone(&cosmic::theme::active());
        let identity = self.next_identity(card, path);
        file_task(identity, self.document_tone)
    }

    fn diff_mode_index(&self) -> usize {
        match self.diff_mode {
            DiffMode::Inline => 0,
            DiffMode::Split => 1,
        }
    }

    fn current_diff_viewport(&self) -> DiffViewportAnchor {
        if let Some(anchor) = self.pending_diff_anchor {
            return anchor;
        }
        let mode = self.diff_mode;
        DiffViewportAnchor {
            offset: self.diff_viewport.offset,
            row: self.diff_layout().map(|layout| {
                layout
                    .folded(mode)
                    .to_model(layout.rows(mode).anchor(self.diff_viewport.offset))
            }),
            horizontal: self.diff_horizontal,
            change: self.diff_change,
            first_change: false,
        }
    }

    fn capture_diff_viewport(&mut self) {
        let index = self.diff_mode_index();
        self.viewports.diff[index] = Some(self.current_diff_viewport());
    }

    fn apply_diff_viewport(&mut self, anchor: DiffViewportAnchor) {
        if anchor.first_change {
            self.diff_horizontal = anchor.horizontal;
            self.diff_change = 0;
            let offset = self.first_change_offset();
            // Until the comparison has rows, the first change has no place;
            // at the top, file-level threads measured later belong above it.
            self.pending_diff_anchor = offset.is_none_or(|offset| offset == 0.0).then_some(anchor);
            self.diff_viewport.offset = offset.unwrap_or(0.0);
            return;
        }
        let awaiting_geometry = anchor.row.is_some()
            && self.diff_rows().is_none_or(|rows| {
                matches!(anchor.row, Some(diff_layout::Anchor::Leading(_)))
                    && rows.leading_height() == 0.0
            });
        self.pending_diff_anchor = awaiting_geometry.then_some(anchor);
        let mode = self.diff_mode;
        self.diff_viewport.offset =
            anchor
                .row
                .zip(self.diff_layout())
                .map_or(anchor.offset, |(anchor, layout)| {
                    let offset = layout
                        .rows(mode)
                        .restore(layout.folded(mode).to_display(anchor));
                    self.reachable_diff_offset(layout.rows(mode), offset)
                });
        self.diff_horizontal = anchor.horizontal;
        self.diff_change = anchor.change;
    }

    /// Where a first visit to the current comparison starts: its first
    /// change, below its [`diff_layout::FOLD_CONTEXT`] rows of context and
    /// the row before them, which is the fold when one precedes the context,
    /// so the reader sees what was folded away. A change that close to the
    /// top starts at the top, which keeps any file-level review threads above
    /// the first row in view. `None` while the comparison is still loading.
    fn first_change_offset(&self) -> Option<f32> {
        let (model, layout) = match self.diff_state(&self.diff_basis)? {
            DiffState::Ready(RenderedDiff::Text { model, .. }, layout) => (model, layout),
            DiffState::Ready(..) => return Some(0.0),
            DiffState::Loading { .. } => return None,
        };
        let Some(&first) = model.change_starts(self.diff_mode).first() else {
            return Some(0.0);
        };
        let row = layout
            .folded(self.diff_mode)
            .display(first)
            .saturating_sub(diff_layout::FOLD_CONTEXT + 1);
        let rows = layout.rows(self.diff_mode);
        Some(if row == 0 {
            0.0
        } else {
            self.reachable_diff_offset(rows, rows.offset(row))
        })
    }

    fn reset_diff_viewports(&mut self) {
        self.pending_diff_source = None;
        self.clear_diff_selection();
        self.clear_diff_find();
        self.viewports.diff = [None; 2];
        self.apply_diff_viewport(DiffViewportAnchor::FIRST_CHANGE);
    }

    /// The editable buffer's scroll position, retained while a surface that
    /// does not mount it is on screen.
    fn capture_native_viewport(&mut self) {
        if !self.editable_surface_visible() {
            return;
        }
        if let DocumentState::Editable(document) = &self.document {
            self.viewports.editor = document.content.scroll_position();
        }
    }

    fn restore_native_viewport(&mut self) {
        if !self.editable_surface_visible() {
            return;
        }
        let position = self.viewports.editor;
        if let DocumentState::Editable(document) = &mut self.document {
            document.content.restore_scroll_position(position);
        }
    }

    fn align_editor_to_diff(&mut self) {
        if self.source_surface != SourceSurface::Diff {
            return;
        }
        if let Some((line, fraction, horizontal)) = self.pending_diff_source.take() {
            self.viewports.editor = text_editor::ScrollPosition {
                line: self.editor_line_for_diff_source(line),
                vertical: fraction as f32 * self.editor_font_size * self.line_height,
                horizontal,
            };
            return;
        }
        let Some(diff_layout::Anchor::Row { row, fraction }) = self.current_diff_viewport().row
        else {
            return;
        };
        let Some(model) = self.diff_model() else {
            return;
        };
        let line = model.new_line_at_row(self.diff_mode, row);
        if let Some(line) = line {
            self.viewports.editor = text_editor::ScrollPosition {
                line: self.editor_line_for_diff_source(line),
                vertical: fraction as f32 * self.editor_font_size * self.line_height,
                horizontal: self.diff_horizontal,
            };
        }
    }

    fn editor_line_for_diff_source(&self, line: u32) -> usize {
        let source = line.saturating_sub(1) as usize;
        let DocumentState::Editable(document) = &self.document else {
            return source;
        };
        if document.content.text_snapshot().protocol_compatible_lines() {
            return source;
        }
        document
            .content
            .source_line_ranges()
            .get(source)
            .map_or_else(
                || document.content.line_count().saturating_sub(1),
                |range| range.start,
            )
    }

    fn diff_source_for_editor_line(&self, line: usize) -> u32 {
        let DocumentState::Editable(document) = &self.document else {
            return line.saturating_add(1) as u32;
        };
        if document.content.text_snapshot().protocol_compatible_lines() {
            return line.saturating_add(1) as u32;
        }
        let ranges = document.content.source_line_ranges();
        ranges
            .partition_point(|range| range.end <= line)
            .min(ranges.len().saturating_sub(1))
            .saturating_add(1) as u32
    }

    fn align_diff_to_editor(&mut self) {
        if !self.editable_surface_visible() {
            return;
        }
        let position = self.viewports.editor;
        let fraction =
            (position.vertical / (self.editor_font_size * self.line_height)).clamp(0.0, 1.0) as f64;
        self.pending_diff_source = Some((
            self.diff_source_for_editor_line(position.line),
            fraction,
            position.horizontal,
        ));
        let applied = self.apply_pending_diff_source();
        if applied
            && (self.diff_basis != DiffBasis::IndexToWorktree
                || (self.working_diff.applied_rich && self.working_annotations_current()))
        {
            self.pending_diff_source = None;
        }
    }

    /// Gutter-only refreshes intentionally leave rich rows stale. Keep the
    /// editor's source line until the replacement arrives; an old model row
    /// cannot name that line correctly after insertions above the viewport.
    fn apply_pending_diff_source(&mut self) -> bool {
        let Some((line, fraction, horizontal)) = self.pending_diff_source else {
            return false;
        };
        let Some(row) = self
            .diff_model()
            .and_then(|model| model.row_at_new_line(self.diff_mode, line))
        else {
            return false;
        };
        let hidden = self.diff_layout().and_then(|layout| {
            let folded = layout.folded(self.diff_mode);
            match folded.row(folded.display(row)) {
                DisplayRow::Fold { fold, .. } => Some(fold),
                _ => None,
            }
        });
        if let Some(fold) = hidden {
            drop(self.expand_diff_fold(fold, FoldExpansion::All));
        }
        self.apply_diff_viewport(DiffViewportAnchor {
            row: Some(diff_layout::Anchor::Row { row, fraction }),
            horizontal,
            first_change: false,
            ..self.current_diff_viewport()
        });
        true
    }

    fn retain_viewports(&mut self) {
        if self.selected.is_none() {
            return;
        }
        self.capture_native_viewport();
        self.capture_diff_viewport();
        let path = self.selected.as_ref().expect("selected file").path.clone();
        if let Some(index) = self
            .retained_viewports
            .iter()
            .position(|(retained, _)| *retained == path)
        {
            self.retained_viewports.remove(index);
        }
        if self.retained_viewports.len() == MAX_RETAINED_FILE_VIEWPORTS {
            self.retained_viewports.pop_front();
        }
        self.retained_viewports.push_back((path, self.viewports));
    }

    /// Run after the new surface has been mounted. Restoring virtual diff
    /// rows also requires the runtime offset, set before building that surface.
    fn restore_visible_scroll(&self) -> Task<RepositoryAction> {
        let (id, x, y) = match self.source_surface {
            SourceSurface::Diff if self.diff_model().is_some() => (
                &self.diff_scroll_id,
                self.diff_horizontal,
                self.diff_viewport.offset,
            ),
            SourceSurface::Editor
                if self.markdown_mode == MarkdownMode::Preview
                    && matches!(&self.document, DocumentState::Editable(_)) =>
            {
                (&self.preview_scroll_id, 0.0, self.viewports.preview)
            }
            _ => return Task::none(),
        };
        iced::widget::scrollable::scroll_to(
            id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: Some(x),
                y: Some(y),
            },
        )
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

    pub(crate) fn accepts_file_list(&self, identity: &AsyncIdentity) -> bool {
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

    /// Capture the model currently ready for this basis in constant time.
    /// A stale display fallback is intentionally excluded. Mutation callers
    /// must also fence the live buffer revision before applying a patch.
    pub fn current_diff_model_snapshot(&self) -> Option<Arc<DiffModel>> {
        match self.diff_state(&self.diff_basis) {
            Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) => Some(Arc::clone(model)),
            _ => None,
        }
    }

    fn diff_model(&self) -> Option<&DiffModel> {
        match self.displayed_diff_state() {
            Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) => Some(model),
            _ => None,
        }
    }

    fn diff_layout(&self) -> Option<&DiffLayout> {
        match self.displayed_diff_state() {
            Some(DiffState::Ready(_, layout)) => Some(layout),
            _ => None,
        }
    }

    fn displayed_diff_state(&self) -> Option<&DiffState> {
        let current = self.diff_state(&self.diff_basis);
        if matches!(current, Some(DiffState::Ready(..))) {
            return current;
        }
        self.stale_diff
            .as_ref()
            .filter(|(path, basis, _)| {
                *basis == self.diff_basis
                    && self
                        .selected
                        .as_ref()
                        .is_some_and(|file| file.path == *path)
            })
            .map(|(_, _, state)| state)
            .or(current)
    }

    fn diff_rows(&self) -> Option<&RowOffsets> {
        self.diff_layout().map(|layout| layout.rows(self.diff_mode))
    }

    /// Installs measured review blocks for the active comparison. The caller
    /// fences document/request identity before applying asynchronous layout
    /// measurements. Heights exclude the code row itself; `leading` precedes
    /// the first row and holds discussions without a live line anchor.
    pub fn set_diff_annotation_heights(
        &mut self,
        mode: DiffMode,
        leading: f32,
        measurements: &[(usize, f32)],
    ) -> Task<RepositoryAction> {
        let anchor = self.current_diff_viewport();
        let Some((_, DiffState::Ready(_, layout))) = self
            .diffs
            .iter_mut()
            .find(|(basis, _)| *basis == self.diff_basis)
        else {
            return Task::none();
        };
        let changed = layout.rows_mut(mode).annotate(leading, measurements);
        if mode != self.diff_mode || (!changed && self.pending_diff_anchor.is_none()) {
            return Task::none();
        }
        self.apply_diff_viewport(anchor);
        self.pending_diff_anchor = None;
        self.capture_diff_viewport();
        self.restore_visible_scroll()
    }

    fn change_count(&self) -> usize {
        self.diff_model()
            .map_or(0, |model| model.change_starts(self.diff_mode).len())
    }

    fn change_target(&self, next: bool) -> Option<usize> {
        let (model, layout) = (self.diff_model()?, self.diff_layout()?);
        let rows = layout.rows(self.diff_mode);
        let starts = model.change_starts(self.diff_mode);
        let offset = self.diff_viewport.offset;
        // A short folded comparison may clamp multiple changes to the same
        // reachable scroll offset. Keep explicit stepping exact there.
        if self
            .current_change_offset()
            .is_some_and(|current| (current - offset).abs() < 0.5)
        {
            return if next {
                (self.diff_change + 1 < starts.len()).then_some(self.diff_change + 1)
            } else {
                self.diff_change.checked_sub(1)
            };
        }
        let position = |row: &usize| rows.offset(layout.folded(self.diff_mode).display(*row));
        if next {
            let target = starts.partition_point(|row| position(row) <= offset + 0.5);
            (target < starts.len()).then_some(target)
        } else {
            starts
                .partition_point(|row| position(row) < offset - 0.5)
                .checked_sub(1)
        }
    }

    fn navigate_change(&mut self, next: bool) -> bool {
        let Some(target) = self.change_target(next) else {
            return false;
        };
        self.diff_change = target;
        true
    }

    /// The offset that puts the current change at the top of the viewport.
    /// Changes are never folded, so the change's own row is always drawn.
    fn current_change_offset(&self) -> Option<f32> {
        let row = *self
            .diff_model()?
            .change_starts(self.diff_mode)
            .get(self.diff_change)?;
        let layout = self.diff_layout()?;
        let rows = layout.rows(self.diff_mode);
        Some(self.reachable_diff_offset(
            rows,
            rows.offset(layout.folded(self.diff_mode).display(row)),
        ))
    }

    /// `offset` limited to what the scrollable can actually reach. Folding
    /// makes comparisons short, so a change near the end is often less than
    /// a viewport from the bottom. The scrollable clamps a jump there on its
    /// own, but virtualization reads this runtime offset: left unclamped, it
    /// mounts rows below the ones on screen and the viewport draws blank.
    fn reachable_diff_offset(&self, rows: &RowOffsets, offset: f32) -> f32 {
        let height = self.diff_viewport.height;
        if !(height.is_finite() && height > 0.0) {
            return offset;
        }
        // The column ends with the horizontal thumb's rail.
        let content = rows.total_height() + crate::design::SCROLLBAR_GUTTER;
        offset.min((content - height).max(0.0))
    }

    fn realize_current_change(&mut self) {
        // An explicit step supersedes whatever position was still waiting for
        // geometry; measuring review threads later must not undo it.
        self.pending_diff_anchor = None;
        if let Some(offset) = self.current_change_offset() {
            self.diff_viewport.offset = offset;
        }
    }

    fn scroll_to_change(&self) -> Task<RepositoryAction> {
        let Some(offset) = self.current_change_offset() else {
            return Task::none();
        };
        iced::widget::scrollable::scroll_to(
            self.diff_scroll_id.clone(),
            iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(offset),
            },
        )
    }

    /// Reveals rows of fold `fold` while keeping the rows on screen where
    /// they are: the viewport is anchored in model rows across the change.
    fn expand_diff_fold(
        &mut self,
        fold: usize,
        expansion: FoldExpansion,
    ) -> Task<RepositoryAction> {
        // The reader is acting on what is on screen, not on a position that
        // was still waiting to be placed.
        self.pending_diff_anchor = None;
        let anchor = self.current_diff_viewport();
        let basis = self.diff_basis.clone();
        let Some((_, DiffState::Ready(RenderedDiff::Text { model, .. }, layout))) = self
            .diffs
            .iter_mut()
            .find(|(candidate, _)| *candidate == basis)
        else {
            return Task::none();
        };
        if !layout.expand(model, fold, expansion) {
            return Task::none();
        }
        // Display rows moved: annotation measurements taken against the old
        // rows no longer name the rows they measured.
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.apply_diff_viewport(anchor);
        self.capture_diff_viewport();
        self.restore_visible_scroll()
    }
}

/// Reads the card's file list off the update thread.
///
/// `deposit` is where a successful read leaves the digest it produced,
/// tagged with the generation that asked for it. It travels in a cell rather
/// than in the action because [`RepositoryAction::FilesLoaded`] is matched by
/// both shells and its shape is not this module's to change; the generation
/// tag stops a superseded read committing its digest over a newer one's,
/// which would leave the card believing a list it never installed.
fn files_task(
    identity: AsyncIdentity,
    directory_only: bool,
    previous: Option<blake3::Hash>,
    deposit: Arc<std::sync::Mutex<Option<(u64, blake3::Hash)>>>,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let generation = identity.generation;
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if directory_only {
                load_directory_files(&worktree).map(|files| FileListRead::Listed(None, files))
            } else {
                read_repository_files(&worktree, previous)
            }
        })
        .await
        .map_err(|error| format!("file-list worker failed: {error}"))
        .and_then(|result| result);
        match result {
            Ok(FileListRead::Unchanged) => RepositoryAction::FileListUnchanged { identity },
            Ok(FileListRead::Listed(digest, files)) => {
                if let Some(digest) = digest {
                    *deposit
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some((generation, digest));
                }
                RepositoryAction::FilesLoaded {
                    identity,
                    result: Ok(files),
                }
            }
            Err(error) => RepositoryAction::FilesLoaded {
                identity,
                result: Err(error),
            },
        }
    })
}

/// How a PDF page's colors should be adapted for `theme`.
///
/// A document is authored on white paper. Reading that on a dark shell at
/// night is what the inverted tone exists for, so the default follows the
/// theme rather than making the reader ask for it every time, and the page's
/// paper is the theme's own raised surface so a document reads as a sheet
/// lying on the shell rather than a hole cut out of it.
///
/// The theme is a parameter rather than read from the global here, because a
/// shell theme change has to re-render the open page with the theme it is
/// changing to, which the application knows before the global is updated.
fn pdf_tone(theme: &cosmic::Theme) -> crate::editor::PdfTone {
    if theme.theme_type.is_dark() {
        pdf_tone_inverted(theme)
    } else {
        crate::editor::PdfTone::Original
    }
}

/// The inverted tone for `theme`.
///
/// Separate because the toolbar's toggle asks for it directly: the reader who
/// presses it has decided they want the page adapted whatever the shell is.
fn pdf_tone_inverted(theme: &cosmic::Theme) -> crate::editor::PdfTone {
    let paper = theme.current_container().component.base;
    crate::editor::PdfTone::Inverted {
        paper: [
            (paper.red * 255.0).round() as u8,
            (paper.green * 255.0).round() as u8,
            (paper.blue * 255.0).round() as u8,
        ],
    }
}

/// A hairline around a rendered document page.
fn page_edge(theme: &cosmic::Theme) -> cosmic::widget::container::Style {
    cosmic::widget::container::Style {
        border: iced::Border {
            color: crate::design::rule(theme),
            width: crate::design::BORDER_WIDTH,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// Reads the file, rendering a document's first page with `tone`.
fn file_task(identity: AsyncIdentity, tone: crate::editor::PdfTone) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || load_file(&worktree, &path, tone))
            .await
            .map_err(|error| FileIoError::Io(format!("file worker failed: {error}")))
            .and_then(|result| result);
        RepositoryAction::FileLoaded { identity, result }
    })
}

/// Rasterizes one page in a child process.
///
/// A page is tens of milliseconds of CPU at reading resolution and a complex
/// one is considerably more, so it never runs on the thread that has to draw
/// the next frame; and because a damaged document can take its renderer down
/// with it, the renderer is not this process.
fn pdf_page_task(
    card: CardId,
    path: PathBuf,
    view: PdfView,
    request: u64,
) -> Task<RepositoryAction> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            crate::editor::render_document_page(&path, view.page, view.tone, view.zoom)
        })
        .await
        .unwrap_or_else(|error| Err(format!("page worker failed: {error}")));
        RepositoryAction::PdfPageRendered {
            card,
            request,
            result,
        }
    })
}

fn diff_task(
    identity: AsyncIdentity,
    basis: DiffBasis,
    request: u64,
    old_path: Option<PathBuf>,
    pathspec: PathBuf,
    syntax_theme: crate::settings::EditorSyntaxTheme,
    ignore_whitespace: bool,
    font: Font,
    cancellation: CancellationToken,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let requested = basis.clone();
    cosmic::task::future(async move {
        let content = tokio::task::spawn_blocking(move || {
            load_repository_diff_with_options_and_font(
                &worktree,
                &pathspec,
                &requested,
                old_path.as_deref(),
                syntax_theme,
                ignore_whitespace,
                font,
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
                .snapshot_with_options(
                    worktree,
                    crate::repository::SnapshotOptions {
                        untracked_changes: false,
                        untracked_files: false,
                    },
                    &cancellation,
                )
                .map_err(|error| error.to_string())?;
            Ok(match snapshot.default_branch_base() {
                Some(base_ref) => return resolved_branch_basis(worktree, base_ref.clone()),
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

fn resolved_branch_basis(
    worktree: &Path,
    base_ref: ValidatedRefName,
) -> Result<ResolvedBasis, String> {
    let (head, merge_base, _) = RepositoryService::system("git")
        .resolve_branch_range(worktree, &base_ref, &CancellationToken::new())
        .map_err(|error| error.to_string())?;
    Ok(ResolvedBasis::Ready(DiffBasis::MergeBaseToHead {
        base_ref,
        head,
        merge_base,
    }))
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

/// Lists one directory without following symlinked directories or walking
/// descendants. Folder trees start collapsed, so a recursive walk would spend
/// the first paint of `Documents/unorganized` enumerating files the panel
/// will not show. Children are listed when the folder is expanded.
pub fn load_directory_files(directory: &Path) -> Result<Vec<RepositoryFile>, String> {
    load_directory_listing(directory, Path::new(""))
}
/// Recursively searches a directory for quick-open results without changing
/// the sidebar's intentionally lazy tree listing.
pub fn search_directory_files_for_quick_open(
    root: &Path,
    query: &str,
) -> Result<Vec<RepositoryFile>, String> {
    search_directory_files_for_quick_open_cancellable(root, query, &CancellationToken::default())
}

/// [`search_directory_files_for_quick_open`] with a caller-owned token, so a
/// superseded quick-open query stops walking instead of finishing a scan
/// nobody will read.
pub fn search_directory_files_for_quick_open_cancellable(
    root: &Path,
    query: &str,
    token: &CancellationToken,
) -> Result<Vec<RepositoryFile>, String> {
    search_directory_files(root, query, token).map(|(files, _)| files)
}

/// Lists expanded ignored folders without allowing one failed read to hide
/// the worktree. Failed folders are retried only after an explicit user action.
pub(crate) fn expand_ignored_directories(
    root: &Path,
    files: &mut crate::repository::WorkingFiles,
    expanded: &[RepoPath],
    previous_errors: &BTreeMap<RepoPath, String>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    for path in expanded {
        if !files
            .ignored_directories
            .iter()
            .any(|directory| path.as_path().starts_with(directory.as_path()))
        {
            continue;
        }
        if cancellation.is_cancelled() {
            return Err("file listing cancelled".into());
        }
        if let Some(error) = previous_errors.get(path) {
            files
                .ignored_directory_errors
                .insert(path.clone(), error.clone());
            continue;
        }
        match load_directory_listing(root, path.as_path()) {
            Ok(entries) => {
                for entry in entries {
                    let Ok(path) = RepoPath::parse(&entry.path) else {
                        continue;
                    };
                    if entry.directory {
                        files.ignored_directories.push(path);
                    } else {
                        files.ignored.push(path);
                    }
                }
            }
            Err(error) => {
                files.ignored_directory_errors.insert(path.clone(), error);
            }
        }
    }
    Ok(())
}

pub(crate) fn load_directory_listing(
    root: &Path,
    relative: &Path,
) -> Result<Vec<RepositoryFile>, String> {
    use std::os::fd::AsRawFd;
    let handle_root = crate::fs_safety::Root::open(root).map_err(|error| error.to_string())?;
    let handle = if relative.as_os_str().is_empty() {
        std::fs::File::open(root).map_err(|error| error.to_string())?
    } else {
        crate::fs_safety::open_beneath(&handle_root, relative).map_err(|error| error.to_string())?
    };
    let descriptor_path = format!("/proc/self/fd/{}", handle.as_raw_fd());
    let mut files = Vec::new();
    let mut entries = 0usize;
    for entry in std::fs::read_dir(descriptor_path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        entries += 1;
        if entries > 20_000 {
            return Err("This folder has more than 20,000 entries; open a narrower folder.".into());
        }
        let name = entry.file_name();
        if name == "." || name == ".." || name == ".git" {
            continue;
        }
        let path = if relative.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            relative.join(&name)
        };
        if crate::repository::RepoPath::parse(&path).is_err() {
            continue;
        }
        let metadata = crate::fs_safety::metadata_beneath(&handle_root, &path)
            .map_err(|error| error.to_string())?;
        files.push(RepositoryFile {
            path,
            directory: metadata.is_dir(),
            ..RepositoryFile::default()
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

const MAX_FILE_SEARCH_VISITS: usize = 50_000;

fn search_directory_files(
    root: &Path,
    query: &str,
    token: &CancellationToken,
) -> Result<(Vec<RepositoryFile>, usize), String> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut queue = VecDeque::from([PathBuf::new()]);
    let mut matches = Vec::new();
    let mut hidden = 0usize;
    let mut visited = 0usize;
    while let Some(directory) = queue.pop_front() {
        if token.is_cancelled() {
            return Ok((matches, hidden));
        }
        let entries = match load_directory_listing(root, &directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_FILE_SEARCH_VISITS {
                hidden = hidden.max(1);
                matches.sort_by(|left, right| left.path.cmp(&right.path));
                return Ok((matches, hidden));
            }
            if entry.directory {
                queue.push_back(entry.path);
                continue;
            }
            let haystack = entry.path.to_string_lossy().to_ascii_lowercase();
            if !haystack.contains(&needle) {
                continue;
            }
            if matches.len() < crate::sidebar::MAX_VISIBLE_FILE_ROWS {
                matches.push(entry);
            } else {
                hidden += 1;
            }
        }
    }
    matches.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((matches, hidden))
}

/// What one file-list read produced.
enum FileListRead {
    /// Git's raw output is byte-for-byte what the card already digested, so
    /// there is nothing to rebuild and nothing to repaint.
    Unchanged,
    /// A fresh list, with the digest of the read behind it. Folder tabs
    /// carry no digest: they list one level and are merged into the tree.
    Listed(Option<blake3::Hash>, Vec<RepositoryFile>),
}

pub fn load_repository_files(worktree: &Path) -> Result<Vec<RepositoryFile>, String> {
    match read_repository_files(worktree, None)? {
        FileListRead::Listed(_, files) => Ok(files),
        // A read with nothing to compare against always lists.
        FileListRead::Unchanged => Ok(Vec::new()),
    }
}

/// Reads the repository's file list, reporting [`FileListRead::Unchanged`]
/// when Git's raw output is byte-for-byte what `previous` digested.
///
/// The digest is taken over the raw NUL-delimited output rather than over the
/// assembled list, so an idle card's ten-second refresh tick costs three Git
/// reads and a hash instead of a `BTreeMap` of every path in the repository,
/// two `PathBuf` allocations per file, and a view invalidation.
fn read_repository_files(
    worktree: &Path,
    previous: Option<blake3::Hash>,
) -> Result<FileListRead, String> {
    let tracked = git_output(worktree, ["ls-files", "--cached", "-z"])?;
    let untracked = git_output(
        worktree,
        ["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let status = git_output(
        worktree,
        ["status", "--porcelain=v1", "--untracked-files=all", "-z"],
    )?;
    let digest = repository_file_digest(&tracked, &untracked, &status);
    if previous == Some(digest) {
        return Ok(FileListRead::Unchanged);
    }
    let files = assemble_repository_files(&tracked, &untracked, &status)?;
    Ok(FileListRead::Listed(Some(digest), files))
}

/// Digest of one file-list read.
///
/// Each part is length-prefixed: the three outputs are concatenated, and
/// without a length a path moving from the tracked list to the untracked one
/// could hash the same as the state before it moved.
fn repository_file_digest(tracked: &[u8], untracked: &[u8], status: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for part in [tracked, untracked, status] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize()
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
                directory: false,
            }
        })
        .collect())
}

/// Git's global options for every invocation this module makes, ahead of
/// `-C` because Git only accepts global options before the subcommand.
///
/// `--literal-pathspecs` stops a file literally named `:(top)x` from being
/// read as a magic pathspec.
///
/// `core.fsmonitor=false` is set for the same reason
/// [`crate::repository::RepositoryService::git_base`] sets it: it is a trust
/// boundary, not a latency measure. Configured to anything but a boolean,
/// `core.fsmonitor` names a program Git runs whenever it refreshes the index,
/// and `ls-files` and `status` both refresh the index. A repository that
/// arrived as an archive carries its own `.git/config`, so without this
/// merely opening a card would run code the repository chose — no click
/// required, since the card lists its files on creation and every ten
/// seconds after. `GIT_OPTIONAL_LOCKS=0` only suppresses writing the
/// refreshed index back; it does not stop the fsmonitor being asked.
const GIT_GLOBAL_OPTIONS: [&str; 3] = ["--literal-pathspecs", "-c", "core.fsmonitor=false"];

fn git_command(worktree: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(GIT_GLOBAL_OPTIONS)
        .arg("-C")
        .arg(worktree)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
    command
}

fn git_output<const N: usize>(worktree: &Path, arguments: [&str; N]) -> Result<Vec<u8>, String> {
    let output = git_command(worktree)
        .args(arguments)
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
    view_with_review(card, runtime, None)
}

pub fn view_with_review<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    review: Option<&'a crate::pull_request_review::State>,
) -> Element<'a, RepositoryAction> {
    // File browsing lives in the sidebar tree, and the flat list exists only
    // for diff/change review. With nothing selected the card reports its
    // file-listing state instead of duplicating the tree as a second flat
    // browser: a loading or failed list must stay visible with its retry.
    let body = if runtime.selected.is_some() {
        selected_file_view(card, runtime, review)
    } else {
        match &runtime.files {
            FileListState::Loading => loading(),
            FileListState::Failed(error) => widget::container(
                widget::column::with_children(vec![
                    information_state("Could not list files", error.clone()).into(),
                    crate::ui_controls::button::secondary("Retry loading files")
                        .on_press_maybe(
                            (!runtime.file_list_loading)
                                .then_some(RepositoryAction::RefreshFiles { card }),
                        )
                        .into(),
                ])
                .align_x(Alignment::Center)
                .spacing(space::L),
            )
            .center(Length::Fill)
            .into(),
            FileListState::Ready(_) => information_state(
                "No file selected",
                "Choose a file from the repository list.",
            ),
        }
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

/// Focus target of the find strip's query input, derived from the card for
/// the same reason [`editor_focus_id`] is: the focus operation the open
/// action returns is performed on a later frame than the one that asked for
/// it.
pub fn find_focus_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.find"))
}

pub fn focus_restore_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.view"))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExternalEditorCommand<'a>(CardId, &'a str);

/// An entry in the editor's context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorMenuCommand {
    Undo(CardId),
    Redo(CardId),
    Clipboard(CardId, ClipboardOp),
}

impl EditorMenuCommand {
    fn command(self) -> crate::workspace_widget::GlobalShortcut {
        use crate::workspace_widget::GlobalShortcut as Command;
        match self {
            Self::Undo(_) => Command::EditorUndo,
            Self::Redo(_) => Command::EditorRedo,
            Self::Clipboard(_, ClipboardOp::Cut) => Command::EditorCut,
            Self::Clipboard(_, ClipboardOp::Copy) => Command::EditorCopy,
            Self::Clipboard(_, ClipboardOp::Paste) => Command::EditorPaste,
            Self::Clipboard(_, ClipboardOp::SelectAll) => Command::EditorSelectAll,
        }
    }
}

/// `Some(None)` retires an original native editing chord after remapping;
/// `None` leaves unrelated native navigation and form input unchanged.
fn registered_editor_binding(card: CardId, runtime: &RepositoryRuntime, press: &iced::widget::text_editor::KeyPress) -> Option<Option<text_editor::Binding<RepositoryAction>>> {
    if !matches!(press.status, iced::widget::text_editor::Status::Focused { .. }) {
        return None;
    }
    if let Some(command) = crate::workspace_widget::editor_shortcut(&press.key, press.physical_key, press.modifiers, &runtime.keyboard_shortcuts) {
        return Some(editor_command_action(card, command).map(text_editor::Binding::Custom));
    }
    use iced::keyboard::Key;
    if text_editor::history_command(press).is_some() || (press.modifiers.command() && !press.modifiers.alt() && !press.modifiers.shift() && matches!(&press.key, Key::Character(key) if matches!(key.to_ascii_lowercase().as_str(), "x" | "c" | "v" | "a"))) { return Some(None); }
    None
}

pub fn editor_command_action(card: CardId, command: crate::workspace_widget::GlobalShortcut) -> Option<RepositoryAction> {
    use crate::workspace_widget::GlobalShortcut as Command;
    Some(match command {
        Command::NextDiagnostic | Command::PreviousDiagnostic => RepositoryAction::Language { card, action: EditorLspAction::Diagnostic(command == Command::NextDiagnostic) },
        Command::EditorUndo => RepositoryAction::Undo { card },
        Command::EditorRedo => RepositoryAction::Redo { card },
        Command::EditorCut => RepositoryAction::Clipboard { card, op: ClipboardOp::Cut },
        Command::EditorCopy => RepositoryAction::Clipboard { card, op: ClipboardOp::Copy },
        Command::EditorPaste => RepositoryAction::Clipboard { card, op: ClipboardOp::Paste },
        Command::EditorSelectAll => RepositoryAction::Clipboard { card, op: ClipboardOp::SelectAll },
        _ => return None,
    })
}

impl widget::menu::Action for EditorMenuCommand {
    type Message = RepositoryAction;
    fn message(&self) -> RepositoryAction {
        let card = match *self { Self::Undo(card) | Self::Redo(card) | Self::Clipboard(card, _) => card };
        editor_command_action(card, self.command()).expect("registered editor menu command")
    }
}

/// The editor's context menu: history, then the clipboard, each entry
/// showing the chord that does the same thing.
fn editor_context_menu(card: CardId, runtime: &RepositoryRuntime) -> Vec<widget::menu::Tree<RepositoryAction>> {
    use widget::menu::Item;
    let commands = [
        EditorMenuCommand::Undo(card), EditorMenuCommand::Redo(card),
        EditorMenuCommand::Clipboard(card, ClipboardOp::Cut), EditorMenuCommand::Clipboard(card, ClipboardOp::Copy),
        EditorMenuCommand::Clipboard(card, ClipboardOp::Paste), EditorMenuCommand::Clipboard(card, ClipboardOp::SelectAll),
    ];
    let capabilities = runtime.editor_command_capabilities();
    let mut hints = HashMap::new();
    let mut items = Vec::with_capacity(7);
    for (index, command) in commands.into_iter().enumerate() {
        let action = command.command();
        if let Some(chord) = crate::workspace_widget::menu_shortcut(action, &runtime.keyboard_shortcuts) { hints.insert(chord, command); }
        if index == 2 { items.push(Item::Divider); }
        let label = crate::workspace_widget::shortcut_descriptor(action).label;
        items.push(if capabilities & crate::workspace_widget::editor_command_bit(action).expect("editor action") != 0 {
            Item::Button(label, None, command)
        } else { Item::ButtonDisabled(label, None, command) });
    }
    widget::menu::items(&hints, items)
}

impl ExternalEditorCommand<'_> {
    fn command(self) -> crate::workspace_widget::GlobalShortcut { crate::workspace_widget::GlobalShortcut::OpenInExternalEditor }
}

fn external_editor_action(card: CardId, editor_id: &str) -> RepositoryAction {
    RepositoryAction::OpenExternal { card, editor_id: editor_id.to_owned() }
}

impl widget::menu::Action for ExternalEditorCommand<'_> {
    type Message = RepositoryAction;
    fn message(&self) -> RepositoryAction { external_editor_action(self.0, self.1) }
}

fn toolbar_group<'a>(
    controls: Vec<Element<'a, RepositoryAction>>,
) -> Element<'a, RepositoryAction> {
    crate::ui_controls::toolbar_actions(controls)
}

/// Dedicated content toolbar, mounted separately from tabs and the scroll viewport.
pub fn selected_file_toolbar(
    card: CardId,
    runtime: &RepositoryRuntime,
) -> Option<Element<'_, RepositoryAction>> {
    selected_file_toolbar_with(card, runtime, None)
}

/// [`selected_file_toolbar`] with a shell's own control group at its trailing
/// edge, such as a pull request review's file actions.
pub fn selected_file_toolbar_with<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    trailing: Option<Element<'a, RepositoryAction>>,
) -> Option<Element<'a, RepositoryAction>> {
    selected_file_toolbar_mapped_with(card, runtime, trailing, std::convert::identity)
}

/// Compose shell actions into the file header while retaining the find strip
/// as a separate row. The shell owns its message type and its extra controls.
pub fn selected_file_toolbar_mapped_with<'a, M: 'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    trailing: Option<Element<'a, M>>,
    map: impl Fn(RepositoryAction) -> M + Clone + 'a,
) -> Option<Element<'a, M>> {
    let selected = runtime.selected.as_ref()?;
    let editing = runtime.source_surface == SourceSurface::Editor
        && runtime.markdown_mode == MarkdownMode::Edit;
    let editable = matches!(runtime.document, DocumentState::Editable(_));
    // A buffer reached through a symbolic link says so in the chrome, even in
    // a standalone window whose tab already names the file: the row the
    // reader clicked is not the file their keystrokes land in, and the file
    // list will report the save against the other name.
    let linked = runtime.linked_source();
    let title = (!runtime.standalone || linked.is_some()).then(|| {
        let file_name = |path: &Path| {
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
        };
        let one_line = |text: widget::text::Text<'static, cosmic::Theme, cosmic::Renderer>| {
            text.wrapping(iced::widget::text::Wrapping::None).ellipsize(
                iced::widget::text::Ellipsize::Start(
                    iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                ),
            )
        };
        let mut label: Vec<Element<'_, RepositoryAction>> = Vec::new();
        if !runtime.standalone {
            label.push(crate::file_icon::icon(&selected.path).into());
            // The name gives way to the caption only when there is one; on
            // its own it fills the slot as it always has.
            label.push(
                one_line(crate::design::body(
                    selected.path.to_string_lossy().into_owned(),
                ))
                .width(if linked.is_some() {
                    Length::Shrink
                } else {
                    Length::Fill
                })
                .into(),
            );
        }
        let tooltip = match linked {
            Some(source) => {
                let caption = if runtime.standalone {
                    format!(
                        "{} \u{2192} {}",
                        file_name(&selected.path),
                        file_name(source)
                    )
                } else {
                    format!("\u{2192} {}", file_name(source))
                };
                label.push(one_line(crate::design::caption(caption)).into());
                format!("{} \u{2192} {}", selected.path.display(), source.display())
            }
            None => selected.path.to_string_lossy().into_owned(),
        };
        widget::container(crate::interaction::tooltip(
            widget::row::with_children(label)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .width(Length::Fill),
            tooltip,
        ))
        .width(Length::FillPortion(1))
        .max_width(220)
    });
    // Noneditable load/error states
    // still expose file information, so inspecting a binary diff is reversible.
    let file_information = !editable && !matches!(runtime.document, DocumentState::Image { .. });
    let preview = icon_button(
        Icon::Preview,
        if file_information {
            "View file information"
        } else {
            "Preview file"
        },
    )
    .selected(
        runtime.source_surface == SourceSurface::Editor
            && (!editable || !editing)
            && (file_information || runtime.can_apply_markdown_mode(true)),
    )
    .on_press_maybe(
        (if file_information {
            !runtime.is_saving()
        } else {
            runtime.can_apply_markdown_mode(true)
        })
        .then(|| {
            if file_information {
                RepositoryAction::ApplySourceSurface { card, diff: false }
            } else {
                RepositoryAction::ApplyMarkdownMode {
                    card,
                    preview: true,
                }
            }
        }),
    );
    let preview = if editable {
        preview
    } else {
        preview.id(focus_restore_id(card))
    };
    let edit = icon_button(Icon::Edit, "Edit file")
        .selected(editable && editing && runtime.can_apply_markdown_mode(false))
        .on_press_maybe(runtime.can_apply_markdown_mode(false).then(|| {
            RepositoryAction::ApplyMarkdownMode {
                card,
                preview: false,
            }
        }));
    let edit = if editable {
        edit.id(focus_restore_id(card))
    } else {
        edit
    };
    let mut controls: Vec<Element<'_, RepositoryAction>> =
        vec![crate::ui_controls::segmented_controls(vec![
            preview.into(),
            edit.into(),
            icon_button(Icon::Diff, "View diff")
                .selected(runtime.source_surface == SourceSurface::Diff && runtime.can_show_diff())
                .on_press_maybe(
                    runtime
                        .can_show_diff()
                        .then(|| RepositoryAction::ApplySourceSurface { card, diff: true }),
                )
                .into(),
        ])];
    // File actions stay mounted whether or not they can run: an inactive
    // icon renders muted but keeps its tooltip, so the toolbar never jumps
    // around as edits, saves, and staging change what is available.
    let mut document_actions = Vec::new();
    if editable {
        document_actions.push(
            icon_button(
                Icon::RevertFile,
                "Discard unsaved edits (Ctrl+Z restores them)",
            )
            .on_press_maybe(
                runtime
                    .can_discard_edits()
                    .then(|| RepositoryAction::DiscardEdits { card }),
            )
            .into(),
        );
        controls.push(
            icon_button(
                Icon::Save,
                if runtime.is_saving() {
                    "Saving file…"
                } else if runtime.is_dirty() {
                    "Save unsaved changes"
                } else {
                    "Save file"
                },
            )
            .selected(runtime.is_dirty())
            .on_press_maybe(runtime.can_save().then(|| RepositoryAction::Save {
                card,
                overwrite: false,
            }))
            .into(),
        );
    }
    // Stage and unstage follow the file the buffer is. On a symbolic link
    // that is the target's row, or nothing when that row has no changes.
    let subject = runtime.subject_file();
    let staged = subject.as_ref().is_some_and(|file| file.staged);
    let unstaged = subject.as_ref().is_some_and(|file| file.unstaged);
    let staging: &[(bool, bool, Icon, &str)] = if runtime.reviews_pull_request() {
        &[]
    } else {
        &[
            (true, unstaged, Icon::Plus, "Stage file changes"),
            (false, staged, Icon::Minus, "Unstage file changes"),
        ]
    };
    for &(stage, has_changes, icon, label) in staging {
        document_actions.push(
            icon_button(icon, label)
                .on_press_maybe(
                    (has_changes
                        && !runtime.mutation_refresh_pending
                        && !runtime.blocks_navigation())
                    .then(|| RepositoryAction::StageSelected { card, stage }),
                )
                .into(),
        );
    }
    if !document_actions.is_empty() && runtime.source_surface != SourceSurface::Diff {
        controls.push(toolbar_group(document_actions));
    }
    // Diff navigation follows the file it compares: view modes, file
    // actions, then diff scope.
    if runtime.source_surface != SourceSurface::Diff
        && !runtime.directory_only
        && !runtime.reviews_pull_request()
    {
        controls.push(diff_basis_controls(card, runtime));
    }
    if editing && runtime.lsp_document.is_some() {
        let errors = runtime
            .language
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == crate::lsp::LspSeverity::Error)
            .count();
        let warnings = runtime
            .language
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == crate::lsp::LspSeverity::Warning)
            .count();
        controls.push(
            widget::button::custom(crate::design::caption(format!(
                "{errors} errors · {warnings} warnings"
            )))
            .class(crate::ui_controls::control_style(false, None))
            .on_press(RepositoryAction::Language {
                card,
                action: EditorLspAction::Diagnostics,
            })
            .into(),
        );
    }
    let mut secondary = Vec::new();
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
        let button = icon_button(
            icon(editor),
            format!("Open in {} · right-click for other editors", editor.name),
        )
        .on_press_maybe(runtime.external_editor_command(card, Some(&editor.id)));
        if runtime.is_saving() {
            secondary.push(button.into());
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
            // The unparameterized binding opens the configured default editor;
            // parameterized alternatives must not advertise that same chord.
            let bindings = runtime.external_editors.first().and_then(|editor| {
                let command = ExternalEditorCommand(card, editor.id.as_str());
                crate::workspace_widget::menu_shortcut(command.command(), &runtime.keyboard_shortcuts).map(|chord| (chord, command))
            }).into_iter().collect();
            secondary.push(crate::interaction::context_menu(
                button,
                widget::menu::items(&bindings, entries),
            ));
        }
    }
    if runtime.source_surface == SourceSurface::Diff {
        controls.extend(secondary);
    } else {
        // The common commands stay visible. File, comparison, review and external
        // editor commands use a popover rather than clipping an icon rail.
        let primary_count = if editable { 2 } else { 1 };
        let mut overflow: Vec<_> = controls.drain(primary_count..).collect();
        overflow.extend(secondary);
        let more = icon_button(Icon::More, "More file actions")
            .selected(runtime.toolbar_overflow_open)
            .on_press(RepositoryAction::ToggleToolbarOverflow { card });
        controls.push(crate::ui_controls::popover(
            more,
            runtime.toolbar_overflow_open.then(|| {
                widget::container(
                    widget::column::with_children(overflow)
                        .spacing(crate::design::TOOLBAR_GROUP_SPACING),
                )
                .padding(crate::design::TOOLBAR_INSET)
                .into()
            }),
            widget::popover::Position::Bottom,
            RepositoryAction::CloseToolbarOverflow { card },
        ));
    }
    let actions = widget::row::with_children(controls)
        .spacing(crate::design::TOOLBAR_GROUP_SPACING)
        .align_y(Alignment::Center)
        .height(crate::design::HEADER_CONTENT_HEIGHT);
    let mut header = Vec::with_capacity(3);
    if let Some(title) = title {
        let title: Element<'a, RepositoryAction> = title.into();
        header.push(title.map(map.clone()));
    }
    let actions: Element<'a, RepositoryAction> = actions.into();
    header.push(actions.map(map.clone()));
    header.extend(trailing);
    let toolbar = widget::container(
        widget::row::with_children(header)
            .spacing(crate::design::TOOLBAR_GROUP_SPACING)
            .align_y(Alignment::Center)
            .width(Length::Fill),
    )
    .padding([0.0, crate::design::TOOLBAR_INSET])
    .height(crate::design::HEADER_CONTENT_HEIGHT)
    .width(Length::Fill);
    // The strip is mounted here rather than beside the buffer because it is
    // a subordinate row of this editor's chrome, not a surface of its own:
    // it must sit under the toolbar and directly above the text it searches.
    let mut rows = vec![toolbar.into()];
    if runtime.source_surface == SourceSurface::Diff {
        let mapper = map.clone();
        rows.push(
            crate::ui_controls::responsive(move |size| {
                diff_action_toolbar(card, runtime, size.width).map(mapper.clone())
            })
            .height(Length::Shrink)
            .into(),
        );
    }
    if let Some(find) = find_strip(card, runtime) {
        rows.push(find.map(map));
    }
    Some(
        widget::column::with_children(rows)
            .width(Length::Fill)
            .into(),
    )
}

/// The find strip, while find is open over a buffer that is on screen.
fn find_strip<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
) -> Option<Element<'a, RepositoryAction>> {
    if !runtime.find_open || !(runtime.editable_surface_visible() || runtime.shows_diff()) {
        return None;
    }
    let matched = runtime.find_total != 0;
    let mut row: Vec<Element<'a, RepositoryAction>> = vec![
        icon_button(
            Icon::Search,
            if runtime.shows_diff() {
                "Find in diff"
            } else {
                "Find and replace"
            },
        )
        .selected(runtime.replace_open && !runtime.shows_diff())
        .on_press_maybe((!runtime.shows_diff()).then_some(RepositoryAction::ToggleReplace { card }))
        .into(),
        crate::ui_controls::field::search("Find in file", &runtime.find_query)
            .id(find_focus_id(card))
            .on_input(move |query| RepositoryAction::FindQueryChanged { card, query })
            .on_focus(RepositoryAction::FindInputFocus {
                card,
                focused: true,
            })
            .on_unfocus(RepositoryAction::FindInputFocus {
                card,
                focused: false,
            })
            .width(Length::Fill)
            .into(),
    ];
    // A count is read off the cache the search filled in; an empty query has
    // not searched for anything and says nothing rather than "No results".
    if !runtime.find_query.is_empty() {
        row.push(
            crate::design::caption(if runtime.find_error.is_some() {
                "Invalid expression".into()
            } else if if runtime.shows_diff() {
                runtime.diff_find_pending()
            } else {
                runtime.find_cache.as_ref() != runtime.wanted_find().as_ref()
            } {
                "Searching…".to_owned()
            } else if matched {
                format!("{} of {}", runtime.find_index + 1, runtime.find_total)
            } else {
                "No results".to_owned()
            })
            .into(),
        );
    }
    for (icon, label, action) in [
        (
            Icon::ChevronUp,
            "Previous match — Shift+Enter in search",
            RepositoryAction::FindPrevious { card },
        ),
        (
            Icon::ChevronDown,
            "Next match — Enter in search",
            RepositoryAction::FindNext { card },
        ),
    ] {
        row.push(
            icon_button(icon, label)
                .on_press_maybe(matched.then_some(action))
                .into(),
        );
    }
    row.push(
        icon_button(Icon::Close, "Close find — Escape")
            .on_press(RepositoryAction::CloseFind { card })
            .into(),
    );
    let find = widget::row::with_children(row)
        .spacing(space::S)
        .align_y(Alignment::Center)
        .width(Length::Fill);
    let mut strips: Vec<Element<'a, RepositoryAction>> = vec![find.into()];
    let mut options = vec![crate::ui_controls::find_options(
        runtime.find_options.case_sensitive,
        runtime.find_options.regex,
        runtime.find_options.whole_word,
        true,
        move |option, enabled| RepositoryAction::FindOptionsChanged {
            card,
            option,
            enabled,
        },
    )];
    if !runtime.shows_diff() {
        let selection_enabled =
            runtime.find_selection.is_some() || runtime.find_selection_candidate.is_some();
        let mut selection = crate::ui_controls::field::checkbox(runtime.find_selection.is_some())
            .label("Selection");
        if selection_enabled {
            selection = selection
                .on_toggle(move |enabled| RepositoryAction::FindSelectionChanged { card, enabled });
        }
        options.push(
            crate::interaction::tooltip(selection, "Search the selection captured when enabled")
                .into(),
        );
    }
    strips.push(
        widget::row::with_children(options)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
    );
    if let Some(error) = &runtime.find_error {
        strips.push(
            widget::text::body(format!("Invalid expression: {error}"))
                .size(12)
                .class(cosmic::theme::Text::Custom(crate::design::destructive_text))
                .into(),
        );
    }
    if runtime.replace_open && !runtime.shows_diff() {
        strips.push(
            widget::row::with_children(vec![
                crate::ui_controls::field::text("Replace with", &runtime.replace_query)
                    .on_input(move |query| RepositoryAction::ReplaceQueryChanged { card, query })
                    .width(Length::Fill)
                    .into(),
                {
                    let mut preserve =
                        crate::ui_controls::field::checkbox(runtime.replace_preserve_case)
                            .label("Preserve case");
                    if !runtime.find_options.regex {
                        preserve = preserve.on_toggle(move |enabled| {
                            RepositoryAction::PreserveCaseChanged { card, enabled }
                        });
                    }
                    crate::interaction::tooltip(
                        preserve,
                        "Keep each matched word's upper, lower or title case (plain text)",
                    )
                    .into()
                },
                crate::ui_controls::button::secondary("Replace")
                    .on_press_maybe(
                        (matched && !runtime.is_saving())
                            .then_some(RepositoryAction::ReplaceMatch { card, all: false }),
                    )
                    .into(),
                crate::ui_controls::button::secondary("Replace all")
                    .on_press_maybe(
                        (matched && !runtime.is_saving())
                            .then_some(RepositoryAction::ReplaceMatch { card, all: true }),
                    )
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
        );
    }
    Some(
        widget::container(widget::column::with_children(strips))
            .padding([0.0, space::M])
            .width(Length::Fill)
            .into(),
    )
}

fn selected_file_view<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    review: Option<&'a crate::pull_request_review::State>,
) -> Element<'a, RepositoryAction> {
    match runtime.source_surface {
        SourceSurface::Editor => editor_surface(card, runtime),
        SourceSurface::Diff => diff_surface(card, runtime, review),
    }
}

fn editor_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let content = match &runtime.document {
        DocumentState::None => information_state(
            "No file selected",
            "Choose a file from the repository list.",
        ),
        DocumentState::Loading => loading(),
        DocumentState::Failed { path, message } => {
            information_state(format!("Could not open {}", path.display()), message)
        }
        DocumentState::ReadOnly { path, reason } => read_only_state(path, reason),
        DocumentState::Editable(document) => editable_document_view(card, runtime, document),
        DocumentState::Image {
            handle,
            width,
            height,
            source: _,
        } => crate::interaction::tooltip(
            widget::container(
                iced::widget::image(handle.clone())
                    .content_fit(iced::ContentFit::Contain)
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .padding(space::M)
            .width(Length::Fill)
            .height(Length::Fill),
            format!("{width} × {height} pixels"),
        )
        .into(),
        DocumentState::Pdf {
            source: _,
            path: _,
            pages,
            page,
            tone,
            zoom,
            handle,
            width,
            height,
            points_wide,
            rendering,
            message,
            chosen_tone: _,
        } => pdf_document_view(
            card,
            *pages,
            PdfView {
                page: *page,
                tone: *tone,
                zoom: *zoom,
            },
            handle,
            *width,
            *height,
            *points_wide,
            rendering.is_some(),
            message,
        ),
    };
    widget::column::with_children([content])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// A paged document: the page, and the controls for moving through it.
///
/// The page counter is a label rather than a field because a reader turning
/// pages wants the next one, not a number entry; `Home` and `End` reach the
/// ends in one press through the first and last controls.
#[allow(clippy::too_many_arguments)]
fn pdf_document_view<'a>(
    card: CardId,
    pages: usize,
    view: PdfView,
    handle: &iced::widget::image::Handle,
    width: u32,
    height: u32,
    points_wide: f32,
    rendering: bool,
    message: &'a Option<String>,
) -> Element<'a, RepositoryAction> {
    let PdfView { page, tone, zoom } = view;
    let inverted = matches!(tone, crate::editor::PdfTone::Inverted { .. });
    let turn = |to: usize, glyph: Icon, label: &'static str, enabled: bool| {
        icon_button(glyph, label).on_press_maybe(enabled.then(|| RepositoryAction::PdfPage {
            card,
            page: to,
            tone,
            zoom,
        }))
    };
    let mut controls: Vec<Element<'a, RepositoryAction>> = vec![
        turn(0, Icon::First, "First page", page > 0).into(),
        turn(
            page.saturating_sub(1),
            Icon::ChevronLeft,
            "Previous page",
            page > 0,
        )
        .into(),
        crate::design::body(format!("{} / {pages}", page + 1)).into(),
        turn(page + 1, Icon::ChevronRight, "Next page", page + 1 < pages).into(),
        turn(pages - 1, Icon::Last, "Last page", page + 1 < pages).into(),
    ];
    // The status sits on the flexible gap's left, so that a render starting or
    // landing cannot move the controls on its right. The zoom controls are the
    // ones a reader presses repeatedly, and a caption appearing beneath the
    // cursor between two presses would put the second press on a different
    // control.
    //
    // A caption takes the gap rather than its own width, because a row lays
    // out its rigid children against what the ones before them left: a long
    // failure message sized to its text would leave the controls after it
    // nothing and collapse them to nothing at all.
    let caption = |text: String| {
        crate::design::caption(text)
            .width(Length::Fill)
            .wrapping(iced::widget::text::Wrapping::None)
    };
    if let Some(message) = message {
        controls.push(caption(message.clone()).into());
    } else if rendering {
        controls.push(caption("Rendering…".to_owned()).into());
    } else {
        controls.push(iced::widget::Space::new().width(Length::Fill).into());
    }
    let step = |to: Option<crate::editor::PdfZoom>, glyph: Icon, label: &'static str| {
        icon_button(glyph, label)
            .on_press_maybe(to.map(|zoom| RepositoryAction::PdfZoom { card, zoom }))
    };
    controls.push(step(zoom.smaller(), Icon::ZoomOut, "Draw the page smaller").into());
    controls.push(
        crate::interaction::tooltip(
            // A fixed width, because the label is between the two controls a
            // reader presses repeatedly and "Fit", "50%" and "125%" are three
            // different widths. The text fills that width so that it is the
            // label rather than the box that is centred on.
            widget::container(
                crate::design::body(
                    zoom.percent()
                        .map_or_else(|| "Fit".to_owned(), |percent| format!("{percent}%")),
                )
                .width(Length::Fill)
                .center(),
            )
            .width(Length::Fixed(ZOOM_LABEL_WIDTH)),
            "How large the page is drawn, against the size it would print",
        )
        .into(),
    );
    controls.push(step(zoom.larger(), Icon::ZoomIn, "Draw the page larger").into());
    controls.push(
        // Enabled even while fitted, because this is the mode the viewer is
        // in rather than an action: pressing it again asks for what is already
        // drawn, which the update rejects, and staying enabled is what lets
        // the selected surface say that fitting is what the size means now.
        icon_button(Icon::ZoomFit, "Fit the page to the pane")
            .selected(zoom.is_fit())
            .on_press(RepositoryAction::PdfZoom {
                card,
                zoom: crate::editor::PdfZoom::Fit,
            })
            .into(),
    );
    controls.push(
        icon_button(
            if inverted {
                Icon::Brightness
            } else {
                Icon::Night
            },
            if inverted {
                "Show the document's own colors"
            } else {
                "Adapt the document to a dark surface"
            },
        )
        .selected(inverted)
        .on_press(RepositoryAction::PdfTone {
            card,
            tone: if inverted {
                crate::editor::PdfTone::Original
            } else {
                // Deliberately re-read rather than remembered: the reader
                // may have changed their theme since the page was opened.
                pdf_tone_inverted(&cosmic::theme::active())
            },
        })
        .into(),
    );
    widget::column::with_children(vec![
        widget::container(
            widget::row::with_children(controls)
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .width(Length::Fill),
        )
        .padding([0.0, space::M])
        .height(crate::design::DENSE_ROW_HEIGHT)
        .width(Length::Fill)
        .into(),
        crate::interaction::tooltip(
            widget::container(pdf_page_surface(
                handle.clone(),
                zoom,
                points_wide,
                width,
                height,
            ))
            .padding(space::M)
            .width(Length::Fill)
            .height(Length::Fill),
            format!("Page {} of {pages}, {width} × {height} pixels", page + 1),
        )
        .into(),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The sheet the page is drawn on.
///
/// At `PdfZoom::Fit` the page fills the pane's width and scrolls, rather than
/// being shrunk to fit its height: a document is read a column at a time, and
/// a whole page scaled into a wide pane is too small to read. Every other step
/// is a fraction of the page's printed width, so nothing here measures the
/// pane. Measuring it would mean `Responsive`, which implements no
/// `a11y_nodes` and would take this region's scroll view and image out of the
/// accessibility tree.
fn pdf_page_surface(
    handle: iced::widget::image::Handle,
    zoom: crate::editor::PdfZoom,
    points_wide: f32,
    raster_width: u32,
    raster_height: u32,
) -> Element<'static, RepositoryAction> {
    let drawn = zoom.logical_width(points_wide).map(|width| {
        // Both axes are stated, at the raster's own aspect, so that
        // `ContentFit::Contain` fills the box exactly whichever way it has to
        // scale and the hairline sits on the paper rather than around a
        // letterboxed image. Left to `Length::Shrink` the height would
        // collapse to the raster's pixel height, which is the wrong shape for
        // any page drawn wider than it was rendered.
        let height = width * raster_height.max(1) as f32 / raster_width.max(1) as f32;
        (width, height)
    });
    let sheet = widget::container(
        iced::widget::image(handle)
            .content_fit(iced::ContentFit::Contain)
            .width(drawn.map_or(Length::Fill, |(width, _)| Length::Fixed(width)))
            .height(drawn.map_or(Length::Shrink, |(_, height)| Length::Fixed(height))),
    )
    // A hairline around the sheet, because a recolored page is close in tone
    // to the surface behind it and a reader needs to see where the paper ends.
    .class(cosmic::theme::Container::Custom(Box::new(page_edge)));
    // Both rails are reserved rather than left to float, because a thumb over
    // the page is a thumb over the text the reader zoomed in to read. A page
    // drawn at a fixed size may need either axis, so a scrollable that can
    // move on both is used whenever the size is the reader's choice. As in the
    // removed-lines pane, the reservation is an inset on the content and the
    // thumbs are anchored to the viewport, so it clears a thumb whenever the
    // other axis is at its end and only narrows the overlap elsewhere.
    //
    // The page's top left corner stays at the pane's, at every size the reader
    // chooses. Centring a page narrower than the pane would need the pane's
    // width, which a two-axis scrollable does not report while its content
    // fits, and it would move the page sideways as it crossed the pane's width
    // on the way in.
    let sheet = widget::container(sheet).padding(if drawn.is_some() {
        crate::design::with_both_rails(iced::Padding::ZERO)
    } else {
        crate::design::with_vertical_rail(iced::Padding::ZERO)
    });
    widget::scrollable(sheet)
        .direction(if drawn.is_some() {
            iced::widget::scrollable::Direction::Both {
                vertical: crate::design::scrollbar(),
                horizontal: crate::design::scrollbar(),
            }
        } else {
            iced::widget::scrollable::Direction::Vertical(crate::design::scrollbar())
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn editable_document_view<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    document: &'a EditorDocument,
) -> Element<'a, RepositoryAction> {
    let mut error_rows: Vec<Element<'a, RepositoryAction>> = Vec::new();
    if let Some(error) = document
        .error
        .as_ref()
        .or(document.durability_warning.as_ref())
    {
        let mut actions: Vec<Element<'a, RepositoryAction>> =
            vec![crate::design::body(error).width(Length::Fill).into()];
        if document.conflict || document.durability_warning.is_some() {
            actions.push(
                icon_button(Icon::Refresh, "Reload file from disk")
                    .on_press_maybe(
                        (!runtime.is_saving()).then(|| RepositoryAction::Reload { card }),
                    )
                    .into(),
            );
        }
        if document.conflict {
            actions.push(
                icon_button(Icon::Save, "Overwrite conflicting file on disk")
                    .on_press_maybe(runtime.can_save().then(|| RepositoryAction::Save {
                        card,
                        overwrite: true,
                    }))
                    .into(),
            );
        } else if document.durability_warning.is_some() {
            actions.push(
                icon_button(Icon::Save, "Retry save")
                    .on_press_maybe(runtime.can_save().then(|| RepositoryAction::Save {
                        card,
                        overwrite: false,
                    }))
                    .into(),
            );
        }
        error_rows.push(
            widget::container(widget::row::with_children(actions).spacing(space::S))
                .class(cosmic::theme::Container::Secondary)
                .padding(space::M)
                .into(),
        );
    }
    let content: Element<'a, RepositoryAction> = if runtime.source_surface == SourceSurface::Editor
        && runtime.markdown_mode == MarkdownMode::Preview
    {
        // The preview is the same document the editor shows, so it reads at
        // the editor's size: switching modes must not shrink the text, and a
        // reader who enlarged their editor gets the same from the rendering.
        let settings =
            crate::design::markdown_settings(&cosmic::theme::active(), runtime.editor_font_size);
        widget::scrollable(
            widget::container(if preview::standalone(&document.path) {
                preview::diagram_view(
                    document
                        .diagrams
                        .get(&blake3::hash(document.preview_text.as_bytes())),
                    &document.preview_text,
                    document.diagram_pending.is_some(),
                    settings.code_size,
                )
            } else if !document.markdown {
                widget::selectable_text(document.preview_text.as_str())
                    .font(Font::MONOSPACE)
                    .size(settings.code_size)
                    .into()
            } else {
                widget::column::with_children(
                    document
                        .markdown_items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let content = markdown::item(
                                &preview::Viewer {
                                    diagrams: &document.diagrams,
                                    loading: document.diagram_pending.is_some(),
                                },
                                settings,
                                item,
                                index,
                            )
                            .map(move |uri| RepositoryAction::MarkdownLink { card, uri });
                            widget::container(content)
                                .id(crate::document_link::heading_id(card, index))
                                .into()
                        })
                        .collect::<Vec<_>>(),
                )
                .spacing(settings.spacing.0)
                .into()
            })
            .padding(space::XL)
            .max_width(900),
        )
        .id(runtime.preview_scroll_id.clone())
        .on_scroll(move |viewport| RepositoryAction::PreviewViewport {
            card,
            offset: viewport.absolute_offset().y,
        })
        .direction(iced::widget::scrollable::Direction::Vertical(
            crate::design::scrollbar(),
        ))
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    } else {
        let popup: Option<Element<'a, RepositoryAction>> = if !runtime
            .language
            .completions
            .is_empty()
        {
            let selected = runtime.language.selected;
            let first = selected.saturating_sub(4);
            let choices = runtime
                .language
                .completions
                .iter()
                .enumerate()
                .skip(first)
                .take(10)
                .map(|(index, item)| {
                    let label = crate::design::body(&item.label).width(Length::Fill);
                    let detail = crate::design::caption(item.detail.as_deref().unwrap_or_default())
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ));
                    widget::button::custom(widget::column::with_children(vec![
                        label.into(),
                        detail.into(),
                    ]))
                    .class(crate::ui_controls::control_style(index == selected, None))
                    .width(Length::Fill)
                    .on_press(RepositoryAction::Language {
                        card,
                        action: EditorLspAction::Choose {
                            index,
                            label: item.label.clone(),
                        },
                    })
                    .into()
                })
                .collect::<Vec<_>>();
            Some(
                widget::container(
                    widget::column::with_children(choices)
                        .spacing(space::XS)
                        .width(360),
                )
                .class(crate::design::surface(
                    Some(crate::design::raised),
                    [crate::design::RADIUS_S; 4],
                    true,
                ))
                .padding(space::M)
                .into(),
            )
        } else if !runtime.language.hover.is_empty() {
            let settings = crate::design::markdown_settings(
                &cosmic::theme::active(),
                crate::design::TEXT_BODY,
            );
            Some(
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(
                            runtime
                                .language
                                .hover
                                .iter()
                                .enumerate()
                                .map(|(index, item)| {
                                    markdown::item(
                                        &crate::document_link::PreviewViewer,
                                        settings,
                                        item,
                                        index,
                                    )
                                    .map(move |uri| RepositoryAction::MarkdownLink { card, uri })
                                })
                                .collect::<Vec<_>>(),
                        )
                        .spacing(space::M),
                    )
                    .height(Length::Shrink),
                )
                .max_height(240)
                .max_width(420)
                .class(crate::design::surface(
                    Some(crate::design::raised),
                    [crate::design::RADIUS_S; 4],
                    true,
                ))
                .padding(space::M)
                .into(),
            )
        } else {
            None
        };
        let editor = text_editor::TextEditor::new(&document.content)
            // Before `highlight_with`: `id` is only offered by the
            // not-yet-highlighted builder.
            .id(editor_focus_id(card))
            .highlight_with::<highlighter::Highlighter>(
                highlighter_settings(
                    &document.source,
                    runtime.syntax_theme,
                    cosmic::theme::is_dark(),
                ),
                |highlight, _theme: &cosmic::Theme| highlight.to_format(),
            )
            .font(runtime.editor_font)
            .find_matches(
                if runtime.find_open {
                    &runtime.find_matches
                } else {
                    &[]
                },
                runtime.find_index,
            )
            .size(runtime.editor_font_size)
            .line_height(iced::widget::text::LineHeight::Relative(
                runtime.line_height,
            ))
            .added_lines(if runtime.working_annotations_current() {
                &runtime.working_diff.additions
            } else {
                &[]
            })
            .diagnostics(&runtime.language.marks)
            .popup(
                popup,
                if runtime.language.completions.is_empty() {
                    runtime.language.hover_at
                } else {
                    None
                },
            )
            .on_hover(move |at| RepositoryAction::Language {
                card,
                action: EditorLspAction::Hover(
                    at.and_then(|at| caret_position(&document.content, at)),
                ),
            })
            .on_definition(move |at| RepositoryAction::Language {
                card,
                action: EditorLspAction::Definition(caret_position(&document.content, at)),
            })
            .wrapping(if runtime.wrap_lines {
                iced::advanced::text::Wrapping::WordOrGlyph
            } else {
                iced::advanced::text::Wrapping::None
            })
            .padding(space::M)
            .height(Length::Fill)
            .style(|theme, status| {
                use iced::widget::text_editor::Catalog;
                let mut style = theme.style(&<cosmic::Theme as Catalog>::default(), status);
                style.border = iced::Border::default();
                style
            })
            .key_binding(move |press| {
                use iced::keyboard::{Key, key::Named};
                if let Some(delta) = crate::interaction::font_zoom_chord(
                    &press.key,
                    press.physical_key,
                    press.modifiers,
                ) {
                    return Some(text_editor::Binding::Custom(RepositoryAction::ZoomFont {
                        card,
                        delta,
                    }));
                }
                if let Some(binding) = registered_editor_binding(card, runtime, &press) { return binding; }
                if !matches!(
                    press.status,
                    iced::widget::text_editor::Status::Focused { .. }
                ) {
                    return text_editor::default_bindings(press);
                }
                let action = match &press.key {
                    Key::Character(character)
                        if character.as_str() == " " && press.modifiers.command() =>
                    {
                        Some(EditorLspAction::Complete)
                    }
                    Key::Named(Named::F12) => Some(if press.modifiers.shift() {
                        EditorLspAction::References
                    } else {
                        EditorLspAction::Definition(None)
                    }),
                    Key::Named(Named::Escape)
                        if !runtime.language.completions.is_empty()
                            || !runtime.language.hover.is_empty() =>
                    {
                        Some(EditorLspAction::Dismiss)
                    }
                    Key::Named(Named::ArrowDown) if !runtime.language.completions.is_empty() => {
                        Some(EditorLspAction::Next(true))
                    }
                    Key::Named(Named::ArrowUp) if !runtime.language.completions.is_empty() => {
                        Some(EditorLspAction::Next(false))
                    }
                    Key::Named(Named::Tab | Named::Enter)
                        if !runtime.language.completions.is_empty() =>
                    {
                        let index = runtime.language.selected;
                        Some(EditorLspAction::Choose {
                            index,
                            label: runtime.language.completions[index].label.clone(),
                        })
                    }
                    _ => None,
                };
                action
                    .map(|action| {
                        text_editor::Binding::Custom(RepositoryAction::Language { card, action })
                    })
                    .or_else(|| text_editor::default_bindings(press))
            });
        let editor =
            editor.on_action(move |action| RepositoryAction::EditorAction { card, action });
        crate::interaction::context_menu(editor, editor_context_menu(card, runtime))
    };
    // Keep the named editor in the same slot when save warnings appear.
    let error_slot = widget::column::with_children(error_rows).width(Length::Fill);
    widget::column::with_children([error_slot.into(), content])
        .height(Length::Fill)
        .into()
}

fn labelled_diff_control<'a>(
    label: &'static str,
    tooltip: &'static str,
    selected: bool,
    action: Option<RepositoryAction>,
) -> Element<'a, RepositoryAction> {
    crate::interaction::tooltip(
        widget::button::custom(crate::design::caption(label))
            .name(label)
            .class(crate::ui_controls::control_style(selected, None))
            .padding([space::XXS, space::S])
            .on_press_maybe(action),
        tooltip,
    )
}

fn diff_basis_controls(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let selected = match &runtime.basis_resolution {
        Some(BasisResolution::Resolving(choice)) => Some(BasisChoice::Committed(*choice)),
        Some(BasisResolution::NeedsBase(_)) => {
            Some(BasisChoice::Committed(CommittedChoice::Branch))
        }
        Some(BasisResolution::NoCommits | BasisResolution::Unavailable(_)) => None,
        None => basis_choice(&runtime.diff_basis),
    };
    let mut controls = Vec::new();
    for (label, tip, choice, enabled, action) in [
        (
            "Working",
            "Compare index with working tree",
            BasisChoice::Unstaged,
            runtime.can_select_diff_basis(&DiffBasis::IndexToWorktree),
            RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::IndexToWorktree,
            },
        ),
        (
            "Staged",
            "Compare HEAD with index",
            BasisChoice::Staged,
            runtime.can_select_diff_basis(&DiffBasis::HeadToIndex),
            RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::HeadToIndex,
            },
        ),
        (
            "Branch",
            "Compare branch with base",
            BasisChoice::Committed(CommittedChoice::Branch),
            runtime.can_select_committed_basis(CommittedChoice::Branch),
            RepositoryAction::SelectCommittedBasis {
                card,
                choice: CommittedChoice::Branch,
            },
        ),
        (
            "History",
            "Compare last commit with its parent",
            BasisChoice::Committed(CommittedChoice::LastCommit),
            runtime.can_select_committed_basis(CommittedChoice::LastCommit),
            RepositoryAction::SelectCommittedBasis {
                card,
                choice: CommittedChoice::LastCommit,
            },
        ),
    ] {
        controls.push(labelled_diff_control(
            label,
            tip,
            enabled && selected == Some(choice),
            enabled.then_some(action),
        ));
    }
    crate::ui_controls::segmented_controls(controls)
}

fn diff_action_toolbar(
    card: CardId,
    runtime: &RepositoryRuntime,
    width: f32,
) -> Element<'_, RepositoryAction> {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if !runtime.reviews_pull_request() {
        let subject = runtime.subject_file();
        for (stage, label, enabled) in [
            (
                true,
                "Stage",
                subject.as_ref().is_some_and(|file| file.unstaged),
            ),
            (
                false,
                "Unstage",
                subject.as_ref().is_some_and(|file| file.staged),
            ),
        ] {
            actions.push(labelled_diff_control(
                label,
                if stage {
                    "Stage file changes · Ctrl+Shift+S"
                } else {
                    "Unstage file changes · Ctrl+Shift+S when no unstaged changes"
                },
                false,
                (enabled && !runtime.mutation_refresh_pending && !runtime.blocks_navigation())
                    .then_some(RepositoryAction::StageSelected { card, stage }),
            ));
        }
        let revert = if runtime.can_discard_edits() {
            Some(RepositoryAction::DiscardEdits { card })
        } else {
            runtime
                .selected_discard_request()
                .map(|_| RepositoryAction::RevertFile { card })
        };
        actions.push(labelled_diff_control(
            "Revert",
            if runtime.can_discard_edits() {
                "Discard unsaved edits · Ctrl+Z restores them"
            } else {
                "Revert working tree file changes…"
            },
            false,
            revert,
        ));
    }
    actions.push(labelled_diff_control(
        "Ignore whitespace",
        "Ignore horizontal whitespace in every diff",
        runtime.diff_ignore_whitespace,
        Some(RepositoryAction::ToggleDiffWhitespace { card }),
    ));
    let mut basis = Vec::new();
    if !runtime.directory_only && !runtime.reviews_pull_request() {
        basis.push(diff_basis_controls(card, runtime));
    }
    if let Some(model) = runtime.diff_model() {
        let counts = crate::repository::line_change_counts(
            Some(u64::from(model.additions)),
            Some(u64::from(model.deletions)),
        );
        if !counts.is_empty() {
            if runtime.reviews_pull_request() {
                actions.push(crate::ui_controls::change_counts(&counts));
            } else {
                basis.push(crate::ui_controls::change_counts(&counts));
            }
        }
    }
    rows.push(
        widget::row::with_children(actions)
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into(),
    );
    if !basis.is_empty() {
        rows.push(
            widget::row::with_children(basis)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        );
    }
    rows.push(diff_review_controls(card, runtime));
    let contents = widget::column::with_children(rows).spacing(space::XS);
    if width >= 340.0 {
        widget::container(contents)
            .padding([space::XS, crate::design::TOOLBAR_INSET])
            .width(Length::Fill)
            .into()
    } else {
        crate::ui_controls::popover(
            labelled_diff_control(
                "Diff actions ⋮",
                "Diff actions",
                runtime.toolbar_overflow_open,
                Some(RepositoryAction::ToggleToolbarOverflow { card }),
            ),
            runtime
                .toolbar_overflow_open
                .then(|| widget::container(contents).padding(space::S).into()),
            widget::popover::Position::Bottom,
            RepositoryAction::CloseToolbarOverflow { card },
        )
    }
}

fn diff_review_controls(
    card: CardId,
    runtime: &RepositoryRuntime,
) -> Element<'_, RepositoryAction> {
    let model = match runtime.displayed_diff_state() {
        Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) => Some(model),
        _ => None,
    };
    let change_count = model.map_or(0, |model| model.change_starts(runtime.diff_mode).len());
    let mut controls = vec![
        crate::interaction::tooltip(
            widget::container(
                if runtime.mutation_refresh_pending
                    || (runtime.stale_diff.is_some()
                        && matches!(
                            runtime.diff_state(&runtime.diff_basis),
                            Some(DiffState::Loading { .. })
                        ))
                {
                    crate::ui_controls::activity_spinner(
                        f32::from(crate::design::ROW_ICON_SIZE),
                        crate::design::warning_color,
                    )
                } else {
                    iced::widget::Space::new().into()
                },
            )
            .width(crate::design::ROW_ICON_SIZE)
            .height(crate::design::ROW_ICON_SIZE),
            "Refreshing diff",
        ),
        crate::ui_controls::segmented_controls(vec![
            labelled_diff_control(
                "Unified",
                "Unified diff",
                runtime.diff_mode == DiffMode::Inline,
                runtime
                    .can_change_diff_layout()
                    .then_some(RepositoryAction::ApplyDiffMode { card, split: false }),
            ),
            labelled_diff_control(
                "Split",
                "Side-by-side diff",
                runtime.diff_mode == DiffMode::Split,
                runtime
                    .can_change_diff_layout()
                    .then_some(RepositoryAction::ApplyDiffMode { card, split: true }),
            ),
        ]),
        // Navigation commands are a plain cluster, distinct from mode choices.
        toolbar_group(vec![
            labelled_diff_control(
                "Previous",
                "Previous hunk · Alt+K / Shift+F7",
                false,
                (runtime.can_navigate_diff() && runtime.change_target(false).is_some())
                    .then_some(RepositoryAction::PreviousChange { card }),
            ),
            crate::interaction::tooltip(
                // A fixed width, because this label sits between the two
                // controls a reader presses repeatedly and "1/4" and "12/128"
                // are different widths. The text fills that width so the
                // label, not its box, is what is centred.
                widget::container(
                    crate::design::caption(if change_count == 0 {
                        "0/0".into()
                    } else {
                        format!("{}/{change_count}", runtime.diff_change + 1)
                    })
                    .width(Length::Fill)
                    .center(),
                )
                .width(Length::Fixed(crate::design::STEPPER_LABEL_WIDTH)),
                basis_label(&runtime.diff_basis),
            ),
            labelled_diff_control(
                "Next",
                "Next hunk · Alt+J / F7",
                false,
                (runtime.can_navigate_diff() && runtime.change_target(true).is_some())
                    .then_some(RepositoryAction::NextChange { card }),
            ),
        ]),
    ];
    if runtime.can_retry_diff() {
        controls.push(toolbar_group(vec![
            icon_button(Icon::Refresh, "Retry diff")
                .on_press(RepositoryAction::RetryDiff { card })
                .into(),
        ]));
    }
    widget::row::with_children(controls)
        .spacing(crate::design::TOOLBAR_GROUP_SPACING)
        .align_y(Alignment::Center)
        .into()
}

fn diff_surface<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    review: Option<&'a crate::pull_request_review::State>,
) -> Element<'a, RepositoryAction> {
    if runtime.mutation_refresh_pending
        && runtime.displayed_diff_state().is_none()
        && !runtime.diff_basis.is_immutable()
    {
        return match &runtime.files {
            FileListState::Failed(error) => {
                information_state("Could not refresh file status", error.clone())
            }
            _ => loading(),
        };
    }
    // A failed live-buffer comparison has to be said rather than papered over
    // with the model it superseded, which describes text the buffer no longer
    // holds.
    if let Some(error) = runtime.working_diff_failure() {
        return information_state("Could not compare the working buffer", error.to_owned());
    }
    match &runtime.basis_resolution {
        Some(BasisResolution::Resolving(_)) => loading(),
        Some(BasisResolution::NeedsBase(candidates)) => base_choice_view(card, candidates),
        Some(BasisResolution::NoCommits) => information_state(
            "Comparison unavailable",
            "This branch has no commits yet, so there is nothing to compare.",
        ),
        Some(BasisResolution::Unavailable(message)) => {
            information_state("Comparison unavailable", message.clone())
        }
        None => match runtime.displayed_diff_state() {
            None => information_state(
                "Nothing to compare",
                "This file has nothing to show for the selected comparison.",
            ),
            Some(DiffState::Loading { .. }) => loading(),
            Some(DiffState::Ready(
                RenderedDiff::Text {
                    model,
                    old_label,
                    new_label,
                },
                _,
            )) => diff_model_view(card, runtime, model, old_label, new_label, review),
            Some(DiffState::Ready(RenderedDiff::NoText(content), _)) => {
                let leading = review.and_then(|review| {
                    let path = runtime.selected.as_ref()?.path.to_str()?;
                    review
                        .has_leading_annotation(path, [0; 2])
                        .then_some((review, path))
                });
                if let Some((review, path)) = leading {
                    widget::scrollable(
                        widget::column::with_children([
                            review
                                .leading_annotation(path, [0; 2])
                                .map(move |action| RepositoryAction::Review { card, action }),
                            no_text_view_height(content, Length::Shrink),
                        ])
                        .width(Length::Fill),
                    )
                    .height(Length::Fill)
                    .width(Length::Fill)
                    .into()
                } else {
                    no_text_view(content)
                }
            }
            Some(DiffState::Ready(RenderedDiff::Error(error), _)) => {
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
    no_text_view_height(content, Length::Fill)
}

fn no_text_view_height(
    content: &DiffContent,
    height: Length,
) -> Element<'static, RepositoryAction> {
    match content {
        DiffContent::Comparison {
            old,
            new,
            old_label,
            new_label,
        } => information_state_height(
            format!("No text diff between {old_label} and {new_label}"),
            [(old_label, old), (new_label, new)]
                .into_iter()
                .filter(|(_, side)| !side.is_diffable())
                .map(|(label, side)| format!("{label}: {}", side_detail(side)))
                .collect::<Vec<_>>()
                .join("\n"),
            height,
        ),
        DiffContent::Unmerged {
            base,
            ours,
            theirs,
            worktree,
        } => information_state_height(
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
            height,
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
    let mut rows: Vec<Element<'static, RepositoryAction>> = vec![
        crate::design::title("Choose a branch comparison base").into(),
        crate::design::body(
            "This repository has no remote HEAD to take a default base from, so there is no branch to compare against yet.",
        )
        .wrapping(iced::widget::text::Wrapping::Word)
        .into(),
    ];
    if candidates.is_empty() {
        rows.push(
            crate::design::body(
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
            crate::ui_controls::field::select(labels, None, move |index| {
                // The dropdown reports an index into the very list it was given.
                RepositoryAction::SelectBranchBase {
                    card,
                    base_ref: refs[index].clone(),
                }
            })
            .into(),
        );
    }
    widget::container(
        widget::container(
            widget::column::with_children(rows)
                .align_x(Alignment::Center)
                .spacing(space::M),
        )
        .class(cosmic::theme::Container::Card)
        .padding(space::XL)
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
    review: Option<&'a crate::pull_request_review::State>,
) -> Element<'a, RepositoryAction> {
    let rows: Element<'a, RepositoryAction> = if model.is_empty()
        && review.is_none()
        && !runtime.find_open
    {
        information_state(
            "No content changes",
            format!("{old_label} and {new_label} hold identical text."),
        )
    } else {
        crate::ui_controls::responsive(move |size| {
            // One native two-axis scrollable owns both coordinates. The canvas
            // counter-translates the fixed gutters/center; only code moves in X.
            let gutter =
                (model.row_count(DiffMode::Inline).max(1).ilog10() + 1).max(4) as f32 * 8.0 + 12.0;
            let code_width = (model.max_line_width[usize::from(cosmic::theme::is_dark())] as f32
                * runtime.editor_font_size
                / DIFF_FONT_SIZE)
                .max(1.0);
            let geometry = DiffScrollGeometry::new(size.width, gutter, runtime.diff_mode);
            let viewport_width = geometry.viewport_width;
            let overflow = geometry.overflow(code_width);
            let horizontal = runtime.diff_horizontal.clamp(0.0, overflow);
            let layout = runtime
                .diff_layout()
                .expect("rendered comparison owns its row layout");
            let rows = layout.rows(runtime.diff_mode);
            let folded = layout.folded(runtime.diff_mode);
            let visible = visible_diff_rows(rows, runtime.diff_viewport);
            let context = review.and_then(|review| {
                let path = runtime.selected.as_ref()?.path.to_str()?;
                Some(DiffReviewContext {
                    card,
                    runtime,
                    review,
                    path,
                    line_counts: model.line_counts,
                })
            });
            // Only mounted rows and the sparse set of previously measured
            // annotations are inspected; large unchanged files stay virtual.
            let retained: BTreeMap<usize, f32> = rows
                .annotations()
                .iter()
                .filter(|(index, _)| {
                    let DisplayRow::Code(row) = folded.row(**index) else {
                        return false;
                    };
                    context.is_some_and(|ctx| {
                        let (old, new) = diff_row_lines(model, runtime.diff_mode, row);
                        ctx.review.has_annotation(ctx.path, old, new, false)
                    })
                })
                .map(|(row, height)| (*row, *height))
                .collect();
            let mut children = Vec::with_capacity(visible.end - visible.start + 3);
            let mut slots = Vec::new();
            if let Some(ctx) =
                context.filter(|ctx| ctx.review.has_leading_annotation(ctx.path, ctx.line_counts))
            {
                slots.push((children.len(), None));
                children.push(ctx.annotation(None, None, true));
            }
            if visible.top_space > 0.0 {
                children.push(iced::widget::Space::new().height(visible.top_space).into());
            }
            for index in visible.start..visible.end {
                let row = match folded.row(index) {
                    DisplayRow::Code(row) => row,
                    DisplayRow::Fold { fold, rows: hidden } => {
                        let discussions = context.map_or(0, |ctx| {
                            let (first, last) = (
                                diff_row_lines(model, runtime.diff_mode, hidden.start),
                                diff_row_lines(model, runtime.diff_mode, hidden.end - 1),
                            );
                            ctx.review.discussions_within(
                                ctx.path,
                                first.0.unwrap_or(0)..=last.0.unwrap_or(0),
                                first.1.unwrap_or(0)..=last.1.unwrap_or(0),
                            )
                        });
                        children.push(diff_fold_row(
                            card,
                            runtime,
                            model,
                            DiffFold {
                                fold,
                                hidden,
                                discussions,
                            },
                            gutter,
                            viewport_width,
                        ));
                        continue;
                    }
                };
                let (old, new) = diff_row_lines(model, runtime.diff_mode, row);
                let code = match runtime.diff_mode {
                    DiffMode::Inline => inline_diff_row_with_review(
                        &model.inline_rows[row],
                        &model.syntax,
                        gutter,
                        code_width,
                        horizontal,
                        context.and_then(|ctx| ctx.line_action(old, new)),
                        runtime.diff_typography(),
                        Some(diff_actions::RowContext { card, runtime, row }),
                    ),
                    DiffMode::Split => split_diff_row_with_review(
                        &model.split_rows[row],
                        &model.syntax,
                        gutter,
                        code_width,
                        horizontal,
                        context.map(|ctx| (ctx.line_action(old, None), ctx.line_action(None, new))),
                        runtime.diff_typography(),
                        Some(diff_actions::RowContext { card, runtime, row }),
                    ),
                };
                if let Some(ctx) =
                    context.filter(|ctx| ctx.review.has_annotation(ctx.path, old, new, false))
                {
                    slots.push((children.len(), Some(index)));
                    children.push(
                        widget::column::with_children([code, ctx.annotation(old, new, false)])
                            .width(Length::Fill)
                            .into(),
                    );
                } else {
                    children.push(code);
                }
            }
            if visible.bottom_space > 0.0 {
                children.push(
                    iced::widget::Space::new()
                        .height(visible.bottom_space)
                        .into(),
                );
            }
            // Both thumbs float over this content, so each axis reserves its
            // own rail: the bottom one here, the right one in
            // `DiffScrollGeometry`. The padding helpers in `design` would
            // reserve the same space, but the canvas's own coordinates are
            // derived from `viewport_width`, so the reservation has to be
            // visible to that arithmetic rather than applied above it.
            children.push(
                iced::widget::Space::new()
                    .height(crate::design::SCROLLBAR_GUTTER)
                    .into(),
            );
            let column = widget::column::with_children(children)
                .width(viewport_width)
                .into();
            let content = if let Some(ctx) = context {
                Element::new(DiffAnnotationMeasure {
                    content: column,
                    identity: runtime.current_identity(card, PathBuf::from(ctx.path)),
                    basis: runtime.diff_basis.clone(),
                    mode: runtime.diff_mode,
                    layout_revision: runtime.diff_layout_revision,
                    review_revision: ctx.review.annotation_revision(),
                    row_height: runtime.diff_typography().line_height,
                    expected_leading: rows.leading_height(),
                    expected: rows.annotations().clone(),
                    retained,
                    slots,
                    pending_anchor: runtime.pending_diff_anchor.is_some(),
                })
            } else {
                column
            };
            let canvas = Element::new(DiffHorizontalContent {
                content,
                width: Length::Fixed(size.width + overflow),
                content_width: viewport_width,
                offset: -horizontal,
                accessible_text: None,
                viewport: Some(iced::Size::new(viewport_width, size.height)),
            });
            let scrollable = widget::scrollable(canvas)
                .direction(iced::widget::scrollable::Direction::Both {
                    vertical: crate::design::scrollbar(),
                    horizontal: crate::design::scrollbar(),
                })
                .id(runtime.diff_scroll_id.clone())
                .on_scroll(move |viewport| RepositoryAction::DiffViewport {
                    card,
                    offset: viewport.absolute_offset().y,
                    height: viewport.bounds().height,
                    horizontal: viewport.absolute_offset().x,
                    user_initiated: false,
                })
                .height(Length::Fill)
                .width(Length::Fill)
                .into();
            Element::new(DiffScrollObserver {
                content: scrollable,
                card: Some(card),
            })
        })
        .into()
    };
    rows
}

#[derive(Clone, Copy)]
struct DiffReviewContext<'a> {
    card: CardId,
    runtime: &'a RepositoryRuntime,
    review: &'a crate::pull_request_review::State,
    path: &'a str,
    line_counts: [usize; 2],
}

impl<'a> DiffReviewContext<'a> {
    fn annotation(
        self,
        old: Option<u32>,
        new: Option<u32>,
        leading: bool,
    ) -> Element<'a, RepositoryAction> {
        let annotation = if leading {
            self.review.leading_annotation(self.path, self.line_counts)
        } else {
            self.review.annotation(self.path, old, new, false)
        };
        annotation.map(move |action| RepositoryAction::Review {
            card: self.card,
            action,
        })
    }

    fn line_action(self, old: Option<u32>, new: Option<u32>) -> Option<RepositoryAction> {
        use crate::github::ReviewDiffSide;
        let (side, line) = if let Some(line) = new {
            (ReviewDiffSide::Right, line)
        } else {
            (ReviewDiffSide::Left, old?)
        };
        let anchor = crate::pull_request_review::discussion::Anchor {
            path: self.path.to_owned(),
            side,
            start_line: u64::from(line),
            line: u64::from(line),
        };
        Some(RepositoryAction::ReviewLine {
            identity: self
                .runtime
                .current_identity(self.card, PathBuf::from(self.path)),
            basis: self.runtime.diff_basis.clone(),
            action: self.review.review_action(
                crate::pull_request_review::discussion_ui::Action::Begin(anchor),
            ),
            extend: false,
        })
    }
}

/// The row of `to` showing the line `row` of `from` shows, or the first row
/// after it when that line is gone. Lines, unlike rows, mean the same thing in
/// both layouts and across a rebuild. A linear scan, run once per rebuild or
/// layout switch, never per frame.
fn corresponding_row(
    (from, from_mode): (&DiffModel, DiffMode),
    row: usize,
    (to, to_mode): (&DiffModel, DiffMode),
) -> usize {
    let (from_rows, to_rows) = (from.row_count(from_mode), to.row_count(to_mode));
    if row >= from_rows {
        return to_rows + (row - from_rows);
    }
    let (old, new) = diff_row_lines(from, from_mode, row);
    (0..to_rows)
        .position(|candidate| {
            let (candidate_old, candidate_new) = diff_row_lines(to, to_mode, candidate);
            // The old side is the comparison's baseline, which an edit to
            // the working file does not renumber; an added line has only
            // its new number.
            match (old, new) {
                (Some(line), _) => candidate_old.is_some_and(|candidate| candidate >= line),
                (None, Some(line)) => candidate_new.is_some_and(|candidate| candidate >= line),
                (None, None) => false,
            }
        })
        .unwrap_or(to_rows)
}

fn diff_row_lines(model: &DiffModel, mode: DiffMode, index: usize) -> (Option<u32>, Option<u32>) {
    match mode {
        DiffMode::Inline => model
            .inline_rows
            .get(index)
            .map(|row| (row.old_line, row.new_line)),
        DiffMode::Split => model
            .split_rows
            .get(index)
            .map(|row| (row.old_line, row.new_line)),
    }
    .unwrap_or_default()
}

/// One folded run of unchanged rows, as drawn.
struct DiffFold {
    /// Which unchanged run of the model this fold belongs to.
    fold: usize,
    /// The model rows the fold stands in for.
    hidden: Range<usize>,
    /// Review discussions anchored to hidden lines.
    discussions: usize,
}

/// The row that stands in for folded unchanged lines.
///
/// Its gutter holds the directional reveals, beside the line numbers they
/// continue; the label reveals the whole run. A run that opens the file has
/// nothing above it to continue, and one that ends the file nothing below.
fn diff_fold_row<'a>(
    card: CardId,
    runtime: &RepositoryRuntime,
    model: &DiffModel,
    fold: DiffFold,
    gutter: f32,
    width: f32,
) -> Element<'a, RepositoryAction> {
    let mode = runtime.diff_mode;
    let run = &model.unchanged_runs(mode)[fold.fold];
    let count = fold.hidden.len();
    let identity = runtime
        .selected
        .as_ref()
        .map(|file| runtime.current_identity(card, file.path.clone()));
    let action = |expansion| {
        identity
            .clone()
            .map(|identity| RepositoryAction::ExpandDiffFold {
                identity,
                basis: runtime.diff_basis.clone(),
                mode,
                fold: fold.fold,
                run: run.clone(),
                expansion,
            })
    };
    let step = diff_layout::FOLD_STEP;
    let reveal = |icon: Icon, label: String, expansion| -> Element<'a, RepositoryAction> {
        crate::ui_controls::icon_button(icon, label)
            .compact()
            .on_press_maybe(action(expansion))
            .into()
    };
    let mut controls = Vec::with_capacity(2);
    // A step that would leave too few rows to fold opens the whole run, so
    // offer that honestly instead of a step that reveals more than it says.
    if count >= step + diff_layout::FOLD_MIN_HIDDEN {
        if run.start > 0 {
            controls.push(reveal(
                Icon::ExpandDown,
                format!("Expand down {step} lines"),
                FoldExpansion::Down,
            ));
        }
        if run.end < model.row_count(mode) {
            controls.push(reveal(
                Icon::ExpandUp,
                format!("Expand up {step} lines"),
                FoldExpansion::Up,
            ));
        }
    } else {
        controls.push(reveal(
            Icon::ExpandAll,
            format!("Show {count} unchanged lines"),
            FoldExpansion::All,
        ));
    }
    // Room for both reveals in either layout; a split half has no marker
    // column, but the fold row spans both halves and needs no alignment.
    let gutter_width = gutter + DIFF_MARKER_WIDTH;
    let mut label = vec![
        widget::text::body(format!("{count} unchanged lines"))
            .size(12)
            .class(cosmic::theme::Text::Custom(crate::design::muted_text))
            .into(),
    ];
    if fold.discussions > 0 {
        label.push(
            widget::text::body(if fold.discussions == 1 {
                "· 1 comment".to_owned()
            } else {
                format!("· {} comments", fold.discussions)
            })
            .size(12)
            .class(cosmic::theme::Text::Custom(|theme| {
                iced::widget::text::Style {
                    color: Some(crate::design::accent_color(theme)),
                    ..Default::default()
                }
            }))
            .into(),
        );
    }
    let expand_all = crate::interaction::tooltip(
        crate::interaction::hover(
            widget::button::custom(
                widget::row::with_children(label)
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .name(format!("Show all {count} unchanged lines"))
            .class(crate::ui_controls::control_style(false, None))
            .padding([space::XXS, space::S])
            .on_press_maybe(action(FoldExpansion::All)),
        ),
        format!("Show all {count} lines"),
    );
    widget::container(
        widget::row::with_children([
            widget::container(
                widget::row::with_children(controls)
                    .spacing(space::XXS)
                    .align_y(Alignment::Center),
            )
            .align_x(Alignment::End)
            .padding([0.0, space::XS])
            .width(gutter_width)
            .into(),
            expand_all,
        ])
        .spacing(DIFF_ROW_PADDING)
        .align_y(Alignment::Center),
    )
    .class(cosmic::theme::Container::custom(|theme| {
        let mut accent = crate::design::accent_color(theme);
        accent.a = if theme.theme_type.is_dark() {
            0.10
        } else {
            0.07
        };
        iced::widget::container::Style {
            background: Some(iced::Background::Color(diff_tint(
                accent,
                theme.cosmic().bg_color().into(),
            ))),
            ..Default::default()
        }
    }))
    .padding([0.0, DIFF_ROW_PADDING])
    .height(runtime.diff_typography().line_height)
    .width(width)
    .align_y(Alignment::Center)
    .clip(true)
    .into()
}

fn review_begin_anchor(
    action: &mut crate::pull_request_review::Action,
) -> Option<&mut crate::pull_request_review::discussion::Anchor> {
    use crate::pull_request_review::{Action, discussion_ui};
    let Action::Discussion(discussion_ui::Action::Fenced { action, .. }) = action else {
        return None;
    };
    if let discussion_ui::Action::Begin(anchor) = action.as_mut() {
        Some(anchor)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct DiffTypography {
    font: Font,
    size: f32,
    line_height: f32,
}
impl Default for DiffTypography {
    fn default() -> Self {
        Self {
            font: Font::MONOSPACE,
            size: DIFF_FONT_SIZE,
            line_height: DIFF_LINE_HEIGHT,
        }
    }
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
        // The vertical thumb's rail, taken off the width every coordinate
        // downstream is measured in, so a code column never ends underneath it.
        let viewport_width = (width - crate::design::SCROLLBAR_GUTTER).max(0.0);
        let code_viewport = match mode {
            DiffMode::Inline => viewport_width - 2.0 * gutter - DIFF_MARKER_WIDTH,
            DiffMode::Split => (viewport_width - DIFF_SPLIT_DIVIDER) / 2.0 - gutter,
        }
        .max(0.0);
        Self {
            viewport_width,
            code_viewport,
        }
    }

    fn overflow(self, code_width: f32) -> f32 {
        (code_width + 2.0 * DIFF_ROW_PADDING - self.code_viewport).max(0.0)
    }
}

#[cfg(test)]
fn inline_diff_row<'a>(
    row: &'a InlineDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
) -> Element<'a, RepositoryAction> {
    inline_diff_row_with_review(
        row,
        syntax,
        gutter,
        code_width,
        horizontal,
        None,
        DiffTypography::default(),
        None,
    )
}

fn inline_diff_row_with_review<'a>(
    row: &'a InlineDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
    action: Option<RepositoryAction>,
    typography: DiffTypography,
    interaction: Option<diff_actions::RowContext<'a>>,
) -> Element<'a, RepositoryAction> {
    let side = if row.new_line.is_none() {
        crate::diff_interaction::Side::Old
    } else {
        crate::diff_interaction::Side::New
    };
    let found =
        interaction.map_or_else(Vec::new, |ctx| ctx.runtime.diff_match_ranges(ctx.row, side));
    let active = interaction.and_then(|ctx| ctx.runtime.diff_active_match(ctx.row, side));
    let marker = match row.kind {
        DiffRowKind::Addition => "+",
        DiffRowKind::Deletion => "−",
        _ => " ",
    };
    let code = diff_container(
        widget::row::with_children(vec![
            diff_review_gutter(
                row.old_line,
                gutter,
                if row.new_line.is_none() {
                    action.clone()
                } else {
                    None
                },
                typography,
            ),
            diff_review_gutter(
                row.new_line,
                gutter,
                if row.new_line.is_some() { action } else { None },
                typography,
            ),
            widget::text::body(marker)
                .size(typography.size)
                .font(typography.font)
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
                typography,
                &found,
                active.as_ref(),
            ),
        ])
        .align_y(Alignment::Center),
        row.kind,
        Length::FillPortion(1),
        typography,
    );
    match interaction {
        Some(ctx) => ctx.wrap(side, code),
        None => code,
    }
}

#[cfg(test)]
fn split_diff_row<'a>(
    row: &'a SplitDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
) -> Element<'a, RepositoryAction> {
    split_diff_row_with_review(
        row,
        syntax,
        gutter,
        code_width,
        horizontal,
        None,
        DiffTypography::default(),
        None,
    )
}

fn split_diff_row_with_review<'a>(
    row: &'a SplitDiffRow,
    syntax: &DiffSyntax,
    gutter: f32,
    code_width: f32,
    horizontal: f32,
    actions: Option<(Option<RepositoryAction>, Option<RepositoryAction>)>,
    typography: DiffTypography,
    interaction: Option<diff_actions::RowContext<'a>>,
) -> Element<'a, RepositoryAction> {
    let (old_action, new_action) = actions.unwrap_or_default();
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
            old_action,
            typography,
            interaction,
            crate::diff_interaction::Side::Old,
        ),
        widget::container(iced::widget::Space::new())
            .width(DIFF_SPLIT_DIVIDER)
            .height(typography.line_height)
            .class(cosmic::theme::Container::custom(|theme| {
                let mut color: iced::Color = theme.current_container().on.into();
                color.a = crate::design::DIFF_DIVIDER_ALPHA;
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
            new_action,
            typography,
            interaction,
            crate::diff_interaction::Side::New,
        ),
    ])
    .height(typography.line_height)
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
    action: Option<RepositoryAction>,
    typography: DiffTypography,
    interaction: Option<diff_actions::RowContext<'a>>,
    side: crate::diff_interaction::Side,
) -> Element<'a, RepositoryAction> {
    let found =
        interaction.map_or_else(Vec::new, |ctx| ctx.runtime.diff_match_ranges(ctx.row, side));
    let active = interaction.and_then(|ctx| ctx.runtime.diff_active_match(ctx.row, side));
    let code = diff_container(
        widget::row::with_children(vec![
            diff_review_gutter(lines.0.or(lines.1), gutter, action, typography),
            diff_text(
                text,
                changed,
                syntax,
                kind,
                code_width,
                horizontal,
                lines,
                typography,
                &found,
                active.as_ref(),
            ),
        ])
        .align_y(Alignment::Center),
        kind,
        Length::FillPortion(1),
        typography,
    );
    match interaction.filter(|_| lines.0.or(lines.1).is_some()) {
        Some(ctx) => ctx.wrap(side, code),
        None => code,
    }
}

fn diff_review_gutter(
    line: Option<u32>,
    width: f32,
    action: Option<RepositoryAction>,
    typography: DiffTypography,
) -> Element<'static, RepositoryAction> {
    let label = diff_gutter(line, width, typography);
    match action {
        Some(action) => crate::interaction::tooltip(
            widget::button::custom(label)
                .on_press(action)
                .padding(0)
                .width(width)
                .height(typography.line_height)
                .class(cosmic::theme::Button::Text),
            "Comment on this line · Shift-click to select a range",
        )
        .into(),
        None => label,
    }
}

fn diff_gutter(
    line: Option<u32>,
    width: f32,
    typography: DiffTypography,
) -> Element<'static, RepositoryAction> {
    widget::container(
        crate::design::caption(line.map_or_else(String::new, |line| line.to_string()))
            .font(typography.font)
            .size(12)
            .align_x(Alignment::End)
            .width(Length::Fill)
            .class(cosmic::theme::Text::Custom(|theme| {
                let mut color: iced::Color = theme.current_container().on.into();
                color.a = crate::design::DIFF_GUTTER_ALPHA;
                iced::widget::text::Style {
                    color: Some(color),
                    ..Default::default()
                }
            })),
    )
    .padding([0.0, space::S])
    .width(width)
    .into()
}

fn diff_highlight_color(theme: &cosmic::Theme, kind: DiffRowKind, alpha: f32) -> iced::Color {
    // Keep additions and deletions distinct without obscuring their text.
    let mut color = match kind {
        DiffRowKind::Addition => crate::design::success_color(theme),
        DiffRowKind::Deletion => crate::design::destructive_color(theme),
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
        return foreground;
    }
    let target = if iced::Color::BLACK.relative_contrast(background)
        >= iced::Color::WHITE.relative_contrast(background)
    {
        iced::Color::BLACK
    } else {
        iced::Color::WHITE
    };
    // Correct only as much luminance as necessary. Replacing a syntax color
    // outright with black/white erased the distinction between token kinds.
    let mix = |amount: f32| {
        iced::Color::from_rgb(
            foreground.r + (target.r - foreground.r) * amount,
            foreground.g + (target.g - foreground.g) * amount,
            foreground.b + (target.b - foreground.b) * amount,
        )
    };
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..12 {
        let mid = (low + high) * 0.5;
        if mix(mid).relative_contrast(background) >= 4.5 {
            high = mid;
        } else {
            low = mid;
        }
    }
    mix(high)
}

/// Mix diff tints in display color space, then draw an opaque background.
/// Linear-light mixing made small alpha values much brighter than expected.
fn diff_tint(tint: iced::Color, background: iced::Color) -> iced::Color {
    iced::Color::from_rgb(
        background.r + (tint.r - background.r) * tint.a,
        background.g + (tint.g - background.g) * tint.a,
        background.b + (tint.b - background.b) * tint.a,
    )
}

fn diff_row_background(theme: &cosmic::Theme, kind: DiffRowKind) -> iced::Color {
    match kind {
        DiffRowKind::Addition => crate::design::diff_added_color(theme),
        DiffRowKind::Deletion => crate::design::diff_removed_color(theme),
        _ => theme.cosmic().bg_color().into(),
    }
}

fn diff_text<'a>(
    text: &'a str,
    changed: &[Range<usize>],
    syntax: &[SyntaxSpan],
    kind: DiffRowKind,
    code_width: f32,
    horizontal: f32,
    lines: (Option<u32>, Option<u32>),
    typography: DiffTypography,
    found: &[Range<usize>],
    active: Option<&Range<usize>>,
) -> Element<'a, RepositoryAction> {
    let theme = cosmic::theme::active();
    let row_background = diff_row_background(&theme, kind);
    // A wholly added/deleted line already has a line tint. Word emphasis is
    // useful only when it distinguishes an edit within otherwise shared text.
    let changed = if changed == [0..text.len()] {
        &[]
    } else {
        changed
    };
    let mut spans = crate::diff_view::line_spans(
        text,
        changed,
        syntax,
        diff_highlight_color(
            &theme,
            kind,
            if cosmic::theme::is_dark() { 0.24 } else { 0.18 },
        ),
    );
    spans = crate::diff_interaction::highlight_matches(text, spans, found, active, &theme);
    for span in &mut spans {
        if let Some(font) = &mut span.font {
            font.family = typography.font.family;
        }
        let background = match span
            .highlight
            .as_mut()
            .map(|highlight| &mut highlight.background)
        {
            Some(iced::Background::Color(color)) => {
                *color = diff_tint(*color, row_background);
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
        content: widget::container(
            iced::widget::rich_text(spans)
                .size(typography.size)
                .font(typography.font)
                .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                    typography.line_height,
                )))
                .wrapping(iced::widget::text::Wrapping::None)
                .width(code_width),
        )
        .padding([0.0, DIFF_ROW_PADDING])
        .into(),
        width: Length::Fill,
        content_width: code_width + 2.0 * DIFF_ROW_PADDING,
        offset: horizontal,
        accessible_text: Some(DiffAccessibleText {
            text,
            lines,
            id: iced::advanced::widget::Id::unique(),
        }),
        viewport: None,
    })
}

/// `width` is the row's cross-axis extent, which the caller must state because
/// a `Direction::Both` scrollable compresses its content's cross axis: a column
/// of width-fluid rows there takes its width from nothing and collapses.
fn diff_container<'a>(
    content: impl Into<Element<'a, RepositoryAction>>,
    kind: DiffRowKind,
    width: Length,
    typography: DiffTypography,
) -> Element<'a, RepositoryAction> {
    widget::container(content)
        .class(cosmic::theme::Container::custom(move |theme| {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(diff_row_background(theme, kind))),
                ..Default::default()
            }
        }))
        .padding([0.0, 0.0])
        .height(typography.line_height)
        .width(width)
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

/// The scrollable reports layout clamps as well as input. Tag its viewport
/// messages at the event boundary without capturing or dispatching extra input.
struct DiffScrollObserver<'a> {
    content: Element<'a, RepositoryAction>,
    card: Option<CardId>,
}

impl Widget<RepositoryAction, cosmic::Theme, cosmic::Renderer> for DiffScrollObserver<'_> {
    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<iced::keyboard::Modifiers>()
    }
    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(iced::keyboard::Modifiers::default())
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> iced::Size<Length> {
        self.content.as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        let modifiers = tree.state.downcast_mut::<iced::keyboard::Modifiers>();
        match event {
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(value)) => {
                *modifiers = *value
            }
            iced::Event::Window(iced::window::Event::Unfocused) => {
                *modifiers = iced::keyboard::Modifiers::default()
            }
            _ => {}
        }
        let extend = modifiers.shift();
        let user_initiated = matches!(
            event,
            iced::Event::Mouse(_) | iced::Event::Touch(_) | iced::Event::Keyboard(_)
        );
        let mut messages = Vec::new();
        let mut local = Shell::new(&mut messages);
        if shell.is_event_captured() {
            local.capture_event();
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            &mut local,
            viewport,
        );
        shell.merge(local, |mut message| {
            if let RepositoryAction::DiffViewport {
                user_initiated: tagged,
                ..
            } = &mut message
            {
                *tagged = user_initiated;
            }
            if let RepositoryAction::ReviewLine { extend: tagged, .. } = &mut message {
                *tagged = extend;
            }
            if let RepositoryAction::SelectDiffRow {
                extend: tagged,
                context: false,
                drag: false,
                ..
            } = &mut message
            {
                *tagged = extend;
            }
            message
        });
        if matches!(
            event,
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left
            )) | iced::Event::Window(iced::window::Event::Unfocused)
        ) {
            if let Some(card) = self.card {
                shell.publish(RepositoryAction::EndDiffSelection { card });
            }
        }
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
        renderer: &cosmic::Renderer,
    ) -> iced::mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &iced::Rectangle,
        translation: iced::Vector,
    ) -> Option<
        iced::advanced::overlay::Element<'b, RepositoryAction, cosmic::Theme, cosmic::Renderer>,
    > {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: iced::mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
}

/// Measures the actual natural-height column after layout. Every child remains
/// part of the ordinary widget tree, including focus, overlays and accessibility.
struct DiffAnnotationMeasure<'a> {
    content: Element<'a, RepositoryAction>,
    identity: AsyncIdentity,
    basis: DiffBasis,
    mode: DiffMode,
    layout_revision: u64,
    review_revision: (u64, u64),
    row_height: f32,
    expected_leading: f32,
    expected: BTreeMap<usize, f32>,
    retained: BTreeMap<usize, f32>,
    slots: Vec<(usize, Option<usize>)>,
    pending_anchor: bool,
}

impl Widget<RepositoryAction, cosmic::Theme, cosmic::Renderer> for DiffAnnotationMeasure<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> iced::Size<Length> {
        self.content.as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if matches!(
            event,
            iced::Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            let mut rows = self.retained.clone();
            let mut leading = 0.0;
            for (child, (_, row)) in layout.children().enumerate().filter_map(|(index, child)| {
                self.slots
                    .iter()
                    .find(|(slot, _)| *slot == index)
                    .map(|slot| (child, slot))
            }) {
                let height = child.bounds().height;
                match row {
                    Some(row) => {
                        rows.insert(*row, (height - self.row_height).max(0.0));
                    }
                    None => leading = height,
                }
            }
            rows.retain(|_, height| height.is_finite() && *height > 0.0);
            if leading != self.expected_leading || rows != self.expected || self.pending_anchor {
                shell.publish(RepositoryAction::DiffAnnotations {
                    identity: self.identity.clone(),
                    basis: self.basis.clone(),
                    mode: self.mode,
                    layout_revision: self.layout_revision,
                    review_revision: self.review_revision,
                    leading,
                    rows: rows.into_iter().collect(),
                });
            }
        }
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
        renderer: &cosmic::Renderer,
    ) -> iced::mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &iced::Rectangle,
        translation: iced::Vector,
    ) -> Option<
        iced::advanced::overlay::Element<'b, RepositoryAction, cosmic::Theme, cosmic::Renderer>,
    > {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: iced::mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
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

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: iced::mouse::Cursor,
        viewport: &iced::Rectangle,
        renderer: &cosmic::Renderer,
    ) -> iced::mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.children().next().expect("diff content"),
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &iced::Rectangle,
        translation: iced::Vector,
    ) -> Option<
        iced::advanced::overlay::Element<'b, RepositoryAction, cosmic::Theme, cosmic::Renderer>,
    > {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next().expect("diff content"),
            renderer,
            viewport,
            translation,
        )
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

/// The sentence under the read-only title, about the file that was classified.
fn read_only_detail(path: &Path, reason: &ReadOnlyReason) -> String {
    match reason {
        // A link is followed when it can be, so the sentence has to say why
        // this one was not: the reader's next move differs for each.
        ReadOnlyReason::Symlink { target, refusal } => format!(
            "{} is a symbolic link to {}, {}",
            path.display(),
            target.display(),
            match refusal {
                LinkRefusal::LeavesWorktree =>
                    "which is outside the worktree or reached through another link.",
                LinkRefusal::Dangling => "which does not exist.",
                LinkRefusal::TooManyLinks => "which leads through too many links to follow.",
            }
        ),
        ReadOnlyReason::Binary => format!("{} contains binary data.", path.display()),
        ReadOnlyReason::InvalidUtf8 => format!("{} is not valid UTF-8.", path.display()),
        ReadOnlyReason::Image(message) | ReadOnlyReason::Document(message) => {
            format!("{}: {message}", path.display())
        }
        // The limit that refused the file is not the same limit for a
        // document as for a text buffer, so the sentence names what was
        // actually applied rather than claiming the editable one for both.
        ReadOnlyReason::Oversize { bytes, limit, kind } => format!(
            "{} is {bytes} bytes; {} are limited to {limit} bytes.",
            path.display(),
            match kind {
                OversizeKind::Editable => "editable files",
                OversizeKind::Document => "documents",
            }
        ),
        ReadOnlyReason::NotRegularFile => format!("{} is not a regular file.", path.display()),
    }
}

fn read_only_state(path: &Path, reason: &ReadOnlyReason) -> Element<'static, RepositoryAction> {
    let detail = read_only_detail(path, reason);
    information_state(
        match reason {
            ReadOnlyReason::Image(_) => "Image preview unavailable",
            ReadOnlyReason::Document(_)
            | ReadOnlyReason::Oversize {
                kind: OversizeKind::Document,
                ..
            } => "Document preview unavailable",
            _ => "Read-only file",
        },
        &detail,
    )
}

/// The card while its file list is still on its way: one spinner, and no
/// caption. The wait is usually a fraction of a second, and a sentence that
/// flashes up and is replaced before it can be read is a flicker rather than
/// a state; the spinner alone says the card is alive and about to fill.
fn loading() -> Element<'static, RepositoryAction> {
    widget::container(crate::ui_controls::activity_spinner(
        24.0,
        crate::design::accent_color,
    ))
    .center(Length::Fill)
    .into()
}

fn information_state<'a>(
    title: impl Into<String>,
    detail: impl Into<String>,
) -> Element<'a, RepositoryAction> {
    information_state_height(title, detail, Length::Fill)
}

fn information_state_height<'a>(
    title: impl Into<String>,
    detail: impl Into<String>,
    height: Length,
) -> Element<'a, RepositoryAction> {
    crate::ui_controls::placeholder(title.into())
        .icon(Icon::Info)
        .detail(detail.into())
        .height(height)
        .into()
}

#[cfg(test)]
mod tests {
    async fn finish_navigation_task(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
        use cosmic::iced::futures::StreamExt;
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    drop(runtime.update(action));
                }
            }
        }
    }

    #[tokio::test]
    async fn async_diff_source_locations_map_lf_lines_across_bare_cr_and_crlf() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mixed.rs"), "first\rcontinued\nsecond\r\nthird\n").unwrap();
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::new_directory(RepositoryView { project: ProjectId::new(), repository: root.path().into(), worktree: root.path().into(), selected_file: None });
        let task = runtime.open_source_file_location(card, "mixed.rs".into(), 1);
        assert!(matches!(&runtime.document, DocumentState::Loading));
        finish_navigation_task(&mut runtime, task).await;
        let DocumentState::Editable(document) = &runtime.document else { panic!("source editor"); };
        assert_eq!(document.content.cursor().position, text_editor::Position { line: 2, column: 0 });
        let task = runtime.open_source_file_location(card, "mixed.rs".into(), 2);
        finish_navigation_task(&mut runtime, task).await;
        let DocumentState::Editable(document) = &runtime.document else { panic!("source editor"); };
        assert_eq!(document.content.cursor().position, text_editor::Position { line: 3, column: 0 });
        let task = runtime.open_source_file_location(card, "mixed.rs".into(), usize::MAX);
        finish_navigation_task(&mut runtime, task).await;
        let DocumentState::Editable(document) = &runtime.document else { panic!("source editor"); };
        assert_eq!(document.content.cursor().position.line, document.content.line_count() - 1);
    }

    #[tokio::test]
    async fn folder_editor_navigation_survives_saved_file_and_sqlite_restart() {
        use crate::model::{CardKind, CardState, WorkspaceState};
        use crate::persistence::{SnapshotBase, StateStore};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("notes.txt"), "first\nsecond\nthird\n").unwrap();
        let card = CardId::new();
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: root.clone(),
            worktree: root.clone(),
            selected_file: None,
        };
        let mut runtime = RepositoryRuntime::new_directory(view.clone());
        let task = runtime.update(RepositoryAction::OpenFile {
            card,
            path: "notes.txt".into(),
        });
        finish_navigation_task(&mut runtime, task).await;
        let task = runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
        });
        drop(task);
        let task = runtime.request_save(card, false);
        finish_navigation_task(&mut runtime, task).await;
        assert!(!runtime.is_dirty());
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!("editable file");
        };
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 2, column: 3 },
            selection: Some(text_editor::Position { line: 1, column: 2 }),
        });
        document
            .content
            .restore_scroll_position(text_editor::ScrollPosition {
                line: 1,
                vertical: 2.5,
                horizontal: 7.0,
            });
        let navigation = runtime.navigation().unwrap();
        assert!(
            runtime.descriptor.selected_file.is_none(),
            "folder mode remains a browser"
        );
        let mut workspace = WorkspaceState::default();
        workspace.cards.push(CardState {
            id: card,
            generation: 0,
            title: "Folder".into(),
            width_fraction: 1.0,
            existence: crate::model::Existence::Open,
            exposure: crate::model::Exposure::Visible,
            residency: crate::model::Residency::Cold,
            kind: CardKind::Repository(view.clone()),
        });
        workspace
            .repository_navigation
            .insert(card, navigation.clone());
        let store = StateStore::open(
            root.join("navigation.sqlite3"),
            crate::model::BootId::new("editor-navigation-test"),
        )
        .unwrap();
        store
            .save_workspace(
                &workspace,
                SnapshotBase {
                    workspace: 0,
                    cards: &std::collections::BTreeMap::new(),
                },
            )
            .unwrap();
        let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
        assert_eq!(restored.repository_navigation[&card], navigation);
        let mut reopened = RepositoryRuntime::new_directory(view);
        reopened.restore_navigation(&restored.repository_navigation[&card]);
        let task = reopened.initial_load(card);
        assert_eq!(
            reopened.navigation(),
            Some(navigation.clone()),
            "a snapshot during hydration retains the pending caret"
        );
        finish_navigation_task(&mut reopened, task).await;
        assert_eq!(reopened.selected_path(), Some(Path::new("notes.txt")));
        assert_eq!(reopened.navigation(), Some(navigation));
        let DocumentState::Editable(document) = &reopened.document else {
            panic!("restored editor");
        };
        assert_eq!(
            document.content.text(),
            std::fs::read_to_string(root.join("notes.txt")).unwrap()
        );
        assert!(!reopened.is_dirty());
        assert_eq!(
            reopened.take_restored_editor_focus(card),
            Some(editor_focus_id(card))
        );
        assert!(reopened.take_restored_editor_focus(card).is_none());
    }

    #[tokio::test]
    async fn restored_repository_navigation_installs_initial_git_status_without_losing_caret() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        run_git(root, ["init", "-q"]);
        std::fs::write(root.join("notes.txt"), "before\n").unwrap();
        run_git(root, ["add", "notes.txt"]);
        run_git(
            root,
            [
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        std::fs::write(root.join("notes.txt"), "after\n").unwrap();
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: root.join(".git"),
            worktree: root.into(),
            selected_file: None,
        };
        let navigation = crate::model::RepositoryNavigation {
            project: view.project,
            worktree: view.worktree.clone(),
            selected_file: Some("notes.txt".into()),
            editor: Some(crate::model::EditorNavigation {
                caret: crate::model::EditorPosition { line: 0, column: 3 },
                selection: None,
                scroll_line: 0,
                scroll_vertical: 0.0,
                scroll_horizontal: 0.0,
            }),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.restore_navigation(&navigation);
        let task = runtime.initial_load(CardId::new());
        finish_navigation_task(&mut runtime, task).await;
        assert!(runtime.selected.as_ref().unwrap().unstaged);
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
        assert_eq!(
            runtime.navigation().unwrap().editor.unwrap().caret,
            navigation.editor.unwrap().caret
        );
    }

    #[tokio::test]
    async fn dedicated_file_navigation_restore_finishes_loading_without_a_file_list() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.txt"), "saved").unwrap();
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: directory.path().into(),
            worktree: directory.path().into(),
            selected_file: Some("notes.txt".into()),
        };
        let navigation = crate::model::RepositoryNavigation {
            project: view.project,
            worktree: view.worktree.clone(),
            selected_file: view.selected_file.clone(),
            editor: None,
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.restore_navigation(&navigation);
        let task = runtime.initial_load(CardId::new());
        finish_navigation_task(&mut runtime, task).await;
        assert!(matches!(runtime.files, FileListState::Ready(_)));
        assert!(matches!(runtime.document, DocumentState::Editable(_)));
        assert_eq!(runtime.selected_path(), Some(Path::new("notes.txt")));
    }

    #[test]
    fn restored_editor_navigation_rejects_other_recipes_and_escaping_paths() {
        let card = CardId::new();
        let (runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "saved");
        let navigation = runtime.navigation().unwrap();
        for variant in 0..4 {
            let mut changed = navigation.clone();
            match variant {
                0 => changed.project = ProjectId::new(),
                1 => changed.worktree = "/other".into(),
                2 => changed.selected_file = Some("../outside".into()),
                _ => changed.selected_file = Some("/outside".into()),
            }
            let mut reopened = RepositoryRuntime::new(runtime.descriptor.clone());
            reopened.restore_navigation(&changed);
            assert!(reopened.restored_navigation.is_none());
        }
        let mut dedicated = runtime.descriptor.clone();
        dedicated.selected_file = Some("other.txt".into());
        let mut reopened = RepositoryRuntime::new(dedicated);
        reopened.restore_navigation(&navigation);
        assert!(reopened.restored_navigation.is_none());
    }

    #[tokio::test]
    async fn restored_editor_caret_clamps_to_changed_file_and_never_applies_to_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("notes.txt"), "é").unwrap();
        let card = CardId::new();
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: root.clone(),
            worktree: root.clone(),
            selected_file: None,
        };
        let navigation = crate::model::RepositoryNavigation {
            project: view.project,
            worktree: root,
            selected_file: Some("notes.txt".into()),
            editor: Some(crate::model::EditorNavigation {
                caret: crate::model::EditorPosition {
                    line: 99,
                    column: 99,
                },
                selection: None,
                scroll_line: 0,
                scroll_vertical: 0.0,
                scroll_horizontal: 0.0,
            }),
        };
        let mut runtime = RepositoryRuntime::new_directory(view.clone());
        runtime.restore_navigation(&navigation);
        let task = runtime.initial_load(card);
        finish_navigation_task(&mut runtime, task).await;
        assert_eq!(
            runtime.navigation().unwrap().editor.unwrap().caret,
            crate::model::EditorPosition { line: 0, column: 2 }
        );
        let mut runtime = RepositoryRuntime::new_directory(view);
        runtime.restore_navigation(&navigation);
        let stale = runtime.initial_load(card);
        let replacement = runtime.open_file_entry(
            card,
            RepositoryFile {
                path: "missing.txt".into(),
                ..RepositoryFile::default()
            },
        );
        finish_navigation_task(&mut runtime, stale).await;
        finish_navigation_task(&mut runtime, replacement).await;
        assert_eq!(runtime.selected_path(), Some(Path::new("missing.txt")));
        assert!(runtime.pending_editor_navigation.is_none());
    }

    #[test]
    fn diff_toolbar_labels_are_visible_at_default_width_and_collapse_when_narrow() {
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        load_fresh_diff(&mut runtime, card, two_change_model());
        let renderer = headless_renderer();
        let height = |width| {
            let mut toolbar = selected_file_toolbar(card, &runtime).unwrap();
            let mut tree = Tree::new(toolbar.as_widget());
            let node = toolbar.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 720.0)),
            );
            assert!(
                node.size().height < 160.0,
                "toolbar must leave space for code"
            );
            let accessible = toolbar.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let labels = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter(|node| !node.node().is_hidden())
                .filter_map(|node| node.node().label())
                .collect::<Vec<_>>();
            if width >= 340.0 {
                for label in [
                    "Stage",
                    "Unstage",
                    "Revert",
                    "Ignore whitespace",
                    "Working",
                    "Staged",
                    "Branch",
                    "History",
                    "Unified",
                    "Split",
                    "Previous",
                    "Next",
                ] {
                    assert!(labels.contains(&label), "missing {label}: {labels:?}");
                }
                assert!(!labels.contains(&"Diff actions ⋮"));
            } else {
                assert!(labels.contains(&"Diff actions ⋮"));
            }
            node.size().height
        };
        let normal = height(crate::design::INSPECTOR_DEFAULT_WIDTH);
        let narrow = height(300.0);
        assert!(normal > narrow + 20.0);
    }

    #[test]
    fn shell_toolbar_actions_stay_in_the_header_when_find_is_open() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};

        #[derive(Clone)]
        enum ShellMessage {
            Preview,
            Repository(RepositoryAction),
        }
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::dirty_fixture(card);
        runtime.find_open = true;
        let renderer = headless_renderer();
        let mut toolbar = selected_file_toolbar_mapped_with(
            card,
            &runtime,
            Some(
                icon_button(Icon::Browser, "Shell preview")
                    .on_press(ShellMessage::Preview)
                    .into(),
            ),
            ShellMessage::Repository,
        )
        .unwrap();
        let mut tree = Tree::new(toolbar.as_widget());
        let node = toolbar.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(600.0, 160.0)),
        );
        let nodes =
            toolbar
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let preview = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some("Shell preview"))
            .expect("shell actions retain their accessible name");
        let bounds = preview.node().bounds().unwrap();
        assert!(bounds.y1 <= f64::from(crate::design::HEADER_CONTENT_HEIGHT));
        assert!(node.children()[1].bounds().y >= crate::design::HEADER_CONTENT_HEIGHT);
    }

    #[test]
    fn retargeted_file_panes_reject_old_reads_even_after_returning_to_the_same_path() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.txt"), "old contents").unwrap();
        let card = crate::model::CardId::new();
        let view = RepositoryView {
            project: crate::model::ProjectId::new(),
            repository: directory.path().to_path_buf(),
            worktree: directory.path().to_path_buf(),
            selected_file: Some("a.txt".into()),
        };
        let mut runtime = RepositoryRuntime::new(view.clone());
        drop(runtime.initial_load(card));
        let old = RepositoryAction::FileLoaded {
            identity: runtime.current_identity(card, "a.txt".into()),
            result: load_file(
                directory.path(),
                Path::new("a.txt"),
                crate::editor::PdfTone::Original,
            ),
        };
        let mut other = view.clone();
        other.selected_file = Some("b.txt".into());
        drop(runtime.retarget_file(card, other));
        std::fs::write(directory.path().join("a.txt"), "new contents").unwrap();
        drop(runtime.retarget_file(card, view));
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: runtime.current_identity(card, "a.txt".into()),
            result: load_file(
                directory.path(),
                Path::new("a.txt"),
                crate::editor::PdfTone::Original,
            ),
        }));
        assert_eq!(runtime.editor_text().as_deref(), Some("new contents"));
        drop(runtime.update(old));
        assert_eq!(runtime.editor_text().as_deref(), Some("new contents"));
        assert!(!runtime.inspector_context().can_back);
    }
    use std::collections::BTreeSet;

    use crate::lsp::{LspRange, LspSnippet, LspSnippetStop};
    use crate::model::ProjectId;

    fn test_working_output(model: DiffModel) -> WorkingDiffOutput {
        WorkingDiffOutput {
            additions: added_source_lines(&model),
            model: Some(model),
        }
    }

    /// A runtime whose working-tree comparison describes the live buffer.
    ///
    /// The file is open for editing and the index baseline is known, which is
    /// what keeps that comparison in step with unsaved edits and what lets the
    /// editor tint the lines the author added.
    fn working_diff_runtime(
        card: CardId,
        before: &str,
        after: &str,
    ) -> (RepositoryRuntime, AsyncIdentity) {
        let (mut runtime, identity) = RepositoryRuntime::lsp_open_fixture(card, after);
        runtime.source_surface = SourceSurface::Diff;
        let model = crate::diff_view::build_diff(before, after);
        runtime.working_diff.additions = added_source_lines(&model);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(model)),
        );
        runtime.working_diff.applied = runtime.wanted_working_diff();
        runtime.working_diff.desired = runtime.wanted_working_diff();
        runtime.working_diff.applied_rich = true;
        runtime.working_diff.desired_rich = true;
        (runtime, identity)
    }

    /// The added-line tint only tells the truth while the comparison it came
    /// from still describes the buffer, so an edit has to retire it until the
    /// next working diff lands.
    #[test]
    fn editing_the_working_source_retires_the_added_line_tint() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "removed\nkept\n", "added\nkept\n");
        // The tint belongs to the buffer the author is typing into, which is
        // the Edit surface. Diff renders the comparison instead.
        runtime.source_surface = SourceSurface::Editor;
        assert_eq!(runtime.working_diff.additions, vec![0]);
        assert!(runtime.working_annotations_current());
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
        }));
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Select(text_editor::Motion::End),
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("native buffer")
        };
        assert_eq!(document.content.selection().as_deref(), Some("added"));
        // IME commits and clipboard paste both take this exact action path.
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("café".into()))),
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("native buffer")
        };
        assert_eq!(document.content.text(), "café\nkept\n");
        assert_eq!(document.saved_text, "added\nkept\n");
        assert!(document.dirty);
        assert_eq!(&*runtime.diff_model().unwrap().before, "removed\nkept\n");
        assert!(
            !runtime.working_annotations_current(),
            "a tint drawn from a superseded comparison must go out with it"
        );
    }

    #[test]
    fn discarding_from_diff_reveals_backing_editor_and_remains_undoable() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "index\n", "saved\n");
        runtime.source_surface = SourceSurface::Editor;
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        let changed = runtime.editor_text().unwrap();
        assert!(runtime.is_dirty());
        runtime.source_surface = SourceSurface::Diff;
        drop(runtime.update(RepositoryAction::DiscardEdits { card }));
        assert!(
            runtime.editor_focus_target(card).is_some(),
            "Ctrl+Z must have a mounted native source editor"
        );
        assert_eq!(runtime.editor_text().as_deref(), Some("saved\n"));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some(changed.as_str()));
        assert!(runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some("saved\n"));
        assert!(!runtime.is_dirty());
    }

    /// The comparison surface mounts no buffer, so an editor action arriving
    /// while it is up is a stale message from a surface that is gone.
    #[test]
    fn the_comparison_surface_takes_no_editor_actions() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
        for basis in [
            DiffBasis::IndexToWorktree,
            DiffBasis::HeadToIndex,
            DiffBasis::CommitToParent {
                oid: fixture_oid(1),
                parent: Some(fixture_oid(2)),
            },
        ] {
            runtime.diff_basis = basis.clone();
            runtime.set_diff_state(
                basis,
                DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                    "old\n",
                    "historic\n",
                ))),
            );
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
            }));
            let DocumentState::Editable(document) = &runtime.document else {
                panic!("native buffer")
            };
            assert_eq!(document.content.text(), "working\n");
            assert!(!document.dirty);
            assert!(runtime.editor_focus_target(card).is_none());
        }
    }

    #[test]
    fn working_diff_coalesces_edits_and_rejects_cancelled_revision_results() {
        let card = CardId::new();
        let (mut runtime, identity) = working_diff_runtime(card, "old\n", "first\n");
        // Edits arrive from the Edit surface; the comparison surface mounts no
        // buffer to type into.
        runtime.source_surface = SourceSurface::Editor;
        runtime.working_diff.applied = None;
        drop(runtime.working_diff_task(card));
        let timer = runtime.working_diff.timer.unwrap();
        for _ in 0..20 {
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
            }));
            drop(runtime.working_diff_task(card));
            assert_eq!(runtime.working_diff.timer, Some(timer));
            assert!(runtime.working_diff.active.is_none());
        }
        runtime.working_diff.deadline = Some(Instant::now());
        drop(runtime.start_working_diff(card, timer));
        let active = runtime.working_diff.active.as_ref().unwrap();
        let revision = active.revision;
        let before = Arc::clone(&active.before);
        let cancellation = active.cancellation.clone();
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('y')),
        }));
        drop(runtime.working_diff_task(card));
        assert!(cancellation.is_cancelled());
        assert!(
            runtime.working_diff.timer.is_none(),
            "a cancelled CPU slot stays occupied until completion"
        );
        drop(runtime.finish_working_diff(
            identity.clone(),
            revision,
            timer,
            before.clone(),
            Ok(Some(test_working_output(crate::diff_view::build_diff(
                &before,
                "obsolete\n",
            )))),
        ));
        assert!(runtime.working_diff.applied.is_none());
        let next = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        drop(runtime.start_working_diff(card, next));
        let active = runtime.working_diff.active.as_ref().unwrap();
        let revision = active.revision;
        let before = Arc::clone(&active.before);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("native buffer")
        };
        let text = document.content.text();
        let model = crate::diff_view::build_diff(&before, &text);
        drop(runtime.finish_working_diff(
            identity,
            revision,
            next,
            before,
            Ok(Some(test_working_output(model))),
        ));
        assert!(runtime.working_annotations_current());
        assert!(runtime.working_diff.timer.is_none());
        assert!(runtime.working_diff.active.is_none());
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("native buffer")
        };
        assert_eq!(document.content.text(), text);
    }

    #[tokio::test]
    async fn gutter_only_diff_materializes_on_worker_and_upgrades_when_revealed() {
        use cosmic::iced::futures::StreamExt;
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
        runtime.source_surface = SourceSurface::Editor;
        runtime.working_diff.applied = None;
        let snapshot = match &runtime.document {
            DocumentState::Editable(document) => document.content.text_snapshot(),
            _ => unreachable!(),
        };
        assert!(!snapshot.is_materialized());
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        let task = runtime.start_working_diff(card, ticket);
        assert!(
            !snapshot.is_materialized(),
            "capturing a debounce must not serialize on the UI thread"
        );
        assert!(!runtime.working_diff.active.as_ref().unwrap().rich);
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        let iced::runtime::Action::Output(action) = stream.next().await.unwrap() else {
            panic!("diff output")
        };
        assert!(snapshot.is_materialized());
        let RepositoryAction::WorkingDiffLoaded {
            result: Ok(Some(output)),
            ..
        } = &action
        else {
            panic!("successful gutter output")
        };
        assert!(
            output.model.is_none(),
            "gutter lane must not build rich rows, syntax, or shape widths"
        );
        assert_eq!(output.additions, vec![0]);
        drop(runtime.update(action));
        assert!(runtime.working_annotations_current());
        assert!(!runtime.working_diff.applied_rich);
        runtime.source_surface = SourceSurface::Diff;
        drop(runtime.working_diff_task(card));
        let ticket = runtime
            .working_diff
            .timer
            .expect("revealing a stale rich comparison upgrades without another edit");
        let task = runtime.start_working_diff(card, ticket);
        assert!(runtime.working_diff.active.as_ref().unwrap().rich);
        finish_navigation_task(&mut runtime, task).await;
        assert!(runtime.working_diff.applied_rich);
        assert!(runtime.working_diff_current());
        assert_eq!(runtime.diff_model().unwrap().after.as_ref(), "new\n");
    }

    #[tokio::test]
    async fn hidden_diff_surface_uses_gutter_lane_until_visible() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
        runtime.set_diff_surface_visible(false);
        runtime.working_diff.applied = None;
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        let task = runtime.start_working_diff(card, ticket);
        assert!(!runtime.working_diff.active.as_ref().unwrap().rich);
        finish_navigation_task(&mut runtime, task).await;
        assert!(!runtime.working_diff.applied_rich);
        runtime.set_diff_surface_visible(true);
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        let task = runtime.start_working_diff(card, ticket);
        assert!(runtime.working_diff.active.as_ref().unwrap().rich);
        finish_navigation_task(&mut runtime, task).await;
        assert!(runtime.working_diff.applied_rich);
    }

    #[test]
    fn hiding_diff_cancels_rich_worker_and_does_not_start_initial_rich_load() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
        runtime.working_diff.applied = None;
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        drop(runtime.start_working_diff(card, ticket));
        let cancellation = runtime
            .working_diff
            .active
            .as_ref()
            .unwrap()
            .cancellation
            .clone();
        assert!(!cancellation.is_cancelled());
        drop(runtime.refresh_diff_visibility(card, false));
        assert!(cancellation.is_cancelled());
        runtime.diffs.clear();
        drop(runtime.ensure_current_diff(card));
        assert!(
            runtime.diffs.is_empty(),
            "an offscreen comparison must not invoke Git/highlight/shape initially"
        );
        runtime.set_diff_surface_visible(true);
        drop(runtime.ensure_current_diff(card));
        assert!(matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Loading { .. })
        ));
        let cancellation = match runtime.diff_state(&DiffBasis::IndexToWorktree).unwrap() {
            DiffState::Loading { cancellation, .. } => cancellation.clone(),
            _ => unreachable!(),
        };
        runtime.set_diff_surface_visible(false);
        assert!(cancellation.is_cancelled());
        assert!(runtime.diffs.is_empty());
    }

    #[test]
    fn diff_gutter_maps_source_lines_to_native_bare_cr_lines() {
        let content =
            text_editor::Content::<cosmic::Renderer>::with_text("first\rsecond\nthird\r\nlast");
        assert_eq!(
            native_added_lines(&content.text_snapshot(), vec![0]),
            vec![0, 1]
        );
        assert_eq!(
            native_added_lines(&content.text_snapshot(), vec![1, 2]),
            vec![2, 3]
        );
    }

    #[test]
    fn equal_baseline_bytes_from_a_new_basis_request_cannot_accept_an_old_worker() {
        let card = CardId::new();
        let (mut runtime, identity) = working_diff_runtime(card, "old\n", "new\n");
        runtime.working_diff.applied = None;
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        drop(runtime.start_working_diff(card, ticket));
        let before = Arc::clone(&runtime.working_diff.active.as_ref().unwrap().before);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                "old\n", "new\n",
            ))),
        );
        drop(runtime.finish_working_diff(
            identity,
            0,
            ticket,
            before,
            Ok(Some(test_working_output(crate::diff_view::build_diff(
                "old\n",
                "obsolete\n",
            )))),
        ));
        assert!(runtime.working_diff.applied.is_none());
        assert_eq!(
            runtime
                .diff_model()
                .unwrap()
                .inline_rows
                .last()
                .unwrap()
                .text,
            "new"
        );
    }

    #[test]
    fn unrelated_status_changes_do_not_invalidate_the_selected_diff() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
        let baseline = Arc::clone(&runtime.diff_model().unwrap().before);
        let FileListState::Ready(files) = &runtime.files else {
            panic!("file list")
        };
        let mut files = files.clone();
        let other = files
            .iter_mut()
            .find(|file| file.path == Path::new("other.txt"))
            .unwrap();
        other.staged = true;
        other.status = Some("MM".into());
        runtime.file_list_loading = true;
        runtime.background_file_refresh = true;
        let identity = AsyncIdentity {
            card,
            path: ".".into(),
            worktree: runtime.descriptor.worktree.clone(),
            generation: runtime.file_list_generation,
        };
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity,
            result: Ok(files),
        }));
        assert!(Arc::ptr_eq(
            &baseline,
            &runtime.diff_model().unwrap().before
        ));
        assert!(runtime.file(Path::new("other.txt")).unwrap().staged);
        assert!(runtime.working_annotations_current());
    }

    #[test]
    fn status_refresh_after_our_save_keeps_the_visible_working_diff() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
        // SaveFinished updates the selected file before the next status read,
        // while the list can still contain its pre-save clean status.
        let selected = runtime.selected.clone().unwrap();
        let FileListState::Ready(files) = &mut runtime.files else {
            panic!("file list")
        };
        let previous = files
            .iter_mut()
            .find(|file| file.path == selected.path)
            .unwrap();
        previous.unstaged = false;
        previous.status = None;
        let mut refreshed = files.clone();
        let refreshed_file = refreshed
            .iter_mut()
            .find(|file| file.path == selected.path)
            .unwrap();
        *refreshed_file = selected;
        drop(runtime.refresh_visible(card));
        let identity = AsyncIdentity {
            card,
            path: ".".into(),
            worktree: runtime.descriptor.worktree.clone(),
            generation: runtime.file_list_generation,
        };
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity,
            result: Ok(refreshed),
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert!(runtime.working_annotations_current());
        assert!(
            matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Ready(_, _))
            ),
            "our own save must not replace the working diff with a loading screen"
        );
    }

    #[test]
    fn branch_only_paths_open_read_only_and_keep_rename_identity_for_retry() {
        let card = CardId::new();
        let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
        let basis = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
        };
        drop(runtime.update(RepositoryAction::OpenCommittedFile {
            card,
            path: RepoPath::parse("deleted-or-renamed.txt").unwrap(),
            old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
            basis: basis.clone(),
        }));
        assert!(!runtime.accepts_selected(&identity));
        assert!(matches!(runtime.document, DocumentState::None));
        assert!(!runtime.can_save());
        assert!(runtime.file(Path::new("deleted-or-renamed.txt")).is_none());
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
        assert_eq!(runtime.diff_basis, basis);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        runtime.set_diff_state(
            basis.clone(),
            DiffState::ready(RenderedDiff::Error("retry".into())),
        );
        drop(runtime.update(RepositoryAction::RetryDiff { card }));
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
    }

    #[test]
    fn commit_only_paths_open_read_only_and_keep_rename_identity_for_retry() {
        let card = CardId::new();
        let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
        let basis = DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        };
        drop(runtime.update(RepositoryAction::OpenCommittedFile {
            card,
            path: RepoPath::parse("deleted-or-renamed.txt").unwrap(),
            old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
            basis: basis.clone(),
        }));
        assert!(!runtime.accepts_selected(&identity));
        assert!(matches!(runtime.document, DocumentState::None));
        assert!(!runtime.can_save());
        assert!(runtime.file(Path::new("deleted-or-renamed.txt")).is_none());
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
        assert_eq!(runtime.diff_basis, basis);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        runtime.set_diff_state(
            basis.clone(),
            DiffState::ready(RenderedDiff::Error("retry".into())),
        );
        drop(runtime.update(RepositoryAction::RetryDiff { card }));
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
    }

    /// A review surface installs a pull request comparison over a working
    /// file list that knows nothing about it. The comparison is read-only,
    /// keeps its rename identity, survives the status refresh a background
    /// change triggers, and can be retried — exactly like the other immutable
    /// bases, which is what the review surface will depend on.
    #[test]
    fn a_pull_request_basis_survives_status_refresh_and_retries() {
        let card = CardId::new();
        let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
        let basis = DiffBasis::PullRequest {
            number: 12,
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
        };
        drop(runtime.update(RepositoryAction::OpenCommittedFile {
            card,
            path: RepoPath::parse("renamed.txt").unwrap(),
            old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
            basis: basis.clone(),
        }));
        assert!(!runtime.accepts_selected(&identity));
        assert!(matches!(runtime.document, DocumentState::None));
        assert!(!runtime.can_save());
        assert_eq!(runtime.diff_basis, basis);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert!(runtime.can_show_diff());
        assert_eq!(basis_label(&basis), "#12 vs main");
        assert_eq!(basis_choice_label(&basis), "Pull request");
        assert_eq!(basis_choice(&basis), None);

        // The refresh a background working-tree change triggers must not
        // replace the review with a working comparison.
        drop(runtime.refresh_selected_status(card));
        assert_eq!(runtime.diff_basis, basis);
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );

        runtime.set_diff_state(
            basis.clone(),
            DiffState::ready(RenderedDiff::Error("retry".into())),
        );
        assert!(runtime.can_retry_diff());
        drop(runtime.update(RepositoryAction::RetryDiff { card }));
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
    }

    #[test]
    fn branch_navigation_cannot_discard_an_unsaved_or_saving_native_buffer() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
        // The buffer is dirtied through the Edit surface, which is the only
        // surface that mounts it.
        runtime.source_surface = SourceSurface::Editor;
        let open = RepositoryAction::OpenCommittedFile {
            card,
            path: RepoPath::parse("deleted.txt").unwrap(),
            old_path: None,
            basis: DiffBasis::MergeBaseToHead {
                base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
                head: fixture_oid(1),
                merge_base: fixture_oid(2),
            },
        };
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.update(open.clone()));
        assert_eq!(
            runtime.selected.as_ref().unwrap().path,
            Path::new("notes.txt")
        );
        assert!(runtime.is_dirty());
        drop(runtime.request_save(card, false));
        drop(runtime.update(open));
        assert_eq!(
            runtime.selected.as_ref().unwrap().path,
            Path::new("notes.txt")
        );
        assert!(runtime.is_saving());
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
    }

    #[test]
    fn faint_diff_syntax_remains_readable_over_both_highlight_layers() {
        for theme in [cosmic::Theme::light(), cosmic::Theme::dark()] {
            for kind in [DiffRowKind::Addition, DiffRowKind::Deletion] {
                let row = diff_row_background(&theme, kind);
                let highlight = color_over(diff_highlight_color(&theme, kind, 0.18), row);
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
    fn mode_switches_keep_independent_viewports_and_the_dirty_editor() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.md", "# Notes\n");
        let editor_position = text_editor::ScrollPosition {
            line: 12,
            vertical: 7.0,
            horizontal: 95.5,
        };
        if let DocumentState::Editable(document) = &mut runtime.document {
            document.markdown = true;
            document.dirty = true;
            document.content.restore_scroll_position(editor_position);
        }
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                "",
                &"line\n".repeat(100),
            ))),
        );
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        drop(runtime.update(RepositoryAction::DiffViewport {
            user_initiated: true,
            card,
            offset: 900.0,
            height: 300.0,
            horizontal: 40.0,
        }));
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
        assert_eq!(runtime.diff_viewport.offset, 900.0);
        drop(runtime.update(RepositoryAction::DiffViewport {
            user_initiated: true,
            card,
            offset: 1500.0,
            height: 300.0,
            horizontal: 80.0,
        }));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        drop(runtime.update(RepositoryAction::PreviewViewport {
            card,
            offset: 700.0,
        }));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("mode changes must not replace the buffer");
        };
        assert!(document.dirty);
        assert_eq!(document.content.text(), "# Notes\n");
        assert_eq!(document.content.scroll_position(), editor_position);
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert_eq!(
            (runtime.diff_viewport.offset, runtime.diff_horizontal),
            (1500.0, 80.0)
        );
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: false }));
        assert_eq!(
            (runtime.diff_viewport.offset, runtime.diff_horizontal),
            (900.0, 40.0)
        );
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        assert_eq!(runtime.viewports.preview, 700.0);
        // Navigation still refuses to discard this unsaved buffer.
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "other.txt".into(),
        }));
        assert_eq!(
            runtime.selected.as_ref().unwrap().path,
            Path::new("notes.md")
        );
    }

    #[test]
    fn reopening_files_restores_viewports_without_accepting_stale_loads() {
        let card = CardId::new();
        let text = format!("# Notes {}\n", "long source line ".repeat(30)).repeat(100);
        let (mut runtime, stale) = RepositoryRuntime::open_fixture(card, "notes.md", &text);
        let editor_position = text_editor::ScrollPosition {
            line: 25,
            vertical: 0.0,
            horizontal: 140.5,
        };
        if let DocumentState::Editable(document) = &mut runtime.document {
            document.markdown = true;
            document.content.restore_scroll_position(editor_position);
        }
        let renderer = headless_renderer();
        let mut tree = Tree::new(editor_surface(card, &runtime).as_widget());
        let cursor = runtime.cursor_position();
        {
            let mut element = editor_surface(card, &runtime);
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
            );
        }
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                "",
                &"line\n".repeat(100),
            ))),
        );
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        drop(runtime.update(RepositoryAction::DiffViewport {
            user_initiated: true,
            card,
            offset: 1200.0,
            height: 300.0,
            horizontal: 35.0,
        }));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        drop(runtime.update(RepositoryAction::PreviewViewport {
            card,
            offset: 800.0,
        }));
        let FileListState::Ready(files) = &runtime.files else {
            panic!("the fixture has a loaded file list");
        };
        let files = files.clone();
        drop(runtime.update(RepositoryAction::BackToFiles { card }));
        let identity = AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: ".".into(),
            generation: runtime.file_list_generation,
        };
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity,
            result: Ok(files),
        }));
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "other.txt".into(),
        }));
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "notes.md".into(),
        }));
        let loaded = || {
            Ok(LoadedFile {
                source: "notes.md".into(),
                content: FileContent::Editable {
                    text: text.clone(),
                    content_hash: blake3::hash(text.as_bytes()),
                    markdown: true,
                },
            })
        };
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: stale,
            result: loaded(),
        }));
        assert!(matches!(runtime.document, DocumentState::Loading));
        let identity = runtime.current_identity(card, "notes.md".into());
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: loaded(),
        }));
        {
            let mut element = editor_surface(card, &runtime);
            tree.diff(element.as_widget_mut());
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
            );
        }
        assert_eq!(runtime.cursor_position(), cursor);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("the current file should finish loading");
        };
        assert_eq!(document.content.scroll_position(), editor_position);
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        assert_eq!(runtime.viewports.preview, 800.0);
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert_eq!(
            (runtime.diff_viewport.offset, runtime.diff_horizontal),
            (1200.0, 35.0)
        );
        let request = runtime.next_diff_request;
        let identity = runtime.current_identity(card, "notes.md".into());
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: DiffBasis::IndexToWorktree,
            request,
            content: staged_text_diff(crate::diff_view::build_diff("", &"line\n".repeat(100))),
        }));
        assert_eq!(runtime.diff_viewport.offset, 1200.0);
        let visible = visible_rows(1000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        assert!(visible.start <= 40 && visible.end > 40);
    }

    #[test]
    fn viewport_retention_evicts_old_paths_but_refreshes_revisited_paths() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "old.txt");
        runtime.set_standalone(true);
        drop(runtime.update(RepositoryAction::DiffViewport {
            user_initiated: true,
            card,
            offset: 600.0,
            height: 300.0,
            horizontal: 0.0,
        }));
        for index in 0..MAX_RETAINED_FILE_VIEWPORTS {
            drop(runtime.update(RepositoryAction::OpenFile {
                card,
                path: format!("file-{index}.txt").into(),
            }));
        }
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "old.txt".into(),
        }));
        assert_eq!(runtime.diff_viewport.offset, 600.0);
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "extra.txt".into(),
        }));
        assert!(runtime.retained_viewports.len() <= MAX_RETAINED_FILE_VIEWPORTS);
        assert!(
            !runtime
                .retained_viewports
                .iter()
                .any(|(path, _)| path == Path::new("file-0.txt"))
        );
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: "old.txt".into(),
        }));
        assert_eq!(runtime.diff_viewport.offset, 600.0);
    }

    #[test]
    fn text_preview_preserves_literal_extensionless_content_and_retains_its_viewport() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "LICENSE");
        let text = "# Literal text\n\tIndented\n<tag>& symbol\n";
        runtime.document = DocumentState::Editable(EditorDocument::unlinked(
            "LICENSE".into(),
            text.into(),
            blake3::hash(text.as_bytes()),
            false,
        ));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert_eq!(document.preview_text, text);
        assert!(document.markdown_items.is_empty());
        assert!(!runtime.editable_surface_visible());
        drop(runtime.update(RepositoryAction::PreviewViewport {
            card,
            offset: 420.0,
        }));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }));
        assert!(runtime.editable_surface_visible());
        assert_eq!(runtime.viewports.preview, 420.0);
    }

    #[test]
    fn diagram_theme_change_cancels_work_and_fences_old_tone_completions() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "chart.mmd");
        runtime.document = DocumentState::Editable(EditorDocument::unlinked(
            "chart.mmd".into(),
            "flowchart LR; A-->B".into(),
            blake3::hash(b"fixture"),
            false,
        ));
        runtime.markdown_mode = MarkdownMode::Preview;
        drop(runtime.diagram_task(card));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        let old_cancel = document.diagram_cancel.as_ref().unwrap().0.clone();
        let other = if document.diagram_dark {
            cosmic::Theme::light()
        } else {
            cosmic::Theme::dark()
        };
        drop(runtime.retone_document(card, &other));
        assert!(old_cancel.is_cancelled());
        let key = blake3::hash(b"flowchart LR; A-->B");
        drop(runtime.update(RepositoryAction::DiagramsRendered {
            identity: identity.clone(),
            revision: 0,
            tone_generation: 0,
            result: vec![(key, preview::Diagram::Error("old tone".into()))],
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert!(document.diagrams.is_empty());
        assert_eq!(document.diagram_pending, Some(0));
        assert_eq!(document.diagram_tone_generation, 1);
        drop(runtime.update(RepositoryAction::DiagramsRendered {
            identity,
            revision: 0,
            tone_generation: 1,
            result: vec![(key, preview::Diagram::Error("new tone".into()))],
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert!(document.diagrams.contains_key(&key));
        assert_eq!(document.diagram_pending, None);
        drop(runtime.retone_document(card, &other));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert_eq!(document.diagram_pending, None);
        assert_eq!(document.diagram_tone_generation, 1);
    }

    #[test]
    fn diagram_completion_is_fenced_by_document_identity_and_buffer_revision() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "chart.mmd");
        runtime.document = DocumentState::Editable(EditorDocument::unlinked(
            "chart.mmd".into(),
            "flowchart LR; A-->B".into(),
            blake3::hash(b"fixture"),
            false,
        ));
        runtime.markdown_mode = MarkdownMode::Preview;
        drop(runtime.diagram_task(card));
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!()
        };
        assert_eq!(document.diagram_pending, Some(0));
        document.revision = 1;
        document.content = text_editor::Content::with_text("flowchart LR; A-->C");
        document.markdown_stale = true;
        drop(runtime.diagram_task(card)); // Coalesces onto the one outstanding worker.
        let key = blake3::hash(b"flowchart LR; A-->B");
        drop(runtime.update(RepositoryAction::DiagramsRendered {
            identity: identity.clone(),
            revision: 0,
            tone_generation: 0,
            result: vec![(key, preview::Diagram::Error("stale".into()))],
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert!(!document.diagrams.contains_key(&key));
        assert_eq!(document.diagram_pending, None);
        let ticket = runtime.markdown_debounce.timer.unwrap();
        runtime.markdown_debounce.deadline = None;
        drop(runtime.finish_markdown(card, ticket));
        let mut stale = identity.clone();
        stale.generation += 1;
        drop(runtime.update(RepositoryAction::DiagramsRendered {
            identity: stale,
            revision: 1,
            tone_generation: 0,
            result: vec![],
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert_eq!(document.diagram_pending, Some(1));
        let key = blake3::hash(b"flowchart LR; A-->C");
        drop(runtime.update(RepositoryAction::DiagramsRendered {
            identity,
            revision: 1,
            tone_generation: 0,
            result: vec![(key, preview::Diagram::Error("invalid".into()))],
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert!(document.diagrams.contains_key(&key));
        assert_eq!(document.diagram_pending, None);
    }

    #[test]
    fn preview_actions_require_a_preview_renderer_and_preserve_the_current_surface() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.document = DocumentState::Editable(EditorDocument::unlinked(
            PathBuf::from("plain.rs"),
            "fn main() {}\n".into(),
            blake3::hash(b"fn main() {}\n"),
            false,
        ));
        assert!(runtime.can_apply_markdown_mode(true));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }));
        assert_eq!(runtime.source_surface, SourceSurface::Editor);

        runtime.document = DocumentState::Editable(EditorDocument::unlinked(
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
            DiffState::ready(RenderedDiff::Error("read failed".into())),
        );
        assert!(runtime.can_retry_diff());
        drop(runtime.update(RepositoryAction::RetryDiff { card }));
        assert!(!runtime.can_retry_diff());
        assert!(matches!(
            runtime.diff_state(&basis),
            Some(DiffState::Loading { .. })
        ));
        runtime.set_diff_state(basis, DiffState::ready(staged_text_diff(model_with(1))));
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
                crate::settings::EditorSyntaxTheme::Auto,
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
                    if mode == DiffMode::Inline {
                        let two_gutter_code =
                            geometry.viewport_width - 2.0 * 44.0 - DIFF_MARKER_WIDTH;
                        assert!((geometry.code_viewport - two_gutter_code).abs() < 0.01);
                    }
                    let offset = geometry.overflow(code_width);
                    if widest + 2.0 * DIFF_ROW_PADDING <= geometry.code_viewport {
                        assert_eq!(offset, 0.0, "short lines must not scroll");
                    } else {
                        assert!(
                            (widest + 2.0 * DIFF_ROW_PADDING - offset - geometry.code_viewport)
                                .abs()
                                < 1.0,
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
                        let padded = code.children().next().unwrap();
                        assert!((padded.bounds().x - code.bounds().x + offset).abs() < 0.01);
                        assert!(
                            (padded.bounds().width - code_width - 2.0 * DIFF_ROW_PADDING).abs()
                                < 0.01
                        );
                        let shaped = padded.children().next().unwrap();
                        assert!(
                            (shaped.bounds().x - code.bounds().x + offset - DIFF_ROW_PADDING).abs()
                                < 0.01
                        );
                        assert!((shaped.bounds().width - code_width).abs() < 0.01);
                    }
                }
            }
        }
    }

    /// Every scroll viewport in a laid-out surface, outermost first, paired
    /// with the content node that scrolls inside it.
    ///
    /// A scroll viewport is the one place a child is laid out larger than the
    /// parent clipping it, so finding them by that shape keeps the assertions
    /// below independent of how the surrounding view nests them.
    fn scroll_viewports(root: &layout::Node) -> Vec<(&layout::Node, &layout::Node)> {
        let mut found = Vec::new();
        let mut pending = std::collections::VecDeque::from([root]);
        while let Some(node) = pending.pop_front() {
            if let [content] = node.children() {
                let outer = node.size();
                let inner = content.size();
                if inner.width > outer.width + 0.5 || inner.height > outer.height + 0.5 {
                    found.push((node, content));
                }
            }
            pending.extend(node.children());
        }
        found
    }

    /// The rectangle a node's drawable children occupy, in that node's own
    /// coordinates. Zero-area children are the virtualization and rail
    /// spacers, which are the reservation rather than content inside it.
    fn occupied(parent: &layout::Node) -> iced::Rectangle {
        parent
            .children()
            .iter()
            .map(layout::Node::bounds)
            .filter(|bounds| bounds.width > 0.0 && bounds.height > 0.0)
            .reduce(|union, bounds| union.union(&bounds))
            .expect("the surface lays out drawable content")
    }

    fn laid_out(
        mut element: Element<'_, RepositoryAction>,
        renderer: &cosmic::Renderer,
        size: iced::Size,
    ) -> layout::Node {
        let mut tree = Tree::new(element.as_widget());
        element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, size),
        )
    }

    /// The scroll rail is a layout promise, so it is read back off laid-out
    /// surfaces rather than off the constants it is built from: a two-axis
    /// scrollable has to leave the corner where its two thumbs meet empty, a
    /// vertical-only one has to stop its content short of the right edge, and
    /// a horizontal-only one must reserve the bottom rail without also
    /// reserving a right rail it has no thumb to hide under.
    #[test]
    fn scroll_rails_keep_content_clear_of_every_thumb_and_their_junction() {
        let renderer = headless_renderer();
        let rail = crate::design::SCROLLBAR_GUTTER;
        let card = CardId::new();
        let line = "let value = compute(argument, another_argument, third_argument);";
        let before = format!("{}kept\n", format!("{line}\n").repeat(24));
        let (runtime, _) = working_diff_runtime(card, &before, "kept\n");
        let model = runtime.diff_model().expect("a text comparison");

        // Two axes: the diff canvas, whose rails are the ones editor content
        // coordinates are derived from.
        let surface = iced::Size::new(420.0, 240.0);
        let diff = laid_out(
            diff_model_view(card, &runtime, model, "HEAD", "Worktree", None),
            &renderer,
            surface,
        );
        let (viewport, canvas) = *scroll_viewports(&diff)
            .first()
            .expect("the diff mounts a scrollable");
        assert!(
            canvas.size().width > viewport.size().width
                && canvas.size().height > viewport.size().height,
            "the fixture has to overflow both axes for either thumb to exist"
        );
        // The canvas holds one column, translated in X as the code pans.
        let column = canvas
            .children()
            .first()
            .expect("the canvas carries the row column");
        let right = viewport.size().width - (column.bounds().x + column.bounds().width);
        assert!(
            (right - rail).abs() < 0.5,
            "diff rows reach {right} from the viewport's right edge, not {rail}"
        );
        let rows = occupied(column);
        let bottom = column.bounds().height - (rows.y + rows.height);
        assert!(
            (bottom - rail).abs() < 0.5,
            "the lowest diff row leaves {bottom} below it, not {rail}"
        );

        // All toolbar targets share the full header's center, including when
        // the action lane overflows. A bottom-only rail used to lift these
        // targets above the filename and the shell's trailing Find button.
        fn check_targets(node: &layout::Node, parent_y: f32, count: &mut usize) {
            let bounds = node.bounds();
            let y = parent_y + bounds.y;
            let target = crate::design::PRIMARY_CONTROL_SIZE;
            if (bounds.width - target).abs() < 0.1 && (bounds.height - target).abs() < 0.1 {
                assert!(
                    (y + bounds.height / 2.0 - crate::design::HEADER_CONTENT_HEIGHT / 2.0).abs()
                        < 0.1,
                    "toolbar target must share the header baseline"
                );
                *count += 1;
            }
            for child in node.children() {
                check_targets(child, y, count);
            }
        }
        for width in [240.0, 1200.0] {
            let toolbar = laid_out(
                selected_file_toolbar(card, &runtime).unwrap(),
                &renderer,
                iced::Size::new(width, crate::design::HEADER_HEIGHT),
            );
            let mut targets = 0;
            check_targets(&toolbar, 0.0, &mut targets);
            assert!(targets > 0);
        }

        // The compact toolbar keeps its primary actions visible and moves
        // secondary actions into a menu instead of a horizontal scroll lane.
        let toolbar = laid_out(
            selected_file_toolbar(card, &runtime).expect("a selected file has a toolbar"),
            &renderer,
            iced::Size::new(240.0, crate::design::HEADER_HEIGHT),
        );
        assert!(scroll_viewports(&toolbar).is_empty());

        // Vertical only: the Markdown preview, whose inset is `space::XL`.
        // That is wider than the rail today, and this is what notices if the
        // inset ever becomes narrower.
        let (mut preview, identity) = RepositoryRuntime::selection_fixture(card, "notes.md");
        let text = "# Notes\n\nA paragraph long enough to wrap inside the preview.\n".repeat(24);
        drop(preview.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "notes.md".into(),
                content: crate::editor::FileContent::Editable {
                    content_hash: blake3::hash(text.as_bytes()),
                    text,
                    markdown: true,
                },
            }),
        }));
        drop(preview.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        }));
        let DocumentState::Editable(document) = &preview.document else {
            panic!("the fixture loads an editable Markdown buffer");
        };
        let rendered = laid_out(
            editable_document_view(card, &preview, document),
            &renderer,
            surface,
        );
        let (viewport, content) = *scroll_viewports(&rendered)
            .first()
            .expect("the preview scrolls");
        assert!(
            content.size().height > viewport.size().height,
            "the fixture has to overflow for a thumb to exist"
        );
        let prose = occupied(content);
        let right = content.size().width - (prose.x + prose.width);
        assert!(
            right >= rail - 0.5,
            "preview prose ends {right} from the right edge, inside the {rail} rail"
        );
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

    /// The working-tree comparison is a comparison, so its deleted lines are
    /// rows in it like any other basis's.
    ///
    /// It used to mount the live editable buffer instead. A buffer cannot show
    /// a line the file no longer contains, so deletions were marked with a
    /// rule and relegated to a second pane holding only the selected hunk's,
    /// which is why a deletion was almost never on screen. The comparison
    /// surface therefore has to lay out exactly what the diff renderer lays
    /// out for the same model, on both layouts.
    #[test]
    fn the_working_comparison_lays_out_its_deleted_lines_as_rows() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "gone\nkept\nalso gone\n", "kept\n");
        let renderer = headless_renderer();
        let size = iced::Size::new(720.0, 600.0);
        fn diff_rows(layout: Layout<'_>) -> usize {
            let mut rows = usize::from((layout.bounds().height - DIFF_ROW_HEIGHT).abs() < 0.01);
            for child in layout.children() {
                rows += diff_rows(child);
            }
            rows
        }
        for mode in [DiffMode::Inline, DiffMode::Split] {
            runtime.diff_mode = mode;
            let model = runtime.diff_model().expect("the fixture computed a diff");
            assert_eq!(
                model
                    .inline_rows
                    .iter()
                    .filter(|row| row.kind == DiffRowKind::Deletion)
                    .count(),
                2,
                "the fixture has to delete lines for this to mean anything"
            );
            let rendered = laid_out(
                diff_model_view(card, &runtime, model, "Index", "Working buffer", None),
                &renderer,
                size,
            );
            let mounted = laid_out(diff_surface(card, &runtime, None), &renderer, size);
            let rendered = diff_rows(Layout::new(&rendered));
            assert!(
                rendered >= model.row_count(mode),
                "{mode:?} carries one row per model row, got {rendered}"
            );
            assert_eq!(
                diff_rows(Layout::new(&mounted)),
                rendered,
                "{mode:?} must mount the comparison, not the buffer"
            );
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
                directory: false,
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
                    status: Some("??".into()),
                    directory: false,
                },
                RepositoryFile {
                    path: PathBuf::from("renamed.md"),
                    staged_old_path: Some(PathBuf::from("old-name.md")),
                    unstaged_old_path: None,
                    staged: true,
                    unstaged: false,
                    status: Some("R ".into()),
                    directory: false,
                },
                RepositoryFile {
                    path: PathBuf::from("tracked.md"),
                    staged_old_path: None,
                    unstaged_old_path: None,
                    staged: false,
                    unstaged: true,
                    status: Some(" M".into()),
                    directory: false,
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
        let mut document = EditorDocument::unlinked(
            PathBuf::from("README.md"),
            "# Title\n".into(),
            blake3::hash(b"# Title\n"),
            true,
        );
        assert!(document.markdown_items.is_empty());
        assert!(document.preview_text.is_empty());
        assert!(document.markdown_stale);
        document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
            std::sync::Arc::new("More".into()),
        )));
        assert!(document.dirty);
        assert!(document.markdown_items.is_empty());
        document.sync_markdown();
        assert!(!document.markdown_items.is_empty());
        assert_eq!(document.preview_text, document.content.text());
        assert!(!document.markdown_stale);
        document.discard_edits();
        assert!(!document.dirty);
        assert!(document.active_save.is_none());
        assert_eq!(document.content.text(), "# Title\n");
        assert!(document.markdown_stale);
        document.sync_markdown();
        assert_eq!(document.preview_text, "# Title\n");
        assert_eq!(document.markdown_items.len(), 1);
    }

    #[test]
    fn save_completion_keeps_edits_made_while_the_write_was_in_flight_dirty() {
        let view = RepositoryView {
            selected_file: None,
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
            directory: false,
        });
        let mut document = EditorDocument::unlinked(
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
            history_revision: if content_matches(&document.content, "submitted\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n").text_snapshot(),
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
            result: Ok(LoadedFile {
                source: "notes.txt".into(),
                content: FileContent::Editable {
                    text: "B\n".into(),
                    content_hash: blake3::hash(b"B\n"),
                    markdown: false,
                },
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
            selected_file: None,
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
            directory: false,
        });
        let mut document = EditorDocument::unlinked(
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
            history_revision: if content_matches(&document.content, "newest\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("newest\n").text_snapshot(),
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
        let mut document = EditorDocument::unlinked(
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
            history_revision: if content_matches(&document.content, "submitted\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n").text_snapshot(),
            snapshot: "submitted\n".into(),
        });
        document.dirty = false;
        let view = RepositoryView {
            selected_file: None,
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
            selected_file: None,
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
            directory: false,
        });
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 4,
        };
        let mut document = EditorDocument::unlinked(
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
            history_revision: if content_matches(&document.content, "submitted\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n").text_snapshot(),
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
            result: Ok(LoadedFile {
                source: identity.path.clone(),
                content: FileContent::Editable {
                    text: "loaded\n".into(),
                    content_hash: blake3::hash(b"loaded\n"),
                    markdown: false,
                },
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
            DiffState::ready(staged_text_diff(model_with(4))),
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
            selected_file: None,
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
            directory: false,
        });
        runtime.document = DocumentState::Loading;

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
            result: Ok(LoadedFile {
                source: path.clone(),
                content: FileContent::Editable {
                    text: "loaded\n".into(),
                    content_hash: blake3::hash(b"loaded\n"),
                    markdown: true,
                },
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
                directory: false,
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
            selected_file: None,
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
            directory: false,
        });
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 7,
        };
        let mut document =
            EditorDocument::unlinked(path, "before\n".into(), blake3::hash(b"before\n"), false);
        document.content = text_editor::Content::with_text("after\n");
        document.dirty = true;
        document.revision = 1;
        document.active_save = Some(SaveTicket {
            id: 2,
            identity: identity.clone(),
            revision: 1,
            history_revision: if content_matches(&document.content, "after\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("after\n").text_snapshot(),
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
    fn starting_a_save_preserves_the_diff_review_surface() {
        let view = RepositoryView {
            selected_file: None,
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
            directory: false,
        });
        let mut document =
            EditorDocument::unlinked(path, "disk\n".into(), blake3::hash(b"disk\n"), true);
        document.content = text_editor::Content::with_text("changed\n");
        document.dirty = true;
        document.revision = 1;
        runtime.document = DocumentState::Editable(document);
        runtime.source_surface = SourceSurface::Diff;
        runtime.markdown_mode = MarkdownMode::Preview;

        let _save = runtime.request_save(card, false);

        assert!(runtime.is_saving());
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
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
            selected_file: None,
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
            directory: false,
        };
        runtime.files = FileListState::Ready(vec![file.clone()]);
        runtime.selected = Some(file);
        let mut document = EditorDocument::unlinked(
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
            DiffState::ready(staged_text_diff(DiffModel {
                line_counts: [0; 2],
                present: [true; 2],
                before: Arc::from(""),
                after: Arc::from(""),
                inline_rows: Vec::new(),
                split_rows: Vec::new(),
                inline_change_starts: vec![0, 2],
                split_change_starts: vec![0, 2],
                unchanged: Default::default(),
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
            DiffState::ready(RenderedDiff::Error("diff failed".into())),
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
            selected_file: None,
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
            directory: false,
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
                selected_file: None,
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
                directory: false,
            });
            let mut document = EditorDocument::unlinked(
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
                history_revision: if content_matches(&document.content, "after\n") { document.history.revision() } else { u64::MAX },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("after\n").text_snapshot(),
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
            selected_file: None,
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
            directory: false,
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
        assert_eq!(runtime.diff_viewport.offset, 600.0);
        let _ = runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: DiffBasis::IndexToWorktree,
            request: 1,
            content: staged_text_diff(model_with(1)),
        });
        assert!(matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) if model.additions == 2
        ));
    }

    #[test]
    fn stale_file_completion_cannot_replace_current_selection() {
        let view = RepositoryView {
            selected_file: None,
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
            directory: false,
        });
        runtime.document = DocumentState::Loading;
        let stale = AsyncIdentity {
            card: CardId::new(),
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("older.rs"),
            generation: 8,
        };
        let _ = runtime.update(RepositoryAction::FileLoaded {
            identity: stale,
            result: Ok(LoadedFile {
                source: "older.rs".into(),
                content: FileContent::Editable {
                    text: "stale".into(),
                    content_hash: blake3::hash(b"stale"),
                    markdown: false,
                },
            }),
        });
        assert!(
            matches!(runtime.document, DocumentState::Loading),
            "a stale load neither fills nor clears the document still on its way"
        );
        assert_eq!(
            runtime.selected.as_ref().map(|file| file.path.as_path()),
            Some(Path::new("current.rs")),
            "the selection the load is for is untouched"
        );
    }

    #[test]
    fn visible_row_projection_bounds_widgets_for_large_virtualized_lists() {
        const ROW_HEIGHT: f32 = 36.0;
        let top = visible_rows(
            100_000,
            ListViewport {
                offset: 0.0,
                height: 720.0,
            },
            ROW_HEIGHT,
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
            top.top_space + (top.end - top.start) as f32 * ROW_HEIGHT + top.bottom_space,
            100_000.0 * ROW_HEIGHT
        );
        assert_eq!(
            middle.top_space
                + (middle.end - middle.start) as f32 * DIFF_ROW_HEIGHT
                + middle.bottom_space,
            1_000_000.0 * DIFF_ROW_HEIGHT
        );
    }

    #[test]
    fn annotation_measurement_uses_natural_layout_and_preserves_child_interaction() {
        let renderer = headless_renderer();
        let card = CardId::new();
        let (_, identity) = diff_runtime(card);
        let content = widget::column::with_children([
            iced::widget::Space::new().height(42.0).into(),
            widget::column::with_children([
                iced::widget::Space::new().height(DIFF_ROW_HEIGHT).into(),
                widget::button::standard("Reply to review")
                    .height(64.0)
                    .on_press(RepositoryAction::NextChange { card })
                    .into(),
            ])
            .into(),
        ])
        .width(300.0)
        .into();
        let mut element = Element::new(DiffAnnotationMeasure {
            content,
            identity,
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            layout_revision: 4,
            review_revision: (2, 3),
            row_height: DIFF_ROW_HEIGHT,
            expected_leading: 0.0,
            expected: BTreeMap::from([(8, 99.0), (20, 80.0)]),
            // Row 8's removed discussion is deliberately absent. An offscreen
            // discussion retains its last measured height until it mounts.
            retained: BTreeMap::from([(20, 80.0)]),
            slots: vec![(0, None), (1, Some(2))],
            pending_anchor: false,
        });
        let mut tree = Tree::new(element.as_widget());
        let bounds = iced::Rectangle::with_size(iced::Size::new(300.0, 600.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        let mut messages = Vec::new();
        let cursor = iced::mouse::Cursor::Available(iced::Point::new(40.0, 90.0));
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            Layout::new(&node),
            cursor,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            matches!(&messages[..], [RepositoryAction::DiffAnnotations { leading, rows, review_revision: (2, 3), .. }] if *leading == 42.0 && rows == &[(2,64.0),(20,80.0)])
        );
        messages.clear();
        for event in [
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        assert!(
            matches!(&messages[..], [RepositoryAction::NextChange { card: pressed }] if *pressed == card)
        );
        let accessible = element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, cursor);
        assert!(!accessible.root().is_empty() || !accessible.children().is_empty());
    }

    #[test]
    fn annotation_measurements_reject_replaced_comparison_and_wrong_document() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        let content = || staged_text_diff(crate::diff_view::build_diff("old\n", "new\n"));
        runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
        let measurement = RepositoryAction::DiffAnnotations {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            layout_revision: runtime.diff_layout_revision,
            review_revision: (0, 0),
            leading: 40.0,
            rows: vec![(0, 80.0)],
        };
        runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
        drop(runtime.update(measurement));
        assert_eq!(runtime.diff_rows().unwrap().leading_height(), 0.0);
        let mut stale = identity.clone();
        stale.path = "different.txt".into();
        for (identity, expected_leading) in [(stale, 0.0), (identity, 40.0)] {
            drop(runtime.update(RepositoryAction::DiffAnnotations {
                identity,
                basis: DiffBasis::IndexToWorktree,
                mode: DiffMode::Inline,
                layout_revision: runtime.diff_layout_revision,
                review_revision: (0, 0),
                leading: 40.0,
                rows: vec![(0, 80.0)],
            }));
            assert_eq!(
                runtime.diff_rows().unwrap().leading_height(),
                expected_leading
            );
        }
        assert_eq!(runtime.diff_rows().unwrap().leading_height(), 40.0);
        assert_eq!(runtime.diff_rows().unwrap().height(0), 110.0);
    }

    #[test]
    fn gutter_ranges_preserve_side_and_original_anchor_when_backtracking() {
        use crate::github::ReviewDiffSide::{Left, Right};
        use crate::pull_request_review::{Action, discussion::Anchor, discussion_ui};
        use cosmic::iced::futures::StreamExt;
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        for (line, side, extend, expected) in [
            (5, Right, false, (5, 5)),
            (9, Right, true, (5, 9)),
            (3, Right, true, (3, 5)),
            (2, Left, true, (2, 2)),
        ] {
            let task = runtime.update(RepositoryAction::ReviewLine {
                identity: identity.clone(),
                basis: DiffBasis::IndexToWorktree,
                extend,
                action: Action::Discussion(discussion_ui::Action::Fenced {
                    generation: 11,
                    authority_revision: 7,
                    action: Box::new(discussion_ui::Action::Begin(Anchor {
                        path: "notes.txt".into(),
                        side,
                        start_line: line,
                        line,
                    })),
                }),
            });
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            let output = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(stream.next())
                .unwrap();
            let iced::runtime::Action::Output(RepositoryAction::Review { mut action, .. }) = output
            else {
                panic!("review action");
            };
            let anchor = review_begin_anchor(&mut action).unwrap();
            assert_eq!((anchor.start_line, anchor.line), expected);
            assert_eq!(anchor.side, side);
            assert!(matches!(
                action,
                Action::Discussion(discussion_ui::Action::Fenced { generation: 11, .. })
            ));
        }
    }

    #[test]
    fn measured_diff_blocks_preserve_anchor_and_change_navigation() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let mut model = model_with(2);
        model.inline_rows = (0..20)
            .map(|line| InlineDiffRow {
                old_line: Some(line + 1),
                new_line: Some(line + 1),
                text: "line".into(),
                kind: DiffRowKind::Context,
                changed: Vec::new(),
            })
            .collect();
        model.inline_change_starts = vec![2, 7];
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(model)),
        );
        runtime.diff_viewport = ListViewport {
            offset: 105.0,
            height: 120.0,
        };
        runtime.diff_horizontal = 44.0;
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 60.0, &[(1, 90.0), (3, 30.0)]));
        assert_eq!(runtime.diff_viewport.offset, 270.0);
        assert_eq!(runtime.diff_horizontal, 44.0);
        let unchanged = runtime.diff_viewport.offset;
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 60.0, &[(1, 90.0), (3, 30.0)]));
        assert_eq!(runtime.diff_viewport.offset, unchanged);
        drop(runtime.update(RepositoryAction::NextChange { card }));
        assert_eq!(runtime.diff_viewport.offset, 390.0);
        drop(runtime.update(RepositoryAction::PreviousChange { card }));
        assert_eq!(runtime.diff_viewport.offset, 210.0);
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 0.0, &[]));
        assert_eq!(runtime.diff_viewport.offset, 60.0);
    }

    #[test]
    fn change_navigation_follows_manual_scroll_and_stops_at_the_ends() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let mut model = model_with(2);
        model.inline_rows = (0..20)
            .map(|line| InlineDiffRow {
                old_line: Some(line + 1),
                new_line: Some(line + 1),
                text: "line".into(),
                kind: DiffRowKind::Context,
                changed: Vec::new(),
            })
            .collect();
        model.inline_change_starts = vec![2, 7, 15];
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(model)),
        );
        let middle = runtime.diff_rows().unwrap().offset(7);
        let last = runtime.diff_rows().unwrap().offset(15);
        runtime.diff_viewport.offset = middle + 1.0;
        runtime.navigate_change(true);
        assert_eq!(runtime.diff_change, 2);
        runtime.realize_current_change();
        assert_eq!(runtime.diff_viewport.offset, last);
        runtime.navigate_change(true);
        assert_eq!(runtime.diff_change, 2);
        runtime.diff_viewport.offset = middle + 1.0;
        runtime.navigate_change(false);
        assert_eq!(runtime.diff_change, 1);
        runtime.diff_viewport.offset = 0.0;
        assert!(!runtime.navigate_change(false));
        assert_eq!(runtime.diff_viewport.offset, 0.0);
    }

    #[test]
    fn mutation_refresh_keeps_only_the_same_files_comparison_visible() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(model_with(3))),
        );
        runtime.diff_viewport.offset = 180.0;
        drop(runtime.refresh_after_mutation(card));
        assert!(runtime.diff_model().is_some());
        assert_eq!(runtime.diff_viewport.offset, 180.0);
        assert!(runtime.selected_staging_selection(true).is_none());
        runtime.selected.as_mut().unwrap().path = PathBuf::from("another.rs");
        assert!(runtime.diff_model().is_none());
    }

    #[test]
    fn folded_changes_step_exactly_when_every_scroll_target_is_clamped() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let before = (0..100)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let after = before
            .replace("line 20\n", "first change\n")
            .replace("line 80\n", "second change\n");
        runtime.diff_viewport.height = 2000.0;
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                &before, &after,
            ))),
        );
        assert_eq!(runtime.change_count(), 2);
        runtime.diff_change = 0;
        runtime.realize_current_change();
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        assert!(runtime.navigate_change(true));
        runtime.realize_current_change();
        assert_eq!(runtime.diff_change, 1);
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        assert!(!runtime.navigate_change(true));
        assert!(runtime.navigate_change(false));
        runtime.realize_current_change();
        assert_eq!(runtime.diff_change, 0);
        assert!(!runtime.navigate_change(false));
    }

    #[test]
    fn changing_folding_preference_preserves_the_logical_viewport() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let before = (0..100)
            .map(|row| format!("line {row}\n"))
            .collect::<String>();
        let after = before.replace("line 50\n", "changed\n");
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                &before, &after,
            ))),
        );
        let row = runtime
            .diff_model()
            .unwrap()
            .change_starts(runtime.diff_mode)[0];
        let display = runtime
            .diff_layout()
            .unwrap()
            .folded(runtime.diff_mode)
            .display(row);
        runtime.diff_viewport = ListViewport {
            offset: runtime.diff_rows().unwrap().offset(display),
            height: 20.0,
        };
        let folded_offset = runtime.diff_viewport.offset;
        drop(runtime.set_diff_hide_unchanged(false));
        assert_eq!(
            runtime.diff_viewport.offset,
            runtime.diff_rows().unwrap().offset(row)
        );
        drop(runtime.set_diff_hide_unchanged(true));
        assert_eq!(runtime.diff_viewport.offset, folded_offset);
    }

    #[test]
    fn file_discussion_is_offered_only_where_the_diff_can_show_it() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::Loading {
                request: 1,
                cancellation: CancellationToken::new(),
            },
        );
        assert!(
            !runtime.can_show_file_discussion(),
            "a pending comparison may fail and strand a new file draft"
        );
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Error("unreadable".into())),
        );
        assert!(!runtime.can_show_file_discussion());
        // Once a retry provides a renderable diff, a draft can be offered.
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff("", "line\n"))),
        );
        assert!(runtime.can_show_file_discussion());
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Error("unreadable".into())),
        );
        assert!(
            !runtime.can_show_file_discussion(),
            "\"Could not compute diff\" has no leading annotation"
        );
        runtime.basis_resolution = Some(BasisResolution::NoCommits);
        assert!(!runtime.can_show_file_discussion());
    }

    #[test]
    fn revealing_the_diff_top_survives_the_leading_block_being_measured() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                "",
                &"line\n".repeat(40),
            ))),
        );
        runtime.source_surface = SourceSurface::Editor;
        runtime.diff_viewport.offset = 300.0;
        drop(runtime.update(RepositoryAction::RevealDiffTop { card }));
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        // A new file discussion is measured after it mounts. Holding the
        // first code row in place would scroll the discussion out of view.
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 180.0, &[]));
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        assert!(runtime.pending_diff_anchor.is_none());
    }

    #[test]
    fn annotated_file_reopen_restores_logical_anchor_after_async_load() {
        for (saved, scrolled, expected) in [
            (350.0, None, 350.0),
            (350.0, Some(60.0), 260.0),
            (100.0, None, 100.0),
        ] {
            let card = CardId::new();
            let (mut runtime, _) = diff_runtime(card);
            let content =
                || staged_text_diff(crate::diff_view::build_diff("", &"line\n".repeat(20)));
            runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
            drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 200.0, &[]));
            runtime.diff_viewport.offset = saved;
            let file = runtime.selected.clone().unwrap();
            drop(runtime.open_file_entry(card, file.clone()));
            assert!(runtime.pending_diff_anchor.is_some());
            runtime.source_surface = SourceSurface::Diff;
            drop(runtime.ensure_current_diff(card));
            let Some(DiffState::Loading { request, .. }) =
                runtime.diff_state(&DiffBasis::IndexToWorktree)
            else {
                panic!("reopened comparison must load afresh");
            };
            let request = *request;
            drop(runtime.update(RepositoryAction::DiffLoaded {
                identity: runtime.current_identity(card, file.path),
                basis: DiffBasis::IndexToWorktree,
                request,
                content: content(),
            }));
            assert_eq!(
                runtime.diff_viewport.offset,
                if saved >= 200.0 { saved - 200.0 } else { 0.0 }
            );
            // A redraw clamps the temporary uniform document into a taller
            // viewport. This is not the reader choosing a different line.
            drop(runtime.update(RepositoryAction::DiffViewport {
                user_initiated: false,
                card,
                offset: 0.0,
                height: 900.0,
                horizontal: 0.0,
            }));
            assert!(runtime.pending_diff_anchor.is_some());
            if let Some(offset) = scrolled {
                drop(runtime.update(RepositoryAction::DiffViewport {
                    user_initiated: true,
                    card,
                    offset,
                    height: 120.0,
                    horizontal: 0.0,
                }));
            }
            // The pane returns to its reading height after the temporary
            // redraw; the retained line must then survive measurement.
            runtime.diff_viewport.height = 120.0;
            drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 200.0, &[]));
            assert_eq!(runtime.diff_viewport.offset, expected);
            assert!(runtime.pending_diff_anchor.is_none());
        }
    }

    #[test]
    fn diff_scroll_observer_distinguishes_redraw_from_wheel_without_duplicate_events() {
        let renderer = headless_renderer();
        let card = CardId::new();
        let content = widget::scrollable(iced::widget::Space::new().height(2000.0))
            .height(200.0)
            .width(300.0)
            .on_scroll(move |viewport| RepositoryAction::DiffViewport {
                card,
                offset: viewport.absolute_offset().y,
                height: viewport.bounds().height,
                horizontal: viewport.absolute_offset().x,
                user_initiated: false,
            });
        let mut element = Element::new(DiffScrollObserver {
            content: content.into(),
            card: None,
        });
        let mut tree = Tree::new(element.as_widget());
        let bounds = iced::Rectangle::with_size(iced::Size::new(300.0, 200.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        for (event, expected_input) in [
            (
                iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
                false,
            ),
            (
                iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: -3.0 },
                }),
                true,
            ),
        ] {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Available(iced::Point::new(20.0, 20.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
            assert_eq!(messages.len(), 1);
            assert!(
                matches!(messages[0], RepositoryAction::DiffViewport { user_initiated, .. } if user_initiated == expected_input)
            );
        }
    }

    #[test]
    fn actual_widget_trees_stay_bounded_at_top_middle_and_end_of_100k_rows() {
        let view = RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
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
            directory: false,
        });
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(DiffModel {
                line_counts: [0; 2],
                present: [true; 2],
                before: Arc::from(""),
                after: Arc::from(""),
                inline_rows: vec![inline; 100_000],
                split_rows: vec![split; 100_000],
                inline_change_starts: vec![0, 50_000, 99_999],
                split_change_starts: vec![0, 50_000, 99_999],
                unchanged: Default::default(),
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
            let element = diff_surface(card, &runtime, None);
            assert!(widget_nodes(&element) < 1_500, "diff widget tree was eager");
        }
    }

    /// Opens the working comparison afresh and completes its load with `model`.
    fn load_fresh_diff(runtime: &mut RepositoryRuntime, card: CardId, model: DiffModel) {
        runtime.diffs.clear();
        runtime.reset_diff_viewports();
        drop(runtime.ensure_current_diff(card));
        let Some(DiffState::Loading { request, .. }) =
            runtime.diff_state(&DiffBasis::IndexToWorktree)
        else {
            panic!("a fresh comparison loads");
        };
        let request = *request;
        let path = runtime.selected.as_ref().unwrap().path.clone();
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity: runtime.current_identity(card, path),
            basis: DiffBasis::IndexToWorktree,
            request,
            content: staged_text_diff(model),
        }));
    }

    /// Two changed lines, at lines 50 and 150 of 200.
    fn two_change_model() -> DiffModel {
        let before: String = (1..=200).map(|line| format!("line {line}\n")).collect();
        let after = before
            .replace("line 50\n", "fifty\n")
            .replace("line 150\n", "one hundred fifty\n");
        crate::diff_view::build_diff(&before, &after)
    }

    #[test]
    fn a_first_visit_opens_at_the_first_change_with_context() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.diff_viewport.height = 300.0;
        // Unfolded rows, as a model without unchanged runs has: the first
        // change is far below the top.
        let mut model = model_with(1);
        model.inline_rows = (0..100)
            .map(|line| InlineDiffRow {
                old_line: Some(line + 1),
                new_line: Some(line + 1),
                text: "line".into(),
                kind: DiffRowKind::Context,
                changed: Vec::new(),
            })
            .collect();
        model.inline_change_starts = vec![60, 80];
        load_fresh_diff(&mut runtime, card, model);
        assert_eq!(
            runtime.diff_viewport.offset,
            (60 - diff_layout::FOLD_CONTEXT - 1) as f32 * DIFF_ROW_HEIGHT
        );
        assert_eq!(runtime.diff_change, 0);

        // Folded, the first change is a fold and three context rows down, so
        // the comparison opens at its top.
        load_fresh_diff(&mut runtime, card, two_change_model());
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        let layout = runtime.diff_layout().unwrap();
        let folded = layout.folded(DiffMode::Inline);
        assert!(matches!(folded.row(0), DisplayRow::Fold { fold: 0, .. }));
        assert_eq!(folded.display(49), 4, "the first change");
        assert!(
            folded.len() < 30,
            "two changes in 200 lines fold to a screen"
        );

        // A position the reader chose is theirs: a reload keeps it.
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: 150.0,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        let file = runtime.selected.clone().unwrap();
        drop(runtime.open_file_entry(card, file.clone()));
        runtime.source_surface = SourceSurface::Diff;
        drop(runtime.ensure_current_diff(card));
        let Some(DiffState::Loading { request, .. }) =
            runtime.diff_state(&DiffBasis::IndexToWorktree)
        else {
            panic!("a reopened comparison loads");
        };
        let request = *request;
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity: runtime.current_identity(card, file.path),
            basis: DiffBasis::IndexToWorktree,
            request,
            content: staged_text_diff(two_change_model()),
        }));
        assert_eq!(runtime.diff_viewport.offset, 150.0);
    }

    #[test]
    fn expanding_a_fold_keeps_the_rows_on_screen_and_navigation_exact() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_viewport.height = 100.0;
        load_fresh_diff(&mut runtime, card, two_change_model());
        let layout = runtime.diff_layout().unwrap();
        let folded = layout.folded(DiffMode::Inline).clone();
        // Model row 148, the second context row above the second change,
        // a third of the way in.
        let display = folded.display(148);
        assert!(matches!(
            folded.row(display - 2),
            DisplayRow::Fold { fold: 1, .. }
        ));
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: display as f32 * DIFF_ROW_HEIGHT + 10.0,
            height: 100.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        let expand = |run: Range<usize>, expansion| RepositoryAction::ExpandDiffFold {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            fold: 1,
            run,
            expansion,
        };
        // A click on a fold drawn from another model names another run.
        drop(runtime.update(expand(52..150, FoldExpansion::Up)));
        assert!(matches!(
            runtime.diff_layout().unwrap().folded(DiffMode::Inline).row(display - 2),
            DisplayRow::Fold { fold: 1, rows } if rows == (54..147)
        ));
        drop(runtime.update(expand(51..150, FoldExpansion::Up)));
        let folded = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .clone();
        let display = folded.display(148);
        assert_eq!(
            display,
            folded.display(127) + 21,
            "twenty rows revealed above"
        );
        assert_eq!(
            runtime.diff_viewport.offset,
            display as f32 * DIFF_ROW_HEIGHT + 10.0,
            "the row that was on screen stays on screen"
        );

        // Change navigation lands on the change's row in the folded layout,
        // or as near it as the now short document can scroll.
        runtime.diff_viewport.height = 300.0;
        drop(runtime.update(RepositoryAction::NextChange { card }));
        let bottom =
            folded.len() as f32 * DIFF_ROW_HEIGHT + crate::design::SCROLLBAR_GUTTER - 300.0;
        assert!(bottom < folded.display(150) as f32 * DIFF_ROW_HEIGHT);
        assert_eq!(runtime.diff_viewport.offset, bottom);
        // Reveals are shared by both layouts.
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
        let split = runtime.diff_layout().unwrap().folded(DiffMode::Split);
        // Split row 126 is line 127, the first line revealed inline.
        let DisplayRow::Code(row) = split.row(split.display(126)) else {
            panic!("the rows revealed inline are revealed side by side");
        };
        assert_eq!(
            runtime.diff_model().unwrap().split_rows[row].new_line,
            Some(127)
        );
        assert!(matches!(
            split.row(split.display(125)),
            DisplayRow::Fold { fold: 1, .. }
        ));
    }

    #[test]
    fn a_rebuild_keeps_what_was_unfolded_and_the_line_on_screen() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_viewport.height = 300.0;
        load_fresh_diff(&mut runtime, card, two_change_model());
        // The top of the viewport on the second fold itself, expanded down:
        // the fold's first rows take its place.
        let fold = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .display(60);
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: fold as f32 * DIFF_ROW_HEIGHT,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        drop(runtime.update(RepositoryAction::ExpandDiffFold {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            fold: 1,
            run: 51..150,
            expansion: FoldExpansion::Down,
        }));
        let folded = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .clone();
        assert_eq!(folded.row(fold), DisplayRow::Code(54));
        assert_eq!(runtime.diff_viewport.offset, fold as f32 * DIFF_ROW_HEIGHT);

        // The same comparison read again keeps its reveal and its place.
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(two_change_model())),
        );
        assert_eq!(
            runtime.diff_layout().unwrap().folded(DiffMode::Inline),
            &folded
        );
        assert_eq!(runtime.diff_viewport.offset, fold as f32 * DIFF_ROW_HEIGHT);

        // An edit above renumbers the new side and shifts every row by one;
        // the runs keep their lengths, so the reveal holds, and the same line
        // stays at the top.
        let before: String = (1..=200).map(|line| format!("line {line}\n")).collect();
        let after = format!("new first line\n{}", before)
            .replace("line 50\n", "fifty\n")
            .replace("line 150\n", "one hundred fifty\n");
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                &before, &after,
            ))),
        );
        let layout = runtime.diff_layout().unwrap();
        let line_54 = runtime
            .diff_model()
            .unwrap()
            .inline_rows
            .iter()
            .position(|row| row.old_line == Some(54))
            .unwrap();
        let diff_layout::Anchor::Row { row, .. } = layout
            .rows(DiffMode::Inline)
            .anchor(runtime.diff_viewport.offset)
        else {
            panic!("a code row is on screen");
        };
        assert_eq!(
            row,
            layout.folded(DiffMode::Inline).display(line_54),
            "line 54, first on screen before the edit, is still at the top"
        );
    }

    #[test]
    fn a_rebuild_awaiting_its_anchor_still_keeps_what_was_unfolded() {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_viewport.height = 300.0;
        load_fresh_diff(&mut runtime, card, two_change_model());
        drop(runtime.update(RepositoryAction::ExpandDiffFold {
            identity,
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            fold: 1,
            run: 51..150,
            expansion: FoldExpansion::Down,
        }));
        let folded = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .clone();

        // A working-diff rebuild lands while the viewport still waits to
        // place an anchor.
        runtime.pending_diff_anchor = Some(runtime.current_diff_viewport());
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(two_change_model())),
        );
        assert_eq!(
            runtime.diff_layout().unwrap().folded(DiffMode::Inline),
            &folded
        );
    }

    #[test]
    fn editor_diff_roundtrip_tracks_the_source_line_and_unfolds_it() {
        let card = CardId::new();
        let before = (0..200)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let after = before
            .replace("line 20\n", "changed 20\n")
            .replace("line 180\n", "changed 180\n");
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "fixture.rs", &after);
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                &before, &after,
            ))),
        );
        runtime.viewports.editor = text_editor::ScrollPosition {
            line: 100,
            vertical: 3.0,
            horizontal: 12.0,
        };
        runtime.align_diff_to_editor();
        runtime.source_surface = SourceSurface::Diff;
        let anchor = runtime.current_diff_viewport();
        let Some(diff_layout::Anchor::Row { row, .. }) = anchor.row else {
            panic!("mapped row");
        };
        assert_eq!(
            runtime
                .diff_model()
                .unwrap()
                .new_line_at_row(DiffMode::Inline, row),
            Some(101)
        );
        let layout = runtime.diff_layout().unwrap();
        assert!(matches!(
            layout
                .folded(DiffMode::Inline)
                .row(layout.folded(DiffMode::Inline).display(row)),
            DisplayRow::Code(_)
        ));
        runtime.align_editor_to_diff();
        assert_eq!(runtime.viewports.editor.line, 100);
        assert!((runtime.viewports.editor.vertical - 3.0).abs() < 0.01);
        assert_eq!(runtime.viewports.editor.horizontal, 12.0);
    }

    #[tokio::test]
    async fn diff_reveal_preserves_editor_source_line_after_gutter_only_insertions() {
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let card = CardId::new();
            let before = (0..100)
                .map(|line| format!("line {line}\n"))
                .collect::<String>();
            let (mut runtime, _) = working_diff_runtime(card, &before, &before);
            runtime.diff_mode = mode;
            runtime.source_surface = SourceSurface::Editor;
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                    "inserted\n".repeat(5),
                ))),
            }));
            drop(runtime.working_diff_task(card));
            runtime.working_diff.deadline = Some(Instant::now());
            let ticket = runtime.working_diff.timer.unwrap();
            let task = runtime.start_working_diff(card, ticket);
            finish_navigation_task(&mut runtime, task).await;
            assert!(!runtime.working_diff.applied_rich);
            let DocumentState::Editable(document) = &mut runtime.document else {
                unreachable!()
            };
            document
                .content
                .restore_scroll_position(text_editor::ScrollPosition {
                    line: 80,
                    vertical: 2.0,
                    horizontal: 9.0,
                });
            drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
            drop(runtime.working_diff_task(card));
            let ticket = runtime.working_diff.timer.unwrap();
            let task = runtime.start_working_diff(card, ticket);
            finish_navigation_task(&mut runtime, task).await;
            let Some(diff_layout::Anchor::Row { row, .. }) = runtime.current_diff_viewport().row
            else {
                panic!("source row")
            };
            assert_eq!(
                runtime.diff_model().unwrap().new_line_at_row(mode, row),
                Some(81)
            );
            runtime.align_editor_to_diff();
            assert_eq!(runtime.viewports.editor.line, 80);
            assert!((runtime.viewports.editor.vertical - 2.0).abs() < 0.01);
            assert_eq!(runtime.viewports.editor.horizontal, 9.0);
        }
    }

    #[test]
    fn initial_staged_diff_applies_source_anchor_when_rows_arrive() {
        let card = CardId::new();
        let text = (0..100)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut runtime, _) = working_diff_runtime(card, &text, &text);
        runtime.source_surface = SourceSurface::Editor;
        runtime.diff_basis = DiffBasis::HeadToIndex;
        runtime.viewports.editor = text_editor::ScrollPosition {
            line: 70,
            vertical: 3.0,
            horizontal: 8.0,
        };
        runtime.align_diff_to_editor();
        assert!(runtime.pending_diff_source.is_some());
        runtime.source_surface = SourceSurface::Diff;
        runtime.set_diff_state(
            DiffBasis::HeadToIndex,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(&text, &text))),
        );
        assert!(runtime.pending_diff_source.is_none());
        let Some(diff_layout::Anchor::Row { row, .. }) = runtime.current_diff_viewport().row else {
            panic!("source row")
        };
        assert_eq!(
            runtime
                .diff_model()
                .unwrap()
                .new_line_at_row(DiffMode::Inline, row),
            Some(71)
        );
        runtime.align_editor_to_diff();
        assert_eq!(runtime.viewports.editor.line, 70);
        assert!((runtime.viewports.editor.vertical - 3.0).abs() < 0.01);
        assert_eq!(runtime.viewports.editor.horizontal, 8.0);
    }

    #[test]
    fn diff_editor_mapping_reconciles_native_bare_cr_line_numbers() {
        let card = CardId::new();
        let text = "first\rsecond\nthird\nlast";
        let (mut runtime, _) = working_diff_runtime(card, text, text);
        runtime.source_surface = SourceSurface::Editor;
        runtime.viewports.editor = text_editor::ScrollPosition {
            line: 2,
            vertical: 0.0,
            horizontal: 0.0,
        };
        runtime.align_diff_to_editor();
        runtime.source_surface = SourceSurface::Diff;
        let Some(diff_layout::Anchor::Row { row, .. }) = runtime.current_diff_viewport().row else {
            panic!("source row")
        };
        assert_eq!(
            runtime
                .diff_model()
                .unwrap()
                .new_line_at_row(DiffMode::Inline, row),
            Some(2)
        );
        runtime.align_editor_to_diff();
        assert_eq!(runtime.viewports.editor.line, 2);
    }

    #[test]
    fn diff_rows_follow_editor_font_size_and_line_height() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        load_fresh_diff(&mut runtime, card, two_change_model());
        runtime.set_editor_font(Font::MONOSPACE, 20.0);
        runtime.set_line_height(1.25);
        assert_eq!(runtime.diff_typography().size, 20.0);
        assert_eq!(runtime.diff_typography().line_height, 25.0);
        let rows = runtime.diff_layout().unwrap().rows(DiffMode::Inline);
        assert!((rows.offset(2) - rows.offset(1) - 25.0).abs() < 0.01);
    }

    #[test]
    fn diff_zoom_reuses_rows_but_family_changes_remeasure_with_the_reading_anchor() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        load_fresh_diff(&mut runtime, card, two_change_model());
        let baseline = Arc::clone(&runtime.diff_model().unwrap().before);
        runtime.set_editor_font(Font::MONOSPACE, 20.0);
        assert!(
            Arc::ptr_eq(&runtime.diff_model().unwrap().before, &baseline),
            "zoom scales existing logical widths"
        );
        let anchor = runtime.current_diff_viewport();
        runtime.set_editor_font(
            Font {
                family: iced::font::Family::SansSerif,
                ..Font::MONOSPACE
            },
            20.0,
        );
        assert!(
            runtime.diff_model().is_none(),
            "another family needs native width measurement"
        );
        assert_eq!(runtime.current_diff_viewport().row, anchor.row);
        drop(runtime.refresh_diff_preferences(card));
        assert!(matches!(
            runtime.diff_state(&runtime.diff_basis),
            Some(DiffState::Loading { .. })
        ));
    }

    #[test]
    fn a_rebuild_that_removes_the_anchored_line_keeps_visible_rows_mounted() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.diff_viewport.height = 300.0;
        let large: String = (1..=200).map(|line| format!("line {line}\n")).collect();
        load_fresh_diff(&mut runtime, card, crate::diff_view::build_diff("", &large));
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: 150.0 * DIFF_ROW_HEIGHT,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));

        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff("", "only\n"))),
        );
        assert_eq!(runtime.diff_viewport.offset, 0.0);
        let visible = visible_diff_rows(runtime.diff_rows().unwrap(), runtime.diff_viewport);
        assert_eq!((visible.start, visible.end), (0, 1));
    }

    #[test]
    fn change_navigation_realizes_far_rows_before_programmatic_scroll() {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        runtime.diff_viewport.height = 720.0;
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(staged_text_diff(DiffModel {
                line_counts: [0; 2],
                present: [true; 2],
                before: Arc::from(""),
                after: Arc::from(""),
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
                unchanged: Default::default(),
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
        runtime
            .editor_text()
            .expect("test expects an editable buffer")
    }

    fn document_revision(runtime: &RepositoryRuntime) -> u64 {
        match &runtime.document {
            DocumentState::Editable(document) => document.revision,
            _ => panic!("test expects an editable buffer"),
        }
    }

    #[test]
    fn editor_diagnostic_marks_map_unicode_and_bare_carriage_return_positions() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "α\rbeta\nsecond\n");
        runtime.set_editor_diagnostics(vec![crate::lsp::LspDiagnostic {
            range: LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 6)),
            severity: crate::lsp::LspSeverity::Error,
            message: "bad beta".into(),
            source: None,
            code: None,
        }]);
        assert_eq!(runtime.language.marks.len(), 1);
        assert_eq!(
            runtime.language.marks[0].start,
            text_editor::Position { line: 1, column: 0 }
        );
        assert_eq!(
            runtime.language.marks[0].end,
            text_editor::Position { line: 1, column: 4 }
        );
        assert_eq!(runtime.language.diagnostics.len(), 1);
        runtime.set_lsp_document(Some(LspDocumentId(1)));
        assert!(
            runtime.language.marks.is_empty(),
            "a new document cannot retain the old document's ranges"
        );
    }

    #[test]
    fn completion_keyboard_navigation_wraps_and_edits_dismiss_the_popup() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "hello\n");
        let item = completion_item(
            completion_edit(
                LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 5)),
                "world",
            ),
            vec![],
        );
        runtime.set_editor_completions(vec![item.clone(), item]);
        drop(runtime.update(RepositoryAction::Language {
            card,
            action: EditorLspAction::Next(false),
        }));
        assert_eq!(runtime.language.selected, 1);
        drop(runtime.update(RepositoryAction::Language {
            card,
            action: EditorLspAction::Next(true),
        }));
        assert_eq!(runtime.language.selected, 0);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
        }));
        assert!(runtime.language.completions.is_empty());
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

    /// A row that is a symbolic link opens the file behind it: the buffer is
    /// that file's bytes, the language server is told about that file, the
    /// chrome names it, and a save goes there rather than over the link.
    #[test]
    fn a_link_opens_edits_and_saves_its_target() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "AGENTS.md");
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "CLAUDE.md".into(),
                content: FileContent::Editable {
                    text: "# Guidance\n".into(),
                    content_hash: blake3::hash(b"# Guidance\n"),
                    markdown: true,
                },
            }),
        }));
        assert_eq!(runtime.selected_path(), Some(Path::new("AGENTS.md")));
        assert_eq!(runtime.linked_source(), Some(Path::new("CLAUDE.md")));
        assert!(matches!(
            runtime.take_lsp_intents().as_slice(),
            [LspIntent::Open { absolute_path, .. }] if absolute_path == Path::new("/repo/CLAUDE.md")
        ));

        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.request_save(card, false));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("the buffer stays editable while it saves");
        };
        let ticket = document
            .active_save
            .as_ref()
            .expect("a dirty buffer starts a save");
        assert_eq!(
            ticket.identity.path,
            Path::new("AGENTS.md"),
            "the save is matched back to the row that was opened"
        );
        assert_eq!(
            document.source,
            Path::new("CLAUDE.md"),
            "and its bytes go to the file the link resolves to"
        );

        let (mut plain, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "plain\n");
        assert_eq!(plain.linked_source(), None);
        drop(plain.take_lsp_intents());
    }

    /// The row stays the link. The comparison, its baseline, and staging
    /// follow the target the buffer was read from.
    #[test]
    fn a_linked_buffer_compares_and_stages_the_target() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "AGENTS.md");
        runtime.files = FileListState::Ready(vec![
            RepositoryFile {
                path: "AGENTS.md".into(),
                staged: true,
                status: Some("M ".into()),
                ..RepositoryFile::default()
            },
            RepositoryFile {
                path: "CLAUDE.md".into(),
                unstaged: true,
                unstaged_old_path: Some("notes.md".into()),
                status: Some(" M".into()),
                ..RepositoryFile::default()
            },
        ]);
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "CLAUDE.md".into(),
                content: FileContent::Editable {
                    text: "new\n".into(),
                    content_hash: blake3::hash(b"new\n"),
                    markdown: false,
                },
            }),
        }));
        assert_eq!(runtime.selected_path(), Some(Path::new("AGENTS.md")));
        assert_eq!(
            runtime.diff_pathspec().as_deref(),
            Some(Path::new("CLAUDE.md")),
            "Git is asked for the file the buffer is, not the link row"
        );
        assert_eq!(
            runtime.comparison_old_path().as_deref(),
            Some(Path::new("notes.md")),
            "the old side is the target's rename, which is the index baseline"
        );
        let selection = runtime
            .selected_staging_selection(true)
            .expect("the target has unstaged changes to stage");
        assert_eq!(
            selection.path.as_path(),
            Path::new("CLAUDE.md"),
            "staging names the target"
        );
        assert_eq!(
            selection.old_path.as_ref().map(|path| path.as_path()),
            Some(Path::new("notes.md"))
        );
        assert!(
            runtime.selected_staging_selection(false).is_none(),
            "the link's own staged bit must not unstage the link"
        );
        assert!(runtime.can_show_diff());

        // A target with nothing to stage hides the command rather than
        // acting on the link entry.
        runtime.files = FileListState::Ready(vec![RepositoryFile {
            path: "AGENTS.md".into(),
            unstaged: true,
            status: Some(" M".into()),
            ..RepositoryFile::default()
        }]);
        assert!(runtime.selected_staging_selection(true).is_none());
        assert!(runtime.selected_staging_selection(false).is_none());
    }

    /// A save through a link changes a row other than the selected one, so
    /// the list is asked again rather than the selected row marked modified.
    #[test]
    fn a_save_through_a_link_refreshes_the_list_instead_of_marking_the_link() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "AGENTS.md");
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: identity.clone(),
            result: Ok(LoadedFile {
                source: "CLAUDE.md".into(),
                content: FileContent::Editable {
                    text: "# Guidance\n".into(),
                    content_hash: blake3::hash(b"# Guidance\n"),
                    markdown: true,
                },
            }),
        }));
        drop(runtime.take_lsp_intents());
        if let Some(selected) = runtime.selected.as_mut() {
            selected.unstaged = false;
            selected.status = None;
        }
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.request_save(card, false));
        let ticket = match &runtime.document {
            DocumentState::Editable(document) => document.active_save.clone().unwrap(),
            _ => panic!("the buffer stays editable while it saves"),
        };
        drop(runtime.update(RepositoryAction::SaveFinished {
            identity,
            ticket: ticket.id,
            revision: ticket.revision,
            saved_text: ticket.snapshot.clone(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(ticket.snapshot.as_bytes()),
                durability: SaveDurability::Durable,
            }),
        }));
        let selected = runtime.selected.as_ref().unwrap();
        assert!(
            !selected.unstaged && selected.status.is_none(),
            "the link itself did not change: {selected:?}"
        );
        assert!(
            runtime.mutation_refresh_pending,
            "the row that did change is learned from Git"
        );
    }

    /// The notice names the link that actually failed, which in a chain is
    /// not the row, and the link's own target rather than the row's.
    #[test]
    fn a_refused_link_is_described_by_the_link_that_failed() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "to-dangling");
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "dangling".into(),
                content: FileContent::ReadOnly(ReadOnlyReason::Symlink {
                    target: "missing".into(),
                    refusal: LinkRefusal::Dangling,
                }),
            }),
        }));
        let DocumentState::ReadOnly { path, reason } = &runtime.document else {
            panic!("a refused link is read-only");
        };
        assert_eq!(
            read_only_detail(path, reason),
            "dangling is a symbolic link to missing, which does not exist."
        );
        assert_eq!(runtime.linked_source(), Some(Path::new("dangling")));
        assert_eq!(
            read_only_detail(
                Path::new("escape"),
                &ReadOnlyReason::Symlink {
                    target: "/etc/passwd".into(),
                    refusal: LinkRefusal::LeavesWorktree,
                }
            ),
            "escape is a symbolic link to /etc/passwd, which is outside the worktree or reached through another link."
        );
        assert_eq!(
            read_only_detail(
                Path::new("loop-a"),
                &ReadOnlyReason::Symlink {
                    target: "loop-b".into(),
                    refusal: LinkRefusal::TooManyLinks,
                }
            ),
            "loop-a is a symbolic link to loop-b, which leads through too many links to follow."
        );
    }

    #[test]
    fn a_read_only_or_unreadable_file_is_never_opened_at_the_boundary() {
        let card = CardId::new();
        for result in [
            Ok(LoadedFile {
                source: "notes.txt".into(),
                content: FileContent::ReadOnly(ReadOnlyReason::Binary),
            }),
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
            result: Ok(LoadedFile {
                source: "notes.txt".into(),
                content: FileContent::Image {
                    width: 1,
                    height: 1,
                    rgba: vec![255, 0, 0, 255].into(),
                },
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

    /// A card with `book.pdf` open at its first page.
    fn document_runtime(card: CardId, pages: usize) -> RepositoryRuntime {
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "book.pdf");
        // What `open_file` records before dispatching the read, so the load
        // lands on the tone the test process's theme asks for and does not
        // immediately reconcile itself.
        runtime.document_tone = pdf_tone(&cosmic::theme::active());
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "book.pdf".into(),
                content: FileContent::Pdf {
                    pages,
                    page: rendered_document_page(0, pages).page,
                },
            }),
        }));
        runtime
    }

    fn rendered_document_page(index: usize, pages: usize) -> crate::editor::DocumentPage {
        crate::editor::DocumentPage {
            pages,
            page: crate::editor::PdfPage {
                index,
                width: 2,
                height: 2,
                points_wide: 612.0,
                rgba: vec![255; 16].into(),
            },
        }
    }

    /// The page, tone, in-flight request and message the viewer is showing.
    fn document_view_state(
        runtime: &RepositoryRuntime,
    ) -> (
        usize,
        crate::editor::PdfTone,
        Option<PdfView>,
        Option<String>,
    ) {
        let DocumentState::Pdf {
            page,
            tone,
            rendering,
            message,
            ..
        } = &runtime.document
        else {
            panic!("the fixture opens a document");
        };
        (
            *page,
            *tone,
            rendering.as_ref().map(|active| active.requested),
            message.clone(),
        )
    }

    /// The zoom the viewer is drawing the open page at.
    fn document_zoom(runtime: &RepositoryRuntime) -> crate::editor::PdfZoom {
        let DocumentState::Pdf { zoom, .. } = &runtime.document else {
            panic!("the fixture opens a document");
        };
        *zoom
    }

    /// The view a test expects to be rendering, at the zoom a document opens
    /// at.
    fn rendering_view(page: usize, tone: crate::editor::PdfTone) -> PdfView {
        PdfView {
            page,
            tone,
            zoom: crate::editor::PdfZoom::default(),
        }
    }

    /// A tone that is not `tone`, so a test can tell a restored tone from the
    /// one it asked for whichever theme the test process runs under.
    fn other_document_tone(tone: crate::editor::PdfTone) -> crate::editor::PdfTone {
        match tone {
            crate::editor::PdfTone::Original => crate::editor::PdfTone::Inverted {
                paper: [16, 16, 16],
            },
            crate::editor::PdfTone::Inverted { .. } => crate::editor::PdfTone::Original,
        }
    }

    #[test]
    fn page_turns_taken_during_a_render_cost_one_more_render_and_land_on_the_last_one() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 30);
        let tone = document_view_state(&runtime).1;
        for page in 1..=20 {
            drop(runtime.update(RepositoryAction::PdfPage {
                card,
                page,
                tone,
                zoom: crate::editor::PdfZoom::default(),
            }));
        }
        let first = runtime.pdf_request;
        assert_eq!(
            first, 1,
            "each render reparses the document and rasterizes a page, so twenty turns over one \
             render must dispatch one render, not twenty"
        );
        let (page, _, requested, _) = document_view_state(&runtime);
        assert_eq!(page, 20, "the counter follows the reader, not the renderer");
        assert_eq!(requested, Some(rendering_view(1, tone)));
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: first,
            result: Ok(rendered_document_page(1, 30)),
        }));
        assert_eq!(
            document_view_state(&runtime).2,
            Some(rendering_view(20, tone)),
            "the turn that was waiting is the last one the reader asked for"
        );
        assert_eq!(runtime.pdf_request, first + 1);
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: first + 1,
            result: Ok(rendered_document_page(20, 30)),
        }));
        let (page, _, requested, _) = document_view_state(&runtime);
        assert_eq!(page, 20, "the reader ends on the page they last asked for");
        assert_eq!(requested, None, "nothing is left queued or in flight");
        assert_eq!(runtime.pdf_request, first + 1);
    }

    #[test]
    fn a_zoom_the_reader_asks_for_renders_that_page_again_at_the_new_size() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 30);
        let (page, tone, requested, _) = document_view_state(&runtime);
        assert_eq!(requested, None, "the fixture opens with nothing in flight");
        let opened = document_zoom(&runtime);
        assert!(opened.is_fit(), "a document opens at the pane's width");
        let larger = opened.larger().expect("the default zoom can grow");
        drop(runtime.update(RepositoryAction::PdfZoom { card, zoom: larger }));
        assert_eq!(
            document_view_state(&runtime).2,
            Some(PdfView {
                page,
                tone,
                zoom: larger
            }),
            "a page drawn at one size and rendered for another is either soft or too large"
        );
        assert_eq!(document_zoom(&runtime), larger);
    }

    #[test]
    fn a_failed_zoom_leaves_the_controls_describing_the_size_still_drawn() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 30);
        let opened = document_zoom(&runtime);
        let larger = opened.larger().expect("the default zoom can grow");
        drop(runtime.update(RepositoryAction::PdfZoom { card, zoom: larger }));
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Err("this page could not be rendered".into()),
        }));
        assert_eq!(
            document_zoom(&runtime),
            opened,
            "the zoom control has to describe the page that is on screen"
        );
    }

    #[test]
    fn the_zoom_steps_stay_ordered_and_bounded() {
        let fit = crate::editor::PdfZoom::default();
        assert!(fit.is_fit(), "a document opens fitted to its pane");
        assert_eq!(
            fit.percent(),
            None,
            "a fitted page has no size of its own to report"
        );
        // Either control has to leave Fit for a size the reader can see is
        // different, which means skipping the printed size a fitted page is
        // usually already close to.
        assert_eq!(fit.smaller().and_then(|zoom| zoom.percent()), Some(75));
        assert_eq!(fit.larger().and_then(|zoom| zoom.percent()), Some(125));
        let mut zoom = fit.smaller().unwrap();
        while let Some(smaller) = zoom.smaller() {
            assert!(
                smaller.factor() < zoom.factor(),
                "stepping out has to draw the page smaller"
            );
            zoom = smaller;
        }
        assert_eq!(zoom.smaller(), None, "the smallest step offers no press");
        let mut zoom = fit.larger().unwrap();
        while let Some(larger) = zoom.larger() {
            assert!(
                larger.factor() > zoom.factor(),
                "stepping in has to draw the page larger"
            );
            zoom = larger;
        }
        assert_eq!(zoom.larger(), None, "the largest step offers no press");
        // A fitted page takes the pane's width, and every other step is the
        // size the page would print at times the step.
        assert_eq!(fit.logical_width(612.0), None);
        let printed = crate::editor::PdfZoom::Scaled(2);
        assert_eq!(printed.percent(), Some(100));
        // A point is 1/72 inch against a 96 pixel inch, so a 1000 point page
        // prints 1333.3 logical pixels wide however coarse or fine the raster
        // behind it turned out to be.
        let width = printed.logical_width(1000.0).unwrap();
        assert!(
            (width - 1333.33).abs() < 0.01,
            "{width} is not the width a 1000 point page prints at"
        );
        let doubled = crate::editor::PdfZoom::Scaled(5)
            .logical_width(1000.0)
            .unwrap();
        assert!(
            (doubled - 2.0 * width).abs() < 0.01,
            "{doubled} is not twice the printed width the 200% step promises"
        );
    }

    #[test]
    fn a_failed_page_render_puts_back_the_page_and_tone_that_are_still_drawn() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 30);
        let (drawn_page, drawn_tone, ..) = document_view_state(&runtime);
        drop(runtime.update(RepositoryAction::PdfPage {
            card,
            page: 7,
            tone: other_document_tone(drawn_tone),
            zoom: crate::editor::PdfZoom::default(),
        }));
        assert_eq!(document_view_state(&runtime).0, 7);
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Err("page 8 is too large to render".into()),
        }));
        let (page, tone, requested, message) = document_view_state(&runtime);
        assert_eq!(
            page, drawn_page,
            "the counter has to describe the page that is on screen"
        );
        assert_eq!(
            tone, drawn_tone,
            "the tone toggle has to describe the page that is on screen"
        );
        assert_eq!(requested, None);
        assert_eq!(message.as_deref(), Some("page 8 is too large to render"));
        drop(runtime.update(RepositoryAction::PdfPage {
            card,
            page: drawn_page + 1,
            tone: drawn_tone,
            zoom: crate::editor::PdfZoom::default(),
        }));
        assert_eq!(
            document_view_state(&runtime).2,
            Some(rendering_view(drawn_page + 1, drawn_tone)),
            "the next turn moves one page from the drawn one rather than skipping it"
        );
    }

    #[test]
    fn a_render_that_fails_under_a_queued_turn_still_takes_the_reader_there() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 30);
        let tone = document_view_state(&runtime).1;
        drop(runtime.update(RepositoryAction::PdfPage {
            card,
            page: 9,
            tone,
            zoom: crate::editor::PdfZoom::default(),
        }));
        let failing = runtime.pdf_request;
        drop(runtime.update(RepositoryAction::PdfPage {
            card,
            page: 4,
            tone,
            zoom: crate::editor::PdfZoom::default(),
        }));
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: failing,
            result: Err("page 10 is too large to render".into()),
        }));
        let (page, _, requested, message) = document_view_state(&runtime);
        assert_eq!(
            requested,
            Some(rendering_view(4, tone)),
            "the queued turn still happens"
        );
        assert_eq!(page, 4);
        assert_eq!(
            message, None,
            "a failure the reader has already turned away from is not worth a notice"
        );
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Ok(rendered_document_page(4, 30)),
        }));
        assert_eq!(document_view_state(&runtime).0, 4);
    }

    /// A shell that changes under an open document leaves the page rendered
    /// for the other one, so the page is rendered again exactly once, and a
    /// theme that asks for the tone already drawn asks for no render at all.
    #[test]
    fn a_shell_theme_change_renders_the_open_page_again_for_the_new_theme() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 4);
        // The tone the open baked, which is the one the test process's theme
        // asks for, so nothing is stale before the shell changes.
        let opened = runtime.document_tone;
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Ok(rendered_document_page(0, 4)),
        }));
        assert_eq!(document_view_state(&runtime).1, opened);
        let dispatched = runtime.pdf_request;
        // The shell the open was not rendered for. Which of the two it is
        // follows from the theme the test process runs under, so the other
        // one is chosen rather than named.
        let (other, other_tone) = [cosmic::Theme::dark(), cosmic::Theme::light()]
            .into_iter()
            .map(|theme| (theme.clone(), pdf_tone(&theme)))
            .find(|(_, tone)| *tone != opened)
            .expect("the two shells ask for different tones");
        drop(runtime.retone_document(card, &other));
        let (_, tone, requested, _) = document_view_state(&runtime);
        assert_eq!(
            tone, other_tone,
            "a page rendered for one shell is wrong on the other"
        );
        assert_eq!(requested, Some(rendering_view(0, other_tone)));
        assert_eq!(runtime.pdf_request, dispatched + 1);
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Ok(rendered_document_page(0, 4)),
        }));
        drop(runtime.retone_document(card, &other));
        assert_eq!(
            runtime.pdf_request,
            dispatched + 1,
            "a theme that leaves the tone alone must not rasterize the page again"
        );
    }

    #[test]
    fn a_theme_change_while_the_first_page_rendered_is_reconciled_when_it_arrives() {
        let card = CardId::new();
        let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "book.pdf");
        let shell = pdf_tone(&cosmic::theme::active());
        // The load was dispatched under the other shell and the reader changed
        // it while the first page rasterized, when there was no page to
        // re-render.
        runtime.document_tone = other_document_tone(shell);
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity,
            result: Ok(LoadedFile {
                source: "book.pdf".into(),
                content: FileContent::Pdf {
                    pages: 4,
                    page: rendered_document_page(0, 4).page,
                },
            }),
        }));
        let (page, tone, requested, _) = document_view_state(&runtime);
        assert_eq!(page, 0);
        assert_eq!(
            tone, shell,
            "the page the load baked for the old shell has to be rendered again for this one"
        );
        assert_eq!(requested, Some(rendering_view(0, shell)));
    }

    /// A reader who turns a page back to its own colours did so to read it.
    /// The shell changing underneath is no reason to take that away, and it
    /// is the only thing that stops following the theme being right.
    #[test]
    fn a_tone_the_reader_chose_outlives_a_theme_change() {
        let card = CardId::new();
        let mut runtime = document_runtime(card, 4);
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Ok(rendered_document_page(0, 4)),
        }));
        let chosen = other_document_tone(runtime.document_tone);
        drop(runtime.update(RepositoryAction::PdfTone { card, tone: chosen }));
        drop(runtime.update(RepositoryAction::PdfPageRendered {
            card,
            request: runtime.pdf_request,
            result: Ok(rendered_document_page(0, 4)),
        }));
        let settled = runtime.pdf_request;
        assert_eq!(document_view_state(&runtime).1, chosen);
        for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
            drop(runtime.retone_document(card, &theme));
        }
        assert_eq!(
            runtime.pdf_request, settled,
            "neither shell may rasterize over the tone the reader asked for"
        );
        assert_eq!(document_view_state(&runtime).1, chosen);
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
            result: Ok(LoadedFile {
                source: "notes.txt".into(),
                content: FileContent::Editable {
                    text: "fn main() {}\n".into(),
                    content_hash: blake3::hash(b"fn main() {}\n"),
                    markdown: false,
                },
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
    fn select_text_selects_first_match_and_clears_missing_matches() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "Alpha beta ALPHA");

        assert!(runtime.select_text("BETA"));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        let cursor = document.content.cursor();
        assert_eq!(
            cursor.position,
            text_editor::Position {
                line: 0,
                column: 10,
            }
        );
        assert_eq!(
            cursor.selection,
            Some(text_editor::Position { line: 0, column: 6 })
        );

        assert_eq!(runtime.select_text_match("alpha", 1), Some(2));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        let cursor = document.content.cursor();
        assert_eq!(
            cursor.position,
            text_editor::Position {
                line: 0,
                column: 16,
            }
        );
        assert_eq!(
            cursor.selection,
            Some(text_editor::Position {
                line: 0,
                column: 11,
            })
        );

        assert!(!runtime.select_text("missing"));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(document.content.cursor().selection, None);
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
    fn registered_diagnostic_bindings_follow_remapping_and_clearing() {
        use crate::workspace_widget::GlobalShortcut as Command;
        use iced::keyboard::{Key, key::{Named, Physical, NativeCode}};
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "source");
        let press = |shift| iced::widget::text_editor::KeyPress {
            key: Key::Named(Named::F8), modified_key: Key::Named(Named::F8),
            physical_key: Physical::Unidentified(NativeCode::Unidentified),
            modifiers: if shift { iced::keyboard::Modifiers::SHIFT } else { iced::keyboard::Modifiers::empty() },
            text: None, status: iced::widget::text_editor::Status::Focused { is_hovered: false },
        };
        for (shift, forward) in [(false, true), (true, false)] {
            assert!(matches!(registered_editor_binding(card, &runtime, &press(shift)), Some(Some(text_editor::Binding::Custom(RepositoryAction::Language { card: id, action: EditorLspAction::Diagnostic(direction) }))) if id == card && direction == forward));
        }
        let mut shortcuts = crate::settings::KeyboardShortcuts::default();
        shortcuts.next_diagnostic.clear();
        shortcuts.previous_diagnostic = vec!["Ctrl+Alt+F8".into()];
        runtime.set_keyboard_shortcuts(Arc::new(shortcuts));
        assert!(registered_editor_binding(card, &runtime, &press(false)).is_none());
        assert!(registered_editor_binding(card, &runtime, &press(true)).is_none());
        let mut remapped = press(false);
        remapped.modifiers = iced::keyboard::Modifiers::CTRL | iced::keyboard::Modifiers::ALT;
        assert!(matches!(registered_editor_binding(card, &runtime, &remapped), Some(Some(text_editor::Binding::Custom(RepositoryAction::Language { action: EditorLspAction::Diagnostic(false), .. })))));
        let descriptor = crate::workspace_widget::shortcut_descriptor(Command::NextDiagnostic);
        let mut context = crate::workspace_widget::CommandContext { focused: crate::workspace_widget::FocusedPane::Editor, ..Default::default() };
        assert!(!descriptor.available(&context));
        context.can_navigate_diagnostics = true;
        assert!(descriptor.available(&context));
        context.focused = crate::workspace_widget::FocusedPane::TextInput;
        assert!(!descriptor.available(&context));
    }

    #[test]
    fn registry_external_editor_template_preserves_the_requested_editor() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "source");
        assert!(!runtime.external_editor_command_available());
        runtime.set_external_editors(["first", "chosen"].into_iter().map(|id| crate::sidebar::SidebarEditor { id: id.into(), name: id.into(), icon: "editor".into() }).collect());
        let command = ExternalEditorCommand(card, "chosen");
        assert_eq!(crate::workspace_widget::shortcut_descriptor(command.command()).id(), "open_in_external_editor");
        assert!(matches!(runtime.external_editor_command(card, Some("chosen")), Some(RepositoryAction::OpenExternal { card: id, editor_id }) if id == card && editor_id == "chosen"));
        assert!(matches!(runtime.external_editor_command(card, None), Some(RepositoryAction::OpenExternal { editor_id, .. }) if editor_id == "first"));
        assert!(runtime.external_editor_command(card, Some("removed")).is_none());
    }

    #[test]
    fn registry_editor_menu_commands_share_native_actions_and_capabilities() {
        use crate::workspace_widget::GlobalShortcut as Command;
        use widget::menu::Action as _;
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "original");
        let commands = [EditorMenuCommand::Undo(card), EditorMenuCommand::Redo(card), EditorMenuCommand::Clipboard(card, ClipboardOp::Cut), EditorMenuCommand::Clipboard(card, ClipboardOp::Copy), EditorMenuCommand::Clipboard(card, ClipboardOp::Paste), EditorMenuCommand::Clipboard(card, ClipboardOp::SelectAll)];
        for command in commands {
            assert_eq!(crate::workspace_widget::shortcut_descriptor(command.command()).id(), command.command().id());
            assert!(editor_command_action(card, command.command()).is_some());
        }
        assert_eq!(runtime.editor_command_capabilities() & 15, 0);
        drop(runtime.update(RepositoryAction::EditorAction { card, action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("changed".into()))) }));
        let changed = buffer_text(&runtime);
        assert_ne!(changed, "original");
        assert_ne!(runtime.editor_command_capabilities() & 1, 0);
        drop(runtime.update(EditorMenuCommand::Undo(card).message()));
        assert_eq!(buffer_text(&runtime), "original");
        assert_ne!(runtime.editor_command_capabilities() & 2, 0);
        drop(runtime.update(editor_command_action(card, Command::EditorRedo).unwrap()));
        assert_eq!(buffer_text(&runtime), changed);
        drop(runtime.update(editor_command_action(card, Command::EditorSelectAll).unwrap()));
        assert_ne!(runtime.editor_command_capabilities() & 12, 0);
        assert_eq!(runtime.selected_text().as_deref(), Some(changed.as_str()));
        runtime.set_search_mode(true);
        assert_eq!(runtime.editor_command_capabilities() & (1|2|4|16), 0);
        assert_ne!(runtime.editor_command_capabilities() & 8, 0);
    }

    #[test]
    fn registered_editor_bindings_ignore_inactive_native_status() {
        use iced::keyboard::{Key, key::{Code, Physical}};
        let card = CardId::new();
        let (runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "original");
        for (key, code, modifiers) in [("z", Code::KeyZ, iced::keyboard::Modifiers::CTRL), ("c", Code::KeyC, iced::keyboard::Modifiers::CTRL), ("a", Code::KeyA, iced::keyboard::Modifiers::CTRL)] {
            let press = iced::widget::text_editor::KeyPress { key: Key::Character(key.into()), modified_key: Key::Character(key.into()), physical_key: Physical::Code(code), modifiers, text: None, status: iced::widget::text_editor::Status::Active };
            assert!(registered_editor_binding(card, &runtime, &press).is_none());
        }
        let press = iced::widget::text_editor::KeyPress { key: Key::Named(iced::keyboard::key::Named::F8), modified_key: Key::Named(iced::keyboard::key::Named::F8), physical_key: Physical::Code(Code::F8), modifiers: iced::keyboard::Modifiers::empty(), text: None, status: iced::widget::text_editor::Status::Active };
        assert!(registered_editor_binding(card, &runtime, &press).is_none());
    }

    #[test]
    fn mounted_editors_keep_sidebar_chords_and_dispatch_only_the_native_focus_owner() {
        use iced::{Event, Rectangle, Size};
        use iced::advanced::{Layout, Shell, clipboard, mouse};
        use iced::keyboard::{Key, key::{Code, Physical}};
        #[derive(Debug, Clone)]
        enum Observation { Editor(RepositoryAction), Sidebar(String) }
        let first = CardId::new();
        let second = CardId::new();
        let (first_runtime, _) = RepositoryRuntime::lsp_open_fixture(first, "first");
        let (second_runtime, _) = RepositoryRuntime::lsp_open_fixture(second, "second");
        let sidebar = widget::Id::new("registry.sidebar");
        let first_focus = first_runtime.editor_focus_target(first).unwrap();
        let second_focus = second_runtime.editor_focus_target(second).unwrap();
        let mut element: Element<'_, Observation> = widget::row::with_children(vec![
            widget::container(editor_surface(first, &first_runtime).map(Observation::Editor)).width(320).height(200).into(),
            widget::container(editor_surface(second, &second_runtime).map(Observation::Editor)).width(320).height(200).into(),
            widget::text_input("Sidebar", "query").id(sidebar.clone()).on_input(Observation::Sidebar).width(240).into(),
        ]).into();
        let renderer = headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(1000.0, 240.0));
        let node = element.as_widget_mut().layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, viewport.size()));
        for (target, expected) in [(sidebar, None), (first_focus, Some(first)), (second_focus, Some(second))] {
            let mut focus = iced::advanced::widget::operation::focusable::focus::<()>(target);
            element.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
            let event = Event::Keyboard(iced::keyboard::Event::KeyPressed { key: Key::Character("z".into()), modified_key: Key::Character("z".into()), physical_key: Physical::Code(Code::KeyZ), location: iced::keyboard::Location::Standard, modifiers: iced::keyboard::Modifiers::CTRL, text: None, repeat: false });
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(&mut tree, &event, Layout::new(&node), mouse::Cursor::Unavailable, &renderer, &mut clipboard::Null, &mut shell, &viewport);
            let actions: Vec<_> = messages.iter().filter_map(|message| match message { Observation::Editor(RepositoryAction::Undo { card }) => Some(*card), _ => None }).collect();
            assert_eq!(actions, expected.into_iter().collect::<Vec<_>>());
            assert!(!messages.iter().any(|message| matches!(message, Observation::Editor(action) if !matches!(action, RepositoryAction::Undo { .. }))), "unfocused editors must not publish another edit action");
        }
        assert_eq!(first_runtime.editor_text(), Some("first".into()));
        assert_eq!(second_runtime.editor_text(), Some("second".into()));
    }

    #[test]
    fn registry_editor_remapping_retires_native_chords_without_touching_other_keys() {
        use iced::keyboard::{Key, key::{Code, Physical}};
        use crate::workspace_widget::GlobalShortcut as Command;
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "original");
        let mut shortcuts = crate::settings::KeyboardShortcuts::default();
        shortcuts.editor_undo = vec!["Ctrl+Alt+K".into()];
        shortcuts.editor_copy.clear();
        runtime.set_keyboard_shortcuts(Arc::new(shortcuts));
        let press = |key: &str, code, modifiers| iced::widget::text_editor::KeyPress { key: Key::Character(key.into()), modified_key: Key::Character(key.into()), physical_key: Physical::Code(code), modifiers, text: None, status: iced::widget::text_editor::Status::Focused { is_hovered: false } };
        assert!(matches!(registered_editor_binding(card, &runtime, &press("z", Code::KeyZ, iced::keyboard::Modifiers::CTRL)), Some(None)));
        assert!(matches!(registered_editor_binding(card, &runtime, &press("c", Code::KeyC, iced::keyboard::Modifiers::CTRL)), Some(None)));
        assert!(matches!(registered_editor_binding(card, &runtime, &press("k", Code::KeyK, iced::keyboard::Modifiers::CTRL|iced::keyboard::Modifiers::ALT)), Some(Some(text_editor::Binding::Custom(RepositoryAction::Undo { card: id }))) if id == card));
        assert!(registered_editor_binding(card, &runtime, &press("a", Code::KeyA, iced::keyboard::Modifiers::empty())).is_none());
        assert_eq!(crate::workspace_widget::shortcut_descriptor(Command::EditorCopy).category(), "Editor");
    }

    #[test]
    fn typing_without_an_attached_server_never_serializes_content() {
        let card = CardId::new();
        let source = "let value = 1;\n".repeat(50_000);
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "huge.rs", &source);
        drop(runtime.take_lsp_intents());
        text_editor::take_text_call_count();
        for character in "changed".chars() {
            drop(runtime.update(RepositoryAction::EditorAction { card,action:text_editor::Action::Edit(text_editor::Edit::Insert(character)) }));
            assert!(runtime.take_lsp_intents().is_empty());
        }
        assert!(runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Undo {card}));
        assert!(runtime.take_lsp_intents().is_empty());
        assert_eq!(text_editor::take_text_call_count(), 0);
    }

    #[test]
    fn diagnostic_navigation_wraps_and_displays_the_message() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "first\nsecond\nthird\n");
        runtime.set_editor_diagnostics(vec![
            crate::lsp::LspDiagnostic { range:LspRange::new(LspPosition::new(2,0),LspPosition::new(2,3)),severity:crate::lsp::LspSeverity::Error,message:"third error".into(),source:None,code:None },
            crate::lsp::LspDiagnostic { range:LspRange::new(LspPosition::new(1,0),LspPosition::new(1,3)),severity:crate::lsp::LspSeverity::Warning,message:"second warning".into(),source:None,code:None },
        ]);
        runtime.move_cursor_to(LspPosition::new(0,0));
        runtime.navigate_diagnostic(true);
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(1,0)));
        assert!(!runtime.language.hover.is_empty());
        runtime.navigate_diagnostic(true);
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(2,0)));
        runtime.navigate_diagnostic(true);
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(1,0)));
        runtime.navigate_diagnostic(false);
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(2,0)));
    }

    #[test]
    fn a_completion_is_one_undo_step_that_restores_the_caret() {
        let card = CardId::new();
        let original = "use std::fmt;\n\nfn main() {\n    prin\n}\n";
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, original);
        drop(runtime.take_lsp_intents());
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::Down),
        }));
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
        let caret = runtime.cursor_position();
        assert!(runtime.apply_completion(&item));
        drop(runtime.take_lsp_intents());

        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(
            buffer_text(&runtime),
            original,
            "both edits come back as one"
        );
        assert_eq!(runtime.cursor_position(), caret);
        assert!(!runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(
            buffer_text(&runtime),
            "use std::fmt;\nuse std::io;\n\nfn main() {\n    println!()\n}\n"
        );
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(4, 14)));
    }

    #[test]
    fn undo_and_redo_step_through_typing_and_report_exact_edits() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
        // Scheduler pauses in a loaded parallel suite must not become a user
        // typing pause. Explicit idle-boundary coverage lives in editor_history.
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!("editable fixture");
        };
        document.history_time = Some(Instant::now());
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        for character in ['a', 'b'] {
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
            }));
        }
        drop(runtime.take_lsp_intents());
        drop(runtime.take_lsp_edits());
        let revision = document_revision(&runtime);

        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "fn main() {}\n");
        assert!(
            !runtime.is_dirty(),
            "undoing to the saved text is not a modification"
        );
        assert_ne!(
            document_revision(&runtime),
            revision,
            "autosave must see the change"
        );
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 0)));
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version: 4,
                text: "fn main() {}\n".into(),
            }]
        );
        assert_eq!(
            runtime.take_lsp_edits(),
            vec![LspTextEdit {
                range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 2)),
                new_text: String::new(),
            }]
        );

        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(buffer_text(&runtime), "abfn main() {}\n");
        assert!(runtime.is_dirty());
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 2)));

        // Nothing left to redo, and undo with nothing to undo, are no-ops the
        // server never hears about.
        drop(runtime.take_lsp_intents());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(runtime.take_lsp_intents(), Vec::new());
    }

    #[test]
    fn discarding_unsaved_edits_can_itself_be_undone() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "one\ntwo\n");
        drop(runtime.take_lsp_intents());
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("zero\n".into()))),
        }));
        assert!(runtime.is_dirty());

        drop(runtime.update(RepositoryAction::DiscardEdits { card }));
        assert_eq!(buffer_text(&runtime), "one\ntwo\n");
        assert!(!runtime.is_dirty());

        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(
            buffer_text(&runtime),
            "zero\none\ntwo\n",
            "a discard is a step like any other, so the edits it dropped come back"
        );
        assert!(runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "one\ntwo\n");
        drop(runtime.update(RepositoryAction::Redo { card }));
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(
            buffer_text(&runtime),
            "one\ntwo\n",
            "redo replays the discard too"
        );
    }

    #[test]
    fn undo_and_redo_work_while_find_stays_open() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha\n");
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
        }));
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "alpha\n");
        assert!(runtime.find_is_open());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(buffer_text(&runtime), "zalpha\n");
        assert!(runtime.find_is_open());
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
                head: head_oid(root.path()),
                merge_base: head_oid(root.path()),
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
        let _ = diff_surface(card, &runtime, None);

        // The same surface with nothing to offer still says what is wrong.
        runtime.basis_resolution = Some(BasisResolution::NeedsBase(Vec::new()));
        let _ = diff_surface(card, &runtime, None);
        assert_eq!(
            runtime.inspector_context().diff_state,
            Some("Choose a base")
        );

        runtime.basis_resolution = Some(BasisResolution::NeedsBase(vec![candidate.clone()]));
        let basis = DiffBasis::MergeBaseToHead {
            base_ref: candidate,
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
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
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
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
            DiffState::ready(staged_text_diff(model_with(1))),
        );
        runtime.source_surface = SourceSurface::Diff;

        for resolution in [
            BasisResolution::Resolving(CommittedChoice::Branch),
            // No candidates at all, so even the ref selector offers nothing.
            BasisResolution::NeedsBase(Vec::new()),
            BasisResolution::NoCommits,
        ] {
            runtime.basis_resolution = Some(resolution.clone());
            // The diff the user was reading is still cached, and the basis
            // never moved - so changing basis back is a real escape.
            assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
            assert!(matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Ready(RenderedDiff::Text { .. }, _))
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
            runtime.set_diff_state(basis, DiffState::ready(staged_text_diff(model_with(1))));
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
                    .filter(|(basis, _)| is_committed_choice(basis))
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
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
        };
        let pull_request = DiffBasis::PullRequest {
            number: 7,
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
        };
        let labels = [
            &DiffBasis::IndexToWorktree,
            &DiffBasis::HeadToIndex,
            &branch,
            &commit,
            &root_commit,
            &pull_request,
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
                "#7 vs main".to_owned(),
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
        assert_eq!(basis_choice_label(&pull_request), "Pull request");
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
            DiffState::ready(RenderedDiff::NoText(DiffContent::Unmerged {
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
        let _ = diff_surface(card, &runtime, None);

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
                DiffState::ready(RenderedDiff::NoText(DiffContent::Comparison {
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
            let _ = diff_surface(card, &runtime, None);
        }
    }

    /// A runtime with one selected file that has both working halves and the
    /// diff surface showing, which is what every basis fence needs to be
    /// observable.
    fn diff_runtime(card: CardId) -> (RepositoryRuntime, AsyncIdentity) {
        let mut runtime = RepositoryRuntime::new(RepositoryView {
            selected_file: None,
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
            directory: false,
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
            model: Arc::new(model),
            old_label: "HEAD".into(),
            new_label: "Index".into(),
        }
    }

    fn model_with(additions: u32) -> DiffModel {
        DiffModel {
            line_counts: [0; 2],
            present: [true; 2],
            before: Arc::from(""),
            after: Arc::from(""),
            inline_rows: Vec::new(),
            split_rows: Vec::new(),
            inline_change_starts: Vec::new(),
            split_change_starts: Vec::new(),
            unchanged: Default::default(),
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

    /// Git reports a symlink as the link itself. The buffer behind that link
    /// is the target's text, and that text is what a comparison has to read
    /// — including the index side a gutter tint is measured from.
    #[test]
    fn a_symlink_comparison_reads_the_target_text() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        git_fixture(root.path());
        std::fs::write(root.path().join("CLAUDE.md"), "old\n").unwrap();
        symlink("CLAUDE.md", root.path().join("AGENTS.md")).unwrap();
        run_git(root.path(), ["add", "CLAUDE.md", "AGENTS.md"]);
        run_git(root.path(), ["commit", "-qm", "add both"]);
        std::fs::write(root.path().join("CLAUDE.md"), "new\n").unwrap();
        let token = CancellationToken::new();
        let link = load_repository_diff(
            root.path(),
            Path::new("AGENTS.md"),
            &DiffBasis::IndexToWorktree,
            None,
            crate::settings::EditorSyntaxTheme::Auto,
            &token,
        );
        assert!(
            matches!(link, RenderedDiff::NoText(_)),
            "the link row is not a text diff: {link:?}"
        );
        let target = load_repository_diff(
            root.path(),
            Path::new("CLAUDE.md"),
            &DiffBasis::IndexToWorktree,
            None,
            crate::settings::EditorSyntaxTheme::Auto,
            &token,
        );
        let RenderedDiff::Text { model, .. } = target else {
            panic!("the target is a text diff: {target:?}");
        };
        assert_eq!(
            model.before.as_ref(),
            "old\n",
            "the baseline is the target's index content"
        );
        assert!(model.additions >= 1);
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

    #[test]
    fn wrapping_reflows_an_open_dirty_buffer_without_changing_its_text() {
        let card = CardId::new();
        let text = format!("{}\nsecond line", "x".repeat(256));
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &text);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
        }));
        let edited = match &runtime.document {
            DocumentState::Editable(document) => document.content.text(),
            _ => panic!("fixture must be editable"),
        };
        let renderer = headless_renderer();
        let mut tree = Tree::new(editor_surface(card, &runtime).as_widget());

        // Hit below the first visual row. Without wrapping it lands on the
        // second logical line; with wrapping it is still inside the long token.
        // Toggling back exercises reflow of an already mounted editor.
        for (wrap, expected_line) in [(false, 1), (true, 0), (false, 1)] {
            let cursor = runtime.cursor_position();
            runtime.set_wrap_lines(wrap);
            assert_eq!(runtime.cursor_position(), cursor);
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
            }));
            {
                let mut element = editor_surface(card, &runtime);
                tree.diff(element.as_widget_mut());
                element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
                );
            }
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Click(iced::Point::new(20.0, 40.0)),
            }));
            assert_eq!(runtime.cursor_position().unwrap().line, expected_line);
            let DocumentState::Editable(document) = &runtime.document else {
                panic!("changing presentation must keep the document open");
            };
            assert_eq!(document.content.text(), edited);
            assert!(document.dirty);
        }
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
            selected_file: None,
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
            directory: false,
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
            result: Ok(LoadedFile {
                source: "picture.png".into(),
                content: FileContent::Image {
                    width: 1,
                    height: 1,
                    rgba: vec![200, 20, 40, 255].into(),
                },
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

    /// With nothing selected the card reports its file-listing state instead
    /// of a second browser. Each state renders its own surface, so a stuck
    /// or failed list cannot hide behind "no file selected" — and the
    /// failure surface keeps the retry that re-arms the load.
    #[test]
    fn empty_selection_reports_file_listing_state() {
        let card = CardId::new();
        let view = RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let render = |files: FileListState| {
            let mut runtime = RepositoryRuntime::new(view.clone());
            runtime.files = files;
            rendered_card(card, &runtime, (520, 200))
        };
        let loading = render(FileListState::Loading);
        let failed = render(FileListState::Failed("status failed".into()));
        let ready = render(FileListState::Ready(Vec::new()));
        for (name, frame) in [
            ("loading", &loading),
            ("failed", &failed),
            ("ready", &ready),
        ] {
            assert!(
                frame.iter().any(|&channel| channel != 0),
                "{name} rendered nothing at all, so the comparisons below would pass vacuously"
            );
        }
        assert_ne!(loading, failed, "a failed list renders as loading");
        assert_ne!(failed, ready, "a failed list renders as no selection");
        assert_ne!(loading, ready, "a loading list renders as no selection");

        let mut runtime = RepositoryRuntime::new(view);
        runtime.files = FileListState::Failed("status failed".into());
        drop(runtime.update(RepositoryAction::RefreshFiles { card }));
        assert!(
            matches!(runtime.files, FileListState::Loading),
            "the failure surface's retry must re-arm the file load"
        );
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
            let settings = highlighter_settings(
                Path::new(path),
                crate::settings::EditorSyntaxTheme::Auto,
                true,
            );
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
            highlighter_settings(
                Path::new("main.rs"),
                crate::settings::EditorSyntaxTheme::Auto,
                true,
            )
            .theme
            .is_dark(),
            "a dark application must highlight with a dark scheme"
        );
        assert!(
            !highlighter_settings(
                Path::new("main.rs"),
                crate::settings::EditorSyntaxTheme::Auto,
                false,
            )
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

    #[test]
    fn directory_editor_accepts_unlisted_nested_files_but_not_escaping_paths() {
        let root = tempfile::tempdir().unwrap();
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: root.path().to_path_buf(),
            worktree: root.path().to_path_buf(),
        });
        let nested = Path::new("nested/child.txt");
        assert!(runtime.can_open_file(nested));
        assert!(!runtime.can_open_file(Path::new("../outside.txt")));
        assert!(!runtime.can_open_file(Path::new("/outside.txt")));
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: nested.to_path_buf(),
        }));
        assert_eq!(runtime.selected_path(), Some(nested));
    }

    #[test]
    fn repository_browser_opens_ignored_files_without_a_git_listing_entry() {
        let root = tempfile::tempdir().unwrap();
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::file_list_fixture(
            RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: root.path().to_path_buf(),
                worktree: root.path().to_path_buf(),
            },
            &["tracked.rs"],
        );
        let path = RepoPath::parse("generated/nested/output.txt").unwrap();
        assert!(!runtime.can_open_file(path.as_path()));
        drop(runtime.update(RepositoryAction::OpenFileFromBrowser {
            card,
            path: path.clone(),
        }));
        assert_eq!(runtime.selected_path(), Some(path.as_path()));
        assert!(matches!(runtime.document, DocumentState::Loading));
        assert!(
            !runtime.selected.as_ref().unwrap().changed(),
            "ignored files are not working changes"
        );
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
    }

    #[test]
    fn ignored_folder_errors_preserve_other_entries_and_wait_for_explicit_retry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("good")).unwrap();
        std::fs::write(root.path().join("good/output.txt"), "generated").unwrap();
        // A folder can be replaced while its expansion is still mounted.
        std::fs::write(root.path().join("bad"), "replaced directory").unwrap();
        let bad = RepoPath::parse("bad").unwrap();
        let good = RepoPath::parse("good").unwrap();
        let expanded = vec![bad.clone(), good.clone()];
        let fresh_listing = || crate::repository::WorkingFiles {
            ignored_directories: expanded.clone(),
            ..Default::default()
        };
        let mut files = fresh_listing();
        expand_ignored_directories(
            root.path(),
            &mut files,
            &expanded,
            &BTreeMap::new(),
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(files.ignored_directory_errors.contains_key(&bad));
        assert!(
            files
                .ignored
                .contains(&RepoPath::parse("good/output.txt").unwrap())
        );
        let failures = files.ignored_directory_errors.clone();

        std::fs::remove_file(root.path().join("bad")).unwrap();
        std::fs::create_dir(root.path().join("bad")).unwrap();
        std::fs::write(root.path().join("bad/recovered.txt"), "recovered").unwrap();
        let mut files = fresh_listing();
        expand_ignored_directories(
            root.path(),
            &mut files,
            &expanded,
            &failures,
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(files.ignored_directory_errors, failures);
        assert!(
            !files
                .ignored
                .contains(&RepoPath::parse("bad/recovered.txt").unwrap()),
            "background refreshes must not retry failed folders"
        );

        let mut files = fresh_listing();
        expand_ignored_directories(
            root.path(),
            &mut files,
            &expanded,
            &BTreeMap::new(),
            &CancellationToken::default(),
        )
        .unwrap();
        assert!(files.ignored_directory_errors.is_empty());
        assert!(
            files
                .ignored
                .contains(&RepoPath::parse("bad/recovered.txt").unwrap()),
            "clearing the failure explicitly must retry the listing"
        );
    }

    #[test]
    fn directory_listings_are_one_level_until_a_folder_is_opened() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("nested/deeper")).unwrap();
        std::fs::write(root.path().join("top.txt"), "a").unwrap();
        std::fs::write(root.path().join("nested/child.txt"), "b").unwrap();
        std::fs::write(root.path().join("nested/deeper/leaf.txt"), "c").unwrap();
        let listed = load_directory_files(root.path()).unwrap();
        let names: Vec<_> = listed
            .iter()
            .map(|file| (file.path.to_string_lossy().into_owned(), file.directory))
            .collect();
        assert_eq!(
            names,
            vec![("nested".into(), true), ("top.txt".into(), false)],
            "the first paint must not walk Documents-sized trees"
        );

        let nested = load_directory_listing(root.path(), Path::new("nested")).unwrap();
        let nested_names: Vec<_> = nested
            .iter()
            .map(|file| file.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(nested_names, vec!["nested/child.txt", "nested/deeper"]);
    }

    #[test]
    fn directory_search_is_on_demand_and_capped_to_visible_rows() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("nested/deeper")).unwrap();
        std::fs::write(root.path().join("top.txt"), "a").unwrap();
        std::fs::write(root.path().join("nested/note.md"), "b").unwrap();
        std::fs::write(root.path().join("nested/deeper/note.txt"), "c").unwrap();
        std::fs::write(root.path().join("other.rs"), "d").unwrap();
        let (matches, hidden) =
            search_directory_files(root.path(), "note", &CancellationToken::new()).unwrap();
        let names: Vec<_> = matches
            .iter()
            .map(|file| file.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["nested/deeper/note.txt", "nested/note.md"],
            "search must find nested files without listing the whole tree first"
        );
        assert_eq!(hidden, 0);
        let (none, _) =
            search_directory_files(root.path(), "missing", &CancellationToken::new()).unwrap();
        assert!(none.is_empty());
    }

    /// `core.fsmonitor` names a program Git runs on every index refresh, and
    /// a repository that arrived as an archive chose it. A card lists its
    /// files on creation and every ten seconds after, so a second copy of
    /// the invocation block that forgets this is a local code-execution
    /// vector reachable without a click.
    #[test]
    fn every_git_invocation_here_disables_the_repository_chosen_fsmonitor_hook() {
        let command = git_command(Path::new("/repo"));
        let argv: Vec<String> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            [
                "git",
                "--literal-pathspecs",
                "-c",
                "core.fsmonitor=false",
                "-C",
                "/repo",
            ]
        );
    }

    #[test]
    fn an_unchanged_worktree_is_reported_without_rebuilding_the_file_list() {
        let root = tempdir().unwrap();
        git_fixture(root.path());
        std::fs::write(root.path().join("tracked.txt"), "a").unwrap();
        run_git(root.path(), ["add", "tracked.txt"]);

        let FileListRead::Listed(Some(digest), files) =
            read_repository_files(root.path(), None).unwrap()
        else {
            panic!("a read with nothing to compare against always lists");
        };
        assert_eq!(files.len(), 1);
        assert!(
            matches!(
                read_repository_files(root.path(), Some(digest)).unwrap(),
                FileListRead::Unchanged
            ),
            "an idle refresh tick must not rebuild a list nothing has moved"
        );

        std::fs::write(root.path().join("new.txt"), "b").unwrap();
        assert!(matches!(
            read_repository_files(root.path(), Some(digest)).unwrap(),
            FileListRead::Listed(..)
        ));
    }

    #[test]
    fn a_refresh_tick_keeps_expanded_folders_expanded() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("top.txt"), "a").unwrap();
        std::fs::write(root.path().join("nested/child.txt"), "b").unwrap();
        let card = CardId::new();
        let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: root.path().to_path_buf(),
            worktree: root.path().to_path_buf(),
        });
        let identity = |runtime: &RepositoryRuntime, path: &str| AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: PathBuf::from(path),
            generation: runtime.file_list_generation,
        };

        drop(runtime.initial_load(card));
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity: identity(&runtime, "."),
            result: Ok(load_directory_files(root.path()).unwrap()),
        }));
        drop(runtime.update(RepositoryAction::ListDirectory {
            card,
            path: "nested".into(),
        }));
        drop(runtime.update(RepositoryAction::DirectoryListed {
            identity: identity(&runtime, "nested"),
            result: Ok(load_directory_listing(root.path(), Path::new("nested")).unwrap()),
        }));
        assert!(runtime.file(Path::new("nested/child.txt")).is_some());

        drop(runtime.refresh_visible(card));
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity: identity(&runtime, "."),
            result: Ok(load_directory_files(root.path()).unwrap()),
        }));
        assert!(
            runtime.file(Path::new("nested/child.txt")).is_some(),
            "a refresh must not collapse the tree the user opened"
        );
        assert!(runtime.listed_directories.contains(Path::new("nested")));

        // A folder that disappeared underneath the card takes its rows and
        // its expansion with it.
        std::fs::remove_dir_all(root.path().join("nested")).unwrap();
        drop(runtime.refresh_visible(card));
        drop(runtime.update(RepositoryAction::FilesLoaded {
            identity: identity(&runtime, "."),
            result: Ok(load_directory_files(root.path()).unwrap()),
        }));
        assert!(runtime.file(Path::new("nested/child.txt")).is_none());
        assert!(!runtime.listed_directories.contains(Path::new("nested")));
    }

    #[test]
    fn discarding_edits_keeps_the_viewport_where_the_user_left_it() {
        let text = "line\n".repeat(400);
        let mut document = EditorDocument::unlinked(
            PathBuf::from("notes.txt"),
            text.clone(),
            blake3::hash(text.as_bytes()),
            false,
        );
        let anchor = text_editor::ScrollPosition {
            line: 120,
            vertical: 0.0,
            horizontal: 0.0,
        };
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('x')));
        assert!(document.dirty);

        // Whatever the buffer was showing when the user gave up on the edit
        // is what it must still be showing afterwards.
        document.content.restore_scroll_position(anchor);
        document.discard_edits();
        assert!(!document.dirty);
        assert_eq!(
            document.content.scroll_position(),
            anchor,
            "abandoning an edit is no reason to throw the reader back to line one"
        );
    }

    #[test]
    fn an_edit_that_restores_the_saved_text_is_not_a_modification() {
        let text = "hello\nworld\n";
        let mut document = EditorDocument::unlinked(
            PathBuf::from("notes.txt"),
            text.into(),
            blake3::hash(text.as_bytes()),
            false,
        );
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
        assert!(document.dirty);
        document.apply(text_editor::Action::Edit(text_editor::Edit::Backspace));
        assert!(
            !document.dirty,
            "dirtiness is the buffer against the saved text, not a count of keystrokes"
        );

        // Same length, different content: the cheap length check cannot
        // answer this one and must not be trusted to.
        document.apply(text_editor::Action::Edit(text_editor::Edit::Delete));
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('H')));
        assert_eq!(document.content.text().len(), text.len());
        assert!(document.dirty);
    }

    #[test]
    fn an_edit_at_a_final_bare_carriage_return_resends_the_whole_document() {
        // The protocol does not count a buffer's final bare `\r`, so removing
        // it has no protocol range measured on the edited buffer alone.
        let text = "a\r";
        let mut document = EditorDocument::unlinked(
            PathBuf::from("mac.txt"),
            text.into(),
            blake3::hash(text.as_bytes()),
            false,
        );
        document.apply(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        assert_eq!(
            document.apply(text_editor::Action::Edit(text_editor::Edit::Backspace)),
            Applied::Changed(None)
        );
        assert_eq!(document.content.text(), "a");
    }

    #[test]
    fn replay_at_a_final_bare_carriage_return_resends_the_whole_document() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "a\r");
        drop(runtime.take_lsp_intents());
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::DocumentEnd),
        }));
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Backspace),
        }));
        drop(runtime.take_lsp_intents());
        drop(runtime.take_lsp_edits());

        for (action, version, text) in [
            (RepositoryAction::Undo { card }, 3, "a\r"),
            (RepositoryAction::Redo { card }, 4, "a"),
        ] {
            drop(runtime.update(action));
            assert_eq!(buffer_text(&runtime), text);
            assert!(
                !runtime.lsp_edits_exact,
                "a trailing CR cannot be addressed by a protocol range"
            );
            assert_eq!(
                runtime.take_lsp_intents(),
                vec![LspIntent::Change {
                    version,
                    text: text.into(),
                }]
            );
            assert!(runtime.take_lsp_edits().is_empty());
        }
    }

    #[test]
    fn stepping_back_from_the_first_match_wraps_to_the_last_one() {
        // `index.wrapping_sub(1) % total` looks like it wraps and does not:
        // `usize::MAX % total` is `total - 1` only where `total` divides
        // 2^64, so Previous on three or five matches silently re-selects the
        // first one and looks like a dead button.
        for total in [1usize, 2, 3, 5, 7] {
            let card = CardId::new();
            let text = "alpha\n".repeat(total);
            let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &text);
            drop(runtime.update(RepositoryAction::OpenFind { card }));
            drop(runtime.update(RepositoryAction::FindQueryChanged {
                card,
                query: "alpha".into(),
            }));
            assert_eq!(runtime.find_total, total);
            assert_eq!(runtime.find_index, 0);

            drop(runtime.update(RepositoryAction::FindPrevious { card }));
            assert_eq!(
                runtime.find_index,
                total - 1,
                "Previous from the first of {total} matches must land on the last"
            );
            let DocumentState::Editable(document) = &runtime.document else {
                unreachable!();
            };
            assert_eq!(
                document.content.cursor().position.line,
                total - 1,
                "the reported occurrence must be the one actually selected"
            );
        }
    }

    #[test]
    fn find_stays_editable_and_closing_it_preserves_the_caret() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha\n");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        assert!(runtime.find_is_open());

        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(
            document.content.text(),
            "zalpha\n",
            "find leaves the buffer editable while it is open"
        );

        drop(runtime.update(RepositoryAction::CloseFind { card }));
        assert!(!runtime.find_is_open());
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(
            document.content.text(),
            "zzalpha\n",
            "closing find preserves the caret after edits"
        );
    }

    #[test]
    #[ignore = "explicit complete 100k-match Replace All and history benchmark"]
    fn large_replace_all_and_history_microbenchmark() {
        let card = CardId::new();
        let original = "token\r\n".repeat(100_000);
        let replaced = "replacement\r\n".repeat(100_000);
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "large.txt", &original);
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        let scan = runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "token".into(),
        });
        drop(runtime.update(find_output(scan)));
        assert_eq!(runtime.find_total, 100_000);
        drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
            card,
            query: "replacement".into(),
        }));
        let started = std::time::Instant::now();
        drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
        let replace_elapsed = started.elapsed();
        assert_eq!(buffer_text(&runtime), replaced);
        drop(runtime.update(RepositoryAction::CloseFind { card }));
        let started = std::time::Instant::now();
        drop(runtime.update(RepositoryAction::Undo { card }));
        let undo_elapsed = started.elapsed();
        assert_eq!(buffer_text(&runtime), original);
        let started = std::time::Instant::now();
        drop(runtime.update(RepositoryAction::Redo { card }));
        let redo_elapsed = started.elapsed();
        assert_eq!(buffer_text(&runtime), replaced);
        eprintln!(
            "100k matches complete Replace All: {replace_elapsed:?}; undo: {undo_elapsed:?}; redo: {redo_elapsed:?} (unoptimized test profile, excludes layout)"
        );
    }

    #[test]
    fn find_edits_refresh_counts_without_moving_the_editing_caret() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        }));
        assert_eq!(runtime.find_total, 2);
        if let DocumentState::Editable(document) = &mut runtime.document {
            document.content.move_to(text_editor::Cursor {
                position: text_editor::Position {
                    line: 0,
                    column: 11,
                },
                selection: None,
            });
        }
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        assert_eq!(buffer_text(&runtime), "alpha alphax");
        // Literal search still matches a prefix until whole-word is enabled.
        assert_eq!(runtime.find_total, 2);
        assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 12)));
        drop(runtime.update(RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::WholeWord,
            enabled: true,
        }));
        assert_eq!(runtime.find_total, 1);
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.find_total, 2);
        assert!(runtime.find_is_open());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(runtime.find_total, 1);
    }

    #[test]
    fn find_options_scope_and_invalid_patterns_are_applied_to_current_revision() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "alpha ALPHA alpha_beta alpha");
        if let DocumentState::Editable(document) = &mut runtime.document {
            document.content.move_to(text_editor::Cursor {
                position: text_editor::Position {
                    line: 0,
                    column: 11,
                },
                selection: Some(text_editor::Position { line: 0, column: 0 }),
            });
        }
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        }));
        assert_eq!(runtime.find_total, 4);
        drop(runtime.update(RepositoryAction::FindSelectionChanged {
            card,
            enabled: true,
        }));
        assert_eq!(runtime.find_total, 2);
        drop(runtime.update(RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::CaseSensitive,
            enabled: true,
        }));
        assert_eq!(runtime.find_total, 1);
        drop(runtime.update(RepositoryAction::FindSelectionChanged {
            card,
            enabled: false,
        }));
        drop(runtime.update(RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::WholeWord,
            enabled: true,
        }));
        assert_eq!(runtime.find_total, 2);
        drop(runtime.update(RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "[".into(),
        }));
        assert_eq!(runtime.find_total, 0);
        assert!(runtime.find_error.is_some());
    }

    #[test]
    fn regex_capture_replace_all_and_preserve_case_are_each_one_undo_step() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "call1 call2");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: r"call(\d)".into(),
        }));
        drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
            card,
            query: "item$1".into(),
        }));
        drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
        assert_eq!(buffer_text(&runtime), "item1 item2");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "call1 call2");
        assert!(runtime.find_is_open());

        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "word Word WORD wOrd");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "word".into(),
        }));
        drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
            card,
            query: "value".into(),
        }));
        drop(runtime.update(RepositoryAction::PreserveCaseChanged {
            card,
            enabled: true,
        }));
        drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
        assert_eq!(buffer_text(&runtime), "value Value VALUE value");
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "word Word WORD wOrd");
    }

    #[test]
    fn unicode_find_replace_all_is_one_edit_and_renews_matches() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "Straße STRASSE\n");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "strasse".into(),
        }));
        assert_eq!(runtime.find_total, 2);
        drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
            card,
            query: "road".into(),
        }));
        drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!()
        };
        assert_eq!(document.content.text(), "road road\n");
        assert!(runtime.is_dirty());
        assert_eq!(runtime.find_total, 0);
        drop(runtime.update(RepositoryAction::CloseFind { card }));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "Straße STRASSE\n");
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(buffer_text(&runtime), "road road\n");
        assert_eq!(runtime.find_total, 0);
    }

    #[test]
    fn find_navigation_reuses_cached_match_storage_and_open_starts_from_selection() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha alpha\n");
        assert_eq!(runtime.select_text_match("alpha", 1), Some(3));
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        assert_eq!(runtime.find_index, 1);
        let cached = runtime.find_matches.as_ptr();
        for _ in 0..100 {
            drop(runtime.update(RepositoryAction::FindNext { card }));
            assert_eq!(runtime.find_matches.as_ptr(), cached);
            drop(runtime.update(RepositoryAction::FindPrevious { card }));
            assert_eq!(runtime.find_matches.as_ptr(), cached);
            assert_eq!(runtime.find_index, 1);
        }
    }

    fn find_output(task: Task<RepositoryAction>) -> RepositoryAction {
        use cosmic::iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("find worker");
        let output = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(stream.next())
            .unwrap();
        let iced::runtime::Action::Output(action) = output else {
            panic!("find output");
        };
        action
    }

    #[test]
    fn large_find_fences_superseded_queries_reuses_snapshot_and_rejects_closed_results() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\r\n".repeat(3000));
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        let first = runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        });
        assert_eq!(
            runtime.find_total, 0,
            "large queries must leave the UI thread"
        );
        let snapshot = Arc::clone(&runtime.find_snapshot.as_ref().unwrap().2);
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "beta".into(),
        }));
        assert!(
            runtime
                .find_active
                .as_ref()
                .unwrap()
                .cancellation
                .is_cancelled()
        );
        // Retire the only outstanding worker before the latest query gets one.
        let latest = runtime.update(find_output(first));
        assert!(Arc::ptr_eq(
            &snapshot,
            &runtime.find_snapshot.as_ref().unwrap().2
        ));
        assert_eq!(runtime.find_total, 0);
        drop(runtime.update(find_output(latest)));
        assert_eq!(runtime.find_total, 3000);
        let cached = runtime.find_matches.as_ptr();
        drop(runtime.update(RepositoryAction::FindNext { card }));
        assert_eq!(runtime.find_matches.as_ptr(), cached);
        assert!(runtime.find_active.is_none());

        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        });
        document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('x')));
        runtime.push_lsp_change(None);
        let revised = runtime.find_task(card);
        assert_eq!(
            runtime.find_index, 1,
            "pending searches preserve the selected occurrence"
        );
        drop(runtime.update(find_output(revised)));
        assert_eq!(runtime.find_total, 3000);
        assert_eq!(runtime.find_index, 1);

        let closed = runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        });
        drop(runtime.update(RepositoryAction::CloseFind { card }));
        drop(runtime.update(find_output(closed)));
        assert_eq!(runtime.find_total, 0);
        assert!(runtime.find_matches.is_empty());
        assert!(runtime.find_snapshot.is_none());
        assert!(runtime.find_active.is_none());
    }

    #[test]
    fn large_find_rejects_a_result_for_the_previous_document_revision() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\n".repeat(3000));
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        let old = runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        });
        let snapshot = Arc::clone(&runtime.find_snapshot.as_ref().unwrap().2);
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        // A buffer edit delivered outside the find bar (such as completion or
        // discard) must invalidate a worker as well as its displayed matches.
        document.content.perform(text_editor::Action::SelectAll);
        document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
            Arc::new("beta gamma\n".repeat(3000)),
        )));
        let current = runtime.update(find_output(old));
        assert!(!Arc::ptr_eq(
            &snapshot,
            &runtime.find_snapshot.as_ref().unwrap().2
        ));
        assert_eq!(runtime.find_total, 0);
        drop(runtime.update(find_output(current)));
        assert_eq!(runtime.find_total, 0);
        assert!(runtime.find_matches.is_empty());
        assert!(runtime.find_active.is_none());
    }

    #[test]
    fn large_find_rejects_a_completed_result_from_a_replaced_card_runtime() {
        let card = CardId::new();
        let (mut old, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha\n".repeat(3000));
        drop(old.update(RepositoryAction::OpenFind { card }));
        let old_task = old.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        });
        // Completion can already be in the application's event queue when
        // Retry replaces a clean repository runtime under the same card ID.
        let old_output = find_output(old_task);
        drop(old);

        let (mut replacement, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\n".repeat(3000));
        drop(replacement.update(RepositoryAction::OpenFind { card }));
        let current = replacement.update(RepositoryAction::FindQueryChanged {
            card,
            query: "beta".into(),
        });
        let ticket = replacement.find_active.as_ref().unwrap().ticket;
        drop(replacement.update(old_output));
        assert_eq!(replacement.find_active.as_ref().unwrap().ticket, ticket);
        assert!(replacement.find_matches.is_empty());
        assert_eq!(replacement.find_total, 0);

        drop(replacement.update(find_output(current)));
        assert_eq!(replacement.find_total, 3000);
        assert_eq!(replacement.find_matches[0].0.column, 6);
        assert!(replacement.find_active.is_none());
    }

    #[test]
    fn returning_from_diff_to_edit_preserves_the_visible_source_line() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "line\n".repeat(100).as_str());
        runtime.source_surface = SourceSurface::Diff;
        let mut model = model_with(1);
        model.inline_rows = (0..100)
            .map(|line| InlineDiffRow {
                old_line: Some(line + 1),
                new_line: Some(line + 1),
                text: "line".into(),
                kind: DiffRowKind::Context,
                changed: Vec::new(),
            })
            .collect();
        runtime.set_diff_state(
            runtime.diff_basis.clone(),
            DiffState::ready(staged_text_diff(model)),
        );
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: 30.0 * DIFF_ROW_HEIGHT,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: false,
        }));
        assert_eq!(runtime.viewports.editor.line, 30);
        assert_eq!(runtime.source_surface, SourceSurface::Editor);
    }

    #[test]
    fn changing_the_buffer_under_find_renews_the_count_it_reports() {
        let card = CardId::new();
        let (mut runtime, _) =
            RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha alpha\n");
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("alpha ".into()))),
        }));
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        }));
        assert_eq!(runtime.find_total, 4);
        drop(runtime.update(RepositoryAction::FindPrevious { card }));
        assert_eq!(runtime.find_index, 3);

        // Discarding rebuilds the buffer behind the open strip: one of the
        // four occurrences it was counting no longer exists.
        drop(runtime.update(RepositoryAction::DiscardEdits { card }));
        assert_eq!(
            runtime.find_total, 3,
            "a stale total keeps claiming matches the buffer no longer has"
        );
        assert!(
            runtime.find_index < runtime.find_total,
            "the selected occurrence must stay inside the total"
        );
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(document.content.selection().as_deref(), Some("alpha"));
    }

    #[test]
    fn an_empty_query_counts_nothing_and_selects_nothing() {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha beta\n");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        }));
        assert_eq!(runtime.find_total, 1);

        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "   ".into(),
        }));
        assert_eq!(runtime.find_total, 0);
        assert_eq!(runtime.find_index, 0);
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(
            document.content.cursor().selection,
            None,
            "a query the user cleared leaves nothing highlighted behind it"
        );
    }
    #[test]
    fn sidebar_working_diff_opens_exact_status_without_a_file_list_refresh() {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
        let generation = runtime.file_list_generation;
        let file = RepositoryFile {
            path: "new-name.txt".into(),
            staged: true,
            staged_old_path: Some("old-name.txt".into()),
            ..Default::default()
        };
        drop(runtime.update(RepositoryAction::OpenWorkingDiff {
            card,
            file,
            basis: DiffBasis::HeadToIndex,
        }));
        assert_eq!(runtime.file_list_generation, generation);
        assert_eq!(
            runtime.selected.as_ref().unwrap().path,
            Path::new("new-name.txt")
        );
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("old-name.txt"))
        );
        assert_eq!(runtime.source_surface, SourceSurface::Diff);
        assert!(matches!(
            runtime.diff_state(&DiffBasis::HeadToIndex),
            Some(DiffState::Loading { .. })
        ));
    }

    #[test]
    fn contrast_correction_retains_syntax_hue_instead_of_replacing_it() {
        for background in [
            iced::Color::from_rgb8(42, 29, 31),
            iced::Color::from_rgb8(251, 231, 233),
        ] {
            let syntax = iced::Color::from_rgb8(110, 75, 170);
            let adjusted = readable_diff_color(syntax, background);
            assert!(adjusted.relative_contrast(background) >= 4.5);
            assert_ne!(adjusted, iced::Color::WHITE);
            assert_ne!(adjusted, iced::Color::BLACK);
            assert!(adjusted.b > adjusted.r && adjusted.r > adjusted.g);
        }
    }
}
