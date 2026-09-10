use cagents::{
    lsp::{LspCompletionItem, LspDiagnostic, LspDocumentId, LspEvent, LspManager, LspPosition},
    model::{CardId, ProjectId, RepositoryView, WorktreeId},
    repository::{
        CancellationToken, MutationOutcome, RepoPath, RepositoryId, RepositoryService,
        RepositorySnapshot, StageRequest, UnstageRequest,
    },
    repository_widget::{self, LspIntent, RepositoryAction, RepositoryRuntime},
    settings::{self, AppSettings, AppTheme},
    sidebar::{
        self, ActiveWorktreeKey, FileListMode, SidebarAction, SidebarFocus, SidebarMode,
        SidebarModel, SidebarProject, SidebarState, SidebarWorktree, SourceControlModel,
    },
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
    lsp_status: String,
    mutation: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Choose(bool),
    Chosen(u64, Result<PathBuf, String>),
    Opened(u64, Result<Opened, String>),
    FileResolved(CardId, u64, Result<PathBuf, String>),
    Select(CardId),
    CloseTab(CardId),
    CloseWindow,
    SaveClose,
    DiscardClose,
    ConfirmClose,
    CancelClose,
    Reaped,
    Repository(RepositoryAction),
    Sidebar(SidebarAction),
    Snapshot(CardId, Result<RepositorySnapshot, String>),
    Mutation(CardId, Result<MutationOutcome, String>),
    Settings,
    AutoSave(bool),
    Theme(AppTheme),
    Scale(f32),
    SettingsSaved(Result<(), String>),
    Lsp(Option<LspEvent>),
    Complete,
    Hover,
    Completion(CardId, usize),
    Jump(CardId, LspPosition),
    Key(keyboard::Event),
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
    closing: Option<CardId>,
    close_confirmed: bool,
    exiting: bool,
    lsp: LspManager,
    next_document: u64,
    file_list_mode: FileListMode,
}

fn repository_task(task: iced::Task<RepositoryAction>) -> Task<Message> {
    task.map(Message::Repository).map(cosmic::Action::App)
}

impl Editor {
    fn tab(&self, id: CardId) -> Option<&Tab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }
    fn tab_mut(&mut self, id: CardId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }
    fn current(&self) -> Option<&Tab> {
        self.active.and_then(|id| self.tab(id))
    }

    fn open(path: PathBuf, selection_epoch: u64) -> Task<Message> {
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let path = path
                    .canonicalize()
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                let directory = if path.is_dir() {
                    path.clone()
                } else {
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
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::Opened(selection_epoch, result)
        })
    }

    fn add(&mut self, opened: Opened, focus: bool) -> Task<Message> {
        if let Some(file) = &opened.file {
            if let Some(owner) = self.file_owner(&opened.root.join(file)) {
                if focus && !self.settings_open && self.closing.is_none() {
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
            lsp_status: String::new(),
            mutation: false,
        });
        if self.active.is_none() || (focus && !self.settings_open && self.closing.is_none()) {
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
        if self.settings_open || self.closing.is_some() || self.exiting {
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
        cosmic::task::future(async move { Message::Lsp(events.recv().await.ok()) })
    }

    fn drain(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let intents = tab.runtime.take_lsp_intents();
        let autosave = repository_task(tab.runtime.auto_save_task(id));
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
                    if let Some(language) = self.lsp.language_for(&absolute_path) {
                        let doc = LspDocumentId(self.next_document);
                        self.next_document += 1;
                        let status =
                            self.lsp
                                .open(doc, &absolute_path, &worktree, language, version, &text);
                        if let Some(tab) = self.tab_mut(id) {
                            tab.runtime.set_lsp_document(Some(doc));
                            tab.lsp_status = format!("{status:?}");
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
        autosave
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
            tab.lsp_status.clear();
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

    fn refresh(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let files = repository_task(tab.runtime.refresh_after_mutation(id));
        if !tab.git {
            return files;
        }
        let root = tab.runtime.descriptor.worktree.clone();
        let snapshot = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .snapshot(&root, &CancellationToken::default())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::Snapshot(id, result)
        });
        Task::batch([files, snapshot])
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

    fn init(core: Core, flags: Flags) -> (Self, Task<Message>) {
        let (settings, mut notice, settings_writable) = match settings::load(&flags.settings_path) {
            Ok(settings) => (settings, None, true),
            Err(error) => (AppSettings::default(), Some(error.to_string()), false),
        };
        let file_list_mode =
            crate::preferences::load(&flags.settings_path).unwrap_or_else(|error| {
                notice = Some(error);
                FileListMode::Tree
            });
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
            closing: None,
            close_confirmed: false,
            exiting: false,
            lsp,
            next_document: 1,
            file_list_mode,
        };
        let mut tasks = vec![app.appearance(), app.lsp_next()];
        if let Some(window) = app.core.main_window_id() {
            tasks.push(app.set_window_title("ceditor".to_owned(), window));
        }
        tasks.extend(flags.paths.into_iter().map(|path| Self::open(path, 0)));
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
        if (self.settings_open || self.closing.is_some())
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
                    Message::Chosen(
                        selection_epoch,
                        response
                            .map_err(|error| error.to_string())
                            .and_then(|response| {
                                response
                                    .url()
                                    .to_file_path()
                                    .map_err(|()| "Only local files are supported".into())
                            }),
                    )
                })
            }
            Message::Chosen(epoch, Ok(path)) => Self::open(path, epoch),
            Message::Chosen(_, Err(error)) | Message::Opened(_, Err(error)) => {
                self.notice = Some(error);
                Task::none()
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
                self.settings_open = false;
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
                    if self
                        .tab_mut(id)
                        .is_some_and(|tab| !tab.mutation && tab.runtime.abandon_unsaved_changes())
                    {
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
            Message::Repository(action) => {
                let id = action.card();
                if self.settings_open || self.closing.is_some() {
                    if !matches!(
                        action,
                        RepositoryAction::FilesLoaded { .. }
                            | RepositoryAction::FileLoaded { .. }
                            | RepositoryAction::DiffLoaded { .. }
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
                let loaded = matches!(action, RepositoryAction::FilesLoaded { .. });
                let focus_loaded = matches!(action, RepositoryAction::FileLoaded { .. })
                    && self.active == Some(id)
                    && !self.settings_open
                    && self.closing.is_none();
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
                let mut tasks = vec![repository_task(tab.runtime.update(action))];
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
                        && self
                            .tab(id)
                            .is_some_and(|tab| !tab.mutation && !tab.runtime.blocks_navigation())
                    {
                        tasks.push(self.request_close(id));
                    } else if self.closing.is_none() {
                        tasks.push(self.refresh(id));
                    }
                }
                Task::batch(tasks)
            }
            Message::Sidebar(action) => {
                if self.closing.is_some() || self.settings_open {
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
                            .and_then(|()| crate::preferences::save(&self.settings_path, *mode));
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
                if self.closing.is_none() {
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
            Message::Dismiss => {
                self.notice = None;
                Task::none()
            }
            Message::Lsp(event) => {
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
                                tab.lsp_status = format!("{status:?}")
                            }
                            LspEvent::RequestFailed { error, .. }
                                if !matches!(error, cagents::lsp::LspError::Cancelled) =>
                            {
                                tab.lsp_status = error.to_string()
                            }
                            LspEvent::DocumentDesynced { error, .. } => {
                                tab.completions.clear();
                                tab.hover = None;
                                tab.diagnostics.clear();
                                tab.lsp_status = error.to_string();
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
                if self.closing.is_some() || self.settings_open {
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
                if self.closing.is_some() || self.settings_open {
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
                if self.closing.is_some() || self.settings_open {
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
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::event::listen_with(|event, _, _| match event {
            iced::Event::Keyboard(event) => Some(Message::Key(event)),
            iced::Event::Window(window::Event::CloseRequested) => Some(Message::CloseWindow),
            _ => None,
        })
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        vec![
            widget::text::title3("ceditor").into(),
            widget::button::text("Open file")
                .on_press(Message::Choose(false))
                .into(),
            widget::button::text("Open folder")
                .on_press(Message::Choose(true))
                .into(),
        ]
    }
    fn header_end(&self) -> Vec<Element<'_, Message>> {
        vec![
            widget::button::text("Settings")
                .on_press(Message::Settings)
                .into(),
        ]
    }
    fn view(&self) -> Element<'_, Message> {
        let tabs: Vec<Element<'_, Message>> = self
            .tabs
            .iter()
            .map(|tab| {
                widget::row::with_children(vec![
                    widget::button::custom(widget::text::body(format!(
                        "{}{}",
                        if tab.runtime.is_dirty() { "• " } else { "" },
                        tab.title
                    )))
                    .class(cosmic::theme::Button::Text)
                    .selected(self.active == Some(tab.id))
                    .on_press(Message::Select(tab.id))
                    .into(),
                    widget::button::text("×")
                        .on_press(Message::CloseTab(tab.id))
                        .into(),
                ])
                .spacing(2)
                .into()
            })
            .collect();
        let mut body = vec![
            widget::scrollable(widget::row::with_children(tabs).spacing(4))
                .direction(iced::widget::scrollable::Direction::Horizontal(
                    Default::default(),
                ))
                .into(),
        ];
        if let Some(notice) = &self.notice {
            body.push(
                widget::row::with_children(vec![
                    widget::text::body(notice).width(Length::Fill).into(),
                    widget::button::text("Dismiss")
                        .on_press(Message::Dismiss)
                        .into(),
                ])
                .padding(8)
                .into(),
            );
        }
        if let Some(tab) = self.current() {
            let mut editor = Vec::new();
            if let Some(toolbar) = repository_widget::selected_file_toolbar(tab.id, &tab.runtime) {
                editor.push(toolbar.map(Message::Repository));
            }
            editor.push(repository_widget::view(tab.id, &tab.runtime).map(Message::Repository));
            if tab.runtime.lsp_document().is_some() {
                editor.push(
                    widget::row::with_children(vec![
                        widget::text::caption(&tab.lsp_status)
                            .width(Length::Fill)
                            .into(),
                        widget::button::text("Complete")
                            .on_press(Message::Complete)
                            .into(),
                        widget::button::text("Hover")
                            .on_press(Message::Hover)
                            .into(),
                    ])
                    .spacing(8)
                    .into(),
                );
            }
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
                editor.push(
                    widget::scrollable(widget::column::with_children(results).spacing(4))
                        .height(150)
                        .into(),
                );
            }
            body.push(
                widget::row::with_children(vec![
                    sidebar::file_panel(
                        &tab.sidebar,
                        &tab.navigation,
                        280.0,
                        self.settings_open || self.closing.is_some(),
                    )
                    .map(Message::Sidebar),
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
                    widget::button::suggested("Close")
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
                        widget::button::suggested("Save and close")
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
        Some(cagents::interaction::opaque(widget::container(widget::column::with_children(vec![
            widget::text::title3("Editor settings").into(),
            widget::checkbox(self.settings.editor_auto_save).label("Autosave after editing").on_toggle(Message::AutoSave).into(),
            widget::row::with_children(vec![widget::button::standard("System theme").on_press(Message::Theme(AppTheme::System)).into(), widget::button::standard("Light").on_press(Message::Theme(AppTheme::Light)).into(), widget::button::standard("Dark").on_press(Message::Theme(AppTheme::Dark)).into()]).spacing(8).into(),
            widget::row::with_children(vec![widget::button::standard("Smaller UI").on_press(Message::Scale((self.settings.ui_scale - 0.1).max(0.75))).into(), widget::text::body(format!("{:.0}%", self.settings.ui_scale * 100.0)).into(), widget::button::standard("Larger UI").on_press(Message::Scale((self.settings.ui_scale + 0.1).min(2.0))).into()]).spacing(8).into(),
            widget::text::body(format!("Language servers and preferences: {}\nEdit language_servers in this file and restart ceditor to apply server changes.", self.settings_path.display())).into(),
            widget::button::suggested("Done").on_press(Message::Settings).into(),
        ]).spacing(16)).class(cosmic::theme::Container::Dialog(false)).padding(24).width(600)))
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
            closing: None,
            close_confirmed: false,
            exiting: false,
            lsp: LspManager::new(Vec::new()),
            next_document: 1,
            file_list_mode: FileListMode::Tree,
        }
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
