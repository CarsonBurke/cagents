use cagents::{
    design,
    lsp::{
        LspCompletionItem, LspDiagnostic, LspDocumentId, LspEvent, LspManager, LspPosition,
        LspStatus,
    },
    model::{CardId, ProjectId, RepositoryView, WorktreeId},
    repository::{
        CancellationToken, DiffBasis, MutationOutcome, PathSelection, RepoPath, RepositoryId,
        RepositoryService, RepositorySnapshot, StageRequest, UnstageRequest,
    },
    repository_widget::{
        self, ClipboardOp, EditorAction, EditorEdit, LspIntent, RepositoryAction, RepositoryRuntime,
    },
    settings::{self, AppSettings, AppTheme, ShortcutBinding, ShortcutKey},
    sidebar::{
        self, ActiveWorktreeKey, FileCommand, FileSearch, SidebarAction, SidebarFocus, SidebarMode,
        SidebarModel, SidebarProject, SidebarState, SidebarWorktree, SourceControlModel,
        WorktreePanel,
    },
    splitter::{self, Splitter},
    ui_controls::{icon_control, popover, segmented_controls, symbolic_icon},
};
use cosmic::app::ApplicationExt;
use cosmic::{
    Application, Core, Element,
    app::Task,
    iced::{self, Length, Subscription, keyboard, window},
    widget,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
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
    pending_diff: Option<bool>,
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
    ResolvedDiffBasis {
        card: CardId,
        basis: DiffBasis,
    },
    Sidebar(SidebarAction),
    Snapshot(CardId, Result<RepositorySnapshot, String>),
    Mutation(CardId, Result<MutationOutcome, String>),
    FileNameChanged(String),
    SubmitFileOperation,
    CancelFileOperation,
    FileOperationFinished(Result<PathBuf, String>),
    Settings,
    SelectSettingsCategory(SettingsCategory),
    ShortcutChanged(EditorShortcut, String),
    FindInFile,
    FindQueryChanged(String),
    CloseFind,
    QuickOpen,
    QuickOpenQueryChanged(String),
    OpenQuickFile(PathBuf),
    CloseQuickOpen,
    QuickOpenSearchFinished {
        card: CardId,
        generation: u64,
        query: String,
        result: Result<Vec<PathBuf>, String>,
    },
    AutoSave(bool),
    WrapLines(bool),
    SyntaxTheme(cagents::settings::EditorSyntaxTheme),
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
    ToggleSidebar,
    NewTab,
    SavedAs(CardId, Result<Option<PathBuf>, String>),
    Frame(Instant),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    Editing,
    Appearance,
    Keyboard,
    LanguageServers,
}

impl SettingsCategory {
    const ALL: [Self; 4] = [
        Self::Editing,
        Self::Appearance,
        Self::Keyboard,
        Self::LanguageServers,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Editing => "Editing",
            Self::Appearance => "Appearance",
            Self::Keyboard => "Keyboard",
            Self::LanguageServers => "Language servers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorShortcut {
    NewTab,
    OpenFile,
    OpenFolder,
    SaveFile,
    CloseTab,
    FindInFile,
    FilterFiles,
    QuickOpen,
    ToggleSidebar,
}
impl crate::preferences::EditorShortcuts {
    fn set(&mut self, action: EditorShortcut, value: String) {
        match action {
            EditorShortcut::NewTab => self.new_tab = value,
            EditorShortcut::OpenFile => self.open_file = value,
            EditorShortcut::OpenFolder => self.open_folder = value,
            EditorShortcut::SaveFile => self.save_file = value,
            EditorShortcut::CloseTab => self.close_tab = value,
            EditorShortcut::FindInFile => self.find_in_file = value,
            EditorShortcut::FilterFiles => self.filter_files = value,
            EditorShortcut::QuickOpen => self.quick_open = value,
            EditorShortcut::ToggleSidebar => self.toggle_sidebar = value,
        }
    }

    fn value(&self, action: EditorShortcut) -> &str {
        match action {
            EditorShortcut::NewTab => &self.new_tab,
            EditorShortcut::OpenFile => &self.open_file,
            EditorShortcut::OpenFolder => &self.open_folder,
            EditorShortcut::SaveFile => &self.save_file,
            EditorShortcut::CloseTab => &self.close_tab,
            EditorShortcut::FindInFile => &self.find_in_file,
            EditorShortcut::FilterFiles => &self.filter_files,
            EditorShortcut::QuickOpen => &self.quick_open,
            EditorShortcut::ToggleSidebar => &self.toggle_sidebar,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let mut seen = Vec::new();
        for value in [
            &self.new_tab,
            &self.open_file,
            &self.open_folder,
            &self.save_file,
            &self.close_tab,
            &self.find_in_file,
            &self.filter_files,
            &self.quick_open,
            &self.toggle_sidebar,
        ] {
            let binding = cagents::settings::parse_shortcut(value)
                .map_err(|error| format!("Invalid shortcut: {error}"))?;
            if seen.contains(&binding) {
                return Err("Shortcuts must not conflict.".into());
            }
            seen.push(binding);
        }
        Ok(())
    }
}
impl Editor {
    fn shortcut_pressed(&self, action: EditorShortcut, event: &keyboard::Event) -> bool {
        cagents::settings::parse_shortcut(self.shortcuts.value(action))
            .is_ok_and(|binding| shortcut_matches(binding, event))
    }

    fn handle_shortcut(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if self.modal_open() {
            return None;
        }
        if self.shortcut_pressed(EditorShortcut::NewTab, event) {
            return Some(self.update(Message::NewTab));
        }
        if self.shortcut_pressed(EditorShortcut::OpenFile, event) {
            return Some(self.update(Message::Choose(false)));
        }
        if self.shortcut_pressed(EditorShortcut::OpenFolder, event) {
            return Some(self.update(Message::Choose(true)));
        }
        if self.shortcut_pressed(EditorShortcut::SaveFile, event) {
            return self.active.map(|card| {
                self.update(Message::Repository(RepositoryAction::Save {
                    card,
                    overwrite: false,
                }))
            });
        }
        if self.shortcut_pressed(EditorShortcut::CloseTab, event) {
            return self.active.map(|card| self.update(Message::CloseTab(card)));
        }
        if self.shortcut_pressed(EditorShortcut::FindInFile, event) {
            return self.active.map(|_| self.update(Message::FindInFile));
        }
        if self.shortcut_pressed(EditorShortcut::FilterFiles, event) {
            return self.active.and_then(|card| {
                self.tab(card)
                    .and_then(|tab| tab.navigation.active_worktree().cloned())
                    .map(|key| {
                        self.update(Message::Sidebar(SidebarAction::ToggleFilesFilter { key }))
                    })
            });
        }
        if self.shortcut_pressed(EditorShortcut::QuickOpen, event) {
            return Some(self.update(Message::QuickOpen));
        }
        if self.shortcut_pressed(EditorShortcut::ToggleSidebar, event) {
            return Some(self.update(Message::ToggleSidebar));
        }
        None
    }
}

fn shortcut_matches(binding: ShortcutBinding, event: &keyboard::Event) -> bool {
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return false;
    };
    binding.control == modifiers.control()
        && binding.alt == modifiers.alt()
        && binding.shift == modifiers.shift()
        && binding.logo == modifiers.logo()
        && match binding.key {
            ShortcutKey::ArrowLeft => {
                matches!(key, keyboard::Key::Named(keyboard::key::Named::ArrowLeft))
            }
            ShortcutKey::ArrowRight => {
                matches!(key, keyboard::Key::Named(keyboard::key::Named::ArrowRight))
            }
            ShortcutKey::Character(expected) => key
                .to_latin(*physical_key)
                .map(|actual| actual.to_ascii_lowercase() == expected)
                .unwrap_or_else(|| {
                    matches!(key, keyboard::Key::Character(value) if value.eq_ignore_ascii_case(&expected.to_string()))
                }),
        }
}
pub struct Editor {
    core: Core,
    tabs: Vec<Tab>,
    active: Option<CardId>,
    selection_epoch: u64,
    settings: AppSettings,
    shortcuts: crate::preferences::EditorShortcuts,
    shortcut_draft: crate::preferences::EditorShortcuts,
    settings_category: SettingsCategory,
    settings_path: PathBuf,
    settings_open: bool,
    settings_writable: bool,
    quick_open_files: Option<Vec<PathBuf>>,
    find_open: bool,
    find_query: String,
    quick_open_generation: u64,
    quick_open: bool,
    quick_open_query: String,
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
    restoring_session: bool,
    saved_session: crate::preferences::Session,
    sidebar_open: bool,
    pending_save_as: Option<(CardId, String)>,
    sidebar_transition: Option<SidebarTransition>,
    sidebar_panel: WorktreePanel,
}

const SIDEBAR_TRANSITION_DURATION: Duration = Duration::from_millis(160);

#[derive(Debug, Clone, Copy)]
struct SidebarTransition {
    started_at: Instant,
    from: f32,
    to: f32,
}

impl SidebarTransition {
    fn duration(self) -> Duration {
        SIDEBAR_TRANSITION_DURATION.mul_f32((self.to - self.from).abs())
    }

    fn progress(self, now: Instant) -> f32 {
        let duration = self.duration();
        if duration.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.started_at);
        let linear = (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0);
        cosmic::anim::slerp(self.from, self.to, linear)
    }

    fn is_complete(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.duration()
    }
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

fn chosen_save_path(
    response: Result<
        cosmic::dialog::file_chooser::save::Response,
        cosmic::dialog::file_chooser::Error,
    >,
) -> Result<Option<PathBuf>, String> {
    match response {
        Ok(response) => response
            .url()
            .ok_or_else(|| "No save location was chosen".to_string())?
            .to_file_path()
            .map(Some)
            .map_err(|()| "Only local files are supported".into()),
        Err(cosmic::dialog::file_chooser::Error::Cancelled) => Ok(None),
        Err(error) if save_chooser_cancelled(&error) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn save_chooser_cancelled(error: &cosmic::dialog::file_chooser::Error) -> bool {
    let text = error.to_string();
    text.contains("cancel") || text.contains("Cancel")
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn untitled_directory(settings_path: &Path) -> Option<PathBuf> {
    let parent = settings_path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join("untitled"))
}

fn untitled_tab_title(file_name: &str) -> String {
    match file_name.strip_prefix("Untitled-") {
        Some(index) if index.chars().all(|character| character.is_ascii_digit()) => {
            format!("Untitled {index}")
        }
        _ if file_name == "Untitled" => "Untitled".into(),
        _ => file_name.to_owned(),
    }
}

fn allocate_untitled_path(directory: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    for index in 1..10_000 {
        let name = if index == 1 {
            "Untitled".to_owned()
        } else {
            format!("Untitled-{index}")
        };
        let path = directory.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                file.sync_all().map_err(|error| error.to_string())?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("Too many untitled notes are already open.".into())
}

fn create_new_file(path: &Path, text: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                "A file already exists at that path.".into()
            } else {
                error.to_string()
            }
        })?;
    if let Err(error) =
        std::io::Write::write_all(&mut file, text.as_bytes()).and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.to_string());
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
    }
    Ok(())
}

fn settings_choice_row(choices: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    widget::row::with_children(choices)
        .spacing(design::GRID_UNIT)
        .align_y(iced::Alignment::Center)
        .into()
}

fn settings_field<'a>(
    title: &'static str,
    detail: &'static str,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    widget::column::with_children(vec![
        widget::text::caption_heading(title).into(),
        design::caption(detail)
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
        control,
    ])
    .spacing(design::GRID_UNIT)
    .width(Length::Fill)
    .into()
}

fn settings_group<'a>(
    title: &'static str,
    detail: &'static str,
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut children = vec![
        widget::text::title4(title).into(),
        design::caption(detail)
            .wrapping(iced::widget::text::Wrapping::Word)
            .width(Length::Fill)
            .into(),
    ];
    children.extend(controls);
    widget::column::with_children(children)
        .spacing(12)
        .width(Length::Fill)
        .into()
}
fn shortcut_setting<'a>(
    title: &'static str,
    detail: &'static str,
    action: EditorShortcut,
    value: &'a str,
) -> Element<'a, Message> {
    settings_field(
        title,
        detail,
        widget::text_input(title, value)
            .on_input(move |value| Message::ShortcutChanged(action, value))
            .into(),
    )
}

fn dialog_surface<'a>(
    content: impl Into<Element<'a, Message>>,
    max_width: f32,
) -> Element<'a, Message> {
    cagents::interaction::opaque(
        widget::container(content)
            .class(cosmic::theme::Container::Dialog(false))
            .padding(16)
            .width(Length::Fill)
            .max_width(max_width)
            .max_height(720),
    )
    .into()
}

fn dialog_overlay(surface: Element<'_, Message>, dismiss: Message) -> Element<'_, Message> {
    let background = widget::container(iced::widget::Space::new())
        .class(cosmic::theme::Container::custom(|_| {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgba(
                    0.0, 0.0, 0.0, 0.48,
                ))),
                ..Default::default()
            }
        }))
        .width(Length::Fill)
        .height(Length::Fill);
    let backdrop = cagents::interaction::opaque(widget::mouse_area(background).on_press(dismiss));
    let centered = widget::container(surface)
        .center(Length::Fill)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill);
    cagents::interaction::layer(backdrop, centered.into())
}

fn find_matches(text: &str, query: &str) -> usize {
    let query = query.trim();
    if query.is_empty() {
        return 0;
    }
    let text = text.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    text.match_indices(&query).count()
}

fn find_bar<'a>(editor: &'a Editor, tab: &'a Tab) -> Element<'a, Message> {
    let matches = find_matches(
        tab.runtime.editor_text().as_deref().unwrap_or_default(),
        &editor.find_query,
    );
    widget::container(
        widget::row::with_children(vec![
            widget::icon(symbolic_icon("system-search-symbolic"))
                .size(16)
                .into(),
            widget::text_input("Find in file", &editor.find_query)
                .id(widget::Id::new("ceditor.find.query"))
                .on_input(Message::FindQueryChanged)
                .on_submit(|_| Message::CloseFind)
                .width(Length::Fill)
                .into(),
            design::caption(if editor.find_query.trim().is_empty() {
                "Type to find".to_owned()
            } else {
                format!("{matches} matches")
            })
            .into(),
            icon_control(
                symbolic_icon("window-close-symbolic"),
                "Close find",
                false,
                true,
                Some(Message::CloseFind),
            )
            .into(),
        ])
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
    .padding([4.0, 8.0])
    .width(Length::Fill)
    .into()
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
            || self.quick_open
            || self.closing.is_some()
            || self.file_dialog.is_some()
            || self.notices_open
    }

    fn settings_footer(&self) -> Element<'_, Message> {
        widget::row::with_children(vec![
            icon_control(
                symbolic_icon("emblem-system-symbolic"),
                "Settings",
                self.settings_open,
                self.closing.is_none() && self.file_dialog.is_none(),
                Some(Message::Settings),
            )
            .into(),
        ])
        .padding(cosmic::theme::spacing().space_xxxs)
        .width(Length::Fill)
        .into()
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
        // Overlay live expansion over the saved map so reopening a folder
        // restores its open tree, including folders whose tabs are closed.
        // Tabs sharing one root union their folders: a fresh tab must not
        // wipe what another tab on the same tree opened.
        let mut live: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for tab in &self.tabs {
            if let Some(key) = tab.navigation.active_worktree() {
                live.entry(key.canonical_path.display().to_string())
                    .or_default()
                    .extend(tab.navigation.expanded_display_paths());
            }
        }
        let mut expanded_directories = self.saved_session.expanded_directories.clone();
        for (root, paths) in live {
            if paths.is_empty() {
                expanded_directories.remove(&root);
            } else {
                expanded_directories.insert(root, paths.into_iter().collect());
            }
        }
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
            sidebar_open: self.sidebar_open,
            expanded_directories,
        }
    }

    fn sidebar_width(&self) -> f32 {
        sidebar::clamp_expanded_width(self.settings.sidebar_width)
    }

    fn sidebar_progress(&self) -> f32 {
        self.sidebar_transition
            .map_or(if self.sidebar_open { 1.0 } else { 0.0 }, |transition| {
                transition.progress(Instant::now())
            })
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
        let untitled = self.is_untitled_root(&opened.root);
        let title = if untitled {
            opened
                .file
                .as_deref()
                .and_then(|path| path.file_name())
                .map(|name| untitled_tab_title(&name.to_string_lossy()))
                .unwrap_or_else(|| "Untitled".into())
        } else {
            opened
                .file
                .as_deref()
                .unwrap_or(&opened.root)
                .file_name()
                .unwrap_or(opened.root.as_os_str())
                .to_string_lossy()
                .into_owned()
        };
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
        runtime.set_syntax_theme(self.settings.editor_syntax_theme);
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
            if let Some(paths) = self
                .saved_session
                .expanded_directories
                .get(&key.canonical_path.display().to_string())
            {
                navigation.restore_expanded_display_paths(paths);
            }
        }
        let task = repository_task(runtime.initial_load(id));
        self.tabs.push(Tab {
            id,
            title,
            runtime,
            sidebar,
            navigation,
            pending_file,
            pending_diff: None,
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

    fn untitled_root(&self) -> Option<PathBuf> {
        untitled_directory(&self.settings_path)
    }

    fn is_untitled_root(&self, root: &Path) -> bool {
        self.untitled_root()
            .is_some_and(|directory| paths_equal(root, &directory))
    }

    fn tab_is_untitled(&self, tab: &Tab) -> bool {
        self.is_untitled_root(&tab.runtime.descriptor.worktree)
    }

    fn untitled_scratch_path(&self, id: CardId) -> Option<PathBuf> {
        let tab = self.tab(id).filter(|tab| self.tab_is_untitled(tab))?;
        tab.owned_file.clone().or_else(|| {
            tab.pending_file.as_ref().map(|pending| {
                pending
                    .canonical
                    .clone()
                    .unwrap_or_else(|| tab.runtime.descriptor.worktree.join(&pending.path))
            })
        })
    }

    fn untitled_text(&self, id: CardId) -> String {
        if let Some(text) = self.tab(id).and_then(|tab| tab.runtime.editor_text()) {
            return text;
        }
        self.untitled_scratch_path(id)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    }

    fn untitled_has_content(&self, id: CardId) -> bool {
        !self.untitled_text(id).trim().is_empty()
    }

    fn new_tab(&mut self) -> Task<Message> {
        if self.modal_open() {
            return Task::none();
        }
        let Some(directory) = self.untitled_root() else {
            self.notice = Some("ceditor configuration directory is unavailable".into());
            return Task::none();
        };
        let path = match allocate_untitled_path(&directory) {
            Ok(path) => path,
            Err(error) => {
                self.notice = Some(error);
                return Task::none();
            }
        };
        let file = path
            .file_name()
            .map(PathBuf::from)
            .expect("allocated untitled path has a file name");
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.add(
            Opened {
                root: directory,
                file: Some(file),
                snapshot: None,
            },
            true,
        )
    }

    fn editor_clipboard(&mut self, id: CardId, op: ClipboardOp) -> Task<Message> {
        match op {
            ClipboardOp::Copy => self
                .tab(id)
                .and_then(|tab| tab.runtime.selected_text())
                .map_or_else(Task::none, |text| {
                    iced::clipboard::write(text).map(cosmic::Action::App)
                }),
            ClipboardOp::Cut => {
                let Some(text) = self.tab(id).and_then(|tab| tab.runtime.selected_text()) else {
                    return Task::none();
                };
                Task::batch([
                    iced::clipboard::write(text).map(cosmic::Action::App),
                    self.update(Message::Repository(RepositoryAction::EditorAction {
                        card: id,
                        action: EditorAction::Edit(EditorEdit::Delete),
                    })),
                ])
            }
            ClipboardOp::Paste => iced::clipboard::read().map(move |text| {
                cosmic::Action::App(match text {
                    Some(text) => Message::Repository(RepositoryAction::EditorAction {
                        card: id,
                        action: EditorAction::Edit(EditorEdit::Paste(std::sync::Arc::new(text))),
                    }),
                    None => Message::RefreshVisible,
                })
            }),
            ClipboardOp::SelectAll => {
                self.update(Message::Repository(RepositoryAction::EditorAction {
                    card: id,
                    action: EditorAction::SelectAll,
                }))
            }
        }
    }

    fn save_as(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab(id).filter(|tab| self.tab_is_untitled(tab)) else {
            return Task::none();
        };
        let file_name = tab
            .pending_file
            .as_ref()
            .map(|pending| pending.path.as_path())
            .or(tab.owned_file.as_deref())
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into());
        self.pending_save_as = Some((id, self.untitled_text(id)));
        cosmic::task::future(async move {
            let dialog = cosmic::dialog::file_chooser::save::Dialog::new()
                .title("Save note".to_owned())
                .file_name(file_name);
            Message::SavedAs(id, chosen_save_path(dialog.save_file().await))
        })
    }

    fn finish_save_as(
        &mut self,
        id: CardId,
        result: Result<Option<PathBuf>, String>,
    ) -> Task<Message> {
        let Some((pending_id, text)) = self.pending_save_as.take() else {
            return Task::none();
        };
        if pending_id != id {
            return Task::none();
        }
        let path = match result {
            Ok(Some(path)) => path,
            Ok(None) => return Task::none(),
            Err(error) => {
                self.notice = Some(error);
                return Task::none();
            }
        };
        if self.untitled_root().is_some_and(|directory| {
            path.parent()
                .is_some_and(|parent| paths_equal(parent, &directory))
        }) {
            self.notice = Some("Choose a location outside Untitled notes.".into());
            return Task::none();
        }
        if let Err(error) = create_new_file(&path, &text) {
            self.notice = Some(error);
            return Task::none();
        }
        if self.tab(id).is_some_and(|tab| self.tab_is_untitled(tab)) {
            self.remove_tab(id);
        }
        if self.exiting {
            return self.continue_exit();
        }
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        Self::open(path, self.selection_epoch)
    }

    fn remove_tab(&mut self, id: CardId) {
        let scratch = (!self.exiting)
            .then(|| self.untitled_scratch_path(id))
            .flatten();
        self.close_document(id);
        self.tabs.retain(|tab| tab.id != id);
        if self.active == Some(id) {
            self.active = self.tabs.last().map(|tab| tab.id);
        }
        if self.closing == Some(id) {
            self.closing = None;
            self.close_confirmed = false;
        }
        if let Some(path) = scratch {
            let _ = std::fs::remove_file(path);
        }
    }

    fn request_file(&mut self, id: CardId, path: PathBuf) -> Task<Message> {
        self.request_file_for_surface(id, path, None)
    }

    fn request_file_for_surface(
        &mut self,
        id: CardId,
        path: PathBuf,
        diff_staged: Option<bool>,
    ) -> Task<Message> {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        tab.pending_file = None;
        if RepoPath::parse(&path).is_err() {
            tab.pending_diff = None;
            return Task::none();
        }
        if tab.runtime.blocks_navigation() {
            tab.pending_diff = None;
            self.notice =
                Some("Save or discard the current file before opening another file.".into());
            return Task::none();
        }
        tab.pending_diff = diff_staged;
        let request = tab.next_open;
        tab.next_open = tab.next_open.wrapping_add(1);
        let absolute = tab.runtime.descriptor.worktree.join(&path);
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
        let untitled_dir = untitled_directory(&self.settings_path);
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
            let file_name = pending
                .path
                .file_name()
                .unwrap_or(pending.path.as_os_str())
                .to_string_lossy()
                .into_owned();
            let untitled = untitled_dir
                .as_ref()
                .is_some_and(|directory| paths_equal(&tab.runtime.descriptor.worktree, directory));
            tab.title = if untitled {
                untitled_tab_title(&file_name)
            } else {
                file_name
            };
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
        let untitled_note = self.tab(id).is_some_and(|tab| self.tab_is_untitled(tab))
            && self.untitled_has_content(id)
            && !self.exiting;
        if self
            .tab(id)
            .is_some_and(|tab| tab.runtime.blocks_navigation() || tab.mutation)
            || (untitled_note && !self.close_confirmed)
        {
            self.active = Some(id);
            self.closing = Some(id);
            self.close_confirmed = false;
            let tab = self.tab(id).unwrap();
            let focus = if untitled_note || tab.runtime.can_save() {
                "ceditor.close.save"
            } else if tab.runtime.can_abandon_unsaved_changes() && !tab.mutation {
                "ceditor.close.discard"
            } else {
                "ceditor.close.cancel"
            };
            return iced::widget::operation::focus(widget::Id::new(focus)).map(cosmic::Action::App);
        }
        self.remove_tab(id);
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
            symbolic_icon(if has_notices {
                "dialog-warning-symbolic"
            } else {
                "dialog-information-symbolic"
            }),
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
                        symbolic_icon("window-close-symbolic"),
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

    fn quick_open_search(&self, query: String, generation: u64) -> Task<Message> {
        let Some(id) = self.active else {
            return Task::none();
        };
        let Some(tab) = self.tab(id) else {
            return Task::none();
        };
        if !tab.runtime.is_directory_only() || query.trim().is_empty() {
            return Task::none();
        }
        let root = tab.runtime.descriptor.worktree.clone();
        let search_query = query.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                repository_widget::search_directory_files_for_quick_open(&root, &search_query).map(
                    |files| {
                        files
                            .into_iter()
                            .filter(|file| !file.directory)
                            .map(|file| file.path)
                            .collect()
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            Message::QuickOpenSearchFinished {
                card: id,
                generation,
                query,
                result,
            }
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
    fn stage_selection(
        &mut self,
        id: CardId,
        stage: bool,
        selection: Vec<PathSelection>,
    ) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        if tab.mutation || tab.runtime.blocks_navigation() || selection.is_empty() {
            return Task::none();
        }
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
                            selection,
                        },
                        &token,
                    )
                } else {
                    service.unstage(
                        &UnstageRequest {
                            worktree: root,
                            selection,
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
        let shortcuts =
            crate::preferences::load_shortcuts(&flags.settings_path).unwrap_or_else(|error| {
                notice = Some(match notice.take() {
                    Some(previous) => format!("{previous}\n{error}"),
                    None => error,
                });
                crate::preferences::EditorShortcuts::default()
            });
        let shortcut_draft = shortcuts.clone();
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
        let sidebar_open = saved_session.sidebar_open;
        let mut app = Self {
            core,
            tabs: Vec::new(),
            active: None,
            selection_epoch: 0,
            settings,
            shortcuts,
            shortcut_draft,
            settings_category: SettingsCategory::Editing,
            settings_path: flags.settings_path,
            settings_open: false,
            settings_writable,
            quick_open_files: None,
            quick_open_generation: 0,
            find_open: false,
            find_query: String::new(),
            quick_open: false,
            quick_open_query: String::new(),
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
            restoring_session: !startup_paths.is_empty(),
            saved_session,
            sidebar_open,
            pending_save_as: None,
            sidebar_transition: None,
            sidebar_panel: WorktreePanel::Files,
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
                | Message::NewTab
                | Message::SavedAs(..)
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
                if self.quick_open {
                    return self.update(Message::CloseQuickOpen);
                }
                if self.find_open {
                    return self.update(Message::CloseFind);
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
                        | Message::ToggleSidebar
                        | Message::NewTab
                        | Message::Key(_)
                        | Message::SelectSettingsCategory(_)
                        | Message::ShortcutChanged(_, _)
                        | Message::QuickOpenQueryChanged(_)
                        | Message::QuickOpenSearchFinished { .. }
                        | Message::OpenQuickFile(_)
                        | Message::CloseQuickOpen
                        | Message::AutoSave(_)
                        | Message::WrapLines(_)
                        | Message::SyntaxTheme(_)
                        | Message::LanguageServerAssistance(_)
                        | Message::Theme(_)
                        | Message::Scale(_)
                        | Message::SettingsSaved(_)
                        | Message::FindQueryChanged(_)
                        | Message::CloseFind
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
                    let Some(card) = self.closing.filter(|id| {
                        self.tab(*id)
                            .is_some_and(|tab| tab.runtime.can_save() || self.tab_is_untitled(tab))
                    }) else {
                        return Task::none();
                    };
                    self.close_confirmed = true;
                    if self.tab(card).is_some_and(|tab| self.tab_is_untitled(tab)) {
                        return self.save_as(card);
                    }
                    self.update(Message::Repository(RepositoryAction::Save {
                        card,
                        overwrite: false,
                    }))
                }
                Message::DiscardClose => {
                    if let Some(id) = self.closing {
                        let untitled = self.tab(id).is_some_and(|tab| self.tab_is_untitled(tab));
                        if untitled {
                            if let Some(tab) = self.tab_mut(id) {
                                if !tab.mutation {
                                    let _ = tab.runtime.abandon_unsaved_changes();
                                }
                            }
                            self.close_confirmed = true;
                            return self.request_close(id);
                        }
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
                                | RepositoryAction::DirectoryListed { .. }
                                | RepositoryAction::DirectorySearchLoaded { .. }
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
                    if let RepositoryAction::Save { card, .. } = &action {
                        if self.tab(*card).is_some_and(|tab| self.tab_is_untitled(tab)) {
                            return self.save_as(*card);
                        }
                    }
                    if let RepositoryAction::DirectorySearchLoaded {
                        generation,
                        query,
                        result,
                        ..
                    } = action
                    {
                        let Some(tab) = self.tab_mut(id) else {
                            return Task::none();
                        };
                        if generation != tab.runtime.file_search_generation() {
                            return Task::none();
                        }
                        if let Some(source) = tab.sidebar.source_control.as_mut() {
                            source.set_file_search(match result {
                                Ok((files, hidden)) => {
                                    SourceControlModel::file_search_results(query, &files, hidden)
                                }
                                Err(message) => FileSearch::Failed { query, message },
                            });
                        }
                        return Task::none();
                    }
                    if let RepositoryAction::Clipboard { card, op } = action {
                        return self.editor_clipboard(card, op);
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
                    let listing_changed = matches!(
                        &action,
                        RepositoryAction::FilesLoaded { result: Ok(_), .. }
                            | RepositoryAction::DirectoryListed { result: Ok(_), .. }
                    );
                    let open_diff = matches!(action, RepositoryAction::FileLoaded { .. })
                        .then(|| tab.pending_diff.take())
                        .flatten();
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
                    if let Some(staged) = open_diff {
                        let surface = tab.runtime.update(RepositoryAction::ApplySourceSurface {
                            card: id,
                            diff: true,
                        });
                        let basis = staged.then(|| {
                            tab.runtime.update(RepositoryAction::ApplyDiffBasis {
                                card: id,
                                basis: DiffBasis::HeadToIndex,
                            })
                        });
                        tasks[0] = tasks.remove(0).chain(repository_task(surface));
                        if let Some(basis) = basis {
                            tasks[0] = tasks.remove(0).chain(repository_task(basis));
                        }
                    }
                    if listing_changed && !tab.git {
                        if let Some(key) = tab.navigation.active_worktree().cloned() {
                            let search = tab
                                .sidebar
                                .source_control
                                .as_ref()
                                .map(|source| source.file_search.clone())
                                .unwrap_or_default();
                            let mut model = SourceControlModel::directory_listing(
                                key,
                                tab.runtime.listed_files(),
                            );
                            model.set_file_search(search);
                            tab.sidebar.source_control = Some(model);
                        }
                    }
                    if back_to_files && tab.runtime.selected_path().is_none() {
                        tab.owned_file = None;
                        tab.pending_file = None;
                    }
                    if loaded {
                        tab.files_loaded = true;
                        if !tab.git {
                            let expanded: Vec<_> = tab
                                .navigation
                                .expanded_directory_paths()
                                .map(|path| path.as_path().to_path_buf())
                                .collect();
                            for path in expanded {
                                tasks.push(repository_task(tab.runtime.list_directory(id, path)));
                            }
                        }
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
                    if matches!(action, SidebarAction::OpenSettings) {
                        return self.update(Message::Settings);
                    }
                    if self.modal_open() {
                        return Task::none();
                    }
                    if let Some(id) = self.active {
                        match &action {
                            SidebarAction::OpenChange { key, path, area }
                                if self.tab(id).is_some_and(|tab| {
                                    tab.navigation.active_worktree() == Some(key)
                                }) =>
                            {
                                if let Some(tab) = self.tab_mut(id) {
                                    tab.navigation.apply(&action);
                                }
                                return self.request_file_for_surface(
                                    id,
                                    path.as_path().to_path_buf(),
                                    Some(*area == cagents::repository::ChangeArea::Staged),
                                );
                            }
                            SidebarAction::Stage { key, selection }
                                if self.tab(id).is_some_and(|tab| {
                                    tab.navigation.active_worktree() == Some(key)
                                }) =>
                            {
                                return self.stage_selection(id, true, selection.clone());
                            }
                            SidebarAction::Unstage { key, selection }
                                if self.tab(id).is_some_and(|tab| {
                                    tab.navigation.active_worktree() == Some(key)
                                }) =>
                            {
                                return self.stage_selection(id, false, selection.clone());
                            }
                            _ => {}
                        }
                    }
                    if let SidebarAction::Resize { width } = &action {
                        // The panel's splitter clamps to its rendered bounds;
                        // clamp again to the persisted settings range.
                        self.settings.sidebar_width =
                            width.clamp(sidebar::EXPANDED_WIDTH_MIN, sidebar::EXPANDED_WIDTH_MAX);
                        return Task::none();
                    }
                    if matches!(&action, SidebarAction::ResizeFinished) {
                        return self.save_settings();
                    }
                    if let SidebarAction::SetWorktreePanel { panel } = action {
                        self.sidebar_panel = panel;
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
                        | SidebarAction::ToggleFilesFilter { key }
                        | SidebarAction::ToggleDirectory { key, .. }
                        | SidebarAction::ToggleSection { key, .. } => Some(key),
                        _ => None,
                    };
                    if key.is_some_and(|key| tab.navigation.active_worktree() != Some(key)) {
                        return Task::none();
                    }
                    tab.navigation.apply(&action);
                    let persist_expansion = matches!(action, SidebarAction::ToggleDirectory { .. });
                    let task = match action {
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
                        SidebarAction::ToggleDirectory { path, .. }
                            if tab.navigation.directory_is_expanded(&path) =>
                        {
                            repository_task(
                                tab.runtime.list_directory(id, path.as_path().to_path_buf()),
                            )
                        }
                        SidebarAction::FilesQueryChanged { query, .. } => {
                            if tab.git {
                                return Task::none();
                            }
                            if let Some(source) = tab.sidebar.source_control.as_mut() {
                                source.set_file_search(if query.trim().is_empty() {
                                    FileSearch::Inactive
                                } else {
                                    FileSearch::Pending {
                                        query: query.clone(),
                                    }
                                });
                            }
                            if query.trim().is_empty() {
                                tab.runtime.cancel_file_search();
                                Task::none()
                            } else {
                                repository_task(tab.runtime.search_directory(id, query))
                            }
                        }
                        SidebarAction::ToggleFilesFilter { key } => {
                            let open = tab.navigation.files_filter_open();
                            let query = tab.navigation.files_query().to_owned();
                            if tab.git {
                                return if open {
                                    widget::text_input::focus(sidebar::files_filter_focus_id(&key))
                                        .map(cosmic::Action::App)
                                } else {
                                    Task::none()
                                };
                            }
                            if open {
                                if !query.trim().is_empty() {
                                    if let Some(source) = tab.sidebar.source_control.as_mut() {
                                        source.set_file_search(FileSearch::Pending {
                                            query: query.clone(),
                                        });
                                    }
                                }
                                let search = if query.trim().is_empty() {
                                    Task::none()
                                } else {
                                    repository_task(tab.runtime.search_directory(id, query))
                                };
                                Task::batch([
                                    widget::text_input::focus(sidebar::files_filter_focus_id(&key))
                                        .map(cosmic::Action::App),
                                    search,
                                ])
                            } else {
                                if let Some(source) = tab.sidebar.source_control.as_mut() {
                                    source.set_file_search(FileSearch::Inactive);
                                }
                                tab.runtime.cancel_file_search();
                                Task::none()
                            }
                        }
                        _ => Task::none(),
                    };
                    // Folder expansion persists across sessions; the save is a
                    // no-op unless the open tree actually changed.
                    if persist_expansion {
                        self.save_session();
                    }
                    task
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
                Message::SelectSettingsCategory(category) => {
                    self.settings_category = category;
                    Task::none()
                }
                Message::ShortcutChanged(action, value) => {
                    self.shortcut_draft.set(action, value);
                    match self.shortcut_draft.validate() {
                        Ok(()) => {
                            self.shortcuts = self.shortcut_draft.clone();
                            if self.settings_writable {
                                if let Err(error) = crate::preferences::save_shortcuts(
                                    &self.settings_path,
                                    &self.shortcuts,
                                ) {
                                    self.notice = Some(error);
                                }
                            }
                        }
                        Err(error) => self.notice = Some(error),
                    }
                    Task::none()
                }
                Message::FindInFile => {
                    self.find_open = !self.find_open;
                    if self.find_open {
                        return iced::widget::operation::focus(widget::Id::new(
                            "ceditor.find.query",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Task::none()
                }
                Message::FindQueryChanged(query) => {
                    self.find_query = query.clone();
                    if let Some(id) = self.active {
                        if let Some(tab) = self.tab_mut(id) {
                            tab.runtime.select_text(&query);
                        }
                    }
                    Task::none()
                }
                Message::CloseFind => {
                    self.find_open = false;
                    Task::none()
                }
                Message::QuickOpen => {
                    self.quick_open = !self.quick_open;
                    self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
                    self.quick_open_files = None;
                    if self.quick_open {
                        self.quick_open_query.clear();
                        return iced::widget::operation::focus(widget::Id::new(
                            "ceditor.quick-open.query",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Task::none()
                }
                Message::QuickOpenQueryChanged(query) => {
                    self.quick_open_query = query.clone();
                    self.quick_open_files = None;
                    self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
                    self.quick_open_search(query, self.quick_open_generation)
                }
                Message::QuickOpenSearchFinished {
                    card,
                    generation,
                    query,
                    result,
                } => {
                    if self.quick_open
                        && self.active == Some(card)
                        && self.quick_open_generation == generation
                        && self.quick_open_query == query
                    {
                        match result {
                            Ok(files) => self.quick_open_files = Some(files),
                            Err(error) => self.notice = Some(error),
                        }
                    }
                    Task::none()
                }
                Message::OpenQuickFile(path) => {
                    let Some(id) = self.active else {
                        return Task::none();
                    };
                    self.quick_open = false;
                    self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
                    self.quick_open_query.clear();
                    self.quick_open_files = None;
                    self.request_file(id, path)
                }
                Message::CloseQuickOpen => {
                    self.quick_open = false;
                    self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
                    self.quick_open_files = None;
                    Task::none()
                }
                Message::ToggleSidebar => {
                    let from = self.sidebar_progress();
                    self.sidebar_open = !self.sidebar_open;
                    self.sidebar_transition = Some(SidebarTransition {
                        started_at: Instant::now(),
                        from,
                        to: if self.sidebar_open { 1.0 } else { 0.0 },
                    });
                    self.save_session();
                    Task::none()
                }
                Message::Frame(now) => {
                    if self
                        .sidebar_transition
                        .is_some_and(|transition| transition.is_complete(now))
                    {
                        self.sidebar_transition = None;
                    }
                    Task::none()
                }
                Message::NewTab => self.new_tab(),
                Message::SavedAs(id, result) => self.finish_save_as(id, result),
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
                Message::SyntaxTheme(theme) => {
                    self.settings.editor_syntax_theme = theme;
                    for tab in &mut self.tabs {
                        tab.runtime.set_syntax_theme(theme);
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
                Message::Key(event) => {
                    if let Some(task) = self.handle_shortcut(&event) {
                        return task;
                    }
                    if let keyboard::Event::KeyPressed {
                        key,
                        modifiers,
                        repeat: false,
                        ..
                    } = &event
                    {
                        if !self.modal_open()
                            && modifiers.control()
                            && matches!(key, keyboard::Key::Character(value) if value == " ")
                        {
                            return self.update(Message::Complete);
                        }
                    }
                    Task::none()
                }
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
        let frames = if self.sidebar_transition.is_some() {
            window::frames().map(|(_, now)| Message::Frame(now))
        } else {
            Subscription::none()
        };
        Subscription::batch([events, refresh, frames])
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        vec![
            icon_control(
                symbolic_icon("view-sidebar-start-symbolic"),
                if self.sidebar_open {
                    "Hide sidebar"
                } else {
                    "Show sidebar"
                },
                self.sidebar_open,
                !self.modal_open(),
                Some(Message::ToggleSidebar),
            )
            .into(),
            icon_control(
                symbolic_icon("document-open-symbolic"),
                "Open file (Ctrl+O)",
                false,
                !self.modal_open(),
                Some(Message::Choose(false)),
            )
            .into(),
            icon_control(
                symbolic_icon("folder-open-symbolic"),
                "Open folder (Ctrl+Shift+O)",
                false,
                !self.modal_open(),
                Some(Message::Choose(true)),
            )
            .into(),
        ]
    }
    fn header_end(&self) -> Vec<Element<'_, Message>> {
        self.notices_control().into_iter().collect()
    }
    fn view(&self) -> Element<'_, Message> {
        let tabs: Vec<Element<'_, Message>> =
            self.tabs
                .iter()
                .map(|tab| {
                    let selected = self.active == Some(tab.id);
                    let title = format!(
                        "{}{}",
                        if tab.runtime.is_dirty() { "• " } else { "" },
                        tab.title
                    );
                    let icon = tab
                        .pending_file
                        .as_ref()
                        .map(|pending| pending.canonical.as_ref().unwrap_or(&pending.path))
                        .or(tab.owned_file.as_ref())
                        .map_or_else(
                            || cagents::file_icon::folder(true),
                            |path| cagents::file_icon::icon(path),
                        );
                    cagents::interaction::hover(
                        widget::container(
                            widget::row::with_children(vec![
                                cagents::interaction::tooltip(
                                    widget::button::custom(
                                        widget::row::with_children(vec![
                                    widget::container(icon).center_y(Length::Fill).into(),
                                    design::body(title.clone())
                                        .wrapping(iced::widget::text::Wrapping::None)
                                        .ellipsize(iced::widget::text::Ellipsize::End(
                                            iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                                        ))
                                        .width(Length::Fill)
                                        .into(),
                                ])
                                        .spacing(6)
                                        .align_y(iced::Alignment::Center)
                                        .height(Length::Fill)
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
                                    symbolic_icon("window-close-symbolic"),
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
                        })),
                    )
                })
                .collect();
        let mut body = Vec::new();
        // The COSMIC header above has no separator of its own: always rule
        // off the content below it, tabs or placeholder alike.
        body.push(widget::divider::horizontal::default().into());
        // Keep the tab strip on the same fixed baseline as the app header.
        let mut strip = Vec::new();
        let mut tab_row = tabs;
        tab_row.push(
            icon_control(
                symbolic_icon("list-add-symbolic"),
                "New tab (Ctrl+T)",
                false,
                !self.modal_open(),
                Some(Message::NewTab),
            )
            .id(widget::Id::new("ceditor.tabs.new"))
            .into(),
        );
        strip.push(
            widget::container(cagents::interaction::overflow_fade_x(
                cagents::interaction::horizontal_wheel(
                    widget::scrollable::horizontal(
                        widget::row::with_children(tab_row)
                            .spacing(4)
                            .align_y(iced::Alignment::Center)
                            .height(design::PRIMARY_CONTROL_SIZE),
                    )
                    .id(widget::Id::new("ceditor.tabs"))
                    .direction(iced::widget::scrollable::Direction::Horizontal(
                        iced::widget::scrollable::Scrollbar::hidden(),
                    ))
                    .width(Length::Fill)
                    .height(design::PRIMARY_CONTROL_SIZE),
                ),
            ))
            .padding([0.0, 8.0])
            .height(design::HEADER_HEIGHT)
            .width(Length::Fill)
            .into(),
        );
        strip.push(widget::divider::horizontal::default().into());
        if let Some(tab) = self.current() {
            let mut editor = Vec::new();
            editor.extend(strip);
            let mut toolbar_controls = Vec::new();
            if let Some(toolbar) = repository_widget::selected_file_toolbar(tab.id, &tab.runtime) {
                toolbar_controls.push(toolbar.map(Message::Repository));
            }
            if tab.runtime.lsp_document().is_some() {
                toolbar_controls.push(
                    widget::container(segmented_controls(vec![
                        icon_control(
                            symbolic_icon("insert-text-symbolic"),
                            "Complete (Ctrl+Space)",
                            false,
                            !self.modal_open(),
                            Some(Message::Complete),
                        )
                        .into(),
                        icon_control(
                            symbolic_icon("dialog-information-symbolic"),
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
            toolbar_controls.push(
                icon_control(
                    symbolic_icon("system-search-symbolic"),
                    "Find in file (Ctrl+F)",
                    self.find_open,
                    !self.modal_open(),
                    Some(Message::FindInFile),
                )
                .into(),
            );
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
            if self.find_open {
                editor.push(find_bar(self, tab));
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
            let mut workspace = Vec::new();
            let progress = self.sidebar_progress();
            if progress > 0.0 {
                let width = self.sidebar_width();
                workspace.push(cagents::interaction::reveal_leading_width(
                    sidebar::worktree_sidebar(
                        &tab.sidebar,
                        &tab.navigation,
                        width,
                        self.modal_open(),
                        if tab.git {
                            self.sidebar_panel
                        } else {
                            WorktreePanel::Files
                        },
                    )
                    .map(Message::Sidebar),
                    width,
                    progress,
                ));
                // The files panel carries no trailing rule of its own: the
                // splitter draws the single sidebar border and the drag
                // handle. A second divider here would double the border.
                workspace.push(
                    Splitter::new(
                        iced::id::Id::new("ceditor.sidebar-splitter"),
                        "Resize sidebar",
                        width,
                        sidebar::EXPANDED_WIDTH_MIN,
                        sidebar::EXPANDED_WIDTH_MAX,
                        splitter::Edge::Trailing,
                        self.modal_open(),
                        |width| Message::Sidebar(SidebarAction::Resize { width }),
                        || Message::Sidebar(SidebarAction::ResizeFinished),
                    )
                    .into(),
                );
            }
            workspace.push(
                widget::column::with_children(editor)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into(),
            );
            body.push(cagents::interaction::drive_animation(
                widget::row::with_children(workspace).height(Length::Fill),
                self.sidebar_transition.is_some(),
            ));
        } else {
            body.extend(strip);
            body.push(
                widget::column::with_children(vec![
                    widget::container(
                        widget::column::with_children(vec![
                            widget::text::title1("Open a file or folder").into(),
                            widget::text::body("Create a new tab to paste a note, or open an existing file or folder. Native editing, safe autosave, Git diffs, and language servers — without a daemon.").into(),
                            widget::button::standard("Open folder")
                                .on_press(Message::Choose(true))
                                .into(),
                        ])
                        .spacing(16),
                    )
                    .padding(32)
                    .height(Length::Fill)
                    .into(),
                    widget::divider::horizontal::default().into(),
                    self.settings_footer(),
                ])
                .height(Length::Fill)
                .into(),
            );
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
            return Some(dialog_overlay(
                dialog_surface(widget::column::with_children(content).spacing(12), 480.0),
                Message::CancelFileOperation,
            ));
        }
        if let Some(id) = self.closing {
            let tab = self.tab(id)?;
            let untitled_note = self.tab_is_untitled(tab) && self.untitled_has_content(id);
            let ready = !tab.runtime.blocks_navigation() && !tab.mutation && !untitled_note;
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
                if (tab.runtime.can_abandon_unsaved_changes() || untitled_note) && !tab.mutation {
                    controls.push(
                        widget::button::destructive("Discard and close")
                            .id(widget::Id::new("ceditor.close.discard"))
                            .on_press(Message::DiscardClose)
                            .into(),
                    );
                }
                if tab.runtime.can_save() || untitled_note {
                    controls.push(
                        widget::button::standard(if untitled_note {
                            "Save as"
                        } else {
                            "Save and close"
                        })
                        .id(widget::Id::new("ceditor.close.save"))
                        .on_press(Message::SaveClose)
                        .into(),
                    );
                }
            }
            return Some(dialog_overlay(
                dialog_surface(
                    widget::column::with_children(vec![
                        widget::text::title3(if untitled_note {
                            "Save this note?"
                        } else if ready {
                            "Close tab?"
                        } else {
                            "Finish saving before closing"
                        })
                        .into(),
                        widget::text::body(tab.runtime.save_error().unwrap_or(if tab.mutation {
                            "A Git operation is still running."
                        } else if untitled_note {
                            "This note has not been saved to a file yet."
                        } else if ready {
                            "The tab is ready to close."
                        } else {
                            "This file has unsaved changes or an unfinished save."
                        }))
                        .into(),
                        widget::row::with_children(controls).spacing(8).into(),
                    ])
                    .spacing(16),
                    520.0,
                ),
                Message::CancelClose,
            ));
        }
        if self.quick_open {
            let query = self.quick_open_query.trim().to_ascii_lowercase();
            let files = self
                .current()
                .map(|tab| {
                    let candidates = self.quick_open_files.clone().unwrap_or_else(|| {
                        tab.runtime
                            .listed_files()
                            .iter()
                            .filter(|file| !file.directory)
                            .map(|file| file.path.as_path().to_path_buf())
                            .collect()
                    });
                    candidates
                        .into_iter()
                        .filter(|path| {
                            query.is_empty()
                                || path.to_string_lossy().to_ascii_lowercase().contains(&query)
                        })
                        .take(40)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut rows: Vec<Element<'_, Message>> = files
                .into_iter()
                .map(|path| {
                    let label = path.to_string_lossy().into_owned();
                    widget::button::text(label)
                        .on_press(Message::OpenQuickFile(path))
                        .width(Length::Fill)
                        .into()
                })
                .collect();
            if rows.is_empty() {
                rows.push(design::caption("No matching files").into());
            }
            return Some(dialog_overlay(
                dialog_surface(
                    widget::column::with_children(vec![
                        widget::text::title3("Quick open").into(),
                        widget::text_input("Search files", &self.quick_open_query)
                            .id(widget::Id::new("ceditor.quick-open.query"))
                            .on_input(Message::QuickOpenQueryChanged)
                            .into(),
                        widget::scrollable(widget::column::with_children(rows).spacing(2))
                            .direction(iced::widget::scrollable::Direction::Vertical(
                                cagents::design::scrollbar(),
                            ))
                            .height(360)
                            .into(),
                    ])
                    .spacing(12),
                    620.0,
                ),
                Message::CloseQuickOpen,
            ));
        }
        if !self.settings_open {
            return None;
        }
        let theme_choices = ["System", "Light", "Dark"]
            .into_iter()
            .zip([AppTheme::System, AppTheme::Light, AppTheme::Dark])
            .map(|(label, theme)| {
                let selected = self.settings.app_theme == theme;
                cagents::interaction::hover(
                    widget::button::custom(design::body(label))
                        .class(if selected {
                            cosmic::theme::Button::Suggested
                        } else {
                            cosmic::theme::Button::Standard
                        })
                        .selected(selected)
                        .on_press(Message::Theme(theme)),
                )
            })
            .collect();
        let syntax_choices = settings::EditorSyntaxTheme::ALL
            .iter()
            .copied()
            .map(|theme| {
                let selected = self.settings.editor_syntax_theme == theme;
                cagents::interaction::hover(
                    widget::button::custom(design::body(theme.label()))
                        .class(if selected {
                            cosmic::theme::Button::Suggested
                        } else {
                            cosmic::theme::Button::Standard
                        })
                        .selected(selected)
                        .on_press(Message::SyntaxTheme(theme)),
                )
            })
            .collect();
        let controls = match self.settings_category {
            SettingsCategory::Editing => vec![settings_group(
                "Editing",
                "Saving and how the buffer wraps.",
                vec![
                    widget::toggler(self.settings.editor_auto_save)
                        .label("Save files automatically".to_owned())
                        .on_toggle(Message::AutoSave)
                        .into(),
                    design::caption("Saves after a pause. Conflicts still need a decision.")
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                    widget::toggler(self.settings.editor_wrap_lines)
                        .label("Wrap long lines".to_owned())
                        .on_toggle(Message::WrapLines)
                        .into(),
                ],
            )],
            SettingsCategory::Appearance => vec![settings_group(
                "Appearance",
                "Window colors, syntax colors, and interface scale.",
                vec![
                    settings_field(
                        "Color scheme",
                        "Follow the desktop, or lock light or dark.",
                        settings_choice_row(theme_choices),
                    ),
                    settings_field(
                        "Syntax highlighting",
                        "Match app follows the color scheme. The others keep a fixed highlighter palette.",
                        settings_choice_row(syntax_choices),
                    ),
                    settings_field(
                        "Interface scale",
                        "Size of text and controls.",
                        widget::row::with_children(vec![
                            icon_control(
                                symbolic_icon("list-remove-symbolic"),
                                "Decrease scale",
                                false,
                                self.settings.ui_scale > 0.75,
                                (self.settings.ui_scale > 0.75).then_some(Message::Scale(
                                    (self.settings.ui_scale - 0.1).max(0.75),
                                )),
                            )
                            .into(),
                            widget::container(design::body(format!(
                                "{:.0}%",
                                self.settings.ui_scale * 100.0
                            )))
                            .center_x(64)
                            .center_y(design::PRIMARY_CONTROL_SIZE)
                            .into(),
                            icon_control(
                                symbolic_icon("list-add-symbolic"),
                                "Increase scale",
                                false,
                                self.settings.ui_scale < 2.0,
                                (self.settings.ui_scale < 2.0).then_some(Message::Scale(
                                    (self.settings.ui_scale + 0.1).min(2.0),
                                )),
                            )
                            .into(),
                        ])
                        .spacing(design::GRID_UNIT)
                        .align_y(iced::Alignment::Center)
                        .into(),
                    ),
                ],
            )],
            SettingsCategory::Keyboard => vec![settings_group(
                "Keyboard shortcuts",
                "Use Ctrl, Alt, or Super plus one key. Changes save immediately when valid.",
                vec![
                    shortcut_setting(
                        "New tab",
                        "Create an untitled editor tab.",
                        EditorShortcut::NewTab,
                        &self.shortcut_draft.new_tab,
                    ),
                    shortcut_setting(
                        "Open file",
                        "Open a file picker.",
                        EditorShortcut::OpenFile,
                        &self.shortcut_draft.open_file,
                    ),
                    shortcut_setting(
                        "Open folder",
                        "Open a folder picker.",
                        EditorShortcut::OpenFolder,
                        &self.shortcut_draft.open_folder,
                    ),
                    shortcut_setting(
                        "Save file",
                        "Save the active file or open Save as for an untitled note.",
                        EditorShortcut::SaveFile,
                        &self.shortcut_draft.save_file,
                    ),
                    shortcut_setting(
                        "Close tab",
                        "Close the active tab.",
                        EditorShortcut::CloseTab,
                        &self.shortcut_draft.close_tab,
                    ),
                    shortcut_setting(
                        "Find in file",
                        "Search inside the active file.",
                        EditorShortcut::FindInFile,
                        &self.shortcut_draft.find_in_file,
                    ),
                    shortcut_setting(
                        "Filter files",
                        "Focus the sidebar file filter.",
                        EditorShortcut::FilterFiles,
                        &self.shortcut_draft.filter_files,
                    ),
                    shortcut_setting(
                        "Quick open",
                        "Search and open a file from the active worktree.",
                        EditorShortcut::QuickOpen,
                        &self.shortcut_draft.quick_open,
                    ),
                    shortcut_setting(
                        "Toggle sidebar",
                        "Show or hide the files sidebar.",
                        EditorShortcut::ToggleSidebar,
                        &self.shortcut_draft.toggle_sidebar,
                    ),
                ],
            )],
            SettingsCategory::LanguageServers => vec![settings_group(
                "Language servers",
                "Semantic diagnostics, completion, and hover. Off by default.",
                vec![
                    widget::toggler(self.language_server_assistance)
                        .label("Language-server assistance".to_owned())
                        .on_toggle(Message::LanguageServerAssistance)
                        .into(),
                    design::caption(
                        "Edit language_servers in the preferences file, then restart ceditor.",
                    )
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
                    design::caption(self.settings_path.to_string_lossy())
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                ],
            )],
        };
        let navigation =
            widget::column::with_children(SettingsCategory::ALL.into_iter().map(|category| {
                cagents::interaction::hover(
                    widget::button::custom(design::body(category.label()))
                        .class(cagents::sidebar::sidebar_button_style(
                            category == self.settings_category,
                        ))
                        .selected(category == self.settings_category)
                        .on_press(Message::SelectSettingsCategory(category))
                        .height(design::ROW_HEIGHT)
                        .width(Length::Fill),
                )
            }))
            .spacing(design::GRID_UNIT)
            .padding([2.0 * design::GRID_UNIT, design::GRID_UNIT])
            .width(180)
            .height(Length::Fill);
        let body = widget::row::with_children(vec![
            navigation.into(),
            widget::divider::vertical::default().into(),
            widget::scrollable(
                widget::container(widget::column::with_children(controls).spacing(20))
                    .padding([4.0 * design::GRID_UNIT, 5.0 * design::GRID_UNIT])
                    .width(Length::Fill)
                    .max_width(620),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                cagents::design::scrollbar(),
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        ])
        .width(Length::Fill)
        .height(Length::Fill);
        let header = widget::row::with_children(vec![
            widget::text::title3("Editor settings")
                .width(Length::Fill)
                .into(),
            icon_control(
                symbolic_icon("window-close-symbolic"),
                "Close settings",
                false,
                true,
                Some(Message::Settings),
            )
            .into(),
        ])
        .align_y(iced::Alignment::Center)
        .height(design::HEADER_HEIGHT);
        Some(dialog_overlay(
            dialog_surface(
                widget::column::with_children(vec![
                    header.into(),
                    widget::divider::horizontal::default().into(),
                    body.into(),
                ])
                .spacing(0)
                .height(Length::Fill),
                900.0,
            ),
            Message::Settings,
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
            shortcuts: crate::preferences::EditorShortcuts::default(),
            shortcut_draft: crate::preferences::EditorShortcuts::default(),
            settings_category: SettingsCategory::Editing,
            settings_path: PathBuf::new(),
            settings_open: false,
            find_open: false,
            find_query: String::new(),
            quick_open: false,
            quick_open_query: String::new(),
            settings_writable: false,
            quick_open_files: None,
            quick_open_generation: 0,
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
            restoring_session: false,
            saved_session: crate::preferences::Session::default(),
            sidebar_open: true,
            pending_save_as: None,
            sidebar_transition: None,
            sidebar_panel: WorktreePanel::Files,
        }
    }
    #[test]
    fn find_matches_counts_case_insensitive_occurrences() {
        assert_eq!(find_matches("Alpha beta ALPHA", "alpha"), 2);
        assert_eq!(find_matches("Alpha beta", ""), 0);
        assert_eq!(find_matches("Alpha beta", "gamma"), 0);
    }
    #[test]
    fn settings_controls_remain_interactive_over_the_overlay() {
        let mut app = editor();
        app.settings_open = true;
        drop(app.update(Message::AutoSave(true)));
        assert!(app.settings.editor_auto_save);
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
    fn toggling_the_sidebar_flips_visibility() {
        let mut app = editor();
        assert!(app.sidebar_open);
        drop(app.update(Message::ToggleSidebar));
        assert!(!app.sidebar_open);
        let closing = app
            .sidebar_transition
            .expect("closing must animate like the cagents inspector");
        assert_eq!(closing.to, 0.0);
        drop(app.update(Message::Frame(Instant::now() + SIDEBAR_TRANSITION_DURATION)));
        assert!(app.sidebar_transition.is_none());
        assert_eq!(app.sidebar_progress(), 0.0);
        drop(app.update(Message::ToggleSidebar));
        assert!(app.sidebar_open);
        assert!(app.sidebar_transition.is_some());
    }

    #[test]
    fn sidebar_can_switch_to_changes_for_a_git_tab() {
        let mut app = editor();
        assert_eq!(app.sidebar_panel, WorktreePanel::Files);
        drop(
            app.update(Message::Sidebar(SidebarAction::SetWorktreePanel {
                panel: WorktreePanel::Changes,
            })),
        );
        assert_eq!(app.sidebar_panel, WorktreePanel::Changes);
        drop(
            app.update(Message::Sidebar(SidebarAction::SetWorktreePanel {
                panel: WorktreePanel::Files,
            })),
        );
        assert_eq!(app.sidebar_panel, WorktreePanel::Files);
    }

    #[test]
    fn settings_opens_from_the_sidebar_footer() {
        let mut app = editor();
        assert!(!app.settings_open);
        drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
        assert!(app.settings_open);
        drop(app.update(Message::Settings));
        assert!(!app.settings_open);
    }

    #[test]
    fn sidebar_resize_clamps_and_persists_the_width() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = editor();
        app.settings_path = directory.path().join("settings.toml");
        app.settings_writable = true;
        drop(app.update(Message::Sidebar(SidebarAction::Resize { width: 9999.0 })));
        assert_eq!(app.sidebar_width(), sidebar::EXPANDED_WIDTH_MAX);
        drop(app.update(Message::Sidebar(SidebarAction::ResizeFinished)));
        let persisted = settings::load(&app.settings_path).unwrap();
        assert_eq!(persisted.sidebar_width, sidebar::EXPANDED_WIDTH_MAX);
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
            sidebar_open: true,
            expanded_directories: Default::default(),
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
                sidebar_open: true,
                expanded_directories: Default::default(),
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
                sidebar_open: true,
                expanded_directories: Default::default(),
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

    #[test]
    fn same_root_tabs_union_expanded_folders_in_session() {
        let mut app = editor();
        let first = add(&mut app, "/workspace", Some("a.rs"));
        add(&mut app, "/workspace", Some("b.rs"));
        // Expand a folder in the first tab while the second tab on the same
        // root never opened anything: the fresh tab must not wipe it.
        app.active = Some(first);
        let key = app
            .tab(first)
            .unwrap()
            .navigation
            .active_worktree()
            .cloned()
            .unwrap();
        drop(app.update(Message::Sidebar(SidebarAction::ToggleDirectory {
            key,
            path: RepoPath::parse("src").unwrap(),
        })));
        assert_eq!(
            app.session().expanded_directories.get("/workspace"),
            Some(&vec!["src".to_owned()]),
            "a fresh same-root tab wiped the opened folder",
        );
        // Collapsing everything still sticks once every tab agrees.
        let key = app
            .tab(first)
            .unwrap()
            .navigation
            .active_worktree()
            .cloned()
            .unwrap();
        drop(app.update(Message::Sidebar(SidebarAction::ToggleDirectory {
            key,
            path: RepoPath::parse("src").unwrap(),
        })));
        assert!(
            !app.session()
                .expanded_directories
                .contains_key("/workspace")
        );
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

    fn configured(directory: &tempfile::TempDir) -> Editor {
        let mut app = editor();
        app.settings_path = directory.path().join("settings.toml");
        app
    }

    fn load_selected(app: &mut Editor, id: CardId) {
        files_loaded(app, id);
        let tab = app.tab(id).unwrap();
        let worktree = tab.runtime.descriptor.worktree.clone();
        let path = tab
            .pending_file
            .as_ref()
            .map(|pending| pending.path.clone())
            .or_else(|| tab.runtime.selected_path().map(Path::to_path_buf))
            .unwrap();
        drop(
            app.update(Message::Repository(RepositoryAction::FileLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: worktree.clone(),
                    path: path.clone(),
                    generation: 1,
                },
                result: cagents::editor::load_file(&worktree, &path),
            })),
        );
    }

    #[test]
    fn untitled_tab_titles_number_scratch_files() {
        assert_eq!(untitled_tab_title("Untitled"), "Untitled");
        assert_eq!(untitled_tab_title("Untitled-2"), "Untitled 2");
        assert_eq!(untitled_tab_title("Untitled-10"), "Untitled 10");
        assert_eq!(untitled_tab_title("notes.md"), "notes.md");
    }

    #[test]
    fn new_tab_creates_scratch_notes_until_they_are_saved() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let first = app.active.unwrap();
        assert_eq!(app.tab(first).unwrap().title, "Untitled");
        assert!(app.tab_is_untitled(app.tab(first).unwrap()));
        let scratch = directory.path().join("untitled").join("Untitled");
        assert!(scratch.is_file());
        load_selected(&mut app, first);
        assert_eq!(
            app.tab(first).unwrap().title,
            "Untitled",
            "opening the scratch file must not rename the tab to the on-disk name"
        );
        drop(app.update(Message::NewTab));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[1].title, "Untitled 2");
        assert!(
            directory
                .path()
                .join("untitled")
                .join("Untitled-2")
                .is_file()
        );
        assert_eq!(
            app.session().paths,
            vec![
                scratch.clone(),
                directory.path().join("untitled").join("Untitled-2")
            ]
        );
    }

    #[test]
    fn new_tab_is_idle_while_a_modal_is_open() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        app.settings_open = true;
        drop(app.update(Message::NewTab));
        assert!(app.tabs.is_empty());
        assert!(!directory.path().join("untitled").exists());
    }

    #[test]
    fn closing_an_empty_untitled_tab_deletes_the_scratch_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        let scratch = app.untitled_scratch_path(id).unwrap();
        drop(app.update(Message::CloseTab(id)));
        assert!(app.tabs.is_empty());
        assert!(!scratch.exists());
    }

    #[test]
    fn closing_an_untitled_note_asks_before_discarding_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        load_selected(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("draft note\n".into()),
                )),
            })),
        );
        drop(app.update(Message::CloseTab(id)));
        assert_eq!(app.closing, Some(id));
        assert!(app.tab(id).is_some());
        drop(app.update(Message::DiscardClose));
        assert!(app.tab(id).is_none());
        assert!(!directory.path().join("untitled").join("Untitled").exists());
    }

    #[test]
    fn save_as_moves_an_untitled_note_to_the_chosen_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        load_selected(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("keep this\n".into()),
                )),
            })),
        );
        let destination = directory.path().join("kept.md");
        let scratch = app.untitled_scratch_path(id).unwrap();
        app.pending_save_as = Some((id, app.untitled_text(id)));
        drop(app.finish_save_as(id, Ok(Some(destination.clone()))));
        assert!(!scratch.exists());
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "keep this\n"
        );
        assert!(app.tabs.is_empty());
        drop(app.update(Message::Opened(
            app.selection_epoch,
            Editor::resolve_open(destination.clone()),
        )));
        let opened = app.active.unwrap();
        assert!(!app.tab_is_untitled(app.tab(opened).unwrap()));
        assert_eq!(app.tab(opened).unwrap().title, "kept.md");
    }

    #[test]
    fn save_as_does_not_replace_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        let destination = directory.path().join("existing.md");
        std::fs::write(&destination, "original\n").unwrap();
        app.pending_save_as = Some((id, app.untitled_text(id)));
        drop(app.finish_save_as(id, Ok(Some(destination.clone()))));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "original\n");
        assert!(app.tab(id).is_some());
        assert_eq!(
            app.notice.as_deref(),
            Some("A file already exists at that path.")
        );
    }

    #[test]
    fn saving_an_untitled_tab_requests_a_destination() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        let task = app.update(Message::Repository(RepositoryAction::Save {
            card: id,
            overwrite: false,
        }));
        drop(task);
        assert!(app.tab(id).is_some());
        assert!(app.tab_is_untitled(app.tab(id).unwrap()));
        assert!(app.pending_save_as.is_some());
    }

    #[test]
    fn window_close_keeps_untitled_scratch_files_for_restore() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        app.settings_writable = true;
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        let scratch = app.untitled_scratch_path(id).unwrap();
        std::fs::write(&scratch, "keep me\n").unwrap();
        load_selected(&mut app, id);
        assert!(app.untitled_has_content(id));
        assert!(!app.tab(id).unwrap().runtime.blocks_navigation());
        drop(app.update(Message::CloseWindow));
        assert!(scratch.is_file());
        assert_eq!(std::fs::read_to_string(&scratch).unwrap(), "keep me\n");
        let session = crate::preferences::load_session(&app.settings_path).unwrap();
        assert_eq!(session.paths, vec![scratch]);
    }

    #[test]
    fn save_as_writes_after_the_untitled_tab_is_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        load_selected(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("still save me\n".into()),
                )),
            })),
        );
        app.pending_save_as = Some((id, app.untitled_text(id)));
        drop(app.update(Message::CloseTab(id)));
        drop(app.update(Message::DiscardClose));
        assert!(app.tabs.is_empty());
        let destination = directory.path().join("survived.txt");
        drop(app.finish_save_as(id, Ok(Some(destination.clone()))));
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "still save me\n"
        );
    }

    #[test]
    fn save_as_during_exit_continues_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = configured(&directory);
        app.settings_writable = true;
        drop(app.update(Message::NewTab));
        let id = app.active.unwrap();
        load_selected(&mut app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("exiting\n".into()),
                )),
            })),
        );
        drop(app.update(Message::CloseWindow));
        assert_eq!(app.closing, Some(id));
        app.pending_save_as = Some((id, app.untitled_text(id)));
        let destination = directory.path().join("exited.txt");
        drop(app.finish_save_as(id, Ok(Some(destination.clone()))));
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "exiting\n");
        assert!(app.tabs.is_empty());
        assert!(app.exiting);
        assert_eq!(app.session().paths, Vec::<PathBuf>::new());
    }
}
