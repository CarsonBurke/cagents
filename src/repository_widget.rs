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
        DiffContent, DiffMode, DiffModel, DiffRowKind, InlineDiffRow, SplitDiffRow,
        load_worktree_diff_from,
    },
    editor::{
        FileContent, FileIoError, ReadOnlyReason, SaveOutcome, load_file, save_file,
        validate_repo_path,
    },
    model::{CardId, RepositoryView},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryFile {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub tracked: bool,
    pub changed: bool,
    pub status: Option<String>,
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
    saving: bool,
    conflict: bool,
    error: Option<String>,
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
            saving: false,
            conflict: false,
            error: None,
        }
    }

    fn apply(&mut self, action: text_editor::Action) {
        let is_edit = action.is_edit();
        self.content.perform(action);
        if is_edit {
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
        self.saving = false;
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
    None,
    Loading(PathBuf),
    Ready(DiffContent),
}

pub struct RepositoryRuntime {
    pub descriptor: RepositoryView,
    generation: u64,
    files: FileListState,
    selected: Option<RepositoryFile>,
    document: DocumentState,
    diff: DiffState,
    source_surface: SourceSurface,
    markdown_mode: MarkdownMode,
    diff_mode: DiffMode,
    source_control: TwoChoiceControl<SourceSurface>,
    markdown_control: TwoChoiceControl<MarkdownMode>,
    diff_control: TwoChoiceControl<DiffMode>,
    diff_change: usize,
    diff_scroll_id: widget::Id,
}

impl RepositoryRuntime {
    pub fn new(descriptor: RepositoryView) -> Self {
        let identity = descriptor.worktree.display().to_string();
        Self {
            descriptor,
            generation: 0,
            files: FileListState::Loading,
            selected: None,
            document: DocumentState::None,
            diff: DiffState::None,
            source_surface: SourceSurface::Editor,
            markdown_mode: MarkdownMode::Edit,
            diff_mode: DiffMode::Inline,
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
            diff_change: 0,
            diff_scroll_id: widget::Id::new(format!("repository.diff.{identity}")),
        }
    }

    pub fn initial_load(&mut self, card: CardId) -> Task<RepositoryAction> {
        self.files = FileListState::Loading;
        let identity = self.next_identity(card, PathBuf::from("."));
        files_task(identity)
    }

    pub fn is_dirty(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.dirty)
    }

    pub fn can_save(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.dirty && !document.saving)
    }

    pub fn is_saving(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.saving)
    }

    pub fn abandon_unsaved_changes(&mut self) {
        if let DocumentState::Editable(document) = &mut self.document {
            document.dirty = false;
            document.saving = false;
        }
    }

    pub fn save_error(&self) -> Option<&str> {
        match &self.document {
            DocumentState::Editable(document) => document.error.as_deref(),
            _ => None,
        }
    }

    pub fn has_save_conflict(&self) -> bool {
        matches!(&self.document, DocumentState::Editable(document) if document.conflict)
    }

    pub fn request_save(&mut self, card: CardId, overwrite: bool) -> Task<RepositoryAction> {
        let (path, text, disk_hash) = match &mut self.document {
            DocumentState::Editable(document) if document.dirty && !document.saving => {
                document.saving = true;
                document.error = None;
                (
                    document.path.clone(),
                    document.content.text(),
                    document.disk_hash,
                )
            }
            _ => return Task::none(),
        };
        let identity = self.current_identity(card, path);
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
                if !self.accepts(&identity) || identity.path != Path::new(".") {
                    return Task::none();
                }
                self.files = result.map_or_else(FileListState::Failed, FileListState::Ready);
                Task::none()
            }
            RepositoryAction::OpenFile { card, path } => self.open_file(card, path),
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
            RepositoryAction::DiffLoaded { identity, content } => {
                if self.accepts_selected(&identity) {
                    self.diff = DiffState::Ready(content);
                    self.diff_change = 0;
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
                saved_text,
                result,
            } => {
                if !self.accepts_selected(&identity) {
                    return Task::none();
                }
                let mut saved = false;
                if let DocumentState::Editable(document) = &mut self.document {
                    document.saving = false;
                    match result {
                        Ok(SaveOutcome::Saved { content_hash }) => {
                            document.saved_text = saved_text;
                            document.disk_hash = content_hash;
                            document.dirty = document.content.text() != document.saved_text;
                            document.conflict = false;
                            document.error = None;
                            saved = true;
                        }
                        Ok(SaveOutcome::Conflict { .. }) => {
                            document.conflict = true;
                            document.error =
                                Some("The file changed on disk after it was opened.".into());
                        }
                        Err(error) => document.error = Some(error.to_string()),
                    }
                }
                if saved {
                    let Some(selected) = self.selected.as_mut() else {
                        return Task::none();
                    };
                    selected.changed = true;
                    self.diff = DiffState::Loading(identity.path.clone());
                    diff_task(identity, selected.tracked, selected.old_path.clone())
                } else {
                    Task::none()
                }
            }
            RepositoryAction::Reload { card } => {
                let Some(path) = self.selected.as_ref().map(|file| file.path.clone()) else {
                    return Task::none();
                };
                self.open_file(card, path)
            }
            RepositoryAction::DiscardEdits { .. } => {
                if let DocumentState::Editable(document) = &mut self.document {
                    document.discard_edits();
                }
                Task::none()
            }
            RepositoryAction::BackToFiles { card } => {
                if self.is_dirty() {
                    return Task::none();
                }
                self.selected = None;
                self.document = DocumentState::None;
                self.diff = DiffState::None;
                self.source_surface = SourceSurface::Editor;
                self.source_control.activate_value(SourceSurface::Editor);
                self.initial_load(card)
            }
            RepositoryAction::SelectSource { card, entity } => {
                if let Some(surface) = self.source_control.activate(entity)
                    && (surface == SourceSurface::Editor
                        || self.selected.as_ref().is_some_and(|file| file.changed))
                {
                    return cosmic::task::message(RepositoryAction::ApplySourceSurface {
                        card,
                        diff: surface == SourceSurface::Diff,
                    });
                }
                Task::none()
            }
            RepositoryAction::ApplySourceSurface { diff, .. } => {
                self.source_surface = if diff {
                    SourceSurface::Diff
                } else {
                    SourceSurface::Editor
                };
                Task::none()
            }
            RepositoryAction::SelectMarkdown { card, entity } => {
                if let Some(mode) = self.markdown_control.activate(entity) {
                    return cosmic::task::message(RepositoryAction::ApplyMarkdownMode {
                        card,
                        preview: mode == MarkdownMode::Preview,
                    });
                }
                Task::none()
            }
            RepositoryAction::ApplyMarkdownMode { preview, .. } => {
                self.markdown_mode = if preview {
                    MarkdownMode::Preview
                } else {
                    MarkdownMode::Edit
                };
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
                self.scroll_to_change()
            }
            RepositoryAction::PreviousChange { .. } => {
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + changes - 1) % changes;
                }
                self.scroll_to_change()
            }
            RepositoryAction::NextChange { .. } => {
                let changes = self.change_count();
                if changes != 0 {
                    self.diff_change = (self.diff_change + 1) % changes;
                }
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
        self.diff = if file.changed {
            DiffState::Loading(path.clone())
        } else {
            DiffState::None
        };
        self.source_surface = SourceSurface::Editor;
        self.source_control.activate_value(SourceSurface::Editor);
        self.markdown_mode = MarkdownMode::Edit;
        self.markdown_control.activate_value(MarkdownMode::Edit);
        self.diff_change = 0;
        let identity = self.next_identity(card, path);
        let file_task = file_task(identity.clone());
        if file.changed {
            Task::batch([file_task, diff_task(identity, file.tracked, file.old_path)])
        } else {
            file_task
        }
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

    fn accepts_selected(&self, identity: &AsyncIdentity) -> bool {
        self.accepts(identity)
            && self
                .selected
                .as_ref()
                .is_some_and(|file| file.path == identity.path)
    }

    fn diff_model(&self) -> Option<&DiffModel> {
        match &self.diff {
            DiffState::Ready(DiffContent::Text(model)) => Some(model),
            _ => None,
        }
    }

    fn change_count(&self) -> usize {
        self.diff_model()
            .map_or(0, |model| model.change_starts(self.diff_mode).len())
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
    tracked: bool,
    old_path: Option<PathBuf>,
) -> Task<RepositoryAction> {
    let worktree = identity.worktree.clone();
    let path = identity.path.clone();
    cosmic::task::future(async move {
        let content = tokio::task::spawn_blocking(move || {
            let previous_path = old_path
                .as_deref()
                .or_else(|| tracked.then_some(path.as_path()));
            load_worktree_diff_from(&worktree, &path, previous_path)
        })
        .await
        .unwrap_or_else(|error| DiffContent::Error(format!("diff worker failed: {error}")));
        RepositoryAction::DiffLoaded { identity, content }
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
        if status.code.as_bytes().contains(&b'R')
            && let Some(old_path) = &status.old_path
        {
            files.remove(old_path);
        }
    }
    Ok(files
        .into_iter()
        .map(|(path, tracked)| {
            let status = statuses.get(&path);
            RepositoryFile {
                changed: status.is_some(),
                old_path: status.and_then(|status| status.old_path.clone()),
                path,
                tracked,
                status: status.map(|status| status.code.clone()),
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
    code: String,
    old_path: Option<PathBuf>,
}

fn parse_status_paths(output: &[u8]) -> Result<HashMap<PathBuf, ParsedStatus>, String> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err("truncated NUL-delimited git status output".into());
    }
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut statuses = HashMap::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.len() < 4 || field[2] != b' ' {
            return Err(format!("malformed git status record {index}"));
        }
        let status = String::from_utf8_lossy(&field[..2]).into_owned();
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
        statuses.insert(
            path,
            ParsedStatus {
                code: status,
                old_path,
            },
        );
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
    let spacing = cosmic::theme::spacing();
    let body = if runtime.selected.is_some() {
        selected_file_view(card, runtime)
    } else {
        file_list_view(card, runtime)
    };
    widget::column::with_children(vec![body])
        .spacing(spacing.space_xxs)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
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
            let rows = files.iter().map(|file| {
                let label = file.path.to_string_lossy();
                let mut children: Vec<Element<'_, RepositoryAction>> = vec![
                    widget::icon::from_name(if file.changed {
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
                .on_press(RepositoryAction::OpenFile {
                    card,
                    path: file.path.clone(),
                })
                .into()
            });
            widget::scrollable(widget::column(rows).spacing(spacing.space_xxxs))
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
            .name("Back to repository files")
            .on_press_maybe((!runtime.is_dirty()).then_some(RepositoryAction::BackToFiles { card }))
            .into(),
        widget::text::heading(selected.path.to_string_lossy())
            .width(Length::Fill)
            .into(),
    ];
    if selected.changed {
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
    match &runtime.document {
        DocumentState::None => information_state(
            "No file selected",
            "Choose a file from the repository list.",
        ),
        DocumentState::Loading(path) => loading(format!("Loading {}…", path.display())),
        DocumentState::Failed { path, message } => {
            information_state(format!("Could not open {}", path.display()), message)
        }
        DocumentState::ReadOnly { path, reason } => read_only_state(path, reason),
        // `MouseArea` supplies a stateful tree boundary. Without it, Iced can
        // reconcile the preceding stateless loading view into the editor and
        // leave the native text editor with an incompatible child state.
        DocumentState::Editable(document) => {
            iced::widget::mouse_area(editable_document_view(card, runtime, document)).into()
        }
    }
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
    if let Some(error) = &document.error {
        let mut actions: Vec<Element<'a, RepositoryAction>> =
            vec![widget::text::body(error).width(Length::Fill).into()];
        if document.conflict {
            actions.push(
                widget::button::standard("Reload")
                    .on_press(RepositoryAction::Reload { card })
                    .into(),
            );
            actions.push(
                widget::button::destructive("Overwrite")
                    .on_press(RepositoryAction::Save {
                        card,
                        overwrite: true,
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
    match &runtime.diff {
        DiffState::None => information_state(
            "No local changes",
            "This file matches its checked-in version.",
        ),
        DiffState::Loading(path) => loading(format!("Computing diff for {}…", path.display())),
        DiffState::Ready(content) => match content {
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
        let children: Vec<Element<'a, RepositoryAction>> = match runtime.diff_mode {
            DiffMode::Inline => model.inline_rows.iter().map(inline_diff_row).collect(),
            DiffMode::Split => model.split_rows.iter().map(split_diff_row).collect(),
        };
        widget::scrollable(widget::column::with_children(children).spacing(0))
            .id(runtime.diff_scroll_id.clone())
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
                .map(|status| status.code.as_str()),
            Some(" M")
        );
        assert_eq!(
            parsed
                .get(Path::new("renamed.md"))
                .map(|status| status.code.as_str()),
            Some("R ")
        );
        assert_eq!(
            parsed
                .get(Path::new("new.txt"))
                .map(|status| status.code.as_str()),
            Some("??")
        );
        assert_eq!(
            parsed
                .get(Path::new("renamed.md"))
                .and_then(|status| status.old_path.as_deref()),
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
                old_path: Some(PathBuf::from("old-name.md")),
                tracked: false,
                changed: true,
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
                    old_path: None,
                    tracked: false,
                    changed: true,
                    status: Some("??".into())
                },
                RepositoryFile {
                    path: PathBuf::from("renamed.md"),
                    old_path: Some(PathBuf::from("old-name.md")),
                    tracked: true,
                    changed: true,
                    status: Some("R ".into())
                },
                RepositoryFile {
                    path: PathBuf::from("tracked.md"),
                    old_path: None,
                    tracked: true,
                    changed: true,
                    status: Some(" M".into())
                },
            ]
        );
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
        document.saving = true;
        document.discard_edits();
        assert!(!document.dirty);
        assert!(!document.saving);
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
            old_path: None,
            tracked: true,
            changed: true,
            status: Some(" M".into()),
        });
        let mut document = EditorDocument::new(
            path.clone(),
            "before\n".into(),
            blake3::hash(b"before\n"),
            false,
        );
        document.saved_text = "submitted\n".into();
        document.content = text_editor::Content::with_text("typed later\n");
        document.dirty = true;
        document.saving = true;
        runtime.document = DocumentState::Editable(document);

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity: AsyncIdentity {
                card,
                worktree: PathBuf::from("/repo"),
                path,
                generation: 1,
            },
            saved_text: "submitted\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"submitted\n"),
            }),
        });

        let DocumentState::Editable(document) = &runtime.document else {
            panic!("document should remain editable");
        };
        assert_eq!(document.saved_text, "submitted\n");
        assert_eq!(document.content.text(), "typed later\n");
        assert!(document.dirty);
        assert!(!document.saving);
    }

    #[test]
    fn successful_save_prepares_a_fresh_diff_for_clean_and_untracked_files() {
        for (tracked, changed) in [(true, false), (false, true)] {
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
                old_path: None,
                tracked,
                changed,
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
            document.saving = true;
            runtime.document = DocumentState::Editable(document);

            let _ = runtime.update(RepositoryAction::SaveFinished {
                identity: AsyncIdentity {
                    card,
                    worktree: PathBuf::from("/repo"),
                    path: path.clone(),
                    generation: 1,
                },
                saved_text: "after\n".into(),
                result: Ok(SaveOutcome::Saved {
                    content_hash: blake3::hash(b"after\n"),
                }),
            });

            assert!(runtime.selected.as_ref().is_some_and(|file| file.changed));
            assert!(matches!(runtime.diff, DiffState::Loading(ref loaded) if loaded == &path));
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
            old_path: None,
            tracked: true,
            changed: true,
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
