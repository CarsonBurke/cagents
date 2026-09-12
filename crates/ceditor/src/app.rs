use cagents::{
    design,
    lsp::{
        LspCompletionItem, LspDiagnostic, LspDocumentId, LspEvent, LspManager, LspPosition,
        LspStatus,
    },
    model::{CardId, ProjectId, RepositoryView, WorktreeId},
    repository::{
        CancellationToken, DiffBasis, MutationOutcome, RepoPath, RepositoryId, RepositoryService,
        RepositorySnapshot, StageRequest, UnstageRequest,
    },
    repository_widget::{self, LspIntent, RepositoryAction, RepositoryRuntime},
    settings::{self, AppSettings, AppTheme},
    sidebar::{
        self, ActiveWorktreeKey, FileCommand, FileListMode, SidebarAction, SidebarFocus,
        SidebarMode, SidebarModel, SidebarProject, SidebarState, SidebarWorktree,
        SourceControlModel,
    },
    ui_controls::{icon_control, popover, segmented_controls},
};
use cosmic::app::ApplicationExt;
use cosmic::{
    Application, Core, Element,
    app::Task,
    iced::{self, Length, Subscription, keyboard, window},
    widget,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct Flags {
    pub paths: Vec<PathBuf>,
    pub settings_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Opened {
    root: PathBuf,
    file: Option<PathBuf>,
    snapshot: Option<RepositorySnapshot>,
}

struct PendingFile {
    path: PathBuf,
    request: u64,
    canonical: Option<PathBuf>,
}

struct Tab {
    id: CardId,
    title: String,
    runtime: RepositoryRuntime,
    sidebar: SidebarModel,
    navigation: SidebarState,
    pending_file: Option<PendingFile>,
    owned_file: Option<PathBuf>,
    next_open: u64,
    files_loaded: bool,
    git: bool,
    diagnostics: Vec<LspDiagnostic>,
    completions: Vec<LspCompletionItem>,
    hover: Option<String>,
    lsp_error: Option<String>,
    mutation: bool,
    snapshot_loading: bool,
    snapshot_refresh_pending: bool,
}

struct FileOperationDialog {
    root: PathBuf,
    source: PathBuf,
    name: String,
    duplicate: bool,
    busy: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Choose(bool),
    Chosen(u64, Result<Option<PathBuf>, String>),
    Opened(u64, Result<Opened, String>),
    SessionOpened(u64, Vec<(bool, Result<Opened, String>)>),
    FileResolved(CardId, u64, Result<PathBuf, String>),
    Select(CardId),
    CloseTab(CardId),
    CloseWindow,
    SaveClose,
    DiscardClose,
    ConfirmClose,
    CancelClose,
    Reaped,
    WindowFocused(window::Id, bool),
    RefreshVisible,
    Repository(RepositoryAction),
    ResolvedDiffBasis { card: CardId, basis: DiffBasis },
    Sidebar(SidebarAction),
    Snapshot(CardId, Result<RepositorySnapshot, String>),
    Mutation(CardId, Result<MutationOutcome, String>),
    FileNameChanged(String),
    SubmitFileOperation,
    CancelFileOperation,
    FileOperationFinished(Result<PathBuf, String>),
    Settings,
    AutoSave(bool),
    WrapLines(bool),
    LanguageServerAssistance(bool),
    Theme(AppTheme),
    Scale(f32),
    SettingsSaved(Result<(), String>),
    Lsp(u64, Option<LspEvent>),
    Complete,
    Hover,
    Completion(CardId, usize),
    Jump(CardId, LspPosition),
    Key(keyboard::Event),
    Notices,
    CloseNotices,
    Dismiss,
}

pub struct Editor {
    core: Core,
    tabs: Vec<Tab>,
    active: Option<CardId>,
    selection_epoch: u64,
    settings: AppSettings,
    settings_path: PathBuf,
    settings_open: bool,
    settings_writable: bool,
    notice: Option<String>,
    notices_open: bool,
    file_dialog: Option<FileOperationDialog>,
    closing: Option<CardId>,
    close_confirmed: bool,
    exiting: bool,
    window_focused: bool,
    lsp: LspManager,
    language_server_assistance: bool,
    next_document: u64,
    file_list_mode: FileListMode,
    restoring_session: bool,
    saved_session: crate::preferences::Session,
}

fn repository_task(task: iced::Task<RepositoryAction>) -> Task<Message> {
    task.map(Message::Repository).map(cosmic::Action::App)
}

fn language_server_failure(status: LspStatus) -> Option<String> {
    match status {
        LspStatus::Unavailable { reason } => Some(reason),
        LspStatus::Failed {
            reason,
            stderr_tail,
        } => {
            if stderr_tail.is_empty() {
                Some(reason)
            } else {
                Some(format!(
                    "{reason}\n\nServer stderr:\n{}",
                    stderr_tail.join("\n")
                ))
            }
        }
        LspStatus::Disabled | LspStatus::Starting | LspStatus::Ready => None,
    }
}

fn file_operation_destination(source: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(leaf)) if leaf == name)
        || components.next().is_some()
    {
        return Err("Enter a file name, not a path.".into());
    }
    let destination = source.parent().unwrap_or(Path::new("")).join(name);
    if destination == source {
        return Err("Choose a different file name.".into());
    }
    Ok(destination)
}

fn chosen_path(
    response: Result<
        cosmic::dialog::file_chooser::open::FileResponse,
        cosmic::dialog::file_chooser::Error,
    >,
) -> Result<Option<PathBuf>, String> {
    match response {
        Ok(response) => response
            .url()
            .to_file_path()
            .map(Some)
            .map_err(|()| "Only local files are supported".into()),
        Err(cosmic::dialog::file_chooser::Error::Cancelled) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn tab_title_style() -> cosmic::theme::Button {
    fn appearance(focused: bool, theme: &cosmic::Theme) -> widget::button::Style {
        widget::button::Style {
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            background: focused
                .then(|| iced::Background::Color(theme.current_container().component.hover.into())),
            border_width: 0.0,
            ..widget::button::Style::new()
        }
    }
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(|_| widget::button::Style::new()),
        hovered: Box::new(|focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(|focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

impl Editor {
    fn modal_open(&self) -> bool {
        self.settings_open
            || self.closing.is_some()
            || self.file_dialog.is_some()
            || self.notices_open
    }

    fn rename_blocked(&self, absolute: &Path) -> bool {
        self.tabs.iter().any(|tab| {
            if !absolute.starts_with(&tab.runtime.descriptor.worktree) {
                return false;
            }
            tab.mutation
                || tab.pending_file.is_some()
                || (tab.owned_file.as_deref() == Some(absolute) && tab.runtime.blocks_navigation())
        })
    }

    fn begin_file_operation(
        &mut self,
        id: CardId,
        source: PathBuf,
        duplicate: bool,
    ) -> Task<Message> {
        let Some(tab) = self.tab(id) else {
            return Task::none();
        };
        let root = tab.runtime.descriptor.worktree.clone();
        if !duplicate && self.rename_blocked(&root.join(&source)) {
            self.notice = Some(
                "Save this file and wait for pending file or Git operations before renaming it."
                    .into(),
            );
            return Task::none();
        }
        let name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.file_dialog = Some(FileOperationDialog {
            root,
            source,
            name,
            duplicate,
            busy: false,
            error: None,
        });
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        iced::widget::operation::focus(widget::Id::new("ceditor.file.name"))
            .map(cosmic::Action::App)
    }

    fn submit_file_operation(&mut self) -> Task<Message> {
        let Some(dialog) = self.file_dialog.as_ref().filter(|dialog| !dialog.busy) else {
            return Task::none();
        };
        let destination = match file_operation_destination(&dialog.source, &dialog.name) {
            Ok(destination) => destination,
            Err(error) => {
                self.file_dialog.as_mut().unwrap().error = Some(error);
                return Task::none();
            }
        };
        if !dialog.duplicate && self.rename_blocked(&dialog.root.join(&dialog.source)) {
            self.file_dialog.as_mut().unwrap().error = Some(
                "Save this file and wait for pending file or Git operations before renaming it."
                    .into(),
            );
            return Task::none();
        }
        let dialog = self.file_dialog.as_mut().unwrap();
        dialog.busy = true;
        dialog.error = None;
        let root = dialog.root.clone();
        let source = dialog.source.clone();
        let duplicate = dialog.duplicate;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let operation = if duplicate {
                    cagents::editor::copy_file
                } else {
                    cagents::editor::rename_file
                };
                operation(&root, &source, &destination)
                    .map(|()| destination)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::FileOperationFinished(result)
        })
    }

    fn finish_file_operation(&mut self, result: Result<PathBuf, String>) -> Task<Message> {
        if !self.file_dialog.as_ref().is_some_and(|dialog| dialog.busy) {
            return Task::none();
        }
        let Some(mut dialog) = self.file_dialog.take() else {
            return Task::none();
        };
        let destination = match result {
            Ok(destination) => destination,
            Err(error) => {
                dialog.busy = false;
                dialog.error = Some(error);
                self.file_dialog = Some(dialog);
                self.exiting = false;
                return Task::none();
            }
        };
        let source = dialog.root.join(&dialog.source);
        let destination = dialog.root.join(destination);
        let mut tasks = Vec::new();
        let mut refresh = Vec::new();
        for tab in &mut self.tabs {
            let root = &tab.runtime.descriptor.worktree;
            if !source.starts_with(root) {
                continue;
            }
            let pending_source = tab.pending_file.as_ref().is_some_and(|pending| {
                pending.canonical.as_deref() == Some(source.as_path())
                    || root.join(&pending.path) == source
            });
            let reopen =
                !dialog.duplicate && (tab.owned_file.as_ref() == Some(&source) || pending_source);
            refresh.push((tab.id, reopen));
            if reopen {
                tasks.push(repository_task(
                    tab.runtime
                        .update(RepositoryAction::BackToFiles { card: tab.id }),
                ));
                tab.owned_file = None;
                let path = destination
                    .strip_prefix(&tab.runtime.descriptor.worktree)
                    .expect("a same-directory rename stays in every containing root")
                    .to_path_buf();
                tab.title = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                tab.pending_file = Some(PendingFile {
                    path,
                    canonical: Some(destination.clone()),
                    request: tab.next_open,
                });
                tab.next_open = tab.next_open.wrapping_add(1);
                tab.files_loaded = false;
            }
        }
        for (id, reopened) in refresh {
            tasks.push(self.drain(id));
            tasks.push(if reopened {
                self.refresh_snapshot(id, true)
            } else {
                self.refresh(id)
            });
        }
        if self.exiting {
            tasks.push(self.continue_exit());
        } else {
            tasks.push(self.resume_pending_files());
        }
        Task::batch(tasks)
    }

    fn reveal_active_tab(&self) -> Task<Message> {
        let Some(index) = self.tabs.iter().position(|tab| Some(tab.id) == self.active) else {
            return Task::none();
        };
        // Equal-width tabs place the active tab between the viewport edges at
        // this fraction, including the first and last tab.
        iced::widget::scrollable::snap_to(
            widget::Id::new("ceditor.tabs"),
            iced::widget::scrollable::RelativeOffset {
                x: Some(index as f32 / self.tabs.len().saturating_sub(1).max(1) as f32),
                y: None,
            },
        )
        .map(cosmic::Action::App)
    }

    fn tab(&self, id: CardId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }
    fn tab_mut(&mut self, id: CardId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }
    fn current(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tab(id))
    }

    fn resolve_open(path: PathBuf) -> Result<Opened, String> {
        let path = path
            .canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let directory = if path.is_dir() {
            path.clone()
        } else {
            if !path.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            std::fs::File::open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            path.parent()
                .ok_or("File has no parent directory")?
                .to_path_buf()
        };
        let service = RepositoryService::system("git");
        let token = CancellationToken::default();
        // Discovery failure does not prevent editing an ordinary folder.
        let repository = service.discover(&directory, &token).ok();
        let repository_root = repository.and_then(|info| info.worktree_root);
        let snapshot = repository_root
            .as_ref()
            .map(|root| {
                service
                    .snapshot(root, &token)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        let root = repository_root.unwrap_or(directory);
        let file = (!path.is_dir())
            .then(|| path.strip_prefix(&root).map(Path::to_path_buf))
            .transpose()
            .map_err(|error| error.to_string())?;
        Ok(Opened {
            root,
            file,
            snapshot,
        })
    }

    fn open(path: PathBuf, selection_epoch: u64) -> Task<Message> {
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || Self::resolve_open(path))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::Opened(selection_epoch, result)
        })
    }

    fn restore_session(paths: Vec<(PathBuf, bool)>) -> Task<Message> {
        cosmic::task::future(async move {
            // Resolve in durable order, not background-task completion order.
            // An unavailable entry never prevents the remaining files opening.
            let opened = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|(path, focus)| (focus, Self::resolve_open(path)))
                    .collect()
            })
            .await
            .unwrap_or_else(|error| vec![(false, Err(error.to_string()))]);
            Message::SessionOpened(0, opened)
        })
    }

    fn session(&self) -> crate::preferences::Session {
        crate::preferences::Session {
            paths: self
                .tabs
                .iter()
                .map(|tab| {
                    tab.pending_file
                        .as_ref()
                        .map(|pending| tab.runtime.descriptor.worktree.join(&pending.path))
                        .or_else(|| tab.owned_file.clone())
                        .unwrap_or_else(|| tab.runtime.descriptor.worktree.clone())
                })
                .collect(),
            active: self.tabs.iter().position(|tab| Some(tab.id) == self.active),
        }
    }

    fn save_session(&mut self) {
        if self.exiting || self.restoring_session || !self.settings_writable {
            return;
        }
        let session = self.session();
        if session == self.saved_session {
            return;
        }
        // These ordered atomic writes run only on navigation changes, never edits.
        // The shared writer validates the private directory without overwriting
        // preferences another window changed.
        let result = settings::save_merged(&self.settings_path, &self.settings, &self.settings)
            .map_err(|error| error.to_string())
            .and_then(|()| crate::preferences::save_session(&self.settings_path, &session));
        match result {
            Ok(()) => self.saved_session = session,
            Err(error) => self.notice = Some(error),
        }
    }

    fn add(&mut self, opened: Opened, focus: bool) -> Task<Message> {
        if let Some(file) = &opened.file {
            if let Some(owner) = self.file_owner(&opened.root.join(file)) {
                if focus && !self.modal_open() {
                    self.active = Some(owner);
                }
                return self.resume_pending_files();
            }
        }
        let id = CardId::new();
        let project = ProjectId::new();
        let worktree = WorktreeId::new();
        let key = ActiveWorktreeKey {
            project_id: project,
            worktree_id: worktree,
            canonical_path: opened.root.clone(),
        };
        let title = opened
            .file
            .as_deref()
            .unwrap_or(&opened.root)
            .file_name()
            .unwrap_or(opened.root.as_os_str())
            .to_string_lossy()
            .into_owned();
        let descriptor = RepositoryView {
            project,
            repository: opened.root.clone(),
            worktree: opened.root.clone(),
        };
        let git = opened.snapshot.is_some();
        let mut runtime = if git {
            RepositoryRuntime::new(descriptor)
        } else {
            RepositoryRuntime::new_directory(descriptor)
        };
        runtime.set_standalone(true);
        runtime.set_auto_save(self.settings.editor_auto_save);
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        let mut sidebar = SidebarModel::ready(
            SidebarMode::Expanded,
            vec![SidebarProject::new(
                project,
                RepositoryId::new(),
                title.clone(),
                opened.root.clone(),
                vec![SidebarWorktree::from_durable(
                    worktree,
                    opened.root.clone(),
                    None,
                )],
                Vec::new(),
            )],
        );
        sidebar.file_management = true;
        sidebar.source_control = Some(match &opened.snapshot {
            Some(snapshot) => SourceControlModel::ready(key, snapshot),
            None => SourceControlModel::loading(key),
        });
        let mut navigation = SidebarState::default();
        let pending_file = opened.file.map(|path| PendingFile {
            canonical: Some(opened.root.join(&path)),
            path,
            request: 0,
        });
        navigation.sync_active_worktree(
            &sidebar,
            &SidebarFocus::Repository {
                worktree: opened.root,
            },
        );
        if let Some(key) = navigation.active_worktree().cloned() {
            navigation.apply(&SidebarAction::SetFileListMode {
                key,
                mode: self.file_list_mode,
            });
        }
        let task = repository_task(runtime.initial_load(id));
        self.tabs.push(Tab {
            id,
            title,
            runtime,
            sidebar,
            navigation,
            pending_file,
            owned_file: None,
            next_open: 1,
            files_loaded: false,
            git,
            diagnostics: Vec::new(),
            completions: Vec::new(),
            hover: None,
            lsp_error: None,
            mutation: false,
            snapshot_loading: false,
            snapshot_refresh_pending: false,
        });
        if self.active.is_none() || (focus && !self.modal_open()) {
            self.active = Some(id);
        }
        task
    }

    // Pending canonical paths reserve ownership too: CLI/picker opens can
    // arrive before their file lists or file contents finish loading.
    fn file_owner(&self, canonical: &Path) -> Option<CardId> {
        self.tabs
            .iter()
            .find(|tab| tab.owned_file.as_deref() == Some(canonical))
            .or_else(|| {
                self.tabs.iter().find(|tab| {
                    tab.pending_file
                        .as_ref()
                        .and_then(|pending| pending.canonical.as_deref())
                        == Some(canonical)
                })
            })
            .map(|tab| tab.id)
    }

    fn request_file(&mut self, id: CardId, path: PathBuf) -> Task<Message> {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        tab.pending_file = None;
        if RepoPath::parse(&path).is_err() {
            return Task::none();
        }
        if tab.runtime.blocks_navigation() {
            self.notice =
                Some("Save or discard the current file before opening another file.".into());
            return Task::none();
        }
        let request = tab.next_open;
        tab.next_open = tab.next_open.wrapping_add(1);
        let absolute = tab.runtime.descriptor.worktree.join(&path);
        // Replacing the pending request also fences any older canonicalization.
        tab.pending_file = Some(PendingFile {
            path,
            request,
            canonical: None,
        });
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                absolute
                    .canonicalize()
                    .map_err(|error| format!("{}: {error}", absolute.display()))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::FileResolved(id, request, result)
        })
    }

    fn resume_pending_files(&mut self) -> Task<Message> {
        if self.modal_open() || self.exiting {
            return Task::none();
        }
        let pending: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let pending = tab.pending_file.as_ref()?;
                (tab.files_loaded && !tab.runtime.blocks_navigation())
                    .then(|| {
                        pending
                            .canonical
                            .clone()
                            .map(|canonical| (tab.id, canonical))
                    })
                    .flatten()
            })
            .collect();
        let mut tasks = Vec::new();
        for (id, canonical) in pending {
            let owner = self.file_owner(&canonical);
            if owner.is_some_and(|owner| owner != id) {
                if let Some(tab) = self.tab_mut(id) {
                    tab.pending_file = None;
                }
                // A background initial open must not steal a later selection.
                if self.active == Some(id) {
                    self.active = owner;
                }
                continue;
            }
            let Some(tab) = self.tab_mut(id) else {
                continue;
            };
            let Some(pending) = tab.pending_file.take() else {
                continue;
            };
            if tab.owned_file.as_ref() == Some(&canonical) {
                continue;
            }
            tab.title = pending
                .path
                .file_name()
                .unwrap_or(pending.path.as_os_str())
                .to_string_lossy()
                .into_owned();
            tab.owned_file = Some(canonical);
            tasks.push(repository_task(tab.runtime.update(
                RepositoryAction::OpenFile {
                    card: id,
                    path: pending.path,
                },
            )));
            tasks.push(self.drain(id));
        }
        Task::batch(tasks)
    }

    fn lsp_next(&self) -> Task<Message> {
        let events = self.lsp.events();
        let generation = self.lsp.generation();
        cosmic::task::future(async move { Message::Lsp(generation, events.recv().await.ok()) })
    }

    fn drain(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let intents = tab.runtime.take_lsp_intents();
        let autosave = repository_task(tab.runtime.auto_save_task(id));
        let working_diff = repository_task(tab.runtime.working_diff_task(id));
        for intent in intents {
            match intent {
                LspIntent::Open {
                    absolute_path: _,
                    worktree,
                    version,
                    text,
                } => {
                    let Some(absolute_path) = self.tab(id).and_then(|tab| tab.owned_file.clone())
                    else {
                        continue;
                    };
                    self.close_document(id);
                    if !self.language_server_assistance {
                        continue;
                    }
                    if let Some(language) = self.lsp.language_for(&absolute_path) {
                        let doc = LspDocumentId(self.next_document);
                        self.next_document += 1;
                        let status =
                            self.lsp
                                .open(doc, &absolute_path, &worktree, language, version, &text);
                        if let Some(tab) = self.tab_mut(id) {
                            tab.runtime.set_lsp_document(Some(doc));
                            if let Some(error) = language_server_failure(status) {
                                tab.lsp_error = Some(error);
                            }
                        }
                    }
                }
                LspIntent::Change { version, text } => {
                    if let Some(doc) = self.tab(id).and_then(|tab| tab.runtime.lsp_document()) {
                        self.lsp.change(doc, version, &[], &text);
                    }
                    if let Some(tab) = self.tab_mut(id) {
                        tab.completions.clear();
                        tab.hover = None;
                        tab.diagnostics.clear();
                    }
                }
                LspIntent::Save { text } => {
                    if let Some(doc) = self.tab(id).and_then(|tab| tab.runtime.lsp_document()) {
                        self.lsp.save(doc, &text);
                    }
                }
                LspIntent::Close => self.close_document(id),
            }
        }
        Task::batch([autosave, working_diff])
    }

    fn close_document(&mut self, id: CardId) {
        if let Some(doc) = self.tab(id).and_then(|tab| tab.runtime.lsp_document()) {
            self.lsp.close(doc);
        }
        if let Some(tab) = self.tab_mut(id) {
            tab.runtime.set_lsp_document(None);
            tab.diagnostics.clear();
            tab.completions.clear();
            tab.hover = None;
        }
    }

    fn request_close(&mut self, id: CardId) -> Task<Message> {
        if self
            .tab(id)
            .is_some_and(|tab| tab.runtime.blocks_navigation() || tab.mutation)
        {
            self.active = Some(id);
            self.closing = Some(id);
            self.close_confirmed = false;
            let tab = self.tab(id).unwrap();
            let focus = if tab.runtime.can_save() {
                "ceditor.close.save"
            } else if tab.runtime.can_abandon_unsaved_changes() && !tab.mutation {
                "ceditor.close.discard"
            } else {
                "ceditor.close.cancel"
            };
            return iced::widget::operation::focus(widget::Id::new(focus)).map(cosmic::Action::App);
        }
        self.close_document(id);
        self.tabs.retain(|tab| tab.id != id);
        if self.active == Some(id) {
            self.active = self.tabs.last().map(|tab| tab.id);
        }
        self.closing = None;
        self.close_confirmed = false;
        if self.exiting {
            self.continue_exit()
        } else {
            self.resume_pending_files()
        }
    }

    fn continue_exit(&mut self) -> Task<Message> {
        if let Some(tab) = self.tabs.first() {
            return self.request_close(tab.id);
        }
        let reaped = self.lsp.begin_shutdown();
        cosmic::task::future(async move {
            reaped.await;
            Message::Reaped
        })
    }

    fn save_settings(&mut self) -> Task<Message> {
        if !self.settings_writable {
            self.notice = Some(
                "Settings could not be loaded; repair the settings file before saving preferences."
                    .into(),
            );
            return Task::none();
        }
        let path = self.settings_path.clone();
        let settings = self.settings.clone();
        // Preference changes are low frequency. One ordered atomic write avoids
        // competing snapshots overwriting newer values.
        let result = settings::save(path, &settings).map_err(|error| error.to_string());
        if let Err(error) = result {
            self.notice = Some(error);
        }
        Task::none()
    }

    fn set_language_server_assistance(&mut self, enabled: bool) -> Task<Message> {
        if self.language_server_assistance == enabled {
            return Task::none();
        }
        self.language_server_assistance = enabled;
        let ids: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        let mut tasks = Vec::new();
        if enabled {
            for id in ids {
                self.tab_mut(id).unwrap().runtime.resync_lsp();
                tasks.push(self.drain(id));
            }
        } else {
            for id in ids {
                self.close_document(id);
            }
            // The manager's shutdown owns its reaper even when this future is dropped.
            drop(self.lsp.begin_shutdown());
            self.lsp = LspManager::new(self.settings.language_servers.clone());
            tasks.push(self.lsp_next());
        }
        if self.settings_writable {
            let result = settings::save(&self.settings_path, &self.settings)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    crate::preferences::save_language_server_assistance(
                        &self.settings_path,
                        enabled,
                    )
                });
            if let Err(error) = result {
                self.notice = Some(error);
            }
        } else {
            self.notice = Some(
                "Repair the settings file before saving the language-server assistance preference."
                    .into(),
            );
        }
        Task::batch(tasks)
    }

    fn close_notices(&mut self) -> Task<Message> {
        if !self.notices_open {
            return Task::none();
        }
        self.notices_open = false;
        let focus = self.current().map_or_else(Task::none, |tab| {
            tab.runtime.focus_editor(tab.id).map(cosmic::Action::App)
        });
        Task::batch([focus, self.resume_pending_files()])
    }

    fn notices_control(&self) -> Option<Element<'_, Message>> {
        let has_notices =
            self.notice.is_some() || self.tabs.iter().any(|tab| tab.lsp_error.is_some());
        if !has_notices && !self.notices_open {
            return None;
        }
        let anchor = icon_control(
            widget::icon::from_name(if has_notices {
                "dialog-warning-symbolic"
            } else {
                "dialog-information-symbolic"
            })
            .handle(),
            if has_notices {
                "Notices — errors need attention"
            } else {
                "Notices"
            },
            self.notices_open,
            self.notices_open || !self.modal_open(),
            Some(Message::Notices),
        );
        let content = self.notices_open.then(|| {
            let mut details = Vec::new();
            if let Some(notice) = &self.notice {
                details.push(widget::text::title4("Editor notice").into());
                details.push(
                    widget::text::body(notice)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                );
            }
            for tab in &self.tabs {
                if let Some(error) = &tab.lsp_error {
                    details.push(
                        widget::text::title4(format!("Language-server error: {}", tab.title))
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .into(),
                    );
                    details.push(
                        widget::text::body(error)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .into(),
                    );
                }
            }
            if !has_notices {
                details.push(widget::text::body("No notices.").into());
            }
            widget::column::with_children(vec![
                widget::row::with_children(vec![
                    widget::text::title3("Notices").width(Length::Fill).into(),
                    icon_control(
                        widget::icon::from_name("window-close-symbolic").handle(),
                        "Close notices (Escape)",
                        false,
                        true,
                        Some(Message::CloseNotices),
                    )
                    .id(widget::Id::new("ceditor.notices.close"))
                    .into(),
                ])
                .align_y(iced::Alignment::Center)
                .into(),
                widget::scrollable(widget::column::with_children(details).spacing(10))
                    .id(widget::Id::new("ceditor.notices.details"))
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(280)
                    .into(),
                widget::button::standard("Dismiss notices")
                    .on_press_maybe(has_notices.then_some(Message::Dismiss))
                    .into(),
            ])
            .spacing(12)
            .padding(10)
            .width(420)
            .into()
        });
        Some(
            popover(
                anchor,
                content,
                widget::popover::Position::Bottom,
                Message::CloseNotices,
            )
            .into(),
        )
    }

    fn appearance(&self) -> Task<Message> {
        let theme = match self.settings.app_theme {
            AppTheme::System => {
                cosmic::Theme::system(Arc::new(self.core.system_theme().cosmic().clone()))
            }
            AppTheme::Dark => cosmic::Theme::dark(),
            AppTheme::Light => cosmic::Theme::light(),
        };
        Task::batch([
            cosmic::command::set_theme(theme),
            Task::done(cosmic::Action::Cosmic(cosmic::app::Action::ScaleFactor(
                self.settings.ui_scale,
            ))),
        ])
    }

    fn can_refresh_visible(&self) -> bool {
        self.window_focused && !self.modal_open() && !self.exiting && self.current().is_some()
    }

    fn refresh_visible(&mut self) -> Task<Message> {
        if !self.can_refresh_visible() {
            return Task::none();
        }
        let id = self.active.unwrap();
        let tab = self.tab_mut(id).unwrap();
        let files = repository_task(tab.runtime.refresh_visible(id));
        Task::batch([files, self.refresh_snapshot(id, false)])
    }

    fn refresh(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let files = repository_task(tab.runtime.refresh_after_mutation(id));
        Task::batch([files, self.refresh_snapshot(id, true)])
    }

    fn refresh_snapshot(&mut self, id: CardId, force: bool) -> Task<Message> {
        let Some(tab) = self.tab_mut(id).filter(|tab| tab.git) else {
            return Task::none();
        };
        if tab.snapshot_loading {
            tab.snapshot_refresh_pending |= force;
            return Task::none();
        }
        tab.snapshot_loading = true;
        let root = tab.runtime.descriptor.worktree.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .snapshot(&root, &CancellationToken::default())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::Snapshot(id, result)
        })
    }

    fn stage(&mut self, id: CardId, stage: bool) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        if tab.mutation || tab.runtime.blocks_navigation() {
            return Task::none();
        }
        let Some(selection) = tab.runtime.selected_staging_selection(stage) else {
            return Task::none();
        };
        let root = tab.runtime.descriptor.worktree.clone();
        tab.mutation = true;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git");
                let token = CancellationToken::default();
                if stage {
                    service.stage(
                        &StageRequest {
                            worktree: root,
                            selection: vec![selection],
                        },
                        &token,
                    )
                } else {
                    service.unstage(
                        &UnstageRequest {
                            worktree: root,
                            selection: vec![selection],
                        },
                        &token,
                    )
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            Message::Mutation(id, result)
        })
    }
}

impl Application for Editor {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = crate::APPLICATION_ID;

    fn init(mut core: Core, flags: Flags) -> (Self, Task<Message>) {
        core.window.border_padding = Some(0);
        let (settings, mut notice, settings_writable) = match settings::load(&flags.settings_path) {
            Ok(settings) => (settings, None, true),
            Err(error) => (AppSettings::default(), Some(error.to_string()), false),
        };
        let file_list_mode =
            crate::preferences::load(&flags.settings_path).unwrap_or_else(|error| {
                notice = Some(error);
                FileListMode::Tree
            });
        let saved_session =
            crate::preferences::load_session(&flags.settings_path).unwrap_or_else(|error| {
                notice = Some(error);
                crate::preferences::Session::default()
            });
        let language_server_assistance =
            crate::preferences::load_language_server_assistance(&flags.settings_path)
                .unwrap_or_else(|error| {
                    notice = Some(error);
                    false
                });
        let mut startup_paths = saved_session.startup_paths(flags.paths);
        if startup_paths.is_empty() {
            match std::env::current_dir() {
                Ok(path) => startup_paths.push((path, true)),
                Err(error) => {
                    let error = format!("Could not open the current directory: {error}");
                    notice = Some(match notice {
                        Some(previous) => format!("{previous}\n{error}"),
                        None => error,
                    });
                }
            }
        }
        let lsp = LspManager::new(settings.language_servers.clone());
        let mut app = Self {
            core,
            tabs: Vec::new(),
            active: None,
            selection_epoch: 0,
            settings,
            settings_path: flags.settings_path,
            settings_open: false,
            settings_writable,
            notice,
            notices_open: false,
            file_dialog: None,
            closing: None,
            close_confirmed: false,
            exiting: false,
            window_focused: false,
            lsp,
            language_server_assistance,
            next_document: 1,
            file_list_mode,
            restoring_session: !startup_paths.is_empty(),
            saved_session,
        };
        let mut tasks = vec![app.appearance(), app.lsp_next()];
        if let Some(window) = app.core.main_window_id() {
            tasks.push(app.set_window_title("ceditor".to_owned(), window));
        }
        if !startup_paths.is_empty() {
            tasks.push(Self::restore_session(startup_paths));
        }
        (app, Task::batch(tasks))
    }
    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }
    fn on_close_requested(&self, _: window::Id) -> Option<Message> {
        Some(Message::CloseWindow)
    }
    fn on_app_exit(&mut self) -> Option<Message> {
        Some(Message::CloseWindow)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let previous_tabs = (self.active, self.tabs.len());
        let was_refresh_visible = self.can_refresh_visible();
        let navigation_changed = matches!(
            &message,
            Message::Opened(..)
                | Message::SessionOpened(..)
                | Message::FileResolved(..)
                | Message::FileOperationFinished(..)
                | Message::CancelClose
                | Message::Sidebar(_)
                | Message::Repository(RepositoryAction::BackToFiles { .. })
                | Message::Repository(RepositoryAction::FilesLoaded { .. })
        );
        let task = (|| {
            if let Message::Key(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) = &message
            {
                if self.notices_open {
                    return self.close_notices();
                }
                if self.file_dialog.is_some() {
                    return self.update(Message::CancelFileOperation);
                }
            }
            if self.modal_open()
                && matches!(
                    &message,
                    Message::Choose(_)
                        | Message::Select(_)
                        | Message::CloseTab(_)
                        | Message::Complete
                        | Message::Hover
                        | Message::Completion(..)
                        | Message::Jump(..)
                        | Message::Sidebar(_)
                        | Message::Key(_)
                )
            {
                return Task::none();
            }
            match message {
                Message::WindowFocused(window, focused) => {
                    if self.core.main_window_id() == Some(window) {
                        self.window_focused = focused;
                    }
                    Task::none()
                }
                Message::RefreshVisible => self.refresh_visible(),
                Message::FileNameChanged(name) => {
                    if let Some(dialog) = self.file_dialog.as_mut().filter(|dialog| !dialog.busy) {
                        dialog.name = name;
                        dialog.error = None;
                    }
                    Task::none()
                }
                Message::SubmitFileOperation => self.submit_file_operation(),
                Message::CancelFileOperation => {
                    if self.file_dialog.as_ref().is_some_and(|dialog| !dialog.busy) {
                        self.file_dialog = None;
                        return self.resume_pending_files();
                    }
                    Task::none()
                }
                Message::FileOperationFinished(result) => self.finish_file_operation(result),
                Message::Choose(folder) => {
                    if self.closing.is_some() {
                        return Task::none();
                    }
                    self.selection_epoch = self.selection_epoch.wrapping_add(1);
                    let selection_epoch = self.selection_epoch;
                    cosmic::task::future(async move {
                        let dialog = cosmic::dialog::file_chooser::open::Dialog::new()
                            .title(if folder { "Open folder" } else { "Open file" });
                        let response = if folder {
                            dialog.open_folder().await
                        } else {
                            dialog.open_file().await
                        };
                        Message::Chosen(selection_epoch, chosen_path(response))
                    })
                }
                Message::Chosen(epoch, Ok(Some(path))) => Self::open(path, epoch),
                Message::Chosen(_, Ok(None)) => Task::none(),
                Message::Chosen(_, Err(error)) | Message::Opened(_, Err(error)) => {
                    self.notice = Some(error);
                    Task::none()
                }
                Message::SessionOpened(epoch, opened) => {
                    self.restoring_session = false;
                    if self.exiting {
                        return Task::none();
                    }
                    let mut tasks = Vec::new();
                    for (focus, result) in opened {
                        match result {
                            Ok(opened) => {
                                tasks.push(self.add(opened, focus && epoch == self.selection_epoch))
                            }
                            Err(error) => self.notice = Some(error),
                        }
                    }
                    Task::batch(tasks)
                }
                Message::Opened(epoch, Ok(opened)) => {
                    if self.exiting {
                        Task::none()
                    } else {
                        self.add(opened, epoch == self.selection_epoch)
                    }
                }
                Message::FileResolved(id, request, result) => {
                    let Some(pending) = self
                        .tab_mut(id)
                        .and_then(|tab| tab.pending_file.as_mut())
                        .filter(|pending| pending.request == request)
                    else {
                        return Task::none();
                    };
                    match result {
                        Ok(canonical) => pending.canonical = Some(canonical),
                        Err(error) => {
                            if let Some(tab) = self.tab_mut(id) {
                                tab.pending_file = None;
                            }
                            self.notice = Some(error);
                        }
                    }
                    self.resume_pending_files()
                }
                Message::Select(id) => {
                    self.selection_epoch = self.selection_epoch.wrapping_add(1);
                    self.active = Some(id);
                    Task::none()
                }
                Message::CloseTab(id) => self.request_close(id),
                Message::CloseWindow => {
                    // Shutdown removes runtime tabs to honor dirty-buffer guards;
                    // keep the user's pre-shutdown tab order and selection.
                    self.save_session();
                    if self.file_dialog.as_ref().is_some_and(|dialog| dialog.busy) {
                        self.exiting = true;
                        return Task::none();
                    }
                    self.file_dialog = None;
                    self.settings_open = false;
                    self.notices_open = false;
                    self.exiting = true;
                    self.continue_exit()
                }
                Message::SaveClose => {
                    let Some(card) = self
                        .closing
                        .filter(|id| self.tab(*id).is_some_and(|tab| tab.runtime.can_save()))
                    else {
                        return Task::none();
                    };
                    self.close_confirmed = true;
                    self.update(Message::Repository(RepositoryAction::Save {
                        card,
                        overwrite: false,
                    }))
                }
                Message::DiscardClose => {
                    if let Some(id) = self.closing {
                        if self.tab_mut(id).is_some_and(|tab| {
                            !tab.mutation && tab.runtime.abandon_unsaved_changes()
                        }) {
                            return self.request_close(id);
                        }
                    }
                    Task::none()
                }
                Message::ConfirmClose => {
                    if let Some(id) = self.closing.filter(|id| {
                        self.tab(*id)
                            .is_some_and(|tab| !tab.mutation && !tab.runtime.blocks_navigation())
                    }) {
                        return self.request_close(id);
                    }
                    Task::none()
                }
                Message::CancelClose => {
                    self.closing = None;
                    self.close_confirmed = false;
                    self.exiting = false;
                    self.resume_pending_files()
                }
                Message::Reaped => iced::exit().map(cosmic::Action::App),
                Message::ResolvedDiffBasis { card, basis } => {
                    let task = self.tab_mut(card).map_or_else(Task::none, |tab| {
                        repository_task(
                            tab.runtime
                                .update(RepositoryAction::ApplyDiffBasis { card, basis }),
                        )
                    });
                    Task::batch([task, self.drain(card)])
                }
                Message::Repository(action) => {
                    let id = action.card();
                    if self.modal_open() {
                        if !matches!(
                            action,
                            RepositoryAction::FilesLoaded { .. }
                                | RepositoryAction::FileLoaded { .. }
                                | RepositoryAction::DiffLoaded { .. }
                                | RepositoryAction::DiffBasisResolved { .. }
                                | RepositoryAction::WorkingDiffDue { .. }
                                | RepositoryAction::WorkingDiffLoaded { .. }
                                | RepositoryAction::SaveFinished { .. }
                                | RepositoryAction::AutoSaveDue { .. }
                                | RepositoryAction::Save { .. }
                                | RepositoryAction::Reload { .. }
                        ) {
                            return Task::none();
                        }
                    }
                    if let RepositoryAction::OpenFile { path, .. } = action {
                        return self.request_file(id, path);
                    }
                    if let RepositoryAction::StageSelected { stage, .. } = action {
                        return self.stage(id, stage);
                    }
                    if let RepositoryAction::RefreshFiles { .. } = action {
                        return self.refresh(id);
                    }
                    if let RepositoryAction::MarkdownLink { uri, .. } = &action {
                        let uri = uri.to_string();
                        if !["https://", "http://", "mailto:"]
                            .iter()
                            .any(|scheme| uri.starts_with(scheme))
                        {
                            self.notice = Some("Only HTTP, HTTPS, and mail links can be opened from document previews.".into());
                            return Task::none();
                        }
                        return cosmic::task::future(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                let status =
                                    std::process::Command::new("xdg-open").arg(uri).status()?;
                                if status.success() {
                                    Ok(())
                                } else {
                                    Err(std::io::Error::other(format!(
                                        "Link opener exited with {status}"
                                    )))
                                }
                            })
                            .await;
                            Message::SettingsSaved(
                                result
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result.map_err(|error| error.to_string())),
                            )
                        });
                    }
                    let basis_resolved =
                        matches!(action, RepositoryAction::DiffBasisResolved { .. });
                    let loaded = matches!(action, RepositoryAction::FilesLoaded { .. });
                    let focus_loaded = matches!(action, RepositoryAction::FileLoaded { .. })
                        && self.active == Some(id)
                        && !self.modal_open();
                    let saved = matches!(action, RepositoryAction::SaveFinished { .. });
                    let back_to_files = matches!(action, RepositoryAction::BackToFiles { .. });
                    let Some(tab) = self.tab_mut(id) else {
                        return Task::none();
                    };
                    if let RepositoryAction::FilesLoaded {
                        result: Ok(files), ..
                    } = &action
                    {
                        if !tab.git {
                            let paths: Vec<_> = files
                                .iter()
                                .filter_map(|file| RepoPath::parse(&file.path).ok())
                                .collect();
                            if let Some(key) = tab.navigation.active_worktree().cloned() {
                                tab.sidebar.source_control =
                                    Some(SourceControlModel::directory(key, &paths));
                            }
                        }
                    }
                    let task = tab
                        .runtime
                        .update(action)
                        .map(move |action| match action {
                            RepositoryAction::ApplyDiffBasis { card, basis } if basis_resolved => {
                                Message::ResolvedDiffBasis { card, basis }
                            }
                            action => Message::Repository(action),
                        })
                        .map(cosmic::Action::App);
                    let mut tasks = vec![task];
                    if back_to_files && tab.runtime.selected_path().is_none() {
                        tab.owned_file = None;
                        tab.pending_file = None;
                    }
                    if loaded {
                        tab.files_loaded = true;
                    }
                    if focus_loaded {
                        tasks.push(tab.runtime.focus_editor(id).map(cosmic::Action::App));
                    }
                    tasks.push(self.drain(id));
                    if loaded || saved {
                        tasks.push(self.resume_pending_files());
                    }
                    if saved {
                        if self.close_confirmed
                            && self.closing == Some(id)
                            && self.tab(id).is_some_and(|tab| {
                                !tab.mutation && !tab.runtime.blocks_navigation()
                            })
                        {
                            tasks.push(self.request_close(id));
                        } else if self.closing.is_none() {
                            if let Some(tab) = self.tab_mut(id) {
                                tasks.push(repository_task(tab.runtime.refresh_visible(id)));
                            }
                            tasks.push(self.refresh_snapshot(id, true));
                        }
                    }
                    Task::batch(tasks)
                }
                Message::Sidebar(action) => {
                    if self.modal_open() {
                        return Task::none();
                    }
                    if let SidebarAction::SetFileListMode { mode, .. } = &action {
                        self.file_list_mode = *mode;
                        for tab in &mut self.tabs {
                            if let Some(key) = tab.navigation.active_worktree().cloned() {
                                tab.navigation
                                    .apply(&SidebarAction::SetFileListMode { key, mode: *mode });
                            }
                        }
                        if self.settings_writable {
                            let result = settings::save(&self.settings_path, &self.settings)
                                .map_err(|error| error.to_string())
                                .and_then(|()| {
                                    crate::preferences::save(&self.settings_path, *mode)
                                });
                            if let Err(error) = result {
                                self.notice = Some(error);
                            }
                        } else {
                            self.notice = Some(
                                "Repair the settings file before saving the file-list preference."
                                    .into(),
                            );
                        }
                        return Task::none();
                    }
                    let Some(id) = self.active else {
                        return Task::none();
                    };
                    let Some(tab) = self.tab_mut(id) else {
                        return Task::none();
                    };
                    let key = match &action {
                        SidebarAction::OpenFile { key, .. }
                        | SidebarAction::FileCommand { key, .. }
                        | SidebarAction::RefreshWorktree { key }
                        | SidebarAction::FilesQueryChanged { key, .. }
                        | SidebarAction::ToggleDirectory { key, .. }
                        | SidebarAction::ToggleSection { key, .. } => Some(key),
                        _ => None,
                    };
                    if key.is_some_and(|key| tab.navigation.active_worktree() != Some(key)) {
                        return Task::none();
                    }
                    tab.navigation.apply(&action);
                    match action {
                        SidebarAction::FileCommand { path, command, .. } => match command {
                            FileCommand::CopyPath => iced::clipboard::write(
                                tab.runtime
                                    .descriptor
                                    .worktree
                                    .join(path.as_path())
                                    .to_string_lossy()
                                    .into_owned(),
                            )
                            .map(cosmic::Action::App),
                            FileCommand::CopyRelativePath => iced::clipboard::write(
                                path.as_path().to_string_lossy().into_owned(),
                            )
                            .map(cosmic::Action::App),
                            FileCommand::Rename | FileCommand::CopyFile => self
                                .begin_file_operation(
                                    id,
                                    path.as_path().to_path_buf(),
                                    command == FileCommand::CopyFile,
                                ),
                        },
                        SidebarAction::OpenFile { path, .. } => {
                            self.request_file(id, path.as_path().to_path_buf())
                        }
                        SidebarAction::RefreshWorktree { .. } | SidebarAction::Refresh => {
                            self.refresh(id)
                        }
                        _ => Task::none(),
                    }
                }
                Message::Snapshot(id, result) => {
                    if let Some(tab) = self.tab_mut(id) {
                        if !tab.snapshot_loading {
                            return Task::none();
                        }
                        tab.snapshot_loading = false;
                        if std::mem::take(&mut tab.snapshot_refresh_pending) {
                            // A mutation completed during this read. Discard its
                            // stale result and run exactly one fresh snapshot.
                            return self.refresh_snapshot(id, false);
                        }
                        match result {
                            Ok(snapshot) => {
                                if let Some(key) = tab.navigation.active_worktree().cloned() {
                                    tab.sidebar.source_control =
                                        Some(SourceControlModel::ready(key, &snapshot));
                                }
                            }
                            Err(error) => self.notice = Some(error),
                        }
                    }
                    Task::none()
                }
                Message::Mutation(id, result) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.mutation = false;
                    }
                    self.notice = match result {
                        Ok(MutationOutcome::Applied { .. }) => None,
                        Ok(MutationOutcome::NotApplied { failure }) => Some(failure.to_string()),
                        Ok(MutationOutcome::OutcomeUnknown { failure }) => {
                            Some(format!("Git outcome is uncertain: {failure}"))
                        }
                        Ok(MutationOutcome::PartiallyApplied { failures, .. }) => Some(format!(
                            "Some paths were not changed: {}",
                            failures
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join("; ")
                        )),
                        Err(error) => Some(error),
                    };
                    if self.close_confirmed
                        && self.closing == Some(id)
                        && self
                            .tab(id)
                            .is_some_and(|tab| !tab.runtime.blocks_navigation())
                    {
                        self.request_close(id)
                    } else {
                        self.refresh(id)
                    }
                }
                Message::Settings => {
                    if self.closing.is_none() && self.file_dialog.is_none() {
                        self.notices_open = false;
                        self.settings_open = !self.settings_open;
                        if self.settings_open {
                            return self.save_settings();
                        }
                    }
                    self.resume_pending_files()
                }
                Message::AutoSave(enabled) => {
                    self.settings.editor_auto_save = enabled;
                    let mut tasks: Vec<_> = self
                        .tabs
                        .iter_mut()
                        .map(|tab| {
                            tab.runtime.set_auto_save(enabled);
                            repository_task(tab.runtime.auto_save_task(tab.id))
                        })
                        .collect();
                    tasks.push(self.save_settings());
                    Task::batch(tasks)
                }
                Message::WrapLines(enabled) => {
                    self.settings.editor_wrap_lines = enabled;
                    for tab in &mut self.tabs {
                        tab.runtime.set_wrap_lines(enabled);
                    }
                    self.save_settings()
                }
                Message::LanguageServerAssistance(enabled) => {
                    self.set_language_server_assistance(enabled)
                }
                Message::Theme(theme) => {
                    self.settings.app_theme = theme;
                    Task::batch([self.save_settings(), self.appearance()])
                }
                Message::Scale(scale) => {
                    self.settings.ui_scale = scale;
                    Task::batch([self.save_settings(), self.appearance()])
                }
                Message::SettingsSaved(result) => {
                    if let Err(error) = result {
                        self.notice = Some(error);
                    }
                    Task::none()
                }
                Message::Notices => {
                    if self.notices_open {
                        return self.close_notices();
                    }
                    if !self.modal_open() {
                        self.notices_open = true;
                        return iced::widget::operation::focus(widget::Id::new(
                            "ceditor.notices.close",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Task::none()
                }
                Message::CloseNotices => self.close_notices(),
                Message::Dismiss => {
                    self.notice = None;
                    for tab in &mut self.tabs {
                        tab.lsp_error = None;
                    }
                    self.close_notices()
                }
                Message::Lsp(generation, event) => {
                    if generation != self.lsp.generation() {
                        return Task::none();
                    }
                    if let Some(event) = event {
                        let doc = match &event {
                            LspEvent::Diagnostics { doc, .. }
                            | LspEvent::Completion { doc, .. }
                            | LspEvent::Hover { doc, .. }
                            | LspEvent::Locations { doc, .. }
                            | LspEvent::Status { doc, .. }
                            | LspEvent::Progress { doc, .. }
                            | LspEvent::RequestFailed { doc, .. }
                            | LspEvent::DocumentDesynced { doc, .. } => *doc,
                        };
                        if let Some(tab) = self
                            .tabs
                            .iter_mut()
                            .find(|tab| tab.runtime.lsp_document() == Some(doc))
                        {
                            match event {
                                LspEvent::Diagnostics { items, .. } => tab.diagnostics = items,
                                LspEvent::Completion { items, .. } => tab.completions = items,
                                LspEvent::Hover { markdown, .. } => tab.hover = markdown,
                                LspEvent::Status { status, .. } => {
                                    if let Some(error) = language_server_failure(status) {
                                        tab.lsp_error = Some(error);
                                    }
                                }
                                LspEvent::RequestFailed { error, .. }
                                    if !matches!(error, cagents::lsp::LspError::Cancelled) =>
                                {
                                    tab.lsp_error = Some(error.to_string())
                                }
                                LspEvent::DocumentDesynced { error, .. } => {
                                    tab.completions.clear();
                                    tab.hover = None;
                                    tab.diagnostics.clear();
                                    tab.lsp_error = Some(error.to_string());
                                }
                                _ => {}
                            }
                        }
                        self.lsp_next()
                    } else {
                        Task::none()
                    }
                }
                Message::Complete | Message::Hover => {
                    if self.modal_open() {
                        return Task::none();
                    }
                    if let Some(tab) = self.current() {
                        if let (Some(doc), Some(at)) =
                            (tab.runtime.lsp_document(), tab.runtime.cursor_position())
                        {
                            if matches!(message, Message::Complete) {
                                self.lsp.request_completion(doc, at);
                            } else {
                                self.lsp.request_hover(doc, at);
                            }
                        }
                    }
                    Task::none()
                }
                Message::Completion(id, index) => {
                    if self.modal_open() {
                        return Task::none();
                    }
                    if let Some(tab) = self.tab_mut(id) {
                        if let Some(item) = tab.completions.get(index).cloned() {
                            tab.runtime.apply_completion(&item);
                            tab.completions.clear();
                        }
                    }
                    self.drain(id)
                }
                Message::Jump(id, at) => {
                    if let Some(tab) = self.tab_mut(id) {
                        tab.runtime.move_cursor_to(at);
                        return tab.runtime.focus_editor(id).map(cosmic::Action::App);
                    }
                    Task::none()
                }
                Message::Key(keyboard::Event::KeyPressed {
                    key,
                    modifiers,
                    repeat: false,
                    ..
                }) => {
                    if self.modal_open() {
                        return Task::none();
                    }
                    if modifiers.control() {
                        if let keyboard::Key::Character(key) = key {
                            match key.as_str().to_ascii_lowercase().as_str() {
                                "o" => return self.update(Message::Choose(modifiers.shift())),
                                "s" => {
                                    if let Some(card) = self.active {
                                        return self.update(Message::Repository(
                                            RepositoryAction::Save {
                                                card,
                                                overwrite: false,
                                            },
                                        ));
                                    }
                                }
                                "w" => {
                                    if let Some(card) = self.active {
                                        return self.request_close(card);
                                    }
                                }
                                " " => return self.update(Message::Complete),
                                _ => {}
                            }
                        }
                    }
                    Task::none()
                }
                Message::Key(_) => Task::none(),
            }
        })();
        if navigation_changed || previous_tabs != (self.active, self.tabs.len()) {
            self.save_session();
        }
        let tabs_changed = previous_tabs != (self.active, self.tabs.len());
        let refresh =
            self.can_refresh_visible() && (previous_tabs.0 != self.active || !was_refresh_visible);
        Task::batch([
            task,
            if tabs_changed {
                self.reveal_active_tab()
            } else {
                Task::none()
            },
            if refresh {
                self.refresh_visible()
            } else {
                Task::none()
            },
        ])
    }

    fn subscription(&self) -> Subscription<Message> {
        let events = iced::event::listen_with(|event, _, window| match event {
            iced::Event::Keyboard(event) => Some(Message::Key(event)),
            iced::Event::Window(window::Event::CloseRequested) => Some(Message::CloseWindow),
            iced::Event::Window(window::Event::Focused) => {
                Some(Message::WindowFocused(window, true))
            }
            iced::Event::Window(window::Event::Unfocused) => {
                Some(Message::WindowFocused(window, false))
            }
            _ => None,
        });
        let refresh = if self.can_refresh_visible() {
            iced::time::every(std::time::Duration::from_secs(10)).map(|_| Message::RefreshVisible)
        } else {
            Subscription::none()
        };
        Subscription::batch([events, refresh])
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        vec![
            icon_control(
                widget::icon::from_name("document-open-symbolic").handle(),
                "Open file (Ctrl+O)",
                false,
                !self.modal_open(),
                Some(Message::Choose(false)),
            )
            .into(),
            icon_control(
                widget::icon::from_name("folder-open-symbolic").handle(),
                "Open folder (Ctrl+Shift+O)",
                false,
                !self.modal_open(),
                Some(Message::Choose(true)),
            )
            .into(),
        ]
    }
    fn header_end(&self) -> Vec<Element<'_, Message>> {
        let mut end: Vec<Element<'_, Message>> = Vec::new();
        if let Some(notices) = self.notices_control() {
            end.push(notices);
        }
        end.push(
            icon_control(
                widget::icon::from_name("emblem-system-symbolic").handle(),
                "Settings",
                self.settings_open,
                self.closing.is_none() && self.file_dialog.is_none(),
                Some(Message::Settings),
            )
            .into(),
        );
        end
    }
    fn view(&self) -> Element<'_, Message> {
        let tabs: Vec<Element<'_, Message>> = self
            .tabs
            .iter()
            .map(|tab| {
                let selected = self.active == Some(tab.id);
                let title = format!(
                    "{}{}",
                    if tab.runtime.is_dirty() { "• " } else { "" },
                    tab.title
                );
                widget::container(
                    widget::row::with_children(vec![
                        cagents::interaction::tooltip(
                            widget::button::custom(
                                widget::row::with_children(vec![
                                    design::body(title.clone())
                                        .wrapping(iced::widget::text::Wrapping::None)
                                        .ellipsize(iced::widget::text::Ellipsize::End(
                                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                                        ))
                                        .width(Length::Fill)
                                        .into(),
                                ])
                                .height(Length::Fill)
                                .align_y(iced::Alignment::Center)
                                .width(Length::Fill),
                            )
                            .name(title.clone())
                            .class(tab_title_style())
                            .padding([0, 8])
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .on_press(Message::Select(tab.id)),
                            title,
                        ),
                        icon_control(
                            widget::icon::from_name("window-close-symbolic").handle(),
                            "Close tab",
                            false,
                            !self.modal_open(),
                            Some(Message::CloseTab(tab.id)),
                        )
                        .into(),
                    ])
                    .align_y(iced::Alignment::Center)
                    .spacing(2)
                    .height(Length::Fill),
                )
                .width(184)
                .height(design::PRIMARY_CONTROL_SIZE)
                .class(cosmic::theme::Container::custom(move |theme| {
                    iced::widget::container::Style {
                        background: selected.then(|| {
                            iced::Background::Color(
                                theme
                                    .current_container()
                                    .component
                                    .selected_state_color()
                                    .into(),
                            )
                        }),
                        border: iced::Border {
                            radius: theme.cosmic().corner_radii.radius_s.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                }))
                .into()
            })
            .collect();
        let mut body = Vec::new();
        // The COSMIC header above has no separator of its own: always rule
        // off the content below it, tabs or placeholder alike.
        body.push(widget::divider::horizontal::default().into());
        if !tabs.is_empty() {
            body.push(
                widget::scrollable::horizontal(
                    widget::row::with_children(tabs)
                        .spacing(2)
                        .height(design::PRIMARY_CONTROL_SIZE),
                )
                .id(widget::Id::new("ceditor.tabs"))
                .direction(iced::widget::scrollable::Direction::Horizontal(
                    iced::widget::scrollable::Scrollbar::hidden(),
                ))
                .width(Length::Fill)
                .height(design::PRIMARY_CONTROL_SIZE)
                .into(),
            );
            body.push(widget::divider::horizontal::default().into());
        }
        if let Some(tab) = self.current() {
            let mut editor = Vec::new();
            let mut toolbar_controls = Vec::new();
            if let Some(toolbar) = repository_widget::selected_file_toolbar(tab.id, &tab.runtime) {
                toolbar_controls.push(toolbar.map(Message::Repository));
            }
            if tab.runtime.lsp_document().is_some() {
                toolbar_controls.push(
                    widget::container(segmented_controls(vec![
                        icon_control(
                            widget::icon::from_name("insert-text-symbolic").handle(),
                            "Complete (Ctrl+Space)",
                            false,
                            !self.modal_open(),
                            Some(Message::Complete),
                        )
                        .into(),
                        icon_control(
                            widget::icon::from_name("dialog-information-symbolic").handle(),
                            "Show symbol information",
                            false,
                            !self.modal_open(),
                            Some(Message::Hover),
                        )
                        .into(),
                    ]))
                    .padding(iced::Padding {
                        right: 12.0,
                        ..iced::Padding::ZERO
                    })
                    .into(),
                );
            }
            if !toolbar_controls.is_empty() {
                editor.push(
                    widget::container(
                        widget::row::with_children(toolbar_controls)
                            .spacing(8)
                            .align_y(iced::Alignment::Center)
                            .width(Length::Fill),
                    )
                    .height(design::HEADER_HEIGHT)
                    .width(Length::Fill)
                    .into(),
                );
                editor.push(widget::divider::horizontal::default().into());
            }
            editor.push(repository_widget::view(tab.id, &tab.runtime).map(Message::Repository));
            if !tab.completions.is_empty() || !tab.diagnostics.is_empty() || tab.hover.is_some() {
                let mut results = Vec::new();
                for (index, item) in tab.completions.iter().enumerate() {
                    results.push(
                        widget::button::text(&item.label)
                            .on_press(Message::Completion(tab.id, index))
                            .into(),
                    );
                }
                if let Some(hover) = &tab.hover {
                    results.push(widget::text::body(hover).into());
                }
                for item in &tab.diagnostics {
                    results.push(
                        widget::button::text(format!(
                            "{}:{} {}",
                            item.range.start.line + 1,
                            item.range.start.character + 1,
                            item.message
                        ))
                        .on_press(Message::Jump(tab.id, item.range.start))
                        .into(),
                    );
                }
                editor.push(widget::divider::horizontal::default().into());
                editor.push(
                    widget::scrollable(widget::column::with_children(results).spacing(4))
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            cagents::design::scrollbar(),
                        ))
                        .height(150)
                        .into(),
                );
            }
            body.push(
                widget::row::with_children(vec![
                    sidebar::file_panel(&tab.sidebar, &tab.navigation, 280.0, self.modal_open())
                        .map(Message::Sidebar),
                    widget::divider::vertical::default().into(),
                    widget::column::with_children(editor)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                ])
                .height(Length::Fill)
                .into(),
            );
        } else {
            body.push(widget::container(widget::column::with_children(vec![widget::text::title1("Open a file or folder").into(), widget::text::body("Native editing, safe autosave, Git diffs, and language servers — without a daemon.").into(), widget::button::standard("Open folder").on_press(Message::Choose(true)).into()]).spacing(16)).padding(32).height(Length::Fill).into());
        }
        widget::column::with_children(body)
            .height(Length::Fill)
            .into()
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        if let Some(dialog) = &self.file_dialog {
            let title = if dialog.duplicate {
                "Duplicate file"
            } else {
                "Rename file"
            };
            let mut name = widget::text_input("File name", &dialog.name)
                .id(widget::Id::new("ceditor.file.name"));
            if !dialog.busy {
                name = name
                    .on_input(Message::FileNameChanged)
                    .on_submit(|_| Message::SubmitFileOperation);
            }
            let mut content = vec![
                widget::text::title3(title).into(),
                widget::text::body(dialog.source.to_string_lossy()).into(),
                design::caption(if dialog.duplicate {
                    "Copies the saved file on disk, not unsaved editor changes. Existing files are never replaced."
                } else {
                    "Rename in the same directory. Existing files are never replaced."
                }).into(),
                name.into(),
            ];
            if let Some(error) = &dialog.error {
                content.push(widget::text::body(error).into());
            }
            content.push(
                widget::row::with_children(vec![
                    widget::button::standard("Cancel")
                        .on_press_maybe((!dialog.busy).then_some(Message::CancelFileOperation))
                        .into(),
                    widget::button::standard(if dialog.busy { "Working…" } else { title })
                        .on_press_maybe((!dialog.busy).then_some(Message::SubmitFileOperation))
                        .into(),
                ])
                .spacing(8)
                .into(),
            );
            return Some(cagents::interaction::opaque(
                widget::container(widget::column::with_children(content).spacing(12))
                    .class(cosmic::theme::Container::Dialog(false))
                    .padding(20)
                    .width(480),
            ));
        }
        if let Some(id) = self.closing {
            let tab = self.tab(id)?;
            let ready = !tab.runtime.blocks_navigation() && !tab.mutation;
            let mut controls = vec![
                widget::button::standard("Cancel")
                    .id(widget::Id::new("ceditor.close.cancel"))
                    .on_press(Message::CancelClose)
                    .into(),
            ];
            if ready {
                controls.push(
                    widget::button::standard("Close")
                        .id(widget::Id::new("ceditor.close.confirm"))
                        .on_press(Message::ConfirmClose)
                        .into(),
                );
            } else {
                if tab.runtime.can_abandon_unsaved_changes() && !tab.mutation {
                    controls.push(
                        widget::button::destructive("Discard and close")
                            .id(widget::Id::new("ceditor.close.discard"))
                            .on_press(Message::DiscardClose)
                            .into(),
                    );
                }
                if tab.runtime.can_save() {
                    controls.push(
                        widget::button::standard("Save and close")
                            .id(widget::Id::new("ceditor.close.save"))
                            .on_press(Message::SaveClose)
                            .into(),
                    );
                }
            }
            return Some(cagents::interaction::opaque(
                widget::container(
                    widget::column::with_children(vec![
                        widget::text::title3(if ready {
                            "Close tab?"
                        } else {
                            "Finish saving before closing"
                        })
                        .into(),
                        widget::text::body(tab.runtime.save_error().unwrap_or(if tab.mutation {
                            "A Git operation is still running."
                        } else if ready {
                            "The tab is ready to close."
                        } else {
                            "This file has unsaved changes or an unfinished save."
                        }))
                        .into(),
                        widget::row::with_children(controls).spacing(8).into(),
                    ])
                    .spacing(16),
                )
                .class(cosmic::theme::Container::Dialog(false))
                .padding(24)
                .width(520),
            ));
        }
        if !self.settings_open {
            return None;
        }
        let theme_choices: Vec<Element<'_, Message>> = [
            ("System", AppTheme::System),
            ("Light", AppTheme::Light),
            ("Dark", AppTheme::Dark),
        ]
        .into_iter()
        .map(|(label, theme)| {
            let selected = self.settings.app_theme == theme;
            widget::button::custom(design::body(label))
                .name(label)
                .class(if selected {
                    cosmic::theme::Button::Suggested
                } else {
                    cosmic::theme::Button::Text
                })
                .selected(selected)
                .width(Length::Fill)
                .on_press(Message::Theme(theme))
                .into()
        })
        .collect();
        let content = widget::column::with_children(vec![
            widget::text::title4("Editing").into(),
            widget::toggler(self.settings.editor_auto_save)
                .label("Save files automatically".to_owned())
                .on_toggle(Message::AutoSave)
                .into(),
            design::caption("Save after a pause. Conflicts and save errors still require your attention.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            widget::toggler(self.settings.editor_wrap_lines)
                .label("Wrap long lines".to_owned())
                .on_toggle(Message::WrapLines)
                .into(),
            design::caption("Fit long lines to the editor width. Off keeps each line on one row.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            widget::text::title4("Appearance").into(),
            widget::text::body("Color scheme").into(),
            widget::row::with_children(theme_choices).spacing(8).into(),
            widget::row::with_children(vec![
                widget::text::body("Interface scale").width(Length::Fill).into(),
                widget::button::text("Smaller")
                    .on_press_maybe((self.settings.ui_scale > 0.75).then_some(
                        Message::Scale((self.settings.ui_scale - 0.1).max(0.75)),
                    ))
                    .into(),
                widget::text::body(format!("{:.0}%", self.settings.ui_scale * 100.0)).into(),
                widget::button::text("Larger")
                    .on_press_maybe((self.settings.ui_scale < 2.0).then_some(
                        Message::Scale((self.settings.ui_scale + 0.1).min(2.0)),
                    ))
                    .into(),
            ]).spacing(12).align_y(iced::Alignment::Center).into(),
            widget::text::title4("Language servers").into(),
            widget::checkbox(self.language_server_assistance)
                .label("Language-server assistance")
                .on_toggle(Message::LanguageServerAssistance)
                .into(),
            design::caption("Opt in to semantic diagnostics, completion, and symbol information. Syntax highlighting, saving, and Git work without a language server.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            design::caption("Edit language_servers in the preferences file, then restart ceditor to apply server changes.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
            design::caption(self.settings_path.to_string_lossy())
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
        ]).spacing(12).width(Length::Fill);
        Some(cagents::interaction::opaque(
            widget::container(widget::column::with_children(vec![
                widget::text::title3("Editor settings").into(),
                design::caption(if self.settings_writable {
                    "Changes apply immediately and save automatically."
                } else {
                    "Changes apply now, but cannot be saved until the preferences file is repaired."
                }).wrapping(iced::widget::text::Wrapping::Word).into(),
                widget::scrollable(content)
                    .direction(iced::widget::scrollable::Direction::Vertical(cagents::design::scrollbar()))
                    .height(Length::Shrink).into(),
                widget::row::with_children(vec![
                    iced::widget::Space::new().width(Length::Fill).into(),
                    widget::button::suggested("Done").on_press(Message::Settings).into(),
                ]).into(),
            ]).spacing(20))
                .class(cosmic::theme::Container::Dialog(false))
                .padding(24)
                .width(560)
                .max_height(680),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagents::repository_widget::AsyncIdentity;

    fn editor() -> Editor {
        Editor {
            core: Core::default(),
            tabs: Vec::new(),
            active: None,
            selection_epoch: 0,
            settings: AppSettings::default(),
            settings_path: PathBuf::new(),
            settings_open: false,
            settings_writable: false,
            notice: None,
            notices_open: false,
            file_dialog: None,
            closing: None,
            close_confirmed: false,
            exiting: false,
            window_focused: false,
            lsp: LspManager::new(Vec::new()),
            language_server_assistance: false,
            next_document: 1,
            file_list_mode: FileListMode::Tree,
            restoring_session: false,
            saved_session: crate::preferences::Session::default(),
        }
    }

    #[test]
    fn notices_control_hides_when_there_is_nothing_to_show() {
        let app = editor();
        assert!(app.notices_control().is_none());

        let mut with_notice = editor();
        with_notice.notice = Some("boom".into());
        assert!(with_notice.notices_control().is_some());

        let mut open_empty = editor();
        open_empty.notices_open = true;
        assert!(open_empty.notices_control().is_some());
    }

    #[test]
    fn assistance_opt_in_and_disable_leave_the_open_buffer_intact() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.rs"), "fn main() {}\n").unwrap();
        let mut app = editor();
        app.settings_path = directory.path().join("settings.toml");
        assert!(!crate::preferences::load_language_server_assistance(&app.settings_path).unwrap());
        app.settings_writable = true;
        for server in &mut app.settings.language_servers {
            server.command = vec![
                directory
                    .path()
                    .join("missing-server")
                    .to_string_lossy()
                    .into_owned(),
            ];
        }
        app.lsp = LspManager::new(app.settings.language_servers.clone());
        let id = add(&mut app, directory.path().to_str().unwrap(), Some("a.rs"));
        files_loaded(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::FileLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: directory.path().to_path_buf(),
                    path: "a.rs".into(),
                    generation: 1,
                },
                result: cagents::editor::load_file(directory.path(), Path::new("a.rs")),
            })),
        );
        assert!(app.tab(id).unwrap().runtime.lsp_document().is_none());
        assert!(app.tab(id).unwrap().lsp_error.is_none());
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("// unsaved\n".into()),
                )),
            })),
        );
        drop(app.update(Message::LanguageServerAssistance(true)));
        let first_document = app.tab(id).unwrap().runtime.lsp_document().unwrap();
        assert!(app.tab(id).unwrap().lsp_error.is_some());
        assert!(crate::preferences::load_language_server_assistance(&app.settings_path).unwrap());
        drop(app.update(Message::LanguageServerAssistance(false)));
        assert!(app.tab(id).unwrap().runtime.lsp_document().is_none());
        assert!(!crate::preferences::load_language_server_assistance(&app.settings_path).unwrap());
        assert!(app.tab(id).unwrap().runtime.is_dirty());
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("a.rs"))
        );
        drop(app.update(Message::LanguageServerAssistance(true)));
        assert_ne!(
            app.tab(id).unwrap().runtime.lsp_document(),
            Some(first_document)
        );
        assert!(app.tab(id).unwrap().runtime.lsp_document().is_some());
        assert!(app.tab(id).unwrap().runtime.is_dirty());
        drop(app.update(Message::LanguageServerAssistance(false)));
    }

    #[test]
    fn closing_notices_preserves_failure_evidence_until_dismissal() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", Some("a.rs"));
        let doc = LspDocumentId(1);
        app.tab_mut(id).unwrap().runtime.set_lsp_document(Some(doc));
        let generation = app.lsp.generation();
        drop(app.update(Message::Lsp(
            generation,
            Some(LspEvent::Status {
                doc,
                status: LspStatus::Failed {
                    reason: "initialize failed".into(),
                    stderr_tail: vec![
                        "toolchain unavailable".into(),
                        "infinite recursion detected".into(),
                    ],
                },
            }),
        )));
        let evidence = app.tab(id).unwrap().lsp_error.clone().unwrap();
        assert!(evidence.contains("initialize failed"));
        assert!(evidence.contains("toolchain unavailable\ninfinite recursion detected"));
        drop(app.update(Message::Notices));
        assert!(app.modal_open());
        drop(app.update(Message::CloseNotices));
        assert!(!app.modal_open());
        assert_eq!(app.tab(id).unwrap().lsp_error.as_ref(), Some(&evidence));
        drop(app.update(Message::Notices));
        drop(app.update(Message::Dismiss));
        assert!(app.tab(id).unwrap().lsp_error.is_none());
        assert!(!app.modal_open());
        drop(app.update(Message::Lsp(
            generation,
            Some(LspEvent::Status {
                doc,
                status: LspStatus::Ready,
            }),
        )));
        assert!(app.tab(id).unwrap().lsp_error.is_none());
    }

    #[test]
    fn mutation_refresh_discards_superseded_snapshot_results() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", None);
        app.tab_mut(id).unwrap().git = true;
        drop(app.refresh_snapshot(id, false));
        drop(app.refresh_snapshot(id, true));
        drop(app.refresh_snapshot(id, true));

        // The old read must not publish even an error after a mutation has
        // requested a replacement.
        drop(app.update(Message::Snapshot(id, Err("superseded".into()))));
        assert!(app.notice.is_none());
        drop(app.update(Message::Snapshot(id, Err("current".into()))));
        assert_eq!(app.notice.as_deref(), Some("current"));

        // Several forced requests coalesce to one replacement, not a queue
        // that can keep publishing results after that replacement completes.
        drop(app.update(Message::Snapshot(id, Err("unexpected".into()))));
        assert_eq!(app.notice.as_deref(), Some("current"));
    }

    #[test]
    fn restoring_tabs_skips_missing_files_without_losing_order_or_selection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let first = root.join("z.txt");
        let last = root.join("a.txt");
        std::fs::write(&first, "first").unwrap();
        std::fs::write(&last, "last").unwrap();
        let session = crate::preferences::Session {
            paths: vec![first.clone(), root.join("missing.txt"), last.clone()],
            active: Some(2),
        };
        let opened = session
            .startup_paths(Vec::new())
            .into_iter()
            .map(|(path, focus)| (focus, Editor::resolve_open(path)))
            .collect();
        let mut app = editor();
        app.restoring_session = true;
        drop(app.update(Message::SessionOpened(0, opened)));
        assert_eq!(
            app.session(),
            crate::preferences::Session {
                paths: vec![first, last],
                active: Some(1),
            }
        );
    }

    #[test]
    fn explicit_startup_file_is_selected_without_duplicating_a_restored_tab() {
        let mut app = editor();
        let opened = |file: &str| {
            Ok(Opened {
                root: "/workspace".into(),
                file: Some(file.into()),
                snapshot: None,
            })
        };
        drop(app.update(Message::SessionOpened(
            0,
            vec![
                (false, opened("a.rs")),
                (false, opened("b.rs")),
                (true, opened("a.rs")),
            ],
        )));
        assert_eq!(
            app.session(),
            crate::preferences::Session {
                paths: vec!["/workspace/a.rs".into(), "/workspace/b.rs".into()],
                active: Some(0),
            }
        );
    }

    #[test]
    fn window_shutdown_keeps_tabs_but_individual_close_removes_them_from_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = editor();
        app.settings_path = directory.path().join("settings.toml");
        app.settings_writable = true;
        let first = add(&mut app, "/workspace", Some("a.rs"));
        let second = add(&mut app, "/workspace", Some("b.rs"));
        drop(app.update(Message::Select(first)));
        let initial = crate::preferences::load_session(&app.settings_path).unwrap();
        assert_eq!(
            initial.paths,
            vec![
                PathBuf::from("/workspace/a.rs"),
                PathBuf::from("/workspace/b.rs")
            ]
        );
        drop(app.update(Message::CloseTab(second)));
        let remaining = crate::preferences::load_session(&app.settings_path).unwrap();
        assert_eq!(remaining.paths, vec![PathBuf::from("/workspace/a.rs")]);
        assert_eq!(remaining.active, Some(0));
        drop(app.update(Message::CloseWindow));
        assert!(app.tabs.is_empty());
        assert_eq!(
            crate::preferences::load_session(&app.settings_path).unwrap(),
            remaining
        );
    }

    #[test]
    fn cancelling_a_chooser_preserves_the_document_and_existing_notice() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", Some("a.rs"));
        app.notice = Some("An earlier save failed".into());
        let result = chosen_path(Err(cosmic::dialog::file_chooser::Error::Cancelled));
        assert_eq!(result, Ok(None));
        drop(app.update(Message::Chosen(app.selection_epoch, result)));
        assert_eq!(app.active, Some(id));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.notice.as_deref(), Some("An earlier save failed"));

        let failure = chosen_path(Err(cosmic::dialog::file_chooser::Error::UrlAbsolute));
        assert!(failure.is_err());
        drop(app.update(Message::Chosen(app.selection_epoch, failure)));
        assert_ne!(app.notice.as_deref(), Some("An earlier save failed"));
    }

    fn add(app: &mut Editor, root: &str, file: Option<&str>) -> CardId {
        drop(app.add(
            Opened {
                root: root.into(),
                file: file.map(PathBuf::from),
                snapshot: None,
            },
            true,
        ));
        app.active.unwrap()
    }

    fn files_loaded(app: &mut Editor, id: CardId) {
        let worktree = app.tab(id).unwrap().runtime.descriptor.worktree.clone();
        drop(
            app.update(Message::Repository(RepositoryAction::FilesLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree,
                    path: ".".into(),
                    generation: 1,
                },
                result: Ok(Vec::new()),
            })),
        );
    }

    #[test]
    fn file_operation_names_stay_in_the_source_directory() {
        let source = Path::new("src/original.txt");
        for name in [
            "",
            ".",
            "..",
            "../outside.txt",
            "/outside.txt",
            "nested/file.txt",
            "file.txt/",
        ] {
            assert!(file_operation_destination(source, name).is_err());
        }
        assert!(file_operation_destination(source, "original.txt").is_err());
        assert_eq!(
            file_operation_destination(source, " renamed λ.txt ").unwrap(),
            Path::new("src/ renamed λ.txt "),
        );
    }

    #[test]
    fn dirty_file_cannot_be_renamed_but_can_be_duplicated_from_disk() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.txt"), "saved").unwrap();
        let mut app = editor();
        let id = add(&mut app, directory.path().to_str().unwrap(), Some("a.txt"));
        files_loaded(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::FileLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: directory.path().to_path_buf(),
                    path: "a.txt".into(),
                    generation: 1,
                },
                result: cagents::editor::load_file(directory.path(), Path::new("a.txt")),
            })),
        );
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("unsaved".into()),
                )),
            })),
        );
        assert!(app.tab(id).unwrap().runtime.is_dirty());
        drop(app.begin_file_operation(id, "a.txt".into(), false));
        assert!(app.file_dialog.is_none());
        assert!(app.tab(id).unwrap().runtime.is_dirty());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "saved"
        );

        drop(app.begin_file_operation(id, "a.txt".into(), true));
        assert!(app.file_dialog.as_ref().unwrap().duplicate);
        drop(app.update(Message::CancelFileOperation));
        assert!(app.tab(id).unwrap().runtime.is_dirty());
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("a.txt"))
        );
    }

    #[test]
    fn renamed_file_reopens_under_its_new_path_and_releases_the_old_owner() {
        let mut app = editor();
        let owner = add(&mut app, "/workspace", Some("src/old.txt"));
        files_loaded(&mut app, owner);
        let folder = add(&mut app, "/workspace/src", None);
        files_loaded(&mut app, folder);
        drop(app.begin_file_operation(folder, "old.txt".into(), false));
        app.file_dialog.as_mut().unwrap().busy = true;
        drop(app.finish_file_operation(Ok("new.txt".into())));
        assert_eq!(app.active, Some(folder));
        assert_eq!(app.file_owner(Path::new("/workspace/src/old.txt")), None);
        assert_eq!(
            app.file_owner(Path::new("/workspace/src/new.txt")),
            Some(owner)
        );
        assert_eq!(app.tab(owner).unwrap().runtime.selected_path(), None);

        // A refreshed file list permits the renamed document to reopen.
        drop(
            app.update(Message::Repository(RepositoryAction::FilesLoaded {
                identity: AsyncIdentity {
                    card: owner,
                    worktree: "/workspace".into(),
                    path: ".".into(),
                    generation: 2,
                },
                result: Ok(Vec::new()),
            })),
        );
        assert_eq!(
            app.tab(owner).unwrap().runtime.selected_path(),
            Some(Path::new("src/new.txt")),
        );
    }
    #[test]
    fn repeated_opens_and_overlapping_folder_tabs_reuse_the_buffer_owner() {
        let mut app = editor();
        let owner = add(&mut app, "/workspace", Some("src/a.rs"));
        assert_eq!(add(&mut app, "/workspace", Some("src/a.rs")), owner);
        assert_eq!(app.tabs.len(), 1);
        let folder = add(&mut app, "/workspace/src", None);
        files_loaded(&mut app, folder);
        drop(app.request_file(folder, "a.rs".into()));
        drop(app.update(Message::FileResolved(
            folder,
            1,
            Ok("/workspace/src/a.rs".into()),
        )));
        assert_eq!(app.active, Some(owner));
        assert_eq!(app.tab(folder).unwrap().runtime.selected_path(), None);
        files_loaded(&mut app, owner);
        assert_eq!(
            app.tab(owner).unwrap().runtime.selected_path(),
            Some(Path::new("src/a.rs"))
        );
        drop(app.update(Message::Select(folder)));
        drop(app.request_file(folder, "a.rs".into()));
        drop(app.update(Message::FileResolved(
            folder,
            2,
            Ok("/workspace/src/a.rs".into()),
        )));
        assert_eq!(app.active, Some(owner));
        assert_eq!(app.tab(folder).unwrap().runtime.selected_path(), None);
    }

    #[test]
    fn returning_to_files_releases_the_buffer_for_reopening_in_either_tab() {
        let mut app = editor();
        let original = add(&mut app, "/workspace", Some("src/a.rs"));
        files_loaded(&mut app, original);
        drop(
            app.update(Message::Repository(RepositoryAction::BackToFiles {
                card: original,
            })),
        );
        drop(app.request_file(original, "src/a.rs".into()));
        drop(app.update(Message::FileResolved(
            original,
            1,
            Ok("/workspace/src/a.rs".into()),
        )));
        assert_eq!(
            app.tab(original).unwrap().runtime.selected_path(),
            Some(Path::new("src/a.rs"))
        );
        drop(
            app.update(Message::Repository(RepositoryAction::BackToFiles {
                card: original,
            })),
        );
        let folder = add(&mut app, "/workspace/src", None);
        files_loaded(&mut app, folder);
        drop(app.request_file(folder, "a.rs".into()));
        drop(app.update(Message::FileResolved(
            folder,
            1,
            Ok("/workspace/src/a.rs".into()),
        )));
        assert_eq!(app.active, Some(folder));
        assert_eq!(
            app.tab(folder).unwrap().runtime.selected_path(),
            Some(Path::new("a.rs"))
        );
        assert_eq!(app.tab(original).unwrap().runtime.selected_path(), None);
    }

    #[test]
    fn modal_dismissal_resumes_inactive_and_new_tabs_without_stealing_focus() {
        for settings in [true, false] {
            let mut app = editor();
            let pending = add(&mut app, "/workspace", Some("a.rs"));
            let active = add(&mut app, "/other", None);
            app.settings_open = settings;
            app.closing = (!settings).then_some(active);
            files_loaded(&mut app, pending);
            drop(app.add(
                Opened {
                    root: "/new".into(),
                    file: Some("b.rs".into()),
                    snapshot: None,
                },
                true,
            ));
            let new = app.tabs.last().unwrap().id;
            files_loaded(&mut app, new);
            assert_eq!(app.tab(pending).unwrap().runtime.selected_path(), None);
            assert_eq!(app.tab(new).unwrap().runtime.selected_path(), None);
            drop(app.update(if settings {
                Message::Settings
            } else {
                Message::CancelClose
            }));
            assert_eq!(app.active, Some(active));
            assert_eq!(
                app.tab(pending).unwrap().runtime.selected_path(),
                Some(Path::new("a.rs"))
            );
            assert_eq!(
                app.tab(new).unwrap().runtime.selected_path(),
                Some(Path::new("b.rs"))
            );
        }
    }

    #[test]
    fn later_navigation_fences_initial_opens_and_older_path_resolution() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", Some("initial.rs"));
        drop(app.request_file(id, "older.rs".into()));
        drop(app.request_file(id, "latest.rs".into()));
        files_loaded(&mut app, id);
        drop(app.update(Message::FileResolved(
            id,
            1,
            Ok("/workspace/older.rs".into()),
        )));
        assert_eq!(app.tab(id).unwrap().runtime.selected_path(), None);
        drop(app.update(Message::FileResolved(
            id,
            2,
            Ok("/workspace/latest.rs".into()),
        )));
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("latest.rs"))
        );
        drop(app.update(Message::Opened(
            0,
            Ok(Opened {
                root: "/late".into(),
                file: Some("arrival.rs".into()),
                snapshot: None,
            }),
        )));
        assert_eq!(app.active, Some(id));
        let late = app.tabs.last().unwrap().id;
        files_loaded(&mut app, late);
        assert_eq!(app.active, Some(id));
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("latest.rs"))
        );
        assert_eq!(
            app.tab(late).unwrap().runtime.selected_path(),
            Some(Path::new("arrival.rs"))
        );
    }

    #[test]
    fn background_completion_requires_an_explicit_close_confirmation() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", None);
        app.tab_mut(id).unwrap().mutation = true;
        drop(app.request_close(id));
        drop(app.update(Message::ConfirmClose));
        assert!(app.tab(id).is_some());
        drop(app.update(Message::Mutation(id, Err("operation failed".into()))));
        assert!(app.tab(id).is_some());
        assert_eq!(app.closing, Some(id));
        drop(app.update(Message::ConfirmClose));
        assert!(app.tab(id).is_none());
    }
}
