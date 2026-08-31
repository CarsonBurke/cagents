//! Native repository-card state, asynchronous file review tasks, and focused UI.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

use cosmic::{
    Element,
    iced::{self, Alignment, Font, Length, Task},
    widget::{self, markdown, segmented_button, text_editor},
};

use crate::{
    diff_view::{
        DiffBasis, DiffContent, DiffMode, DiffModel, DiffRowKind, InlineDiffRow, SplitDiffRow,
        load_repository_diff,
    },
    editor::{
        FileContent, FileIoError, ReadOnlyReason, SaveDurability, SaveOutcome, load_file,
        save_file, validate_repo_path,
    },
    model::{CardId, RepositoryView},
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

    fn has_basis(&self, basis: DiffBasis) -> bool {
        match basis {
            DiffBasis::Staged => self.staged,
            DiffBasis::Unstaged => self.unstaged,
        }
    }

    fn previous_path(&self, basis: DiffBasis) -> Option<PathBuf> {
        match basis {
            DiffBasis::Staged => self.staged_old_path.clone(),
            DiffBasis::Unstaged => self.unstaged_old_path.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncIdentity {
    pub card: CardId,
    pub worktree: PathBuf,
    pub path: PathBuf,
    pub generation: u64,
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
        content: DiffContent,
    },
    EditorAction {
        card: CardId,
        action: text_editor::Action,
    },
    Save {
        card: CardId,
        overwrite: bool,
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
    SelectSource {
        card: CardId,
        entity: segmented_button::Entity,
    },
    ApplySourceSurface {
        card: CardId,
        diff: bool,
    },
    SelectMarkdown {
        card: CardId,
        entity: segmented_button::Entity,
    },
    ApplyMarkdownMode {
        card: CardId,
        preview: bool,
    },
    SelectDiffMode {
        card: CardId,
        entity: segmented_button::Entity,
    },
    ApplyDiffMode {
        card: CardId,
        split: bool,
    },
    SelectDiffBasis {
        card: CardId,
        entity: segmented_button::Entity,
    },
    ApplyDiffBasis {
        card: CardId,
        staged: bool,
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
            | Self::SelectSource { card, .. }
            | Self::ApplySourceSurface { card, .. }
            | Self::SelectMarkdown { card, .. }
            | Self::ApplyMarkdownMode { card, .. }
            | Self::SelectDiffMode { card, .. }
            | Self::ApplyDiffMode { card, .. }
            | Self::SelectDiffBasis { card, .. }
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

struct TwoChoiceControl<T> {
    model: segmented_button::SingleSelectModel,
    first: segmented_button::Entity,
    second: segmented_button::Entity,
    first_value: T,
    second_value: T,
}

impl<T: Copy> TwoChoiceControl<T> {
    fn new(
        first_label: &'static str,
        first_value: T,
        second_label: &'static str,
        second_value: T,
    ) -> Self {
        let mut model = segmented_button::Model::builder()
            .insert(|builder| builder.text(first_label).activate())
            .insert(|builder| builder.text(second_label))
            .build();
        Self {
            first: model.entity_at(0).expect("first segmented choice exists"),
            second: model.entity_at(1).expect("second segmented choice exists"),
            model,
            first_value,
            second_value,
        }
    }

    fn activate(&mut self, entity: segmented_button::Entity) -> Option<T> {
        let value = if entity == self.first {
            self.first_value
        } else if entity == self.second {
            self.second_value
        } else {
            return None;
        };
        self.model.activate(entity);
        Some(value)
    }

    fn activate_value(&mut self, value: T)
    where
        T: PartialEq,
    {
        self.model.activate(if value == self.first_value {
            self.first
        } else {
            self.second
        });
    }
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
            self.conflict = false;
            self.error = None;
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
    Loading,
    Ready(DiffContent),
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
    /// Epoch for selected-document, diff, and save work. File-list refreshes
    /// deliberately use a separate epoch so they cannot obsolete an active
    /// document operation.
    generation: u64,
    file_list_generation: u64,
    files: FileListState,
    selected: Option<RepositoryFile>,
    document: DocumentState,
    diffs: BTreeMap<DiffBasis, DiffState>,
    source_surface: SourceSurface,
    markdown_mode: MarkdownMode,
    diff_mode: DiffMode,
    diff_basis: DiffBasis,
    source_control: TwoChoiceControl<SourceSurface>,
    markdown_control: TwoChoiceControl<MarkdownMode>,
    diff_control: TwoChoiceControl<DiffMode>,
    diff_basis_control: TwoChoiceControl<DiffBasis>,
    diff_change: usize,
    diff_scroll_id: widget::Id,
    file_viewport: ListViewport,
    diff_viewport: ListViewport,
    next_save_ticket: u64,
}

/// Owned, renderer-free projection of the active repository source surface.
///
/// The contextual inspector is rebuilt independently from the repository card
/// widget tree, so it must consume a snapshot rather than reach into editor or
/// segmented-control widget state directly.
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
            generation: 0,
            file_list_generation: 0,
            files: FileListState::Loading,
            selected: None,
            document: DocumentState::None,
            diffs: BTreeMap::new(),
            source_surface: SourceSurface::Editor,
            markdown_mode: MarkdownMode::Edit,
            diff_mode: DiffMode::Inline,
            diff_basis: DiffBasis::Unstaged,
            source_control: TwoChoiceControl::new(
                "Edit",
                SourceSurface::Editor,
                "Diff",
                SourceSurface::Diff,
            ),
            markdown_control: TwoChoiceControl::new(
                "Edit",
                MarkdownMode::Edit,
                "Preview",
                MarkdownMode::Preview,
            ),
            diff_control: TwoChoiceControl::new(
                "Inline",
                DiffMode::Inline,
                "Split",
                DiffMode::Split,
            ),
            diff_basis_control: TwoChoiceControl::new(
                "Staged",
                DiffBasis::Staged,
                "Unstaged",
                DiffBasis::Unstaged,
            ),
            diff_change: 0,
            diff_scroll_id: widget::Id::new(format!("repository.diff.{identity}")),
            file_viewport: ListViewport::default(),
            diff_viewport: ListViewport::default(),
            next_save_ticket: 0,
        }
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
        files_task(identity)
    }

    pub fn is_dirty(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.dirty)
    }

    pub fn can_save(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if (document.dirty || document.durability_warning.is_some()) && document.active_save.is_none())
    }

    pub fn is_saving(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.active_save.is_some())
    }

    pub fn blocks_navigation(&self) -> bool {
        self.is_dirty()
            || self.is_saving()
            || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning.is_some())
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
        let (diff_state, diff_error, diff_additions, diff_deletions, diff_change) = if diff {
            match self.diffs.get(&self.diff_basis) {
                None => (Some("No changes"), None, Some(0), Some(0), None),
                Some(DiffState::Loading) => (Some("Loading"), None, None, None, None),
                Some(DiffState::Ready(DiffContent::Text(model))) => {
                    let changes = model.change_starts(self.diff_mode).len();
                    (
                        Some(if changes == 0 { "No changes" } else { "Ready" }),
                        None,
                        Some(model.additions),
                        Some(model.deletions),
                        (changes != 0).then_some((self.diff_change + 1, changes)),
                    )
                }
                Some(DiffState::Ready(DiffContent::Error(error))) => {
                    (Some("Unavailable"), Some(error.clone()), None, None, None)
                }
                Some(DiffState::Ready(content)) => (
                    Some(match content {
                        DiffContent::Binary { .. } => "Binary",
                        DiffContent::InvalidUtf8 { .. } => "Invalid UTF-8",
                        DiffContent::Symlink { .. } => "Symlink",
                        DiffContent::Oversize { .. } => "Too large",
                        DiffContent::NotRegularFile => "Not a regular file",
                        DiffContent::Text(_) | DiffContent::Error(_) => unreachable!(),
                    }),
                    None,
                    None,
                    None,
                    None,
                ),
            }
        } else {
            (None, None, None, None, None)
        };
        let can_retry_diff = diff_error.is_some();
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
            diff_basis: diff.then_some(self.diff_basis.label()),
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
            can_discard: self.is_dirty() && !self.is_saving(),
            can_reload: conflict
                || matches!(&self.document, DocumentState::Editable(document) if document.durability_warning.is_some())
                || matches!(&self.document, DocumentState::Failed { .. }),
            can_retry_diff,
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
            _ if self.source_surface == SourceSurface::Diff => {
                match self.diffs.get(&self.diff_basis) {
                    Some(DiffState::Loading) => RepositoryRuntimeStatus::Loading,
                    Some(DiffState::Ready(DiffContent::Error(_))) => {
                        RepositoryRuntimeStatus::Unavailable
                    }
                    _ => RepositoryRuntimeStatus::Ready,
                }
            }
            _ => RepositoryRuntimeStatus::Ready,
        }
    }

    pub fn abandon_unsaved_changes(&mut self) {
        if let DocumentState::Editable(document) = &mut self.document {
            document.discard_edits();
        }
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

    pub fn request_save(&mut self, card: CardId, overwrite: bool) -> Task<RepositoryAction> {
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
        // Save failures and conflict recovery render with the editor document.
        // Force that surface before the worker starts so no completion can be
        // applied invisibly behind a diff or Markdown preview.
        self.source_surface = SourceSurface::Editor;
        self.source_control.activate_value(SourceSurface::Editor);
        self.markdown_mode = MarkdownMode::Edit;
        self.markdown_control.activate_value(MarkdownMode::Edit);
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
            RepositoryAction::FilesLoaded { identity, result } => {
                if !self.accepts_file_list(&identity) || identity.path != Path::new(".") {
                    return Task::none();
                }
                // A selection, its loaded diff bases, and an active save form
                // one snapshot. A file-list request may have run before a save
                // or an external index change, so never splice its per-path
                // status into that snapshot. Returning to the file list starts
                // another refresh before a new selection is made.
                self.files = result.map_or_else(FileListState::Failed, FileListState::Ready);
                Task::none()
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
                    Ok(FileContent::ReadOnly(reason)) => DocumentState::ReadOnly {
                        path: identity.path,
                        reason,
                    },
                    Err(error) => DocumentState::Failed {
                        path: identity.path,
                        message: error.to_string(),
                    },
                };
                Task::none()
            }
            RepositoryAction::DiffLoaded {
                identity,
                basis,
                content,
            } => {
                if self.accepts_selected(&identity) {
                    self.diffs.insert(basis, DiffState::Ready(content));
                    self.diff_change = 0;
                    self.diff_viewport = ListViewport::default();
                }
                Task::none()
            }
            RepositoryAction::EditorAction { action, .. } => {
                if let DocumentState::Editable(document) = &mut self.document {
                    document.apply(action);
                }
                Task::none()
            }
            RepositoryAction::Save { card, overwrite } => self.request_save(card, overwrite),
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
                if saved {
                    let Some(selected) = self.selected.as_mut() else {
                        return Task::none();
                    };
                    selected.unstaged = true;
                    self.diffs.insert(DiffBasis::Unstaged, DiffState::Loading);
                    diff_task(
                        identity,
                        DiffBasis::Unstaged,
                        selected.previous_path(DiffBasis::Unstaged),
                    )
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
                if self.is_saving() {
                    return Task::none();
                }
                if let DocumentState::Editable(document) = &mut self.document {
                    document.discard_edits();
                }
                Task::none()
            }
            RepositoryAction::BackToFiles { card } => {
                if self.blocks_navigation() {
                    return Task::none();
                }
                self.selected = None;
                self.document = DocumentState::None;
                self.diffs.clear();
                self.source_surface = SourceSurface::Editor;
                self.source_control.activate_value(SourceSurface::Editor);
                self.initial_load(card)
            }
            RepositoryAction::SelectSource { card, entity } => {
                if self.is_saving() {
                    self.source_control.activate_value(self.source_surface);
                    return Task::none();
                }
                if let Some(surface) = self.source_control.activate(entity)
                    && (surface == SourceSurface::Editor
                        || self.selected.as_ref().is_some_and(RepositoryFile::changed))
                {
                    return cosmic::task::message(RepositoryAction::ApplySourceSurface {
                        card,
                        diff: surface == SourceSurface::Diff,
                    });
                }
                Task::none()
            }
            RepositoryAction::ApplySourceSurface { diff, .. } => {
                let surface = if diff {
                    SourceSurface::Diff
                } else {
                    SourceSurface::Editor
                };
                if self.is_saving()
                    || (surface == SourceSurface::Diff
                        && !self.selected.as_ref().is_some_and(RepositoryFile::changed))
                {
                    self.source_control.activate_value(self.source_surface);
                    return Task::none();
                }
                self.source_surface = surface;
                self.source_control.activate_value(surface);
                Task::none()
            }
            RepositoryAction::SelectMarkdown { card, entity } => {
                if self.is_saving() {
                    self.markdown_control.activate_value(self.markdown_mode);
                    return Task::none();
                }
                if let Some(mode) = self.markdown_control.activate(entity) {
                    return cosmic::task::message(RepositoryAction::ApplyMarkdownMode {
                        card,
                        preview: mode == MarkdownMode::Preview,
                    });
                }
                Task::none()
            }
            RepositoryAction::ApplyMarkdownMode { preview, .. } => {
                if self.is_saving() {
                    self.markdown_control.activate_value(self.markdown_mode);
                    return Task::none();
                }
                self.markdown_mode = if preview {
                    MarkdownMode::Preview
                } else {
                    MarkdownMode::Edit
                };
                self.markdown_control.activate_value(self.markdown_mode);
                Task::none()
            }
            RepositoryAction::SelectDiffMode { card, entity } => {
                if let Some(mode) = self.diff_control.activate(entity) {
                    return cosmic::task::message(RepositoryAction::ApplyDiffMode {
                        card,
                        split: mode == DiffMode::Split,
                    });
                }
                Task::none()
            }
            RepositoryAction::ApplyDiffMode { split, .. } => {
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
            RepositoryAction::SelectDiffBasis { card, entity } => {
                if let Some(basis) = self.diff_basis_control.activate(entity)
                    && self
                        .selected
                        .as_ref()
                        .is_some_and(|file| file.has_basis(basis))
                {
                    return cosmic::task::message(RepositoryAction::ApplyDiffBasis {
                        card,
                        staged: basis == DiffBasis::Staged,
                    });
                }
                self.diff_basis_control.activate_value(self.diff_basis);
                Task::none()
            }
            RepositoryAction::ApplyDiffBasis { staged, .. } => {
                let basis = if staged {
                    DiffBasis::Staged
                } else {
                    DiffBasis::Unstaged
                };
                if !self
                    .selected
                    .as_ref()
                    .is_some_and(|file| file.has_basis(basis))
                {
                    self.diff_basis_control.activate_value(self.diff_basis);
                    return Task::none();
                }
                self.diff_basis = basis;
                self.diff_basis_control.activate_value(basis);
                self.diff_change = 0;
                self.diff_viewport = ListViewport::default();
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::RetryDiff { card } => {
                let Some(selected) = self.selected.as_ref() else {
                    return Task::none();
                };
                if !selected.has_basis(self.diff_basis) {
                    return Task::none();
                }
                let previous_path = selected.previous_path(self.diff_basis);
                let identity = self.current_identity(card, selected.path.clone());
                self.diffs.insert(self.diff_basis, DiffState::Loading);
                diff_task(identity, self.diff_basis, previous_path)
            }
            RepositoryAction::FileViewport { offset, height, .. } => {
                self.file_viewport = ListViewport { offset, height };
                Task::none()
            }
            RepositoryAction::DiffViewport { offset, height, .. } => {
                self.diff_viewport = ListViewport { offset, height };
                Task::none()
            }
            RepositoryAction::PreviousChange { .. } => {
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + changes - 1) % changes;
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::NextChange { .. } => {
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + 1) % changes;
                }
                self.realize_current_change();
                self.scroll_to_change()
            }
            RepositoryAction::MarkdownLink { .. } => Task::none(),
        }
    }

    fn open_file(&mut self, card: CardId, path: PathBuf) -> Task<RepositoryAction> {
        let Some(file) = self.file(&path).cloned() else {
            return Task::none();
        };
        self.selected = Some(file.clone());
        self.document = DocumentState::Loading(path.clone());
        self.diffs.clear();
        self.diff_viewport = ListViewport::default();
        for basis in [DiffBasis::Staged, DiffBasis::Unstaged] {
            if file.has_basis(basis) {
                self.diffs.insert(basis, DiffState::Loading);
            }
        }
        self.diff_basis = if file.unstaged {
            DiffBasis::Unstaged
        } else {
            DiffBasis::Staged
        };
        self.diff_basis_control.activate_value(self.diff_basis);
        self.source_surface = SourceSurface::Editor;
        self.source_control.activate_value(SourceSurface::Editor);
        self.markdown_mode = MarkdownMode::Edit;
        self.markdown_control.activate_value(MarkdownMode::Edit);
        self.diff_change = 0;
        let identity = self.next_identity(card, path);
        let file_task = file_task(identity.clone());
        let mut tasks = vec![file_task];
        for basis in [DiffBasis::Staged, DiffBasis::Unstaged] {
            if file.has_basis(basis) {
                tasks.push(diff_task(
                    identity.clone(),
                    basis,
                    file.previous_path(basis),
                ));
            }
        }
        Task::batch(tasks)
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
        match self.diffs.get(&self.diff_basis) {
            Some(DiffState::Ready(DiffContent::Text(model))) => Some(model),
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

fn files_task(identity: AsyncIdentity) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || load_repository_files(&worktree))
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
    old_path: Option<PathBuf>,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let content = tokio::task::spawn_blocking(move || {
            load_repository_diff(&worktree, &path, basis, old_path.as_deref())
        })
        .await
        .unwrap_or_else(|error| DiffContent::Error(format!("diff worker failed: {error}")));
        RepositoryAction::DiffLoaded {
            identity,
            basis,
            content,
        }
    })
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
    let (surface_key, body) = if runtime.selected.is_some() {
        (1_u8, selected_file_view(card, runtime))
    } else {
        (0, file_list_view(card, runtime))
    };
    // The file list and selected-file editor are both columns but have
    // unrelated stateful descendants. Key the surface so the reconciler does
    // not reuse a list row's stateless tree as editor state when a fast file
    // load skips the intermediate loading frame.
    iced::widget::keyed_column([(surface_key, body)])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

pub fn editor_focus_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.editor"))
}

pub fn focus_restore_id(card: CardId) -> widget::Id {
    widget::Id::new(format!("repository.{card}.back"))
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
            widget::icon::from_name("folder-symbolic").size(20).into(),
            widget::column::with_children(vec![
                widget::text::title3(title).into(),
                widget::text::caption(runtime.descriptor.worktree.display().to_string()).into(),
            ])
            .width(Length::Fill)
            .into(),
            widget::button::standard("New terminal here")
                .leading_icon(widget::icon::from_name("utilities-terminal-symbolic").size(16))
                .on_press(RepositoryAction::NewTerminal { card })
                .into(),
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
                    widget::icon::from_name(if file.changed() {
                        "document-edit-symbolic"
                    } else {
                        "text-x-generic-symbolic"
                    })
                    .size(16)
                    .into(),
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
        widget::divider::horizontal::light().into(),
        files,
    ])
    .height(Length::Fill)
    .into()
}

fn selected_file_view(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let spacing = cosmic::theme::spacing();
    let selected = runtime.selected.as_ref().expect("selected file exists");
    let mut controls: Vec<Element<'_, RepositoryAction>> = vec![
        widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
            .id(focus_restore_id(card))
            .name("Back to repository files")
            .on_press_maybe(
                (!runtime.blocks_navigation()).then_some(RepositoryAction::BackToFiles { card }),
            )
            .into(),
        widget::text::heading(selected.path.to_string_lossy())
            .width(Length::Fill)
            .into(),
    ];
    if selected.changed() {
        controls.push(
            widget::segmented_control::horizontal(&runtime.source_control.model)
                .on_activate(move |entity| RepositoryAction::SelectSource { card, entity })
                .into(),
        );
    }
    if runtime.is_dirty() {
        controls.push(widget::text::caption("Unsaved").into());
        controls.push(
            widget::button::standard("Discard changes")
                .on_press_maybe(
                    (!runtime.is_saving()).then_some(RepositoryAction::DiscardEdits { card }),
                )
                .into(),
        );
    }
    controls.push(
        widget::button::suggested("Save")
            .on_press_maybe(runtime.can_save().then_some(RepositoryAction::Save {
                card,
                overwrite: false,
            }))
            .into(),
    );
    let header = widget::container(
        widget::row::with_children(controls)
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
    )
    .padding([spacing.space_xxs, spacing.space_s]);
    let body = match runtime.source_surface {
        SourceSurface::Editor => editor_surface(card, runtime),
        SourceSurface::Diff => diff_surface(card, runtime),
    };
    widget::column::with_children(vec![
        header.into(),
        widget::divider::horizontal::light().into(),
        body,
    ])
    .height(Length::Fill)
    .width(Length::Fill)
    .into()
}

fn editor_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    let (state_key, content) = match &runtime.document {
        DocumentState::None => (
            0_u8,
            information_state(
                "No file selected",
                "Choose a file from the repository list.",
            ),
        ),
        DocumentState::Loading(path) => (1, loading(format!("Loading {}…", path.display()))),
        DocumentState::Failed { path, message } => (
            2,
            information_state(format!("Could not open {}", path.display()), message),
        ),
        DocumentState::ReadOnly { path, reason } => (3, read_only_state(path, reason)),
        // The key forces Iced to build a fresh subtree when an asynchronous
        // load replaces a stateless placeholder with the stateful editor.
        // A generic wrapper is insufficient because its own identity can be
        // retained while its child still carries the placeholder's state.
        DocumentState::Editable(document) => (4, editable_document_view(card, runtime, document)),
    };
    iced::widget::keyed_column([(state_key, content)])
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
    let mut rows: Vec<Element<'a, RepositoryAction>> = Vec::new();
    if document.markdown {
        rows.push(
            widget::container(
                widget::segmented_control::horizontal(&runtime.markdown_control.model)
                    .on_activate(move |entity| RepositoryAction::SelectMarkdown { card, entity }),
            )
            .padding([spacing.space_xxs, spacing.space_s])
            .into(),
        );
        rows.push(widget::divider::horizontal::light().into());
    }
    if let Some(error) = document
        .error
        .as_ref()
        .or(document.durability_warning.as_ref())
    {
        let mut actions: Vec<Element<'a, RepositoryAction>> =
            vec![widget::text::body(error).width(Length::Fill).into()];
        if document.conflict || document.durability_warning.is_some() {
            actions.push(
                widget::button::standard("Reload")
                    .on_press(RepositoryAction::Reload { card })
                    .into(),
            );
        }
        if document.conflict {
            actions.push(
                widget::button::destructive("Overwrite")
                    .on_press(RepositoryAction::Save {
                        card,
                        overwrite: true,
                    })
                    .into(),
            );
        } else if document.durability_warning.is_some() {
            actions.push(
                widget::button::suggested("Retry save")
                    .on_press(RepositoryAction::Save {
                        card,
                        overwrite: false,
                    })
                    .into(),
            );
        }
        rows.push(
            widget::container(widget::row::with_children(actions).spacing(spacing.space_xxs))
                .class(cosmic::theme::Container::Secondary)
                .padding(spacing.space_xs)
                .into(),
        );
    }
    let content: Element<'a, RepositoryAction> =
        if document.markdown && runtime.markdown_mode == MarkdownMode::Preview {
            widget::scrollable(
                widget::container(
                    markdown::view(&document.markdown_items, cosmic::iced::Theme::Dark)
                        .map(move |uri| RepositoryAction::MarkdownLink { card, uri }),
                )
                .padding(spacing.space_m)
                .max_width(900),
            )
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
        } else {
            iced::widget::mouse_area(
                text_editor::text_editor(&document.content)
                    // libcosmic's context-menu wrapper owns a second widget
                    // tree whose child can retain the preceding stateless
                    // placeholder during an async file-load transition. The
                    // native editor itself provides the required focus and
                    // editing state without that wrapper.
                    .context_menu(false)
                    .id(editor_focus_id(card))
                    .font(Font::MONOSPACE)
                    .padding(spacing.space_xs)
                    .height(Length::Fill)
                    .on_action(move |action| RepositoryAction::EditorAction { card, action }),
            )
            .into()
        };
    rows.push(content);
    widget::column::with_children(rows)
        .height(Length::Fill)
        .into()
}

fn diff_surface(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
    match runtime.diffs.get(&runtime.diff_basis) {
        None => information_state(
            "No local changes",
            "This file has no changes for the selected basis.",
        ),
        Some(DiffState::Loading) => loading(format!(
            "Computing {} diff for {}…",
            runtime.diff_basis.label().to_lowercase(),
            runtime
                .selected
                .as_ref()
                .map_or_else(String::new, |file| file.path.display().to_string())
        )),
        Some(DiffState::Ready(content)) => match content {
            DiffContent::Text(model) => diff_model_view(card, runtime, model),
            DiffContent::Binary { side } => information_state(
                "Binary diff unavailable",
                format!("The {side:?} side contains binary data."),
            ),
            DiffContent::InvalidUtf8 { side } => information_state(
                "Text diff unavailable",
                format!("The {side:?} side is not valid UTF-8."),
            ),
            DiffContent::Symlink { target } => information_state(
                "Symlink diff is read-only",
                format!("Target: {}", target.display()),
            ),
            DiffContent::Oversize { side, bytes, limit } => information_state(
                "Diff is too large",
                format!("The {side:?} side is {bytes} bytes; the safe limit is {limit} bytes."),
            ),
            DiffContent::NotRegularFile => information_state(
                "Diff unavailable",
                "The selected path is not a regular file.",
            ),
            DiffContent::Error(error) => information_state("Could not compute diff", error),
        },
    }
}

fn diff_model_view<'a>(
    card: CardId,
    runtime: &'a RepositoryRuntime,
    model: &'a DiffModel,
) -> Element<'a, RepositoryAction> {
    let spacing = cosmic::theme::spacing();
    let change_count = model.change_starts(runtime.diff_mode).len();
    let toolbar = widget::container(
        widget::row::with_children(vec![
            widget::segmented_control::horizontal(&runtime.diff_basis_control.model)
                .on_activate(move |entity| RepositoryAction::SelectDiffBasis { card, entity })
                .into(),
            widget::segmented_control::horizontal(&runtime.diff_control.model)
                .on_activate(move |entity| RepositoryAction::SelectDiffMode { card, entity })
                .into(),
            widget::text::caption(format!("+{}  −{}", model.additions, model.deletions)).into(),
            iced::widget::Space::new().width(Length::Fill).into(),
            widget::button::standard("Previous change")
                .on_press_maybe(
                    (change_count != 0).then_some(RepositoryAction::PreviousChange { card }),
                )
                .into(),
            widget::text::caption(if change_count == 0 {
                "0 / 0".into()
            } else {
                format!("{} / {change_count}", runtime.diff_change + 1)
            })
            .into(),
            widget::button::standard("Next change")
                .on_press_maybe(
                    (change_count != 0).then_some(RepositoryAction::NextChange { card }),
                )
                .into(),
            widget::button::suggested("Open in editor")
                .on_press(RepositoryAction::SelectSource {
                    card,
                    entity: runtime.source_control.first,
                })
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs),
    )
    .padding([spacing.space_xxs, spacing.space_s]);
    let rows: Element<'a, RepositoryAction> = if model.is_empty() {
        information_state(
            "No content changes",
            "The working file and checked-in file have identical text.",
        )
    } else {
        let total = model.row_count(runtime.diff_mode);
        let visible = visible_rows(total, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        let mut children = Vec::with_capacity(visible.end - visible.start + 2);
        if visible.top_space > 0.0 {
            children.push(iced::widget::Space::new().height(visible.top_space).into());
        }
        match runtime.diff_mode {
            DiffMode::Inline => children.extend(
                model.inline_rows[visible.start..visible.end]
                    .iter()
                    .map(inline_diff_row),
            ),
            DiffMode::Split => children.extend(
                model.split_rows[visible.start..visible.end]
                    .iter()
                    .map(split_diff_row),
            ),
        };
        if visible.bottom_space > 0.0 {
            children.push(
                iced::widget::Space::new()
                    .height(visible.bottom_space)
                    .into(),
            );
        }
        widget::scrollable(widget::column::with_children(children).spacing(0))
            .id(runtime.diff_scroll_id.clone())
            .on_scroll(move |viewport| RepositoryAction::DiffViewport {
                card,
                offset: viewport.absolute_offset().y,
                height: viewport.bounds().height,
            })
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    };
    widget::column::with_children(vec![
        toolbar.into(),
        widget::divider::horizontal::light().into(),
        rows,
    ])
    .height(Length::Fill)
    .into()
}

fn inline_diff_row(row: &InlineDiffRow) -> Element<'_, RepositoryAction> {
    let marker = match row.kind {
        DiffRowKind::Addition => "+",
        DiffRowKind::Deletion => "−",
        DiffRowKind::Context | DiffRowKind::Modification => " ",
    };
    let line = format_line_numbers(row.old_line, row.new_line);
    diff_container(
        widget::row::with_children(vec![
            widget::text::caption(line)
                .font(Font::MONOSPACE)
                .width(Length::Fixed(82.0))
                .into(),
            widget::text::body(marker)
                .font(Font::MONOSPACE)
                .width(Length::Fixed(18.0))
                .into(),
            widget::text::body(&row.text)
                .font(Font::MONOSPACE)
                .width(Length::Fill)
                .into(),
        ]),
        row.kind,
    )
}

fn split_diff_row(row: &SplitDiffRow) -> Element<'_, RepositoryAction> {
    let old = diff_half(
        row.old_line,
        row.old_text.as_deref().unwrap_or(""),
        row.old_text.is_some(),
    );
    let new = diff_half(
        row.new_line,
        row.new_text.as_deref().unwrap_or(""),
        row.new_text.is_some(),
    );
    diff_container(
        widget::row::with_children(vec![old, new]).spacing(1),
        row.kind,
    )
}

fn diff_half(line: Option<u32>, text: &str, present: bool) -> Element<'_, RepositoryAction> {
    widget::container(
        widget::row::with_children(vec![
            widget::text::caption(line.map_or_else(String::new, |line| line.to_string()))
                .font(Font::MONOSPACE)
                .width(Length::Fixed(42.0))
                .into(),
            widget::text::body(if present { text } else { "" })
                .font(Font::MONOSPACE)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(6),
    )
    .padding([3, 8])
    .width(Length::FillPortion(1))
    .into()
}

fn diff_container<'a>(
    content: impl Into<Element<'a, RepositoryAction>>,
    kind: DiffRowKind,
) -> Element<'a, RepositoryAction> {
    widget::container(content)
        .class(cosmic::theme::Container::custom(move |theme| {
            let component = match kind {
                DiffRowKind::Addition => Some(&theme.cosmic().success),
                DiffRowKind::Deletion => Some(&theme.cosmic().destructive),
                DiffRowKind::Modification => Some(&theme.cosmic().warning),
                DiffRowKind::Context => None,
            };
            let background = component.map(|component| {
                let mut color: iced::Color = component.base.into();
                color.a = 0.16;
                iced::Background::Color(color)
            });
            iced::widget::container::Style {
                background,
                ..Default::default()
            }
        }))
        .padding([3, 8])
        .height(Length::Fixed(DIFF_ROW_HEIGHT))
        .width(Length::Fill)
        .into()
}

fn format_line_numbers(old: Option<u32>, new: Option<u32>) -> String {
    format!(
        "{:>4} {:>4}",
        old.map_or_else(String::new, |line| line.to_string()),
        new.map_or_else(String::new, |line| line.to_string())
    )
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
        ReadOnlyReason::Oversize { bytes, limit } => format!(
            "{} is {bytes} bytes; editable files are limited to {limit} bytes.",
            path.display()
        ),
        ReadOnlyReason::NotRegularFile => format!("{} is not a regular file.", path.display()),
    };
    information_state("Read-only file", &detail)
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
    use crate::model::ProjectId;
    use tempfile::tempdir;

    use super::*;

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
        let diff = runtime.source_control.second;
        let preview = runtime.markdown_control.second;

        for action in [
            RepositoryAction::DiscardEdits { card },
            RepositoryAction::Reload { card },
            RepositoryAction::OpenFile {
                card,
                path: PathBuf::from("other.txt"),
            },
            RepositoryAction::SelectSource { card, entity: diff },
            RepositoryAction::ApplySourceSurface { card, diff: true },
            RepositoryAction::SelectMarkdown {
                card,
                entity: preview,
            },
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
        assert_eq!(runtime.diff_basis, DiffBasis::Unstaged);
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
        assert!(matches!(
            runtime.diffs.get(&DiffBasis::Unstaged),
            Some(DiffState::Loading)
        ));
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
        runtime.diff_basis = DiffBasis::Staged;
        runtime.diff_mode = DiffMode::Split;
        runtime.diffs.insert(
            DiffBasis::Staged,
            DiffState::Ready(DiffContent::Text(DiffModel {
                inline_rows: Vec::new(),
                split_rows: Vec::new(),
                inline_change_starts: vec![0, 2],
                split_change_starts: vec![0, 2],
                additions: 3,
                deletions: 2,
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

        runtime.diffs.insert(DiffBasis::Staged, DiffState::Loading);
        assert_eq!(
            runtime.inspector_runtime_status(),
            RepositoryRuntimeStatus::Loading
        );
        runtime.diffs.insert(
            DiffBasis::Staged,
            DiffState::Ready(DiffContent::Error("diff failed".into())),
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
        runtime.diff_basis = DiffBasis::Unstaged;
        let card = CardId::new();
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis { card, staged: true });
        assert_eq!(runtime.diff_basis, DiffBasis::Unstaged);
    }

    #[test]
    fn successful_save_prepares_a_fresh_diff_for_clean_and_untracked_files() {
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
            assert!(matches!(
                runtime.diffs.get(&DiffBasis::Unstaged),
                Some(DiffState::Loading)
            ));
        }
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
    fn segmented_modes_are_persistent_runtime_state() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let preview = runtime.markdown_control.second;
        let split = runtime.diff_control.second;
        let _ = runtime.update(RepositoryAction::SelectMarkdown {
            card,
            entity: preview,
        });
        let _ = runtime.update(RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        });
        let _ = runtime.update(RepositoryAction::SelectDiffMode {
            card,
            entity: split,
        });
        let _ = runtime.update(RepositoryAction::ApplyDiffMode { card, split: true });
        assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
        assert_eq!(runtime.diff_mode, DiffMode::Split);
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
        fn tree_size(tree: &iced::advanced::widget::Tree) -> usize {
            1 + tree.children.iter().map(tree_size).sum::<usize>()
        }

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
            let tree = iced::advanced::widget::Tree::new(&element);
            assert!(tree_size(&tree) < 1_000, "file widget tree was eager");
        }

        let inline = InlineDiffRow {
            old_line: Some(1),
            new_line: Some(1),
            text: "line".into(),
            kind: DiffRowKind::Context,
        };
        let split = SplitDiffRow {
            old_line: Some(1),
            new_line: Some(1),
            old_text: Some("line".into()),
            new_text: Some("line".into()),
            kind: DiffRowKind::Context,
        };
        runtime.selected = Some(RepositoryFile {
            path: PathBuf::from("large.txt"),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: true,
            status: Some(" M".into()),
        });
        runtime.diffs.insert(
            DiffBasis::Unstaged,
            DiffState::Ready(DiffContent::Text(DiffModel {
                inline_rows: vec![inline; 100_000],
                split_rows: vec![split; 100_000],
                inline_change_starts: vec![0, 50_000, 99_999],
                split_change_starts: vec![0, 50_000, 99_999],
                additions: 1,
                deletions: 1,
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
            let tree = iced::advanced::widget::Tree::new(&element);
            assert!(tree_size(&tree) < 1_500, "diff widget tree was eager");
        }
    }

    #[test]
    fn change_navigation_realizes_far_rows_before_programmatic_scroll() {
        let view = RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        runtime.diff_viewport.height = 720.0;
        runtime.diffs.insert(
            DiffBasis::Unstaged,
            DiffState::Ready(DiffContent::Text(DiffModel {
                inline_rows: vec![
                    InlineDiffRow {
                        old_line: Some(1),
                        new_line: Some(1),
                        text: "line".into(),
                        kind: DiffRowKind::Context,
                    };
                    100_000
                ],
                split_rows: Vec::new(),
                inline_change_starts: vec![0, 50_000, 99_999],
                split_change_starts: Vec::new(),
                additions: 1,
                deletions: 1,
            })),
        );
        let card = CardId::new();
        let _ = runtime.update(RepositoryAction::NextChange { card });
        let middle = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        assert!(middle.start <= 50_000 && middle.end > 50_000);
        let _ = runtime.update(RepositoryAction::NextChange { card });
        let end = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
        assert!(end.start <= 99_999 && end.end > 99_999);
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
}
