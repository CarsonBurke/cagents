//! Native repository-card state, asynchronous file review tasks, and focused UI.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

#[path = "editor_widget.rs"]
pub(crate) mod text_editor;

mod diff_layout;
mod document_watch;
#[cfg(test)]
mod live_tests;
use document_watch::DocumentWatch;
mod preview;
#[cfg(test)]
pub(crate) mod retained_save_tests;
use diff_layout::{DiffLayout, DisplayRow, RowOffsets};

pub use text_editor::{Action as EditorAction, Edit as EditorEdit};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::{
    Element,
    iced::{self, Alignment, Font, Length, Task, highlighter, keyboard},
    widget::{self, markdown},
};

use crate::{
    design::space,
    diff_view::{
        DIFF_FONT_SIZE, DIFF_LINE_HEIGHT, DiffMode, DiffModel, DiffRowKind, DiffSyntax,
        InlineDiffRow, RenderedDiff, SplitDiffRow, SyntaxSpan, build_document_diff,
        highlighter_settings, load_repository_diff,
    },
    editor::{
        FileContent, FileIoError, LinkRefusal, LoadedFile, OversizeKind, ReadOnlyReason,
        SaveDurability, SaveOutcome,
    },
    editor_history::{EditBuffer, EditKind, History, Replay, TextChange},
    lsp::{
        LspCompletionEdit, LspCompletionItem, LspDocumentId, LspPosition, LspRange, LspTextEdit,
    },
    model::{CardId, RepositoryView},
    repository::{
        CancellationToken, ChangeArea, ChangeKind, ChangedPath, ConflictKind, DiffBasis,
        DiffContent, DiffSide, FileInventoryDigest, FileInventoryRead, HistoryRequest,
        HistoryRevision, MAX_DIFF_SIDE_BYTES, RepoPath, RepositoryService, RepositorySnapshot,
        StatusSnapshot, ValidatedRefName, WorkingFiles,
    },
    ui_controls::{Icon, icon_button},
};

/// The other path Git paired with a changed file in one area.
///
/// The old side of that area's comparison reads it either way. Only a rename
/// makes it part of the change: a copy's source is an independent file whose
/// own staged or unstaged changes a mutation of the copy must leave alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldPath {
    Renamed(PathBuf),
    Copied(PathBuf),
}

impl OldPath {
    /// The pairing a source-control status row reports, by its change kind.
    pub fn from_changed_path(entry: &ChangedPath) -> Option<Self> {
        let path = entry.old_path.as_ref()?.as_path().to_owned();
        match entry.kind {
            ChangeKind::Renamed => Some(Self::Renamed(path)),
            ChangeKind::Copied => Some(Self::Copied(path)),
            _ => None,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Renamed(path) | Self::Copied(path) => path,
        }
    }

    /// The source a mutation must act on together with the path itself.
    fn rename_source(&self) -> Option<&Path> {
        match self {
            Self::Renamed(path) => Some(path),
            Self::Copied(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepositoryFile {
    pub path: PathBuf,
    pub staged_old_path: Option<OldPath>,
    pub unstaged_old_path: Option<OldPath>,
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

    /// The rename or copy source the old side of this comparison must read.
    ///
    /// `git status` reports a rename source per area, which is where both
    /// working bases get theirs. A committed comparison's rename detection
    /// belongs to the range that produced the file list, and the card's list
    /// comes from the working tree, so nothing is invented here.
    fn previous_path(&self, basis: &DiffBasis) -> Option<PathBuf> {
        match basis {
            DiffBasis::HeadToIndex => self.staged_old_path.as_ref(),
            DiffBasis::IndexToWorktree => self.unstaged_old_path.as_ref(),
            DiffBasis::MergeBaseToHead { .. }
            | DiffBasis::CommitToParent { .. }
            | DiffBasis::PullRequest { .. } => None,
        }
        .map(|old| old.path().to_owned())
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

/// Why a pane cannot show a comparison beside its open buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffBasisRefusal {
    /// A save of the buffer is in flight.
    Saving,
    /// The pane shows a folder's files, which have no comparisons.
    NoComparisons,
    /// A Git change is being re-read; working comparisons wait for it.
    Refreshing,
    /// The pane's file status does not show this comparison yet.
    NotListed,
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
/// Deferred text for a protocol change. Ordinary edits retain only an
/// immutable root; full serialization belongs to the session debounce.
#[derive(Debug, Clone)]
pub enum LspChangeText {
    Text(String),
    Snapshot(text_editor::TextSnapshot),
}
impl From<String> for LspChangeText {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}
impl From<&str> for LspChangeText {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}
impl PartialEq for LspChangeText {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Snapshot(left), Self::Snapshot(right)) => left.same_text(right),
            (Self::Snapshot(source), Self::Text(text))
            | (Self::Text(text), Self::Snapshot(source)) => source.text().as_ref() == text,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LspIntent {
    /// The buffer became the card's open document. `absolute_path` is
    /// resolvable on disk; [`AsyncIdentity::path`] is worktree-relative and a
    /// document URI cannot be built from it alone.
    Open {
        absolute_path: PathBuf,
        worktree: PathBuf,
        version: i32,
        text: LspChangeText,
    },
    /// The buffer changed. Exact ranges accompany an immutable text root;
    /// only full/inexact synchronization materializes that root on a worker.
    Change { version: i32, text: LspChangeText },
    /// The buffer reached disk. `text` is what was written.
    Save { text: String },
    /// The buffer is gone: another file was opened over it, or the card
    /// returned to its file list.
    Close,
}

/// One accepted completion list, distinct even after a runtime is replaced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompletionEpoch(uuid::Uuid);

impl CompletionEpoch {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

/// Immutable editor and edit identity captured when a candidate was presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionTarget {
    identity: Arc<AsyncIdentity>,
    document: LspDocumentId,
    revision: u64,
    epoch: CompletionEpoch,
    index: usize,
    item: Arc<LspCompletionItem>,
}

impl CompletionTarget {
    pub fn card(&self) -> CardId {
        self.identity.card
    }
    pub fn item(&self) -> &LspCompletionItem {
        &self.item
    }
    pub fn native_key(&self) -> (CardId, LspDocumentId, u64, u64, CompletionEpoch, usize) {
        (
            self.card(),
            self.document,
            self.identity.generation,
            self.revision,
            self.epoch,
            self.index,
        )
    }
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
        target: CompletionTarget,
    },
    Next(bool),
    Dismiss,
}

#[derive(Default)]
struct EditorLanguageState {
    diagnostics: Vec<crate::lsp::LspDiagnostic>,
    selected_diagnostic: Option<usize>,
    hover_diagnostic: Option<crate::lsp::LspDiagnostic>,
    marks: Vec<text_editor::Diagnostic>,
    completions: Vec<Arc<LspCompletionItem>>,
    completion_epoch: CompletionEpoch,
    completion_popup: bool,
    selected: usize,
    hover: Vec<markdown::Item>,
    hover_at: Option<text_editor::Position>,
}

impl EditorLanguageState {
    fn retire_completions(&mut self, intent: &mut u64) {
        *intent = intent.wrapping_add(1);
        self.completions.clear();
    }
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
    /// Actual file-worker completion, carrying the root used for the read.
    RetryDocumentWatch {
        card: CardId,
    },
    DocumentReloaded {
        identity: AsyncIdentity,
        result: Result<crate::editor::FileRead, FileIoError>,
    },
    DocumentChanged {
        card: CardId,
        watch: uuid::Uuid,
    },
    FileLoadedAt {
        identity: AsyncIdentity,
        result: Result<crate::editor::FileRead, FileIoError>,
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
        result: Result<Option<DiffModel>, String>,
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
    RetainedSaveFinished {
        identity: AsyncIdentity,
        ticket: u64,
        revision: u64,
        saved_text: String,
        result: Result<
            crate::editor::retained_save::RetainedSaveOutcome,
            crate::editor::retained_save::RetainedSaveError,
        >,
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
    CopyDiagnostic {
        card: CardId,
        diagnostic: crate::lsp::LspDiagnostic,
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
    ReplaceInputFocus {
        card: CardId,
    },
    FindLoaded {
        card: CardId,
        ticket: uuid::Uuid,
        result: Result<Option<text_editor::MatchIndex>, String>,
    },
    FindReplacementLoaded {
        card: CardId,
        ticket: uuid::Uuid,
        result: Result<Option<PreparedFindReplacement>, String>,
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
    pub fn recovery_authority(&self) -> Option<&crate::recovery_authority::RecoveryAuthority> {
        match self {
            Self::RetainedSaveFinished { result, .. } => match result {
                Ok(outcome) => outcome.recovery_authority(),
                Err(error) => error.recovery_authority(),
            },
            _ => None,
        }
    }
    pub fn recovery_scope(&self) -> Option<&Path> {
        match self {
            Self::RetainedSaveFinished { identity, .. } => Some(&identity.worktree),
            _ => None,
        }
    }
    pub fn card(&self) -> CardId {
        match self {
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
            | Self::RetryDocumentWatch { card }
            | Self::DocumentChanged { card, .. }
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
            | Self::CopyDiagnostic { card, .. }
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
            | Self::ReplaceInputFocus { card }
            | Self::FindLoaded { card, .. }
            | Self::FindReplacementLoaded { card, .. }
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
            | Self::DocumentReloaded { identity, .. }
            | Self::FileLoadedAt { identity, .. }
            | Self::DiagramsRendered { identity, .. }
            | Self::DiffLoaded { identity, .. }
            | Self::WorkingDiffLoaded { identity, .. }
            | Self::DiffBasisResolved { identity, .. }
            | Self::AutoSaveDue { identity, .. }
            | Self::SaveFinished { identity, .. }
            | Self::RetainedSaveFinished { identity, .. } => identity.card,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveTrigger {
    Explicit,
    Automatic,
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
    error_from_document_watch: bool,
    durability_warning: Option<String>,
    cleanup_recovery: Option<crate::recovery_authority::RecoveryAuthority>,
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
    fn durability_warning(&self) -> Option<&str> {
        // Cleanup resolution does not qualify the earlier disk durability.
        // Keep the warning visible until Reload or a successful Save does.
        self.durability_warning.as_deref()
    }
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
            error_from_document_watch: false,
            durability_warning: None,
            cleanup_recovery: None,
            history: History::default(),
            #[cfg(test)]
            history_time: None,
        }
    }

    fn note_document_watch_error(&mut self, error: String) {
        self.conflict = true;
        // An observation must not replace a newer Save failure.
        if self.error.is_none() || self.error_from_document_watch {
            self.error = Some(error);
            self.error_from_document_watch = true;
        }
    }

    fn clear_document_watch_error(&mut self) {
        if self.error_from_document_watch {
            self.conflict = false;
            self.error = None;
            self.error_from_document_watch = false;
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
        self.error_from_document_watch = false;
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
        if !self.dirty {
            self.history.adopt_revision(self.saved_history_revision);
        }
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
    // `Snapshot::utf16_position` refuses that outright — silently failing every
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
        let column = line.text[..char_boundary(&line.text, caret.column)]
            .chars()
            .count();
        return Some(LspPosition::new(
            u32::try_from(caret.line).ok()?,
            u32::try_from(column).ok()?,
        ));
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

/// The editor's caret as an external editor counts it.
///
/// VS Code and Zed read the saved text and break lines on `\n`, `\r\n` and
/// a lone `\r`, one break each. This editor's lines carry the same
/// characters but can split a `\r\n` across two terminators: the `\r` that
/// ends one line and the `\n` that ends the empty line after it.
///
/// - `\n\r\n` loads as `\n\r` and then an empty line ending in `\n`.
/// - Deleting the text of the line after a lone `\r` leaves that `\r`, then
///   an empty line ending in `\n`.
///
/// Either way the saved text has one break where the terminators look like
/// two.
fn external_caret(
    content: &text_editor::Content<cosmic::Renderer>,
    caret: text_editor::Position,
) -> Option<crate::external_editor::Caret> {
    let lines = content.line_count();
    if caret.line >= lines {
        return None;
    }
    // Whether the `\r` ending line `index` pairs with the `\n` that ends the
    // empty line after it. That line exists whenever this is asked: it is at
    // most the caret's.
    let pairs_with_next = |index: usize| -> Option<bool> {
        let ends_in_cr = content.with_line(index, |_, ending| {
            terminator_str(ending, index + 1 == lines).ends_with('\r')
        })?;
        let next_is_newline = content.with_line(index + 1, |text, ending| {
            text.is_empty() && terminator_str(ending, index + 2 == lines).starts_with('\n')
        })?;
        Some(ends_in_cr && next_is_newline)
    };
    let mut line = 0u32;
    for index in 0..caret.line {
        let ending = content.with_line(index, |_, ending| ending)?;
        // Each terminator is one break and `\n\r` two; an unterminated line
        // before the last is written with `\n`.
        let mut breaks = if ending == text_editor::LineEnding::LfCr {
            2
        } else {
            1
        };
        if pairs_with_next(index)? {
            breaks -= 1;
        }
        line = line.saturating_add(breaks);
    }
    // A caret on the empty line between a paired `\r` and `\n` is inside a
    // `\r\n`, where an external editor cannot put one. The nearest place is
    // the end of the line that `\r\n` ends: after `\n\r` that is the empty
    // line between them, which the caret's own empty line already gives;
    // after a lone `\r` it is the previous line.
    if let Some(previous) = caret.line.checked_sub(1)
        && pairs_with_next(previous)?
        && content.with_line(previous, |_, ending| ending == text_editor::LineEnding::Cr)?
    {
        return content.with_line(previous, |text, _| {
            crate::external_editor::Caret::new(line, text)
        });
    }
    content.with_line(caret.line, |text, _| {
        crate::external_editor::Caret::new(line, &text[..char_boundary(text, caret.column)])
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
        let column = line
            .text
            .char_indices()
            .nth(usize::try_from(at.character).ok()?)
            .map_or(line.text.len(), |(byte, _)| byte);
        return Some(text_editor::Position {
            line: line_index,
            column,
        });
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

/// The last input inside this editor, independent of toolbar-click blur.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EditorInput {
    #[default]
    Source,
    Find,
    Replace,
}

enum DocumentState {
    None,
    Loading {
        // A dirty/conflicted Reload is only a proposed replacement. Keep its
        // exact buffer and history until an editable read can replace it.
        predecessor: Option<EditorDocument>,
    },
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

// `Ready` is the steady state, and boxing it would break the nested `Ready(RenderedDiff::…)` patterns.
#[allow(clippy::large_enum_variant)]
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

/// A comparison kept on screen after it stopped being current.
struct RetainedDiff {
    path: PathBuf,
    basis: DiffBasis,
    state: DiffState,
    /// Retained only because its presentation changed. It still compares
    /// what the surface compares, so the live buffer can rebuild it without
    /// asking Git. A comparison retained across a Git refresh compares
    /// a baseline that may have moved, and only Git can replace it.
    restyle: bool,
}

/// One immutable snapshot is in flight; further edits only advance the desired
/// revision/deadline. Cancellation does not release the slot until it returns.
struct WorkingDiffJob {
    identity: AsyncIdentity,
    revision: u64,
    ticket: u64,
    before: Arc<str>,
    /// The [`RepositoryRuntime::diff_presentation`] its rows are styled in.
    presentation: u64,
    cancellation: CancellationToken,
}

/// The outstanding file-list read, cancelled when it is dropped.
struct FileListReadJob(CancellationToken);

impl Drop for FileListReadJob {
    fn drop(&mut self) {
        self.0.cancel();
    }
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
    /// The presentation the applied comparison was styled in. Its line
    /// markers hold in any presentation; its rows are rebuilt in a new one.
    presentation: u64,
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

/// A position in the new-side source, shared by the editor and the diff.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SourceLineAnchor {
    /// One-based source line, as diff rows number it.
    line: u32,
    /// How far into that line, in line heights.
    fraction: f64,
    horizontal: f32,
}

/// Scroll coordinates only: the selected document remains the sole live buffer.
#[derive(Debug, Clone, Copy, Default)]
struct FileViewportAnchors {
    editor: text_editor::ScrollPosition,
    /// Whether the reader has moved the editor since it was last placed for
    /// them, on opening or on following the comparison back to Edit. Until
    /// they do, entering Diff keeps the comparison's own place.
    editor_moved: bool,
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

/// A diff row at the default editor typography: one editor line. A runtime
/// uses [`RepositoryRuntime::diff_typography`], which follows the editor.
const DIFF_ROW_HEIGHT: f32 = DIFF_LINE_HEIGHT;
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

fn scoped_matches(matches: &mut text_editor::MatchIndex, scope: Option<FindScope>) {
    *matches = matches.scoped(scope.map(|(first, last)| {
        (
            text_editor::Position {
                line: first.0,
                column: first.1,
            },
            text_editor::Position {
                line: last.0,
                column: last.1,
            },
        )
    }));
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

/// Immutable replacement work prepared from one exact Find revision. The
/// payload is opaque and shared so delivery does not copy a large output.
#[derive(Clone)]
pub struct PreparedFindReplacement(Arc<String>);

impl std::fmt::Debug for PreparedFindReplacement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedFindReplacement")
            .field("bytes", &self.0.len())
            .finish()
    }
}

struct FindReplacementJob {
    key: FindKey,
    replacement: String,
    preserve_case: bool,
    ticket: uuid::Uuid,
    cancellation: CancellationToken,
}

impl Drop for FindReplacementJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub struct RepositoryRuntime {
    pub descriptor: RepositoryView,
    original_editor_root: Option<Arc<crate::fs_safety::Root>>,
    original_read_attested: bool,
    save_recovery_window: Option<crate::operation_recovery::Shared>,
    known_checkout_paths: Option<Vec<PathBuf>>,
    editor_recovery: Option<crate::recovery_authority::RecoveryAuthority>,
    worktree_protection: Option<crate::worktree_cleanup::Protection>,
    directory_only: bool,
    standalone: bool,
    external_editors: Vec<crate::sidebar::SidebarEditor>,
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
    file_list_digest: Option<FileInventoryDigest>,
    /// Cancels the outstanding file-list read when a newer read supersedes
    /// it, the card is retargeted, or the card is closed.
    file_list_read: Option<FileListReadJob>,
    /// Where the worker leaves the digest of the read in flight, tagged with
    /// the generation that asked for it. Committed into
    /// [`RepositoryRuntime::file_list_digest`] only once the matching list
    /// has actually been installed.
    file_list_deposit: Arc<std::sync::Mutex<Option<(u64, FileInventoryDigest)>>>,
    file_list_refresh_queued: bool,
    background_file_refresh: bool,
    mutation_refresh_pending: bool,
    /// Last comparison stays readable while Git refreshes this file, or
    /// while it is rebuilt in a new presentation.
    stale_diff: Option<RetainedDiff>,
    /// Advances whenever comparisons must be styled or measured again: a
    /// syntax palette or editor font family change.
    diff_presentation: u64,
    diff_hide_unchanged: bool,
    files: FileListState,
    selected: Option<RepositoryFile>,
    document: DocumentState,
    document_watch: Option<DocumentWatch>,
    document_reading: bool,
    document_reload_pending: bool,
    search_mode: bool,
    /// Accepted caret/selection intent, independent of text version and response epoch.
    completion_intent: u64,
    editor_font: Font,
    editor_font_size: f32,
    find_open: bool,
    /// The find bar's query exactly as the user typed it.
    find_query: String,
    find_options: crate::editor_search::FindOptions,
    find_selection: Option<FindScope>,
    /// Captured scope coordinates belong only to this native buffer revision.
    find_selection_revision: Option<u64>,
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
    last_editor_input: EditorInput,
    reload_input: Option<EditorInput>,
    replace_open: bool,
    replace_query: String,
    toolbar_overflow_open: bool,
    find_matches: text_editor::MatchIndex,
    find_cache: Option<FindKey>,
    find_snapshot: Option<(u64, u64, Arc<text_editor::SearchSnapshot>)>,
    find_active: Option<FindJob>,
    find_replacement: Option<FindReplacementJob>,
    #[cfg(test)]
    find_replacement_pause: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    find_origin: Option<(usize, usize)>,
    find_error: Option<String>,
    /// One entry per comparison this selection has been asked for.
    ///
    /// A list rather than a map: at most six bases are live at once, and
    /// [`DiffBasis`] is deliberately neither `Ord` nor `Hash` — it owns a ref
    /// name and an object id.
    diffs: Vec<(DiffBasis, DiffState)>,
    working_diff: WorkingDiffRefresh,
    source_surface: SourceSurface,
    markdown_mode: MarkdownMode,
    pending_fragment: Option<String>,
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
    /// The editor line Diff was entered from, held until rows that describe
    /// the current buffer place it: a stale working comparison's rows cannot
    /// name that line after edits above it.
    pending_diff_source: Option<SourceLineAnchor>,
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

impl RepositoryRuntime {
    fn protect_editor_worktree(&mut self, directory: &Path) -> std::io::Result<()> {
        // Compared as named, so a directory reached through a link is not
        // resolved and leased again on every refresh. If the link is
        // retargeted meanwhile, the lease stays on the original directory,
        // and the pane's retained read root refuses the replacement anyway.
        if self
            .worktree_protection
            .as_ref()
            .is_some_and(|guard| guard.requested() == directory)
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
            original_editor_root: None,
            original_read_attested: false,
            save_recovery_window: None,
            known_checkout_paths: None,
            editor_recovery: None,
            worktree_protection: None,
            standalone: false,
            external_editors: Vec::new(),
            generation: 0,
            file_list_generation: 0,
            pdf_request: 0,
            document_tone: crate::editor::PdfTone::Original,
            file_list_loading: false,
            file_list_started: None,
            file_list_digest: None,
            file_list_read: None,
            file_list_deposit: Arc::new(std::sync::Mutex::new(None)),
            file_list_refresh_queued: false,
            background_file_refresh: false,
            mutation_refresh_pending: false,
            stale_diff: None,
            diff_presentation: 0,
            diff_hide_unchanged: true,
            files: FileListState::Loading,
            selected: None,
            search_mode: false,
            completion_intent: 0,
            editor_font: Font::MONOSPACE,
            editor_font_size: 14.0,
            find_open: false,
            find_query: String::new(),
            find_options: crate::editor_search::FindOptions::default(),
            find_selection: None,
            find_selection_revision: None,
            find_selection_candidate: None,
            find_select_on_finish: false,
            replace_preserve_case: false,
            find_index: 0,
            find_total: 0,
            find_input_focused: false,
            last_editor_input: EditorInput::Source,
            reload_input: None,
            replace_open: false,
            replace_query: String::new(),
            toolbar_overflow_open: false,
            find_matches: text_editor::MatchIndex::default(),
            find_cache: None,
            find_snapshot: None,
            find_active: None,
            find_replacement: None,
            #[cfg(test)]
            find_replacement_pause: None,
            find_origin: None,
            find_error: None,
            document: DocumentState::None,
            document_watch: None,
            document_reading: false,
            document_reload_pending: false,
            diffs: Vec::new(),
            working_diff: WorkingDiffRefresh::default(),
            next_diff_request: 0,
            source_surface: SourceSurface::Editor,
            markdown_mode: MarkdownMode::Edit,
            pending_fragment: None,
            committed_selection: None,
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

    /// Pure handoff of an original reader/creator root. Never replace an
    /// existing physical scope through a later pathname or sidebar binding.
    pub fn set_original_editor_root(&mut self, root: Arc<crate::fs_safety::Root>) -> bool {
        if let Some(original) = &self.original_editor_root {
            return Arc::ptr_eq(original, &root);
        }
        self.original_editor_root = Some(root);
        true
    }

    pub fn retained_editor_root(&self) -> Option<Arc<crate::fs_safety::Root>> {
        self.original_editor_root.clone()
    }

    /// Pure window-owned custody and negative checkout census. The worker
    /// acquires the real original-root cleanup protection before any effect.
    pub fn configure_save_recovery(
        &mut self,
        window: crate::operation_recovery::Shared,
        known: Vec<PathBuf>,
    ) -> bool {
        if known.len() > 256
            || self
                .save_recovery_window
                .as_ref()
                .is_some_and(|original| !Arc::ptr_eq(original, &window))
        {
            return false;
        }
        self.save_recovery_window = Some(window);
        self.known_checkout_paths = Some(known);
        true
    }
    pub fn has_editor_recovery(&self) -> bool {
        self.editor_recovery
            .as_ref()
            .is_some_and(|authority| !authority.is_resolved())
    }
    fn clear_resolved_operation_recovery(&mut self) {
        if let DocumentState::Editable(document) = &mut self.document
            && document
                .cleanup_recovery
                .as_ref()
                .is_some_and(|authority| authority.is_resolved())
        {
            document.cleanup_recovery = None;
            // Namespace/content recovery completing does not prove earlier
            // directory durability. Explicit Reload or another successful
            // Save owns that baseline transition, as in the ordinary writer.
        }
        if self
            .editor_recovery
            .as_ref()
            .is_some_and(|authority| authority.is_resolved())
        {
            self.editor_recovery = None;
        }
    }
    pub fn reconcile_operation_recovery(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.clear_resolved_operation_recovery();
        self.auto_save_task(card)
    }
    pub fn retire_auto_save_due(
        &mut self,
        identity: &AsyncIdentity,
        revision: u64,
        ticket: u64,
    ) -> bool {
        if ticket == self.next_auto_save_ticket
            && self.accepts_selected(identity)
            && self.auto_save_pending == Some((identity.generation, revision))
            && self.auto_save_revision() == Some(revision)
        {
            self.auto_save_pending = None;
            return true;
        }
        false
    }

    /// Hides workbench launch controls in an independent editor window.
    pub fn set_standalone(&mut self, standalone: bool) {
        self.standalone = standalone;
    }

    pub fn set_external_editors(&mut self, editors: Vec<crate::sidebar::SidebarEditor>) {
        self.external_editors = editors;
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
    /// the old one; [`Self::refresh_diff_presentation`] requests them again.
    ///
    /// The Edit surface re-derives its highlighter in `view` and repaints at
    /// once; a [`DiffModel`] carries baked spans and would keep showing the
    /// previous palette until the next revision happened to rebuild it.
    pub fn set_syntax_theme(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        if self.syntax_theme == theme {
            return;
        }
        self.syntax_theme = theme;
        self.discard_rendered_diffs();
    }

    /// Drops every comparison built with a previous presentation.
    ///
    /// The one on screen stays readable until its successor arrives, which
    /// keeps what the reader unfolded and the row they are on, and keeps the
    /// editor's added-line markers: those name source lines, which no
    /// presentation changes. A working comparison already in flight is
    /// styled the old way, so it is cancelled and started again.
    fn discard_rendered_diffs(&mut self) {
        self.diff_presentation = self.diff_presentation.wrapping_add(1);
        if let Some(active) = &self.working_diff.active {
            active.cancellation.cancel();
        }
        if let Some(path) = self.selected.as_ref().map(|file| file.path.clone())
            && let Some(index) = self.diffs.iter().position(|(basis, state)| {
                *basis == self.diff_basis && matches!(state, DiffState::Ready(..))
            })
        {
            let (basis, state) = self.diffs.remove(index);
            self.stale_diff = Some(RetainedDiff {
                path,
                basis,
                state,
                restyle: true,
            });
        }
        self.diffs.clear();
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
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
        if let Some(RetainedDiff {
            state: DiffState::Ready(RenderedDiff::Text { model, .. }, layout),
            ..
        }) = &mut self.stale_diff
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
            history_revision: if content_matches(&document.content, "B\n") {
                document.history.revision()
            } else {
                u64::MAX
            },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("B\n")
                .text_snapshot(),
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

    /// An unstaged file with its comparison on screen, for callers that
    /// change the settings comparisons are presented in.
    #[cfg(test)]
    pub(crate) fn shown_diff_fixture(card: CardId) -> Self {
        let (mut runtime, _) = Self::open_fixture(card, "notes.txt", "added\nkept\n");
        runtime.source_surface = SourceSurface::Diff;
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::Text {
                model: crate::diff_view::build_diff("removed\nkept\n", "added\nkept\n"),
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            }),
        );
        runtime
    }

    /// Whether the comparison on screen has been requested again: from Git,
    /// or, for one retained only for a restyle, rebuilt from the live buffer,
    /// which then asks Git nothing. A cancelled or differently styled rebuild
    /// does not count.
    #[cfg(test)]
    pub(crate) fn requests_shown_diff(&self) -> bool {
        matches!(
            self.diff_state(&self.diff_basis),
            Some(DiffState::Loading { .. })
        ) || (self
            .retained_diff()
            .is_some_and(|retained| retained.restyle)
            && (self.working_diff.timer.is_some()
                || self.working_diff.active.as_ref().is_some_and(|active| {
                    active.presentation == self.diff_presentation
                        && !active.cancellation.is_cancelled()
                })))
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

    /// Drives a document load as the shell would, including the re-read the
    /// runtime starts when its watch invalidates a read, and reports whether
    /// the real file worker answered. Other follow-up tasks are dropped.
    #[cfg(test)]
    pub(crate) async fn settle_document_read(&mut self, task: Task<RepositoryAction>) -> bool {
        retained_save_tests::finish(self, task).await.answered()
    }

    /// Whether `action` is a file-read completion for the selected document.
    /// Shell-level drivers check this before applying the action.
    #[cfg(test)]
    pub(crate) fn accepts_document_read(&self, action: &RepositoryAction) -> bool {
        matches!(action,
            RepositoryAction::FileLoadedAt { identity, .. }
                | RepositoryAction::DocumentReloaded { identity, .. }
                if self.accepts_selected(identity))
    }

    /// Whether an accepted read left the document loading behind a re-read.
    #[cfg(test)]
    pub(crate) fn rereading_document(&self) -> bool {
        matches!(self.document, DocumentState::Loading { .. }) && self.document_reading
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
            let path = self
                .descriptor
                .selected_file
                .clone()
                .or_else(|| {
                    self.restored_navigation
                        .as_ref()
                        .and_then(|navigation| navigation.selected_file.clone())
                })
                .expect("the protected load has a selected file");
            self.fail_file_open(card, path, &error);
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
        if self.descriptor.selected_file.is_none()
            || view.selected_file.is_none()
            || self.blocks_navigation()
        {
            return Task::none();
        }
        let protection = self.protect_editor_worktree(&view.worktree);
        if self.descriptor.worktree != view.worktree {
            self.original_editor_root = None;
            self.original_read_attested = false;
        }
        self.descriptor = view;
        self.retained_viewports.clear();
        if let Err(error) = protection {
            self.fail_file_open(
                card,
                self.descriptor
                    .selected_file
                    .clone()
                    .expect("a retargeted file pane has a selected file"),
                &error,
            );
            return Task::none();
        }
        self.initial_load(card)
    }

    /// Poll only visible panes. Keep the mounted list, viewport, and document
    /// intact while discovering external changes, and never overlap a read.
    pub fn refresh_visible(&mut self, card: CardId) -> Task<RepositoryAction> {
        if self.descriptor.selected_file.is_some() {
            return Task::none();
        }
        if self.file_list_read_outstanding() {
            return Task::none();
        }
        self.background_file_refresh = true;
        self.load_files(card)
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
        let cancellation = CancellationToken::new();
        // Replacing the job cancels the read it supersedes.
        self.file_list_read = Some(FileListReadJob(cancellation.clone()));
        files_task(
            identity,
            self.directory_only,
            self.file_list_digest,
            Arc::clone(&self.file_list_deposit),
            cancellation,
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
            self.stale_diff = Some(RetainedDiff {
                path,
                basis,
                state,
                restyle: false,
            });
        }
        // Resolved branch ranges and commits name immutable objects.
        self.drop_mutable_diffs();
        self.initial_load(card)
    }

    /// Drops the comparisons Git may since have changed.
    fn drop_mutable_diffs(&mut self) {
        self.diffs.retain(|(basis, _)| basis.is_immutable());
        self.expect_git_comparison();
    }

    /// A comparison kept on screen may now be out of date in what it
    /// compares, not just in its style, so only Git may replace it.
    fn expect_git_comparison(&mut self) {
        if let Some(retained) = &mut self.stale_diff {
            retained.restyle = false;
        }
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

    pub fn diagnostic_navigation_available(&self) -> bool {
        self.editable_surface_visible() && !self.language.diagnostics.is_empty()
    }

    /// The caret as an external editor counts it, so the selected file opens
    /// there where the reader left it.
    ///
    /// `None` unless the selected file's own buffer is open. A buffer with
    /// unsaved edits still reports its caret, although the external editor
    /// opens the saved file: the position is then approximate, and VS Code
    /// and Zed clamp a line or column past the end to the nearest place.
    #[must_use]
    pub fn external_caret(&self) -> Option<crate::external_editor::Caret> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        if self.selected_path() != Some(document.path.as_path()) {
            return None;
        }
        external_caret(&document.content, document.content.cursor().position)
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
        if self.language.hover_diagnostic.is_some() {
            self.dismiss_editor_results();
        }
        self.language.diagnostics = diagnostics;
        self.language.selected_diagnostic = None;
    }

    fn navigate_diagnostic(&mut self, forward: bool) {
        let Some(at) = self.cursor_position() else {
            return;
        };
        let mut order: Vec<_> = self
            .language
            .diagnostics
            .iter()
            .enumerate()
            .map(|(index, item)| (item.range.start, index))
            .collect();
        order.sort_by_key(|entry| entry.0);
        if order.is_empty() {
            return;
        }
        let current = self.language.selected_diagnostic.and_then(|selected| {
            order
                .iter()
                .position(|(position, index)| *index == selected && *position == at)
        });
        let next = if let Some(current) = current {
            if forward {
                (current + 1) % order.len()
            } else {
                (current + order.len() - 1) % order.len()
            }
        } else if forward {
            order
                .iter()
                .position(|(position, _)| *position > at)
                .unwrap_or(0)
        } else {
            order
                .iter()
                .rposition(|(position, _)| *position < at)
                .unwrap_or(order.len() - 1)
        };
        let (position, index) = order[next];
        let diagnostic = self.language.diagnostics[index].clone();
        if self.move_cursor_to(position) {
            self.dismiss_editor_results();
            // Diagnostic.message is plain text, not MarkupContent. In
            // particular HTML/metadata-looking text must not hide a truncation
            // marker or the explicit full-copy control.
            self.set_editor_hover(None, Some(position));
            self.language.hover_diagnostic = Some(diagnostic);
            self.language.selected_diagnostic = Some(index);
        }
    }

    pub fn set_editor_completions(&mut self, items: Vec<LspCompletionItem>) {
        self.language.selected = items.iter().position(|item| item.preselect).unwrap_or(0);
        self.language.completion_popup = true;
        self.language.completion_epoch = CompletionEpoch::new();
        self.language.completions = items.into_iter().map(Arc::new).collect();
        self.language.hover.clear();
        self.language.hover_diagnostic = None;
    }

    /// Cache the same response for a host that renders its own completion list.
    pub fn set_editor_completion_list(&mut self, items: Vec<LspCompletionItem>) {
        self.set_editor_completions(items);
        self.language.completion_popup = false;
    }

    pub fn editor_completion_targets(&self, card: CardId) -> Vec<CompletionTarget> {
        let Some(first) = self.completion_target(card, 0) else {
            return Vec::new();
        };
        self.language
            .completions
            .iter()
            .enumerate()
            .map(|(index, item)| CompletionTarget {
                identity: first.identity.clone(),
                document: first.document,
                revision: first.revision,
                epoch: first.epoch,
                index,
                item: item.clone(),
            })
            .collect()
    }

    pub fn set_editor_hover(&mut self, markdown: Option<&str>, at: Option<LspPosition>) {
        self.language.hover_diagnostic = None;
        self.language.hover = markdown.map_or_else(Vec::new, |raw| markdown::parse(raw).collect());
        self.language.hover_at = at.and_then(|at| match &self.document {
            DocumentState::Editable(document) => caret_for_position(&document.content, at),
            _ => None,
        });
    }

    /// Changes even for an accepted no-op caret action or a Find round trip.
    /// Controllers use it to retire pending answers without changing text versions.
    pub fn editor_completion_intent(&self) -> u64 {
        self.completion_intent
    }

    pub fn dismiss_editor_results(&mut self) {
        self.language
            .retire_completions(&mut self.completion_intent);
        self.language.hover.clear();
        self.language.hover_diagnostic = None;
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
        let position = match &self.document {
            DocumentState::Editable(document) => caret_for_position(&document.content, at),
            _ => None,
        };
        let Some(position) = position else {
            return false;
        };
        self.language
            .retire_completions(&mut self.completion_intent);
        let DocumentState::Editable(document) = &mut self.document else {
            unreachable!("the validated editor remains mounted");
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
        if let Some(active) = &self.find_replacement
            && !self.replacement_is_current(active)
        {
            active.cancellation.cancel();
        }
    }

    /// Apply shared preferences and dispatch any refreshed asynchronous search.
    pub fn update_find_preferences(
        &mut self,
        card: CardId,
        options: crate::editor_search::FindOptions,
        preserve_case: bool,
    ) -> Task<RepositoryAction> {
        self.set_find_preferences(options, preserve_case);
        self.find_task(card)
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
            self.find_matches = document
                .content
                .find_matches_with(query, Default::default())
                .ok()?;
            self.find_cache = Some(key);
        }
        self.select_cached_match(occurrence)
    }

    fn select_cached_match(&mut self, occurrence: usize) -> Option<usize> {
        if !self.editable_surface_visible() {
            return None;
        }
        self.language
            .retire_completions(&mut self.completion_intent);
        let DocumentState::Editable(document) = &mut self.document else {
            unreachable!("the validated editor remains mounted");
        };
        document.history.seal();
        let total = self.find_matches.len();
        if total == 0 {
            document
                .content
                .perform(text_editor::Action::ClearSelection);
            return None;
        }
        let (start, end) = self
            .find_matches
            .get(occurrence % total)
            .expect("current find occurrence");
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

    /// An accepted foreground buffer keeps its focus handoff until the app can
    /// mount it. Reading the file can finish before the first viewport arrives.
    pub fn take_restored_editor_focus(&mut self, card: CardId) -> Option<widget::Id> {
        let identity = self.pending_editor_focus.as_ref()?;
        if identity.card != card || !self.accepts_selected(identity) {
            self.pending_editor_focus = None;
            return None;
        }
        let target = self.loaded_focus_target(card)?;
        self.pending_editor_focus = None;
        Some(target)
    }

    /// Whether the pending explicit Reload still owns its predecessor.
    pub fn retains_reload_buffer(&self) -> bool {
        matches!(
            &self.document,
            DocumentState::Loading {
                predecessor: Some(_)
            }
        )
    }

    /// The control a completed read restores when no other control owns input.
    /// A retained Reload keeps a focused Find field with its predecessor.
    pub fn loaded_focus_target(&self, card: CardId) -> Option<widget::Id> {
        self.editor_focus_target(card)
            .map(|editor| match self.last_editor_input {
                EditorInput::Replace if self.find_open && self.replace_open => {
                    replace_focus_id(card)
                }
                EditorInput::Find | EditorInput::Replace if self.find_open => find_focus_id(card),
                _ => editor,
            })
    }

    /// Whether the editable buffer is the surface actually on screen.
    ///
    /// Only Edit mounts it. The comparison surface renders the diff model, and
    /// Markdown preview renders the parsed document, so neither has a buffer
    /// to focus or search.
    pub(crate) fn editable_surface_visible(&self) -> bool {
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

    /// Current selected match (zero based) and total, after the current
    /// query and buffer revision have settled. Pending or closed Find has
    /// no current result; a settled query without matches returns `(0, 0)`.
    pub fn find_match_position(&self) -> Option<(usize, usize)> {
        (self.find_open && self.find_cache.as_ref() == self.wanted_find().as_ref())
            .then_some((self.find_index, self.find_total))
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
        if !self.editable_surface_visible() {
            return Task::none();
        }
        self.last_editor_input = EditorInput::Find;
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
        self.find_open = false;
        self.find_query.clear();
        self.find_index = 0;
        self.find_total = 0;
        self.find_input_focused = false;
        self.last_editor_input = EditorInput::Source;
        self.reload_input = None;
        self.find_matches.clear();
        self.find_cache = None;
        self.find_snapshot = None;
        self.find_origin = None;
        self.find_error = None;
        self.find_selection = None;
        self.find_selection_revision = None;
        self.find_selection_candidate = None;
        self.find_select_on_finish = false;
        if let Some(active) = &self.find_active {
            active.cancellation.cancel();
        }
        if let Some(active) = &self.find_replacement {
            active.cancellation.cancel();
        }
        self.set_search_mode(false);
    }

    /// Selects one occurrence and records the count that search reported,
    /// which is the only place the count is ever computed.
    fn select_find_match(&mut self, occurrence: usize) {
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

    /// Small literal searches complete immediately. Regex and larger buffers
    /// scan a reusable immutable snapshot off the update thread, preserving
    /// the engine's greedy semantics rather than enabling earliest-match mode.
    fn prepare_find_matches(&mut self) {
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
        if wanted.selection.is_some() && self.find_selection_revision != Some(document.revision) {
            // Native edits, Undo, completion and other transactions can all
            // move a captured span. Never reinterpret its old coordinates as
            // a different region or widen replacement to the whole file.
            self.find_selection_candidate = None;
            self.find_error = Some(
                "The captured selection changed. Select a new range and turn Selection off and on to search it.".into(),
            );
            self.find_cache = Some(wanted);
            return;
        }
        if wanted.query.is_empty()
            || (!wanted.options.regex
                && document.content.line_count() <= 2048
                && document.content.text_len() <= 64 * 1024)
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
        result: Result<Option<text_editor::MatchIndex>, String>,
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
        if std::mem::take(&mut self.find_select_on_finish) && self.editable_surface_visible() {
            self.select_find_match(occurrence);
        } else {
            self.find_total = self.find_matches.len();
            self.find_index = self.find_index.min(self.find_total.saturating_sub(1));
        }
    }

    fn replace_find_matches(
        &mut self,
        card: CardId,
        all: bool,
        prepared: Option<PreparedFindReplacement>,
    ) -> Task<RepositoryAction> {
        if !self.editable_surface_visible()
            || self.is_saving()
            || self.find_cache.as_ref() != self.wanted_find().as_ref()
        {
            return Task::none();
        }
        if prepared.is_none() {
            if self.find_replacement.is_some() {
                return Task::none();
            }
            if all
                && self.find_matches.len() > 256
                && matches!(&self.document, DocumentState::Editable(document) if document.content.text_len() > 64 * 1024)
            {
                return self.prepare_find_replacement(card);
            }
        }
        let DocumentState::Editable(document) = &mut self.document else {
            return Task::none();
        };
        if self.find_matches.is_empty() {
            return Task::none();
        }
        let mut protocols = Vec::new();
        if all && self.find_matches.len() <= 256 && document.content.text_len() > 64 * 1024 {
            // Keep sparse fixes in a large file undoable without
            // retaining unchanged megabytes as a whole-file paste.
            // Dense replacement retains the single-splice path below.
            let snapshot = document.content.search_snapshot();
            let replacements = match snapshot.replacement_edits(
                &self.find_matches,
                &self.find_query,
                &self.replace_query,
                self.find_options,
                self.replace_preserve_case,
            ) {
                Ok(edits) => edits,
                Err(error) => {
                    self.find_error = Some(error);
                    return Task::none();
                }
            };
            let before = document.content.cursor();
            let mut changes = Vec::with_capacity(replacements.len());
            for (start, end, inserted) in replacements.into_iter().rev() {
                let trailing_cr = trailing_carriage_return(&document.content).is_some();
                if let Some(change) = document
                    .content
                    .replace_range_recorded(start, end, &inserted)
                {
                    protocols.push(
                        if trailing_cr || trailing_carriage_return(&document.content).is_some() {
                            None
                        } else {
                            protocol_edit(&document.content, &change)
                        },
                    );
                    changes.push(change);
                }
            }
            if !changes.is_empty() {
                document.history.record_group(
                    changes,
                    before,
                    document.content.cursor(),
                    Instant::now(),
                );
                document.text_changed();
            }
        } else if all {
            let snapshot = document.content.search_snapshot();
            let text = if let Some(prepared) = prepared {
                prepared.0
            } else {
                match snapshot.replace_matches_with(
                    &self.find_matches,
                    &self.find_query,
                    &self.replace_query,
                    self.find_options,
                    self.replace_preserve_case,
                ) {
                    Ok(text) => Arc::new(text),
                    Err(error) => {
                        self.find_error = Some(error);
                        return Task::none();
                    }
                }
            };
            self.language
                .retire_completions(&mut self.completion_intent);
            document.content.perform(text_editor::Action::SelectAll);
            if let Applied::Changed(protocol) =
                document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(text)))
            {
                protocols.push(protocol);
            }
        } else {
            let (start, end) = self
                .find_matches
                .get(self.find_index)
                .expect("current find occurrence");
            let limit = crate::editor_search::replacement_budget(document.content.text_len())
                - (document.content.text_len() - (end.column - start.column));
            let replacement = document.content.with_line(start.line, |line, _| {
                crate::editor_search::Replacement::new(
                    &self.find_query,
                    &self.replace_query,
                    self.find_options,
                    self.replace_preserve_case,
                )
                .and_then(|replacement| {
                    replacement.expand_bounded(line, start.column, end.column, limit)
                })
            });
            let replacement = match replacement {
                Some(Ok(replacement)) => replacement,
                Some(Err(error)) => {
                    self.find_error = Some(error);
                    return Task::none();
                }
                None => {
                    self.find_error = Some("The selected match changed; search again.".into());
                    return Task::none();
                }
            };
            self.language
                .retire_completions(&mut self.completion_intent);
            document.content.move_to(text_editor::Cursor {
                position: end,
                selection: Some(start),
            });
            if let Applied::Changed(protocol) = document.apply(text_editor::Action::Edit(
                text_editor::Edit::Paste(Arc::new(replacement)),
            )) {
                protocols.push(protocol);
            }
        }
        let mut protocols = protocols.into_iter();
        let Some(first) = protocols.next() else {
            return Task::none();
        };
        self.push_lsp_change(first);
        if self.pending_lsp_change.is_some() {
            for protocol in protocols {
                self.append_lsp_edit(protocol);
            }
        }
        self.markdown_task(card)
    }

    fn replacement_is_current(&self, active: &FindReplacementJob) -> bool {
        !active.cancellation.is_cancelled()
            && self.wanted_find().as_ref() == Some(&active.key)
            && self.find_cache.as_ref() == Some(&active.key)
            && self.replace_query == active.replacement
            && self.replace_preserve_case == active.preserve_case
            && self.editable_surface_visible()
            && !self.is_saving()
    }

    fn prepare_find_replacement(&mut self, card: CardId) -> Task<RepositoryAction> {
        let Some(key) = self.wanted_find() else {
            return Task::none();
        };
        let DocumentState::Editable(document) = &self.document else {
            return Task::none();
        };
        let snapshot = document.content.search_snapshot();
        let matches = self.find_matches.clone();
        let replacement = self.replace_query.clone();
        let preserve_case = self.replace_preserve_case;
        let ticket = uuid::Uuid::new_v4();
        let cancellation = CancellationToken::new();
        self.find_replacement = Some(FindReplacementJob {
            key: key.clone(),
            replacement: replacement.clone(),
            preserve_case,
            ticket,
            cancellation: cancellation.clone(),
        });
        #[cfg(test)]
        let pause = self.find_replacement_pause.take();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some((entered, release)) = pause {
                    let _ = entered.send(());
                    release
                        .recv()
                        .map_err(|_| "Replacement fixture pause disconnected.".to_string())?;
                }
                let result = snapshot.replace_matches_with_cancellation(
                    &matches,
                    &key.query,
                    &replacement,
                    key.options,
                    preserve_case,
                    || cancellation.is_cancelled(),
                );
                if cancellation.is_cancelled() {
                    return Ok(None);
                }
                result.map(|text| Some(PreparedFindReplacement(Arc::new(text))))
            })
            .await
            .map_err(|error| format!("Replacement worker failed: {error}"))
            .and_then(std::convert::identity);
            RepositoryAction::FindReplacementLoaded {
                card,
                ticket,
                result,
            }
        })
    }

    fn finish_find_replacement(
        &mut self,
        card: CardId,
        ticket: uuid::Uuid,
        result: Result<Option<PreparedFindReplacement>, String>,
    ) -> Task<RepositoryAction> {
        if self
            .find_replacement
            .as_ref()
            .is_none_or(|active| active.ticket != ticket)
        {
            return Task::none();
        }
        let active = self.find_replacement.take().unwrap();
        if !self.replacement_is_current(&active) {
            return Task::none();
        }
        match result {
            Ok(Some(prepared)) => self.replace_find_matches(card, true, Some(prepared)),
            Ok(None) => Task::none(),
            Err(error) => {
                self.find_error = Some(error);
                Task::none()
            }
        }
    }

    /// Renews the cached count after something changed the buffer under an
    /// open find strip, clamping the selected occurrence into the new total.
    ///
    /// Without this the strip keeps claiming "3 of 7" over text that no
    /// longer contains seven of anything.
    fn refresh_find_matches(&mut self) {
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
            .partition_point(|(start, _)| (start.line, start.column) < origin)
    }

    fn completion_target(&self, card: CardId, index: usize) -> Option<CompletionTarget> {
        let DocumentState::Editable(document) = &self.document else {
            return None;
        };
        Some(CompletionTarget {
            identity: Arc::new(AsyncIdentity {
                card,
                worktree: self.descriptor.worktree.clone(),
                path: self.selected_path()?.to_path_buf(),
                generation: self.generation,
            }),
            document: self.lsp_document()?,
            revision: document.revision,
            epoch: self.language.completion_epoch,
            index,
            item: self.language.completions.get(index)?.clone(),
        })
    }

    pub fn apply_completion_target(&mut self, card: CardId, target: &CompletionTarget) -> bool {
        if target.card() != card
            || target.epoch != self.language.completion_epoch
            || !self
                .language
                .completions
                .get(target.index)
                .is_some_and(|item| Arc::ptr_eq(item, &target.item))
            || !self.accepts_selected(&target.identity)
            || self.lsp_document() != Some(target.document)
            || !matches!(&self.document, DocumentState::Editable(document) if document.revision == target.revision)
        {
            return false;
        }
        self.apply_completion(target.item())
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
                text: LspChangeText::Snapshot(document.content.text_snapshot()),
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
            text: LspChangeText::Snapshot(document.content.text_snapshot()),
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
            DocumentState::None | DocumentState::Loading { .. } | DocumentState::Failed { .. } => {
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
        if matches!(self.document, DocumentState::Loading { .. }) {
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
                .and_then(OldPath::rename_source)
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
        self.append_lsp_edit(edit);
    }

    fn append_lsp_edit(&mut self, edit: Option<LspTextEdit>) {
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
        let text = LspChangeText::Snapshot(document.content.text_snapshot());
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

    /// Closes the language server's copy without changing the reader's Find
    /// state. A same-document replacement reopens that protocol document while
    /// keeping the query, options and keyboard focus in the mounted strip.
    fn push_lsp_close_notification(&mut self) {
        self.flush_lsp_change();
        self.lsp_edits.clear();
        self.lsp_edits_exact = true;
        if matches!(&self.document, DocumentState::Editable(_)) {
            self.lsp_intents.push(LspIntent::Close);
        }
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
        self.push_lsp_close_notification();
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
            Arc::clone(self.working_baseline()?),
        ))
    }

    /// The index text a live comparison is built against: the current
    /// comparison's, or one retained only for a restyle. One retained across
    /// a Git refresh may name an index that has since moved, and a rebuild
    /// against it would replace the comparison Git is reading.
    fn working_baseline(&self) -> Option<&Arc<str>> {
        let state = match self.diff_state(&self.diff_basis) {
            Some(state @ DiffState::Ready(..)) => state,
            _ => {
                &self
                    .retained_diff()
                    .filter(|retained| retained.restyle)?
                    .state
            }
        };
        match state {
            DiffState::Ready(RenderedDiff::Text { model, .. }, _) => Some(&model.before),
            _ => None,
        }
    }

    /// Whether the live-buffer comparison wanted is the one applied, styled
    /// in the current presentation.
    fn working_diff_settled(&self) -> bool {
        same_diff_revision(&self.working_diff.desired, &self.working_diff.applied)
            && self.working_diff.presentation == self.diff_presentation
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

    /// Call after repository actions and LSP edits, alongside autosave. At most
    /// one timer or one immutable CPU snapshot exists, even under continuous
    /// typing or selection/basis churn. The timer observes the latest deadline.
    pub fn working_diff_task(&mut self, card: CardId) -> Task<RepositoryAction> {
        let wanted = self.wanted_working_diff();
        if !same_diff_revision(&wanted, &self.working_diff.desired) {
            self.working_diff.desired = wanted;
            self.working_diff.deadline = Some(Instant::now() + Duration::from_millis(180));
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
            || self.working_diff_settled()
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
        if self.working_diff.active.is_some() || self.working_diff_settled() {
            return Task::none();
        }
        let DocumentState::Editable(document) = &self.document else {
            return Task::none();
        };
        let identity = self.current_identity(card, document.path.clone());
        // Highlighting follows the bytes, which is the source; the identity
        // stays the row, which is what the result is matched against.
        let source = document.source.clone();
        // Only the debounce boundary serializes the native buffer. No Git is
        // invoked, no second editable Content is created, and queued edits do
        // not retain copies of superseded text.
        let after = document.content.text();
        if after.len() as u64 > MAX_DIFF_SIDE_BYTES {
            self.working_diff.applied = self.working_diff.desired.clone();
            self.working_diff.presentation = self.diff_presentation;
            self.working_diff.error = Some("The unsaved buffer exceeds the diff size limit. Editing and safe saving remain available.".into());
            // A restyle left Git's comparison to this rebuild.
            return self.ensure_current_diff(card);
        }
        let cancellation = CancellationToken::new();
        let syntax_theme = self.syntax_theme;
        let font = self.editor_font;
        self.working_diff.active = Some(WorkingDiffJob {
            identity: identity.clone(),
            revision,
            ticket,
            before: Arc::clone(&before),
            presentation: self.diff_presentation,
            cancellation: cancellation.clone(),
        });
        cosmic::task::future(async move {
            let baseline = Arc::clone(&before);
            let result = tokio::task::spawn_blocking(move || {
                build_document_diff(baseline, &after, &source, syntax_theme, font, &cancellation)
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
        result: Result<Option<DiffModel>, String>,
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
        // A job styled in a superseded presentation is cancelled when that
        // presentation is dropped; checking both keeps a result already
        // queued from installing the old palette or measurements.
        let cancelled = self.working_diff.active.as_ref().is_some_and(|active| {
            active.cancellation.is_cancelled() || active.presentation != self.diff_presentation
        });
        self.working_diff.active = None;
        let mut installed = false;
        let completed = Some((identity.generation, revision, Arc::clone(&before)));
        if !cancelled
            && self.accepts_selected(&identity)
            && same_diff_revision(&completed, &self.wanted_working_diff())
        {
            match result {
                Ok(Some(mut model)) => {
                    model.before = before;
                    self.working_diff.additions = added_source_lines(&model);
                    if let DocumentState::Editable(document) = &self.document {
                        let lines = document.content.source_line_ranges();
                        self.working_diff.additions = self
                            .working_diff
                            .additions
                            .iter()
                            .filter_map(|line| lines.get(*line))
                            .cloned()
                            .flatten()
                            .collect();
                    }
                    self.working_diff.applied = completed;
                    self.working_diff.presentation = self.diff_presentation;
                    self.working_diff.error = None;
                    self.set_diff_state(
                        DiffBasis::IndexToWorktree,
                        DiffState::ready(RenderedDiff::Text {
                            model,
                            old_label: "Index".into(),
                            new_label: "Working buffer".into(),
                        }),
                    );
                    self.diff_change = self.diff_change.min(self.change_count().saturating_sub(1));
                    installed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    self.working_diff.applied = completed;
                    self.working_diff.presentation = self.diff_presentation;
                    self.working_diff.error = Some(error);
                }
            }
        }
        let mut next = self.working_diff_task(identity.card);
        if self.working_diff.error.is_some() {
            // A restyle may have left Git's comparison to this rebuild.
            next = Task::batch([next, self.ensure_current_diff(identity.card)]);
        }
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

    /// Why the open file cannot switch to `basis` in place, as the toolbar's
    /// comparison control does without leaving its buffer, or `None` when it
    /// can.
    pub fn diff_basis_refusal(&self, basis: &DiffBasis) -> Option<DiffBasisRefusal> {
        if self.is_saving() {
            Some(DiffBasisRefusal::Saving)
        } else if self.directory_only {
            Some(DiffBasisRefusal::NoComparisons)
        } else if self.mutation_refresh_pending && !basis.is_immutable() {
            Some(DiffBasisRefusal::Refreshing)
        } else if !self
            .subject_file()
            .is_some_and(|file| self.file_has_basis(&file, basis))
        {
            Some(DiffBasisRefusal::NotListed)
        } else {
            None
        }
    }

    fn can_select_diff_basis(&self, basis: &DiffBasis) -> bool {
        self.diff_basis_refusal(basis).is_none()
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
        matches!(&self.document,
            DocumentState::Editable(document)
                | DocumentState::Loading { predecessor: Some(document) }
                if document.dirty)
    }

    pub fn can_save(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if (document.dirty || document.durability_warning().is_some()) && document.active_save.is_none())
    }

    fn can_discard_edits(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.dirty)
            && !self.is_saving()
            && !self.has_editor_recovery()
    }

    pub fn can_open_external(&self) -> bool {
        !self.standalone && self.selected.is_some() && !self.is_saving()
    }

    pub fn is_saving(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.active_save.is_some())
    }

    pub fn blocks_navigation(&self) -> bool {
        self.has_editor_recovery()
            || self.is_dirty()
            || self.is_saving()
            || matches!(
                &self.document,
                DocumentState::Loading {
                    predecessor: Some(_)
                }
            )
            || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning().is_some())
    }

    /// Whether abandoning the editor buffer is sufficient to release every
    /// repository navigation fence.
    ///
    /// A post-persist durability warning describes unresolved disk state, not
    /// merely unsaved editor contents. Discarding the buffer cannot resolve it.
    pub fn can_abandon_unsaved_changes(&self) -> bool {
        !self.has_editor_recovery()
            && matches!(
                &self.document,
                DocumentState::Editable(document)
                    if document.dirty
                        && document.active_save.is_none()
                        && document.durability_warning().is_none()
            )
    }

    pub fn has_durability_warning(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.durability_warning().is_some())
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
            DocumentState::Loading { .. } => "Loading",
            DocumentState::Editable(_) if self.is_saving() => "Saving",
            DocumentState::Editable(document) if document.conflict => "Save conflict",
            DocumentState::Editable(document) if document.durability_warning().is_some() => {
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
                || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning().is_some())
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

    /// Diff rows are editor lines: they follow the editor's family, size and
    /// line height, keeping the reader's logical position.
    ///
    /// A size change scales the measured line widths. Another family has
    /// other advances, so its comparisons are dropped like a palette change
    /// and measured again; [`Self::refresh_diff_presentation`] requests them.
    pub fn set_editor_font(&mut self, font: Font, size: f32) {
        if self.editor_font == font && self.editor_font_size == size {
            return;
        }
        let family_changed = self.editor_font.family != font.family;
        self.editor_font = font;
        self.editor_font_size = size;
        if family_changed {
            self.discard_rendered_diffs();
        }
        self.update_diff_typography();
    }

    /// Requests the comparisons a presentation change dropped, the live
    /// buffer's or else the Git one on screen, and moves the mounted surface
    /// to its retained position.
    pub fn refresh_diff_presentation(&mut self, card: CardId) -> Task<RepositoryAction> {
        Task::batch([
            self.ensure_current_diff(card),
            self.working_diff_task(card),
            self.restore_visible_scroll(),
        ])
    }

    fn diff_typography(&self) -> DiffTypography {
        DiffTypography {
            font: self.editor_font,
            size: self.editor_font_size,
            line_height: self.editor_font_size * self.line_height,
        }
    }

    /// Re-lays every retained comparison at the current row height, keeping
    /// the logical row the reader is on.
    fn update_diff_typography(&mut self) {
        let anchor = self.current_diff_viewport();
        let height = self.diff_typography().line_height;
        for (_, state) in &mut self.diffs {
            if let DiffState::Ready(_, layout) = state {
                layout.set_row_height(height);
            }
        }
        if let Some(RetainedDiff {
            state: DiffState::Ready(_, layout),
            ..
        }) = &mut self.stale_diff
        {
            layout.set_row_height(height);
        }
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        self.apply_diff_viewport(anchor);
    }

    fn auto_save_revision(&self) -> Option<u64> {
        if self.has_editor_recovery() {
            return None;
        }
        match &self.document {
            DocumentState::Editable(document)
                if self.auto_save
                    && document.dirty
                    && document.active_save.is_none()
                    && !document.conflict
                    && document.error.is_none()
                    && document.durability_warning().is_none() =>
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
        // Save is an editing boundary even when the current bytes are already
        // saved, or a background write currently prevents starting a new one.
        if let DocumentState::Editable(document) = &mut self.document {
            document.history.seal();
        }
        self.request_save_inner(card, overwrite, SaveTrigger::Explicit)
    }

    fn request_save_inner(
        &mut self,
        card: CardId,
        overwrite: bool,
        trigger: SaveTrigger,
    ) -> Task<RepositoryAction> {
        self.request_save_with_writer(
            card,
            overwrite,
            trigger == SaveTrigger::Explicit,
            crate::editor::retained_save::save_file_at,
        )
    }

    #[cfg(test)]
    pub(crate) fn request_save_with_sync_warning_for_test(
        &mut self,
        card: CardId,
    ) -> Task<RepositoryAction> {
        self.request_save_with_writer(
            card,
            false,
            true,
            |root, path, text, hash, overwrite, context| {
                crate::editor::retained_save::save_file_with_cleanup_warning_at(
                    root,
                    path,
                    text,
                    hash,
                    overwrite,
                    context,
                    |_, _| Ok(()),
                )
            },
        )
    }

    fn request_save_with_writer(
        &mut self,
        card: CardId,
        overwrite: bool,
        reveal_editor: bool,
        write: impl FnOnce(
            &crate::fs_safety::Root,
            &Path,
            &str,
            blake3::Hash,
            bool,
            &crate::filesystem_recovery_context::FilesystemRecoveryContext,
        ) -> Result<
            crate::editor::retained_save::RetainedSaveOutcome,
            crate::editor::retained_save::RetainedSaveError,
        > + Send
        + 'static,
    ) -> Task<RepositoryAction> {
        self.clear_resolved_operation_recovery();
        if self.has_editor_recovery() {
            return Task::none();
        }
        let Some(root) = self
            .original_editor_root
            .clone()
            .filter(|_| self.original_read_attested)
        else {
            if let DocumentState::Editable(document) = &mut self.document {
                document.error = Some(
                    "The original read root is unavailable; reopen this file before saving.".into(),
                );
                document.error_from_document_watch = false;
            }
            return Task::none();
        };
        let Some((window, known)) = self
            .save_recovery_window
            .clone()
            .zip(self.known_checkout_paths.clone())
        else {
            if let DocumentState::Editable(document) = &mut self.document {
                document.error = Some("File recovery custody is unavailable; the save was refused before changing files.".into());
                document.error_from_document_watch = false;
            }
            return Task::none();
        };
        let (path, destination, text, disk_hash, revision, history_revision, snapshot_source) =
            match &self.document {
                DocumentState::Editable(document)
                    if (document.dirty || document.durability_warning().is_some())
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
        let operation = match crate::operation_recovery::begin_operation(
            &window,
            self.descriptor.worktree.clone(),
            root.clone(),
        ) {
            Ok(operation) => operation,
            Err(error) => {
                if let DocumentState::Editable(document) = &mut self.document {
                    document.error = Some(error);
                    document.error_from_document_watch = false;
                }
                return Task::none();
            }
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
            // Background persistence is not a new editing gesture. Its
            // 800 ms debounce can expire inside a one-second typing group;
            // explicit Save seals that group at its public entry point.
            document.active_save = Some(ticket.clone());
            document.error = None;
            document.error_from_document_watch = false;
        }
        // Keep diff review mounted while saving. Only a real save failure or
        // conflict reveals recovery; explicit saves may still leave preview.
        if reveal_editor && self.source_surface != SourceSurface::Diff {
            self.source_surface = SourceSurface::Editor;
            self.markdown_mode = MarkdownMode::Edit;
        }
        let saved_text = text.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                operation.start();
                let result =
                    crate::filesystem_recovery_context::FilesystemRecoveryContext::acquire_for_save(
                        &root, &destination, &known,
                    )
                    .map_err(|error| {
                        #[cfg(test)]
                        {
                            use std::io::Write as _;
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "[save-worker-diagnostic] stage=acquire_for_save boundary=Err fd_role=original_source source={:?} destination={:?} raw_errno=not_retained error={}",
                                root.path(),
                                destination,
                                error,
                            );
                        }
                        match error.authority {
                            Some(authority) => {
                                crate::editor::retained_save::RetainedSaveError::Recovery {
                                    message: error.message,
                                    authority,
                                }
                            }
                            None => crate::editor::retained_save::RetainedSaveError::Io(error.message),
                        }
                    })
                    .and_then(|context| {
                        let result = write(
                            &root,
                            &destination,
                            &text,
                            disk_hash,
                            overwrite,
                            &context,
                        );
                        #[cfg(test)]
                        if let Err(error) = &result {
                            use std::io::Write as _;
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "[save-worker-diagnostic] stage=write boundary=Err fd_roles=original_source,recovery_placement source={:?} destination={:?} placement={:?} raw_errno=not_retained error={}",
                                root.path(),
                                destination,
                                context.placement().directory().path(),
                                error,
                            );
                        }
                        crate::editor::retained_save::finish_context(&context, result)
                    });
                let authority = match &result {
                    Ok(outcome) => outcome.recovery_authority(),
                    Err(error) => error.recovery_authority(),
                }
                .cloned();
                operation.finish(authority);
                result
            })
            .await
            .map_err(|error| {
                crate::editor::retained_save::RetainedSaveError::Io(format!(
                    "save worker failed: {error}"
                ))
            })
            .and_then(|result| result);
            RepositoryAction::RetainedSaveFinished {
                identity,
                ticket: ticket.id,
                revision,
                saved_text,
                result,
            }
        })
    }

    /// The shell owns this subscription for every open buffer, including
    /// background tabs. Its identity is the document lifetime, not its path.
    pub fn document_subscription(&self, card: CardId) -> iced::Subscription<RepositoryAction> {
        let Some(watch) = &self.document_watch else {
            return iced::Subscription::none();
        };
        #[derive(Clone)]
        struct Source(CardId, DocumentWatch);
        impl std::hash::Hash for Source {
            fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
                std::hash::Hash::hash(&self.0, hasher);
                std::hash::Hash::hash(&self.1.id(), hasher);
            }
        }
        iced::Subscription::run_with(Source(card, watch.clone()), |source| {
            let card = source.0;
            let watch = source.1.id();
            let receiver = source.1.receiver();
            iced::futures::stream::unfold(receiver, move |receiver| async move {
                receiver.recv().await.ok()?;
                Some((RepositoryAction::DocumentChanged { card, watch }, receiver))
            })
        })
    }

    fn reload_changed_document(&mut self, card: CardId) -> Task<RepositoryAction> {
        if !self.document_reload_pending
            || self.document_reading
            || self.is_saving()
            || self.has_editor_recovery()
        {
            return Task::none();
        }
        let (Some(watch), Some(selected)) = (&self.document_watch, &self.selected) else {
            return Task::none();
        };
        self.document_reload_pending = false;
        self.document_reading = true;
        let identity = self.current_identity(card, selected.path.clone());
        let retained_source = match &self.document {
            DocumentState::Editable(document)
            | DocumentState::Loading {
                predecessor: Some(document),
            } => Some(document.source.clone()),
            _ => None,
        };
        file_task(
            identity,
            self.document_tone,
            self.original_editor_root.clone(),
            retained_source,
            Some(watch.clone()),
            true,
        )
    }

    pub fn update(&mut self, action: RepositoryAction) -> Task<RepositoryAction> {
        self.clear_resolved_operation_recovery();
        if let Some(authority) = action
            .recovery_authority()
            .filter(|authority| !authority.is_resolved())
        {
            self.editor_recovery = Some(
                match self.editor_recovery.take().filter(|old| !old.is_resolved()) {
                    Some(old) => old.combine(authority.clone()),
                    None => authority.clone(),
                },
            );
        }
        let card = action.card();
        let task = self.update_inner(action);
        if let Some(active) = &self.find_replacement
            && !self.replacement_is_current(active)
        {
            active.cancellation.cancel();
        }
        Task::batch([
            task,
            self.find_task(card),
            self.reload_changed_document(card),
        ])
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
                    self.drop_mutable_diffs();
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
            RepositoryAction::DocumentReloaded { identity, result } => {
                self.update_inner(RepositoryAction::FileLoadedAt { identity, result })
            }
            RepositoryAction::RetryDocumentWatch { .. } => {
                self.document_reload_pending = self.document_watch.is_some();
                Task::none()
            }
            RepositoryAction::DocumentChanged { card: _, watch } => {
                if self
                    .document_watch
                    .as_ref()
                    .is_some_and(|current| current.id() == watch)
                {
                    self.document_reload_pending = true;
                }
                Task::none()
            }
            RepositoryAction::FileLoadedAt {
                mut identity,
                result,
            } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                self.document_reading = false;
                let watched = result.as_ref().ok().and_then(|read| read.watch_revision);
                if let (Some(revision), Some(watch)) = (watched, &self.document_watch)
                    && revision != watch.revision()
                {
                    self.document_reload_pending = true;
                    return Task::none();
                }
                let background = self.document_watch.is_some()
                    && !matches!(self.document, DocumentState::Loading { .. });
                // A save's events are reconciled against its final disk hash.
                if background && (self.is_saving() || self.has_editor_recovery()) {
                    self.document_reload_pending = true;
                    return Task::none();
                }
                let result = match result {
                    Ok(read) if read.worktree() == identity.worktree => {
                        let (root, result) = read.into_parts();
                        if !self.set_original_editor_root(root) {
                            Err(FileIoError::Io(
                                "The file read belongs to another physical root; reopen the pane"
                                    .into(),
                            ))
                        } else {
                            result
                        }
                    }
                    Ok(_) => Err(FileIoError::Io(
                        "The file read belongs to another worktree".into(),
                    )),
                    Err(error) => Err(error),
                };
                // A refused noneditable replacement does not attest the old
                // retained buffer merely because its read succeeded.
                let attested = result.is_ok() && !self.rejects_reload_replacement(&result);
                if background {
                    if let DocumentState::Editable(document) = &mut self.document {
                        let unchanged = matches!(&result, Ok(LoadedFile { source, content: FileContent::Editable { content_hash, .. } }) if *content_hash == document.disk_hash && *source == document.source);
                        if unchanged {
                            // The validated original-root read may recover a
                            // failed explicit Reload without replacing its
                            // retained buffer or clearing a newer Save error.
                            self.original_read_attested = attested;
                            document.clear_document_watch_error();
                            return Task::none();
                        }
                        // A missing or refused symlink target is classified
                        // as read-only rather than an I/O error. It still
                        // cannot replace the last readable editor buffer.
                        if result.is_err()
                            || matches!(
                                &result,
                                Ok(LoadedFile {
                                    content: FileContent::ReadOnly(ReadOnlyReason::Symlink { .. }),
                                    ..
                                })
                            )
                            || document.dirty
                            || document.durability_warning().is_some()
                            || (document.error.is_some() && !document.error_from_document_watch)
                        {
                            document.note_document_watch_error(match &result {
                                Err(error) => format!("File unavailable on disk: {error}. Your text is preserved. Reload to retry."),
                                Ok(LoadedFile { source, content: FileContent::ReadOnly(reason @ ReadOnlyReason::Symlink { .. }) }) => format!("File unavailable on disk: {} Your text is preserved. Reload to retry.", read_only_detail(source, reason)),
                                _ => "File changed on disk. Your edits are preserved. Reload or overwrite to resolve.".into(),
                            });
                            return Task::none();
                        }
                    }
                    // A new buffer must not share revision-zero jobs or find
                    // caches with the old document, even at the same path.
                    identity = self.next_identity(identity.card, identity.path.clone());
                    self.capture_native_viewport();
                    if let Some(navigation) = self.navigation()
                        && let Some(editor) = navigation.editor
                    {
                        self.pending_editor_navigation = Some((identity.path.clone(), editor));
                    }
                    self.push_lsp_close_notification();
                    self.dismiss_editor_results();
                    self.drop_mutable_diffs();
                }
                let card = identity.card;
                let task = self.update_inner(RepositoryAction::FileLoaded { identity, result });
                self.original_read_attested = attested;
                if background {
                    // Restoring the caret is not a foreground navigation.
                    self.pending_editor_focus = None;
                    self.refresh_find_matches();
                    let diff = if self.source_surface == SourceSurface::Diff {
                        self.ensure_current_diff(card)
                    } else {
                        Task::none()
                    };
                    Task::batch([task, self.diagram_task(card), diff])
                } else {
                    task
                }
            }
            RepositoryAction::FileLoaded { identity, result } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                if let Some(input) = self.reload_input.take() {
                    self.last_editor_input = input;
                }
                self.original_read_attested = false;
                if self.rejects_reload_replacement(&result) {
                    let detail = match &result {
                        Err(error) => {
                            local_file_error_detail(&identity.worktree.join(&identity.path), error)
                        }
                        Ok(LoadedFile {
                            source,
                            content: FileContent::ReadOnly(reason),
                        }) => read_only_detail(source, reason),
                        Ok(_) => "The file no longer opens as editable text.".into(),
                    };
                    let DocumentState::Loading { predecessor } = &mut self.document else {
                        unreachable!("only a retained reload rejects its replacement");
                    };
                    let mut document = predecessor
                        .take()
                        .expect("the rejected reload retains its buffer");
                    document.error = Some(format!(
                        "Reload did not replace the editor buffer; your edits were kept. {detail}"
                    ));
                    document.error_from_document_watch = false;
                    self.document = DocumentState::Editable(document);
                    // A location requested for the rejected disk read must not
                    // move the retained buffer's original caret or selection.
                    self.pending_editor_navigation = None;
                    // The accepted read can finish while another surface or
                    // dialog owns the keyboard. Let the shell restore focus
                    // only when this editor is mounted and owns input again.
                    self.pending_editor_focus = Some(identity.clone());
                    self.pending_fragment = None;
                    self.push_lsp_open(&identity.worktree);
                    self.refresh_find_matches();
                    return self.restore_visible_scroll();
                }
                // Captured coordinates belong to the old buffer, even when
                // both documents have revision zero. Keep an active scope so
                // Find reports its existing stale-selection refusal until the
                // user deliberately captures a range in the replacement.
                self.find_selection_revision = None;
                self.find_selection_candidate = None;
                // Fresh navigation needs the same loading-to-editor handoff
                // as a restored caret. Background reads clear this request
                // before returning to the shell; they never claim input.
                let loaded_focus = (matches!(self.document, DocumentState::Loading { .. })
                    || self.restores_editor_navigation(&identity))
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
                    Err(error) => {
                        let path = identity.worktree.join(&identity.path);
                        let message = local_file_error_detail(&path, &error);
                        DocumentState::Failed { path, message }
                    }
                };
                if let DocumentState::Editable(document) = &mut self.document {
                    self.pending_editor_focus = loaded_focus;
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
                        self.viewports.editor = text_editor::ScrollPosition {
                            line: editor.scroll_line,
                            vertical: editor.scroll_vertical,
                            horizontal: editor.scroll_horizontal,
                        };
                        // A navigation names the place to read.
                        self.viewports.editor_moved = true;
                    }
                    document
                        .content
                        .restore_scroll_position(self.viewports.editor);
                }
                self.refresh_find_matches();
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
                    self.drop_mutable_diffs();
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
                if !self.editable_surface_visible() {
                    return Task::none();
                }
                if !matches!(
                    action,
                    text_editor::Action::Scroll { .. } | text_editor::Action::ClearSelection
                ) {
                    self.last_editor_input = EditorInput::Source;
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
                if diagnostic {
                    self.focus_editor(card)
                } else {
                    Task::none()
                }
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
                self.request_save_inner(identity.card, false, SaveTrigger::Automatic)
            }
            RepositoryAction::RetainedSaveFinished {
                identity,
                ticket,
                revision,
                saved_text,
                result,
            } => {
                let cleanup = result
                    .as_ref()
                    .ok()
                    .and_then(|outcome| outcome.recovery_authority())
                    .cloned();
                let owns_snapshot = matches!(&self.document, DocumentState::Editable(document)
                    if document.active_save.as_ref().is_some_and(|active| active.id == ticket && active.identity == identity && active.revision == revision && active.snapshot == saved_text));
                let result = result
                    .map(|outcome| outcome.into_parts().0)
                    .map_err(|error| FileIoError::Io(error.to_string()));
                let task = self.update_inner(RepositoryAction::SaveFinished {
                    identity,
                    ticket,
                    revision,
                    saved_text,
                    result,
                });
                if owns_snapshot && let DocumentState::Editable(document) = &mut self.document {
                    document.cleanup_recovery = cleanup;
                }
                task
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
                        document.error_from_document_watch = false;
                    }
                    return Task::none();
                }
                let mut saved = false;
                if let DocumentState::Editable(document) = &mut self.document {
                    let ticket = document.active_save.take().expect("matched save ticket");
                    document.error_from_document_watch = false;
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
                if self.is_saving()
                    || matches!(
                        &self.document,
                        DocumentState::Loading {
                            predecessor: Some(_)
                        }
                    )
                {
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
                Task::batch([self.markdown_task(card), self.focus_editor(card)])
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
                self.document_watch = None;
                self.document_reading = false;
                self.document_reload_pending = false;
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
            RepositoryAction::CopyDiagnostic { diagnostic, .. } => {
                iced::clipboard::write(diagnostic.copy_text())
            }
            RepositoryAction::MarkdownLink { .. } | RepositoryAction::OpenExternal { .. } => {
                Task::none()
            }
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
                self.last_editor_input = EditorInput::Find;
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
                self.find_selection_revision = self.find_selection.and(match &self.document {
                    DocumentState::Editable(document) => Some(document.revision),
                    _ => None,
                });
                // Recapturing the same coordinates after a change must also
                // replace a cached stale-scope refusal for this revision.
                self.find_cache = None;
                self.select_find_match(0);
                Task::none()
            }
            RepositoryAction::PreserveCaseChanged { enabled, .. } => {
                self.replace_preserve_case = enabled;
                Task::none()
            }
            RepositoryAction::FindNext { .. } => {
                self.step_find(true);
                Task::none()
            }
            RepositoryAction::FindPrevious { .. } => {
                self.step_find(false);
                Task::none()
            }
            RepositoryAction::FindInputFocus { focused, .. } => {
                self.find_input_focused = focused;
                if focused {
                    self.last_editor_input = EditorInput::Find;
                }
                Task::none()
            }
            RepositoryAction::ReplaceInputFocus { .. } => {
                self.last_editor_input = EditorInput::Replace;
                Task::none()
            }
            RepositoryAction::FindLoaded { ticket, result, .. } => {
                self.finish_find(ticket, result);
                Task::none()
            }
            RepositoryAction::ReplaceQueryChanged { query, .. } => {
                self.last_editor_input = EditorInput::Replace;
                self.replace_query = query;
                Task::none()
            }
            RepositoryAction::ToggleReplace { .. } => {
                self.replace_open = !self.replace_open;
                Task::none()
            }
            RepositoryAction::ReplaceMatch { card, all } => {
                self.replace_find_matches(card, all, None)
            }
            RepositoryAction::FindReplacementLoaded {
                card,
                ticket,
                result,
            } => self.finish_find_replacement(card, ticket, result),
            RepositoryAction::ToggleToolbarOverflow { .. } => {
                self.toolbar_overflow_open = !self.toolbar_overflow_open;
                Task::none()
            }
            RepositoryAction::CloseToolbarOverflow { .. } => {
                self.toolbar_overflow_open = false;
                Task::none()
            }
            RepositoryAction::ZoomFont { .. } => Task::none(),
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
        if let DiffState::Ready(_, layout) = &mut state {
            layout.set_row_height(self.diff_typography().line_height);
        }
        self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        let restores_current = basis == self.diff_basis && matches!(&state, DiffState::Ready(_, _));
        // The rows on screen before a rebuild, which a rebuild must not move:
        // display rows and pixel offsets both change meaning with the model.
        // They may be a stale comparison kept on screen while this one loaded.
        let mut rebuilt = None;
        if restores_current
            && let Some(DiffState::Ready(
                RenderedDiff::Text {
                    model: previous, ..
                },
                previous_layout,
            )) = self.displayed_diff_state()
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
        if restores_current {
            self.stale_diff = None;
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
        if restores_current && self.source_surface == SourceSurface::Diff {
            self.settle_pending_diff_source();
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
        let showing_diff =
            self.source_surface == SourceSurface::Diff && self.basis_resolution.is_none();
        self.diffs.retain(|(basis, state)| {
            !matches!(state, DiffState::Loading { .. })
                || (showing_diff && *basis == self.diff_basis)
        });
        if !showing_diff || self.diff_state(&self.diff_basis).is_some() {
            return Task::none();
        }
        // A comparison retained for a presentation change is rebuilt from
        // the live buffer. Git compares the file on disk instead, so its rows
        // could replace the retained ones before the buffer's arrive. A buffer
        // the live comparison could not cover still asks Git.
        if self
            .retained_diff()
            .is_some_and(|retained| retained.restyle)
            && self.wanted_working_diff().is_some()
            && self.working_diff.error.is_none()
        {
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
        if self.mutation_refresh_pending && !basis.is_immutable() {
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
        self.document_watch = None;
        self.document_reading = false;
        self.document_reload_pending = false;
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
            self.fail_file_open(card, file.path, &error);
            return Task::none();
        }
        self.pending_fragment = None;
        self.pending_editor_navigation = None;
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
        let retain_buffer = reopening
            && matches!(&self.document, DocumentState::Editable(document)
                if document.dirty || document.conflict || document.durability_warning().is_some());
        // A retained Reload proposes replacement; its Find state still belongs
        // to the predecessor if the disk read fails. Close only the protocol.
        if retain_buffer {
            self.push_lsp_close_notification();
        } else {
            self.push_lsp_close();
        }
        self.committed_selection = None;
        self.selected = Some(file.clone());
        self.original_read_attested = false;
        self.reload_input = retain_buffer.then_some(self.last_editor_input);
        let predecessor = match std::mem::replace(&mut self.document, DocumentState::None) {
            DocumentState::Editable(mut document) if retain_buffer => {
                // Its old-generation callback can never be admitted after
                // Reload. Retain finished previews, not a permanently busy slot.
                if let Some(cancel) = document.diagram_cancel.take() {
                    cancel.0.cancel();
                }
                document.diagram_pending = None;
                Some(document)
            }
            _ => None,
        };
        let retained_source = predecessor.as_ref().map(|document| document.source.clone());
        self.document = DocumentState::Loading { predecessor };
        self.diffs.clear();
        self.expect_git_comparison();
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
        let watch = DocumentWatch::new();
        self.document_watch = Some(watch.clone());
        self.document_reading = true;
        self.document_reload_pending = false;
        file_task(
            identity,
            self.document_tone,
            self.original_editor_root.clone(),
            retained_source,
            Some(watch),
            false,
        )
    }

    /// A refused explicit read is a failure of that file, even when acquiring
    /// its directory lease failed before the read could start. Replace the
    /// departing pixels and fence their callbacks without releasing its lease.
    fn fail_file_open(&mut self, card: CardId, path: PathBuf, error: &std::io::Error) {
        let absolute = self.descriptor.worktree.join(&path);
        let message = local_file_error_detail(&absolute, error);
        self.files = FileListState::Failed(message.clone());
        // A refresh must never abandon a dirty buffer, recovery, or an
        // outstanding save merely because its directory became unavailable.
        if self.blocks_navigation() {
            return;
        }
        // A refused destination owns no document watch. A queued event from
        // the departed buffer must not reopen it without the protection that
        // just failed (nor reuse its cleared original-root attestation).
        self.document_watch = None;
        self.document_reading = false;
        self.document_reload_pending = false;
        self.push_lsp_close();
        self.pending_fragment = None;
        self.pending_editor_navigation = None;
        self.pending_editor_focus = None;
        self.restored_navigation = None;
        self.committed_selection = None;
        self.original_editor_root = None;
        self.original_read_attested = false;
        self.selected = Some(RepositoryFile {
            path: path.clone(),
            ..RepositoryFile::default()
        });
        if self.descriptor.selected_file.is_some() {
            self.descriptor.selected_file = Some(path.clone());
        }
        self.document = DocumentState::Failed {
            path: absolute,
            message,
        };
        self.diffs.clear();
        self.stale_diff = None;
        self.basis_resolution = None;
        self.source_surface = SourceSurface::Editor;
        self.markdown_mode = MarkdownMode::Edit;
        self.next_identity(card, path);
        self.file_list_generation = self.file_list_generation.wrapping_add(1).max(1);
        self.file_list_loading = false;
        self.file_list_started = None;
        self.file_list_refresh_queued = false;
        self.background_file_refresh = false;
        self.file_list_digest = None;
        self.file_list_read = None;
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
            let position = document.content.scroll_position();
            self.viewports.editor_moved |= position != self.viewports.editor;
            self.viewports.editor = position;
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

    /// The editor line `source` (one-based) starts on. The native buffer
    /// also breaks lines at a bare CR, which the comparison's rows do not.
    fn editor_line_for_source(&self, source: u32) -> usize {
        let line = source.saturating_sub(1) as usize;
        let DocumentState::Editable(document) = &self.document else {
            return line;
        };
        if document.content.text_snapshot().protocol_compatible_lines() {
            return line;
        }
        document.content.source_line_ranges().get(line).map_or_else(
            || document.content.line_count().saturating_sub(1),
            |range| range.start,
        )
    }

    /// The one-based source line holding editor line `line`.
    fn source_for_editor_line(&self, line: usize) -> u32 {
        let source = |line: usize| u32::try_from(line.saturating_add(1)).unwrap_or(u32::MAX);
        let DocumentState::Editable(document) = &self.document else {
            return source(line);
        };
        if document.content.text_snapshot().protocol_compatible_lines() {
            return source(line);
        }
        let ranges = document.content.source_line_ranges();
        source(
            ranges
                .partition_point(|range| range.end <= line)
                .min(ranges.len().saturating_sub(1)),
        )
    }

    fn editor_line_height(&self) -> f32 {
        self.editor_font_size * self.line_height
    }

    fn align_editor_to_diff(&mut self) {
        if self.source_surface != SourceSurface::Diff {
            return;
        }
        // Rows have not yet placed the line Diff was entered from: return
        // to exactly that line.
        let source = self.pending_diff_source.take().or_else(|| {
            let Some(diff_layout::Anchor::Row { row, fraction }) = self.current_diff_viewport().row
            else {
                return None;
            };
            let line = self.diff_model()?.new_line_at_row(self.diff_mode, row)?;
            Some(SourceLineAnchor {
                line,
                fraction,
                horizontal: self.diff_horizontal,
            })
        });
        if let Some(source) = source {
            self.viewports.editor = text_editor::ScrollPosition {
                line: self.editor_line_for_source(source.line),
                vertical: source.fraction as f32 * self.editor_line_height(),
                horizontal: source.horizontal,
            };
            self.viewports.editor_moved = false;
        }
    }

    /// Entering Diff from the editor shows the line the reader was on.
    ///
    /// An editor nobody has moved since it was placed says nothing about
    /// where to read: a freshly opened file keeps the first change, and a
    /// return from Diff keeps the comparison where it was left.
    fn align_diff_to_editor(&mut self) {
        if !self.editable_surface_visible() || !self.viewports.editor_moved {
            return;
        }
        let position = self.viewports.editor;
        self.pending_diff_source = Some(SourceLineAnchor {
            line: self.source_for_editor_line(position.line),
            fraction: f64::from((position.vertical / self.editor_line_height()).clamp(0.0, 1.0)),
            horizontal: position.horizontal,
        });
        self.settle_pending_diff_source();
    }

    /// Places the pending source line, and lets it go once the rows it was
    /// placed in describe the current buffer. A working comparison behind
    /// the buffer keeps it until the rebuild for the current revision.
    fn settle_pending_diff_source(&mut self) {
        let current = self.diff_basis != DiffBasis::IndexToWorktree
            || !matches!(self.document, DocumentState::Editable(_))
            || self.working_annotations_current();
        if self.place_pending_diff_source() && current {
            self.pending_diff_source = None;
        }
    }

    /// Scrolls the comparison to the pending source line, unfolding it if
    /// it is hidden. False while there are no rows to place it in.
    fn place_pending_diff_source(&mut self) -> bool {
        let Some(source) = self.pending_diff_source else {
            return false;
        };
        let Some(row) = self
            .diff_model()
            .and_then(|model| model.row_at_new_line(self.diff_mode, source.line))
        else {
            return false;
        };
        self.reveal_diff_row(row);
        // A deleted row shown above the line starts at its top; only the
        // line itself carries the editor's offset within it.
        let fraction = if self
            .diff_model()
            .is_some_and(|model| diff_row_lines(model, self.diff_mode, row).1 == Some(source.line))
        {
            source.fraction
        } else {
            Default::default()
        };
        // The other layout's saved place predates this line; switching
        // layouts now keeps the line on screen.
        self.viewports.diff = [None; 2];
        self.apply_diff_viewport(DiffViewportAnchor {
            row: Some(diff_layout::Anchor::Row { row, fraction }),
            horizontal: source.horizontal,
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

    fn rejects_reload_replacement(&self, result: &Result<LoadedFile, FileIoError>) -> bool {
        matches!(
            &self.document,
            DocumentState::Loading {
                predecessor: Some(_)
            }
        ) && !matches!(
            result,
            Ok(LoadedFile {
                content: FileContent::Editable { .. },
                ..
            })
        )
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

    /// The retained comparison, while it belongs to the selected file and
    /// basis.
    fn retained_diff(&self) -> Option<&RetainedDiff> {
        self.stale_diff.as_ref().filter(|retained| {
            retained.basis == self.diff_basis
                && self
                    .selected
                    .as_ref()
                    .is_some_and(|file| file.path == retained.path)
        })
    }

    fn displayed_diff_state(&self) -> Option<&DiffState> {
        let current = self.diff_state(&self.diff_basis);
        if matches!(current, Some(DiffState::Ready(..))) {
            return current;
        }
        self.retained_diff()
            .map(|retained| &retained.state)
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

    /// Opens just enough of the fold hiding model `row` to show it.
    fn reveal_diff_row(&mut self, row: usize) {
        let (basis, mode) = (self.diff_basis.clone(), self.diff_mode);
        let Some((_, DiffState::Ready(RenderedDiff::Text { model, .. }, layout))) = self
            .diffs
            .iter_mut()
            .find(|(candidate, _)| *candidate == basis)
        else {
            return;
        };
        if layout.reveal_row(model, mode, row) {
            self.diff_layout_revision = self.diff_layout_revision.wrapping_add(1);
        }
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
    previous: Option<FileInventoryDigest>,
    deposit: Arc<std::sync::Mutex<Option<(u64, FileInventoryDigest)>>>,
    cancellation: CancellationToken,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let generation = identity.generation;
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            if directory_only {
                load_directory_files(&worktree).map(|files| FileListRead::Listed(None, files))
            } else {
                read_repository_files(&worktree, previous, &cancellation)
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
fn file_task(
    identity: AsyncIdentity,
    tone: crate::editor::PdfTone,
    original: Option<Arc<crate::fs_safety::Root>>,
    retained_source: Option<PathBuf>,
    watch: Option<DocumentWatch>,
    background: bool,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || {
            let mut targets = vec![worktree.join(&path)];
            // A dangling link read names the refused link, so it cannot
            // rediscover its old target. Keep watching the retained buffer's
            // source for recreation, including across a failed Reload.
            if let Some(source) = retained_source
                && !targets.contains(&worktree.join(&source))
            {
                targets.push(worktree.join(source));
            }
            if let Some(watch) = &watch {
                watch.arm(targets.clone());
            }
            // A symlink can be retargeted while its source is discovered.
            // Accept only a read whose actual source was already watched.
            for _ in 0..4 {
                let revision = watch.as_ref().map(DocumentWatch::revision);
                let mut result =
                    crate::editor::read_file_at(&worktree, &path, tone, original.clone());
                if let (Some(watch), Ok(read)) = (&watch, &result)
                    && let Some(source) = read.source()
                    && !targets.contains(&worktree.join(source))
                {
                    targets.push(worktree.join(source));
                    watch.arm(targets.clone());
                    continue;
                }
                if let Ok(read) = &mut result {
                    read.watch_revision = revision;
                }
                return result;
            }
            Err(FileIoError::Io(
                "The file's link target keeps changing; retry opening the file".into(),
            ))
        })
        .await
        .map_err(|error| FileIoError::Io(format!("file worker failed: {error}")))
        .and_then(|result| result);
        if background {
            RepositoryAction::DocumentReloaded { identity, result }
        } else {
            RepositoryAction::FileLoadedAt { identity, result }
        }
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
    font: Font,
    cancellation: CancellationToken,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let requested = basis.clone();
    cosmic::task::future(async move {
        let content = tokio::task::spawn_blocking(move || {
            load_repository_diff(
                &worktree,
                &pathspec,
                &requested,
                old_path.as_deref(),
                syntax_theme,
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
    files: &mut WorkingFiles,
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
    // The folder may itself be the user's link, such as `~/.gemini` into a
    // dotfiles checkout. List the directory it names, as opening it would;
    // links among its entries are still listed rather than followed.
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let handle_root = crate::fs_safety::Root::open(&root).map_err(|error| error.to_string())?;
    let handle = if relative.as_os_str().is_empty() {
        std::fs::File::open(&root).map_err(|error| error.to_string())?
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
    Listed(Option<FileInventoryDigest>, Vec<RepositoryFile>),
}

/// Reads the repository's file list through [`RepositoryService`], reporting
/// [`FileListRead::Unchanged`] when Git's raw output is byte-for-byte what
/// `previous` digested, so an idle card's ten-second refresh tick costs two
/// bounded Git reads and a hash rather than a rebuilt list and a repaint.
fn read_repository_files(
    worktree: &Path,
    previous: Option<FileInventoryDigest>,
    cancellation: &CancellationToken,
) -> Result<FileListRead, String> {
    match RepositoryService::system("git")
        .file_inventory(worktree, previous, cancellation)
        .map_err(|error| format!("Git failed while listing repository files: {error}"))?
    {
        FileInventoryRead::Unchanged => Ok(FileListRead::Unchanged),
        FileInventoryRead::Changed(inventory) => Ok(FileListRead::Listed(
            Some(inventory.digest),
            repository_files(&inventory.files, &inventory.status),
        )),
    }
}

/// One path's porcelain status, gathered from its per-area entries.
#[derive(Debug, Default)]
struct FileStatus {
    staged: Option<u8>,
    unstaged: Option<u8>,
    staged_old_path: Option<OldPath>,
    unstaged_old_path: Option<OldPath>,
}

impl FileStatus {
    /// The two-letter `XY` code the card shows, `??` for an untracked file.
    fn display_code(&self) -> String {
        match (self.staged, self.unstaged) {
            (None, Some(b'?')) => "??".into(),
            (staged, unstaged) => [staged.unwrap_or(b' '), unstaged.unwrap_or(b' ')]
                .map(char::from)
                .into_iter()
                .collect(),
        }
    }
}

/// The card's rows: every indexed and untracked file, plus status-only paths
/// such as staged deletions, in path order.
fn repository_files(files: &WorkingFiles, status: &StatusSnapshot) -> Vec<RepositoryFile> {
    let mut statuses = HashMap::<&Path, FileStatus>::new();
    for entry in &status.entries {
        let status = statuses.entry(entry.path.as_path()).or_default();
        match entry.area {
            ChangeArea::Staged => {
                status.staged = Some(entry.kind.code());
                status.staged_old_path = OldPath::from_changed_path(entry);
            }
            ChangeArea::Unstaged => {
                status.unstaged = Some(entry.kind.code());
                status.unstaged_old_path = OldPath::from_changed_path(entry);
            }
            ChangeArea::Untracked => status.unstaged = Some(b'?'),
            ChangeArea::Conflict => {
                let [staged, unstaged] = entry.conflict.map_or([b'U', b'U'], ConflictKind::code);
                status.staged = Some(staged);
                status.unstaged = Some(unstaged);
            }
        }
    }
    let mut paths = files
        .paths
        .iter()
        .map(RepoPath::as_path)
        .collect::<BTreeSet<_>>();
    // A rename source no longer exists under that name, so the destination
    // row stands for it. A copy source is still a file of its own.
    for status in statuses.values() {
        for source in [&status.staged_old_path, &status.unstaged_old_path]
            .into_iter()
            .flatten()
            .filter_map(OldPath::rename_source)
        {
            paths.remove(source);
        }
    }
    paths.extend(statuses.keys().copied());
    paths
        .into_iter()
        .map(|path| {
            let status = statuses.get(path);
            RepositoryFile {
                path: path.to_owned(),
                staged_old_path: status.and_then(|status| status.staged_old_path.clone()),
                unstaged_old_path: status.and_then(|status| status.unstaged_old_path.clone()),
                staged: status.is_some_and(|status| status.staged.is_some()),
                unstaged: status.is_some_and(|status| status.unstaged.is_some()),
                status: status.map(FileStatus::display_code),
                directory: false,
            }
        })
        .collect()
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
                    information_state("Could not list files", error.clone()),
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

fn replace_focus_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.replace"))
}

pub fn focus_restore_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.view"))
}
fn branch_comparison_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round"><circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="5" r="2"/><path d="M6 7v10M18 7c0 7-12 3-12 10"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
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

impl widget::menu::Action for EditorMenuCommand {
    type Message = RepositoryAction;

    fn message(&self) -> RepositoryAction {
        match *self {
            Self::Undo(card) => RepositoryAction::Undo { card },
            Self::Redo(card) => RepositoryAction::Redo { card },
            Self::Clipboard(card, op) => RepositoryAction::Clipboard { card, op },
        }
    }
}

/// The editor's context menu: history, then the clipboard, each entry
/// showing the chord that does the same thing.
fn editor_context_menu(
    card: CardId,
    document: &EditorDocument,
) -> Vec<widget::menu::Tree<RepositoryAction>> {
    use widget::menu::{Item, KeyBind, key_bind::Modifier};
    let chord = |modifiers: &[Modifier], key: &str| KeyBind {
        modifiers: modifiers.to_vec(),
        key: iced::keyboard::Key::Character(key.into()),
    };
    let ctrl = [Modifier::Ctrl];
    let entries = [
        (
            "Undo",
            EditorMenuCommand::Undo(card),
            chord(&ctrl, "z"),
            document.history.can_undo(),
        ),
        (
            "Redo",
            EditorMenuCommand::Redo(card),
            chord(&[Modifier::Ctrl, Modifier::Shift], "z"),
            document.history.can_redo(),
        ),
        (
            "Cut",
            EditorMenuCommand::Clipboard(card, ClipboardOp::Cut),
            chord(&ctrl, "x"),
            true,
        ),
        (
            "Copy",
            EditorMenuCommand::Clipboard(card, ClipboardOp::Copy),
            chord(&ctrl, "c"),
            true,
        ),
        (
            "Paste",
            EditorMenuCommand::Clipboard(card, ClipboardOp::Paste),
            chord(&ctrl, "v"),
            true,
        ),
        (
            "Select all",
            EditorMenuCommand::Clipboard(card, ClipboardOp::SelectAll),
            chord(&ctrl, "a"),
            true,
        ),
    ];
    let key_binds = entries
        .iter()
        .map(|(_, command, chord, _)| (chord.clone(), *command))
        .collect::<HashMap<_, _>>();
    let mut items = Vec::with_capacity(entries.len() + 1);
    for (index, (label, command, _, enabled)) in entries.into_iter().enumerate() {
        if index == 2 {
            items.push(Item::Divider);
        }
        items.push(if enabled {
            Item::Button(label, None, command)
        } else {
            Item::ButtonDisabled(label, None, command)
        });
    }
    widget::menu::items(&key_binds, items)
}

impl widget::menu::Action for ExternalEditorCommand<'_> {
    type Message = RepositoryAction;

    fn message(&self) -> RepositoryAction {
        RepositoryAction::OpenExternal {
            card: self.0,
            editor_id: self.1.to_owned(),
        }
    }
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
        // A path is told apart by its end, so it gives way at its start.
        let one_line = |text: crate::ui_controls::FadeText<'static>| {
            text.edge(crate::ui_controls::FadeEdge::Start)
        };
        let mut label: Vec<Element<'_, RepositoryAction>> = Vec::new();
        let caption = linked.map(|source| {
            if runtime.standalone {
                format!(
                    "{} \u{2192} {}",
                    file_name(&selected.path),
                    file_name(source)
                )
            } else {
                format!("\u{2192} {}", file_name(source))
            }
        });
        let caption =
            caption.map(|caption| one_line(crate::ui_controls::FadeText::caption(caption)));
        if runtime.standalone {
            label.extend(caption.map(Element::from));
        } else {
            label.push(crate::file_icon::icon(&selected.path).into());
            let path = one_line(crate::ui_controls::fade_text(
                selected.path.to_string_lossy().into_owned(),
            ));
            // The path gives way to the caption when there is one; on its
            // own it fills the slot as it always has.
            label.push(match caption {
                Some(caption) => crate::ui_controls::trailing(path, caption, space::S),
                None => path.width(Length::Fill).into(),
            });
        }
        let tooltip = match linked {
            Some(source) => format!("{} \u{2192} {}", selected.path.display(), source.display()),
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
    if !document_actions.is_empty() {
        controls.push(toolbar_group(document_actions));
    }
    // Diff navigation follows the file it compares: view modes, file
    // actions, then diff scope.
    if !runtime.directory_only && !runtime.reviews_pull_request() {
        controls.push(diff_basis_controls(card, runtime));
    }
    if runtime.source_surface == SourceSurface::Diff {
        controls.push(diff_review_controls(card, runtime));
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
        .on_press_maybe(runtime.can_open_external().then(|| {
            RepositoryAction::OpenExternal {
                card,
                editor_id: editor.id.clone(),
            }
        }));
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
            secondary.push(crate::interaction::context_menu(
                button,
                widget::menu::items(&HashMap::new(), entries),
            ));
        }
    }
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
    let Some(find) = find_strip(card, runtime) else {
        return Some(toolbar.into());
    };
    Some(
        widget::column::with_children(vec![toolbar.into(), find.map(map)])
            .width(Length::Fill)
            .into(),
    )
}

/// The find strip, while find is open over a buffer that is on screen.
fn find_strip<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
) -> Option<Element<'a, RepositoryAction>> {
    if !runtime.find_open || !runtime.editable_surface_visible() {
        return None;
    }
    let matched = runtime.find_total != 0;
    let mut row: Vec<Element<'a, RepositoryAction>> = vec![
        icon_button(Icon::Search, "Find and replace")
            .selected(runtime.replace_open)
            .on_press(RepositoryAction::ToggleReplace { card })
            .into(),
        crate::interaction::focused_key(
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
                .width(Length::Fill),
            move |event| {
                let keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    modifiers,
                    ..
                } = event
                else {
                    return None;
                };
                match key {
                    keyboard::key::Named::Escape => Some(RepositoryAction::CloseFind { card }),
                    keyboard::key::Named::Enter
                        if !modifiers.control() && !modifiers.alt() && !modifiers.logo() =>
                    {
                        Some(if modifiers.shift() {
                            RepositoryAction::FindPrevious { card }
                        } else {
                            RepositoryAction::FindNext { card }
                        })
                    }
                    _ => None,
                }
            },
        ),
    ];
    // A count is read off the cache the search filled in; an empty query has
    // not searched for anything and says nothing rather than "No results".
    if !runtime.find_query.is_empty() {
        row.push(
            crate::design::caption(if runtime.find_error.is_some() {
                "Invalid expression".into()
            } else if runtime.find_cache.as_ref() != runtime.wanted_find().as_ref() {
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
    let selection_enabled =
        runtime.find_selection.is_some() || runtime.find_selection_candidate.is_some();
    let mut selection =
        crate::ui_controls::field::checkbox(runtime.find_selection.is_some()).label("Selection");
    if selection_enabled {
        selection = selection
            .on_toggle(move |enabled| RepositoryAction::FindSelectionChanged { card, enabled });
    }
    options.push(crate::interaction::tooltip(
        selection,
        "Search the selection captured when enabled",
    ));
    strips.push(
        widget::row::with_children(options)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
    );
    if let Some(error) = &runtime.find_error {
        strips.push(
            widget::text::body(format!("Find: {error}"))
                .size(12)
                .class(cosmic::theme::Text::Custom(crate::design::destructive_text))
                .into(),
        );
    }
    if runtime.find_replacement.is_some() {
        strips.push(
            widget::text::body("Preparing replacements… You can keep editing to cancel.")
                .size(12)
                .into(),
        );
    }
    if runtime.replace_open {
        strips.push(
            widget::row::with_children(vec![
                crate::interaction::focused_escape(
                    crate::ui_controls::field::text("Replace with", &runtime.replace_query)
                        .id(replace_focus_id(card))
                        .on_focus(RepositoryAction::ReplaceInputFocus { card })
                        .on_input(move |query| RepositoryAction::ReplaceQueryChanged {
                            card,
                            query,
                        })
                        .width(Length::Fill),
                    move || RepositoryAction::CloseFind { card },
                ),
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
                },
                crate::ui_controls::button::secondary("Replace")
                    .on_press_maybe(
                        (matched && !runtime.is_saving() && runtime.find_replacement.is_none())
                            .then_some(RepositoryAction::ReplaceMatch { card, all: false }),
                    )
                    .into(),
                crate::ui_controls::button::secondary("Replace all")
                    .on_press_maybe(
                        (matched && !runtime.is_saving() && runtime.find_replacement.is_none())
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

fn local_file_error_detail(path: &Path, error: impl std::fmt::Display) -> String {
    format!("{} on this device: {error}", path.display())
}

fn editor_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let content = match &runtime.document {
        DocumentState::None => information_state(
            "No file selected",
            "Choose a file from the repository list.",
        ),
        DocumentState::Loading { .. } => loading(),
        DocumentState::Failed { path, message } => {
            let name = path.file_name().unwrap_or(path.as_os_str());
            information_state(
                format!("Could not open {}", name.to_string_lossy()),
                message,
            )
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
        ),
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
    let mut rows = Vec::new();
    if let Some(error) = runtime
        .document_watch
        .as_ref()
        .and_then(DocumentWatch::error)
    {
        rows.push(
            widget::row::with_children(vec![
                crate::design::body(format!("Live updates unavailable: {error}"))
                    .width(Length::Fill)
                    .into(),
                icon_button(Icon::Refresh, "Retry live updates")
                    .on_press(RepositoryAction::RetryDocumentWatch { card })
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        );
    }
    rows.push(content);
    widget::column::with_children(rows)
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
    controls.push(crate::interaction::tooltip(
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
    ));
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
        ),
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

fn completion_row(
    card: CardId,
    runtime: &RepositoryRuntime,
    index: usize,
) -> Option<Element<'_, RepositoryAction>> {
    let item = runtime.language.completions.get(index)?;
    let target = runtime.completion_target(card, index)?;
    let key = target.native_key();
    let label = crate::design::body(&item.label).width(Length::Fill);
    let detail = crate::ui_controls::FadeText::caption(item.detail.as_deref().unwrap_or_default());
    let button = widget::button::custom(widget::column::with_children(vec![
        label.into(),
        detail.into(),
    ]))
    .class(crate::ui_controls::control_style(
        index == runtime.language.selected,
        None,
    ))
    .width(Length::Fill)
    .on_press(RepositoryAction::Language {
        card,
        action: EditorLspAction::Choose { index, target },
    });
    Some(crate::ui_controls::owned(key, button))
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
        if document.conflict || document.durability_warning().is_some() {
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
        } else if document.durability_warning().is_some() {
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
        let has_completions =
            runtime.language.completion_popup && !runtime.language.completions.is_empty();
        let popup: Option<Element<'a, RepositoryAction>> = if has_completions {
            let selected = runtime.language.selected;
            let first = selected.saturating_sub(4);
            let choices = runtime
                .language
                .completions
                .iter()
                .enumerate()
                .skip(first)
                .take(10)
                .filter_map(|(index, _)| completion_row(card, runtime, index))
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
        } else if runtime.language.hover_diagnostic.is_some() || !runtime.language.hover.is_empty()
        {
            let settings = crate::design::markdown_settings(
                &cosmic::theme::active(),
                crate::design::TEXT_BODY,
            );
            let copy: Option<Element<'_, RepositoryAction>> = runtime
                .language
                .hover_diagnostic
                .as_ref()
                .map(|diagnostic| {
                    widget::button::text("Copy full diagnostic")
                        .on_press(RepositoryAction::CopyDiagnostic {
                            card,
                            diagnostic: diagnostic.clone(),
                        })
                        .into()
                });
            let rows: Vec<Element<'_, RepositoryAction>> = if let Some(diagnostic) =
                &runtime.language.hover_diagnostic
            {
                vec![
                    crate::design::body(diagnostic.message_preview().into_owned())
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .width(Length::Fill)
                        .into(),
                ]
            } else {
                runtime
                    .language
                    .hover
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        markdown::item(&crate::document_link::PreviewViewer, settings, item, index)
                            .map(move |uri| RepositoryAction::MarkdownLink { card, uri })
                    })
                    .collect()
            };
            Some(
                widget::container(
                    widget::scrollable(
                        widget::column::with_children(rows)
                            .push_maybe(copy)
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
                runtime.find_open.then_some(&runtime.find_matches),
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
                if let Some(command) = text_editor::history_command(&press) {
                    return Some(text_editor::Binding::Custom(match command {
                        text_editor::HistoryCommand::Undo => RepositoryAction::Undo { card },
                        text_editor::HistoryCommand::Redo => RepositoryAction::Redo { card },
                    }));
                }
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
                    Key::Named(Named::F8)
                        if !press.modifiers.control()
                            && !press.modifiers.alt()
                            && !press.modifiers.logo() =>
                    {
                        Some(EditorLspAction::Diagnostic(!press.modifiers.shift()))
                    }
                    Key::Named(Named::F12) => Some(if press.modifiers.shift() {
                        EditorLspAction::References
                    } else {
                        EditorLspAction::Definition(None)
                    }),
                    Key::Named(Named::Escape)
                        if has_completions
                            || !runtime.language.hover.is_empty()
                            || runtime.language.hover_diagnostic.is_some() =>
                    {
                        Some(EditorLspAction::Dismiss)
                    }
                    Key::Named(Named::ArrowDown) if has_completions => {
                        Some(EditorLspAction::Next(true))
                    }
                    Key::Named(Named::ArrowUp) if has_completions => {
                        Some(EditorLspAction::Next(false))
                    }
                    Key::Named(Named::Tab | Named::Enter) if has_completions => {
                        let index = runtime.language.selected;
                        runtime
                            .completion_target(card, index)
                            .map(|target| EditorLspAction::Choose { index, target })
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
        crate::interaction::context_menu(editor, editor_context_menu(card, document))
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
        Some(BasisResolution::NoCommits | BasisResolution::Unavailable(_)) => None,
        None => basis_choice(&runtime.diff_basis),
    };
    let unstaged = runtime.can_select_diff_basis(&DiffBasis::IndexToWorktree);
    let staged = runtime.can_select_diff_basis(&DiffBasis::HeadToIndex);
    crate::ui_controls::segmented_controls(vec![
        icon_button(Icon::Changes, "Unstaged changes")
            .selected(unstaged && selected == Some(BasisChoice::Unstaged))
            .on_press_maybe(unstaged.then(|| RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::IndexToWorktree,
            }))
            .into(),
        icon_button(Icon::Select, "Staged changes")
            .selected(staged && selected == Some(BasisChoice::Staged))
            .on_press_maybe(staged.then(|| RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::HeadToIndex,
            }))
            .into(),
        icon_button(branch_comparison_icon(), "Compare branch with base")
            .selected(
                runtime.can_select_committed_basis(CommittedChoice::Branch)
                    && selected == Some(BasisChoice::Committed(CommittedChoice::Branch)),
            )
            .on_press_maybe(
                runtime
                    .can_select_committed_basis(CommittedChoice::Branch)
                    .then(|| RepositoryAction::SelectCommittedBasis {
                        card,
                        choice: CommittedChoice::Branch,
                    }),
            )
            .into(),
        icon_button(Icon::History, "Last commit changes")
            .selected(
                runtime.can_select_committed_basis(CommittedChoice::LastCommit)
                    && selected == Some(BasisChoice::Committed(CommittedChoice::LastCommit)),
            )
            .on_press_maybe(
                runtime
                    .can_select_committed_basis(CommittedChoice::LastCommit)
                    .then(|| RepositoryAction::SelectCommittedBasis {
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
            icon_button(Icon::List, "Inline diff")
                .selected(runtime.can_change_diff_layout() && runtime.diff_mode == DiffMode::Inline)
                .on_press_maybe(
                    runtime
                        .can_change_diff_layout()
                        .then(|| RepositoryAction::ApplyDiffMode { card, split: false }),
                )
                .into(),
            icon_button(Icon::Diff, "Side-by-side diff")
                .selected(runtime.can_change_diff_layout() && runtime.diff_mode == DiffMode::Split)
                .on_press_maybe(
                    runtime
                        .can_change_diff_layout()
                        .then(|| RepositoryAction::ApplyDiffMode { card, split: true }),
                )
                .into(),
        ]),
        // Navigation commands are a plain cluster, distinct from mode choices.
        toolbar_group(vec![
            icon_button(Icon::ChevronUp, "Previous change")
                .on_press_maybe(
                    (runtime.can_navigate_diff() && runtime.change_target(false).is_some())
                        .then(|| RepositoryAction::PreviousChange { card }),
                )
                .into(),
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
            icon_button(Icon::ChevronDown, "Next change")
                .on_press_maybe(
                    (runtime.can_navigate_diff() && runtime.change_target(true).is_some())
                        .then(|| RepositoryAction::NextChange { card }),
                )
                .into(),
        ]),
    ];
    if runtime.can_retry_diff() {
        controls.push(toolbar_group(vec![
            icon_button(Icon::Refresh, "Retry diff")
                .on_press(RepositoryAction::RetryDiff { card })
                .into(),
        ]));
    }
    if let Some(model) = model
        && let counts = crate::repository::line_change_counts(
            Some(u64::from(model.additions)),
            Some(u64::from(model.deletions)),
        )
        && !counts.is_empty()
    {
        controls.push(crate::interaction::tooltip(
            crate::ui_controls::change_counts(&counts),
            match runtime.diff_state(&runtime.diff_basis) {
                Some(DiffState::Ready(
                    RenderedDiff::Text {
                        old_label,
                        new_label,
                        ..
                    },
                    _,
                )) => format!(
                    "{}: {old_label} → {new_label}",
                    basis_label(&runtime.diff_basis)
                ),
                _ => basis_label(&runtime.diff_basis),
            },
        ));
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
    let rows: Element<'a, RepositoryAction> = if model.is_empty() && review.is_none() {
        information_state(
            "No content changes",
            format!("{old_label} and {new_label} hold identical text."),
        )
    } else {
        crate::ui_controls::responsive(move |size| {
            // One native two-axis scrollable owns both coordinates. The canvas
            // counter-translates the fixed gutters/center; only code moves in X.
            let typography = runtime.diff_typography();
            let gutter = typography.gutter_width(model.row_count(DiffMode::Inline));
            let code_width = typography.code_width(model);
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
                        typography,
                    ),
                    DiffMode::Split => split_diff_row_with_review(
                        &model.split_rows[row],
                        &model.syntax,
                        gutter,
                        code_width,
                        horizontal,
                        context.map(|ctx| (ctx.line_action(old, None), ctx.line_action(None, new))),
                        typography,
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
                    row_height: typography.line_height,
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

/// The editor's family, size and line height, which every diff row uses.
#[derive(Debug, Clone, Copy, PartialEq)]
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

impl DiffTypography {
    /// The widest line at this size. The model measured its own family at
    /// [`DIFF_FONT_SIZE`], and advances scale linearly with the size.
    fn code_width(self, model: &DiffModel) -> f32 {
        (model.max_line_width[usize::from(cosmic::theme::is_dark())] as f32 * self.size
            / DIFF_FONT_SIZE)
            .max(1.0)
    }

    /// Line numbers keep their proportion to the code: 12 px beside the
    /// default 14 px editor font.
    fn gutter_size(self) -> f32 {
        self.size * 12.0 / DIFF_FONT_SIZE
    }

    /// A line-number column wide enough for `rows`' numbers at
    /// [`Self::gutter_size`], and never fewer than four digits.
    fn gutter_width(self, rows: usize) -> f32 {
        let digits = (rows.max(1).ilog10() + 1).max(4) as f32;
        digits * self.gutter_size() * 2.0 / 3.0 + 12.0
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
) -> Element<'a, RepositoryAction> {
    let marker = match row.kind {
        DiffRowKind::Addition => "+",
        DiffRowKind::Deletion => "−",
        _ => " ",
    };
    diff_container(
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
            ),
        ])
        .align_y(Alignment::Center),
        row.kind,
        Length::FillPortion(1),
        typography,
    )
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
) -> Element<'a, RepositoryAction> {
    diff_container(
        widget::row::with_children(vec![
            diff_review_gutter(lines.0.or(lines.1), gutter, action, typography),
            diff_text(
                text, changed, syntax, kind, code_width, horizontal, lines, typography,
            ),
        ])
        .align_y(Alignment::Center),
        kind,
        Length::FillPortion(1),
        typography,
    )
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
        ),
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
            .size(typography.gutter_size())
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
    for span in &mut spans {
        // Syntax keeps its weight and style in the editor's family, as the
        // model measured it.
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
            message
        });
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
    /// The code row's own height; the rest of a slot is its annotation.
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

impl DiffHorizontalContent<'_> {
    /// The translated content, still in the enclosing scrollable's
    /// coordinates. The real X translation is part of the child's layout, so
    /// it must not consume the virtual offset descendants use for screen
    /// positions, popup anchors and operation bounds.
    fn content_layout(layout: Layout<'_>) -> Layout<'_> {
        layout
            .children()
            .next()
            .expect("diff content")
            .with_virtual_offset(layout.virtual_offset())
    }
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
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        let Some(child) = layout.children().next() else {
            return;
        };
        let mut inner = cosmic::iced::advanced::clipboard::DndDestinationRectangles::new();
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            child.with_virtual_offset(layout.virtual_offset()),
            renderer,
            &mut inner,
        );
        let mut rectangles = inner.into_rectangles();
        crate::workspace_widget::clip_drag_destinations(&mut rectangles, layout.bounds());
        destinations.append(&mut rectangles);
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
            Self::content_layout(layout),
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
            Self::content_layout(layout),
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
                    Self::content_layout(layout),
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
            Self::content_layout(layout),
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
            Self::content_layout(layout),
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
        crate::ui_controls::accessible_bounds::place(&mut accessible, offset, Some(clip));
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
mod tests;
