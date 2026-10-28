use cagents::settings_widgets::{Setting, field as settings_field, group as settings_group};
use cagents::{
    design,
    github::GitHubService,
    lsp::{
        LspCompletionItem, LspDiagnostic, LspDocumentId, LspEvent, LspManager, LspPosition,
        LspStatus,
    },
    model::{CardId, ProjectId, RepositoryView, WorktreeId},
    pull_request_review::{self, ReviewSource},
    repository::{
        CancellationToken, CommitOutcome, CommitRequest, DiffBasis, MutationOutcome, PathSelection,
        PushOutcome, PushRequest, RepoPath, RepositoryId, RepositoryService, RepositorySnapshot,
        StageRequest, UnstageRequest, WorktreeAuthority,
    },
    repository_widget::{
        self, ClipboardOp, EditorAction, EditorEdit, LspIntent, RepositoryAction, RepositoryFile,
        RepositoryRuntime,
    },
    settings::{self, AppSettings, AppTheme},
    sidebar::{
        self, ActiveWorktreeKey, FileCommand, FileSearch, MutationKind, SidebarAction,
        SidebarFocus, SidebarMode, SidebarModel, SidebarProject, SidebarState, SidebarWorktree,
        SourceControlModel, WorktreePanel,
    },
    splitter::{self, Splitter},
    ui_controls::{Icon, icon_button, popover, toolbar_actions},
    workspace_widget,
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

const QUICK_OPEN_ROW_HEIGHT: f32 = 40.0;
const QUICK_OPEN_ROW_SPACING: f32 = 2.0;

/// Display limits never authorize a mutation; full-group workers use status().
fn display_snapshot_options(status_limit: usize) -> cagents::repository::SnapshotOptions {
    cagents::repository::SnapshotOptions {
        status_limit: Some(status_limit),
        ..Default::default()
    }
}

/// Platform read fallback is deliberately separate from mutation authority.
/// A supported backend's capture/runtime failure never reaches this fallback.
fn read_display_snapshot(root: &Path, status_limit: usize, authority: Option<WorktreeAuthority>) -> Result<RepositorySnapshot, String> {
    let service = RepositoryService::system("git");
    let cancellation = CancellationToken::default();
    match authority {
        Some(authority) => {
            let snapshot = service.with_worktree_authority(authority.clone()).and_then(|service| service.snapshot_with_options(root, display_snapshot_options(status_limit), &cancellation)).map_err(|error| error.to_string())?;
            authority.validate().map_err(|error| error.to_string())?;
            Ok(snapshot)
        }
        None if !WorktreeAuthority::supported() => read_portable_display_snapshot(root, status_limit),
        None => Err("The original worktree binding is unavailable. Reopen the worktree before reading Git changes.".into()),
    }
}

fn read_portable_display_snapshot(root: &Path, status_limit: usize) -> Result<RepositorySnapshot, String> {
    RepositoryService::system("git").snapshot_with_options(root, display_snapshot_options(status_limit), &CancellationToken::default()).map_err(|error| error.to_string())
}

mod checkout;
mod git_mutations;
mod notification_ui;

#[derive(Clone)]
pub struct Flags {
    pub notification_window: Option<String>,
    pub paths: Vec<PathBuf>,
    pub settings_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Opened {
    root: PathBuf,
    file: Option<PathBuf>,
    snapshot: Option<RepositorySnapshot>,
    authority: Option<WorktreeAuthority>,
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
    pending_fragment: Option<String>,
    owned_file: Option<PathBuf>,
    next_open: u64,
    files_loaded: bool,
    git: bool,
    diagnostics: Vec<LspDiagnostic>,
    /// The document version the server computed `diagnostics` for, as it
    /// stamped them. Compared against the manager's current version and never
    /// against a count of this side's own edits: the manager re-opens a
    /// document on a restart or respawn, neither of which is an edit here.
    diagnostics_version: Option<i32>,
    completions: Vec<LspCompletionItem>,
    hover: Option<String>,
    lsp_error: Option<String>,
    mutation: Option<MutationKind>,
    snapshot_loading: bool,
    snapshot_refresh_pending: bool,
    change_source_revision: u64,
    worktree_authority: Option<WorktreeAuthority>,
    discard_authority: Option<WorktreeAuthority>,
    discard_queue_turn: Option<u64>,
    marker_authority: Option<WorktreeAuthority>,
    mutation_operation: u64,
    mutation_cancel: Option<CancellationToken>,
    git_mutation_revision: u64,
    discard_recovery: Option<Arc<cagents::repository::DiscardRecovery>>,
    /// The pull request review behind the sidebar's Pull requests panel. It
    /// lists the GitHub repository the worktree's remotes name and drives
    /// `runtime` through the files of whichever pull request is open.
    review: pull_request_review::State,
}

/// The GitHub repository a worktree's remotes name, `origin` first, for the
/// review to list pull requests from.
fn review_source(root: &Path, snapshot: Option<&RepositorySnapshot>) -> Option<ReviewSource> {
    review_source_for_remotes(root, &snapshot?.remotes)
}

fn review_source_for_remotes(
    root: &Path,
    remotes: &[cagents::repository::RemoteRecord],
) -> Option<ReviewSource> {
    ReviewSource::from_remotes(root, remotes)
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
    Checkout(checkout::Action),
    Choose(bool),
    Chosen(u64, Result<Option<PathBuf>, String>),
    Opened(u64, Result<Opened, String>),
    DocumentOpened(u64, Option<String>, Result<Opened, String>),
    SessionOpened(u64, Vec<(bool, Result<Opened, String>)>),
    FileResolved(CardId, u64, Result<PathBuf, String>),
    Select(CardId),
    ReorderTab {
        card: CardId,
        before: Option<CardId>,
    },
    CloseTab(CardId),
    CloseWindow,
    SaveClose,
    DiscardClose,
    ConfirmClose,
    CancelClose,
    Reaped,
    WindowFocused(window::Id, bool),
    WindowResized(iced::Size),
    RefreshVisible,
    /// The shell's light or dark mode changed, so a rendered document page no
    /// longer matches the surface it sits on.
    DocumentToneStale,
    Repository(RepositoryAction),
    ResolvedDiffBasis {
        card: CardId,
        basis: DiffBasis,
    },
    Sidebar(SidebarAction),
    /// One step of a tab's pull request review; see [`Tab::review`].
    PullRequestReview(CardId, pull_request_review::Action),
    ReviewRequests(pull_request_review::request_ui::monitor::Action),
    Notification(notification_ui::Action),
    StartupWindowOpened,
    Snapshot(CardId, Result<RepositorySnapshot, String>),
    RevealedInFiles {
        card: CardId,
        key: sidebar::ActiveWorktreeKey,
        requested: RepoPath,
        result: Result<(RepositorySnapshot, Option<RepoPath>), String>,
    },
    ChangeGroupResolved {
        request: git_mutations::GroupRequest,
        result: Result<git_mutations::GroupSelection, String>,
    },
    DiffPatchPrepared {
        origin: git_mutations::GroupOrigin,
        result: Result<Option<repository_widget::PreparedDiffMutation>, String>,
    },
    UndoMutationFinished {
        card: CardId,
        expires_unix_ms: u64,
        paths: Vec<RepoPath>,
        result: Result<MutationOutcome, String>,
    },
    PatchMutationFinished {
        card: CardId,
        result: Result<(MutationOutcome, Option<cagents::repository::DiscardRecovery>), String>,
    },
    StageMarkers {
        card: CardId,
        operation: u64,
        authority: WorktreeAuthority,
        selection: Vec<PathSelection>,
        markers: BTreeMap<RepoPath, cagents::repository::ConflictMarkerVersion>,
    },
    Mutation(CardId, Result<MutationOutcome, String>),
    Discarded(CardId, Vec<RepoPath>, Result<MutationOutcome, String>),
    Committed(CardId, String, Result<CommitOutcome, String>),
    Pushed(CardId, Result<PushOutcome, String>),
    FileNameChanged(String),
    SubmitFileOperation,
    CancelFileOperation,
    FileOperationFinished(Result<PathBuf, String>),
    Settings,
    SelectSettingsCategory(SettingsCategory),
    SettingsSearchChanged(String),
    ResetSettings,
    ShortcutChanged(EditorShortcut, String),
    FindInFile,
    FindQueryChanged(String),
    CloseFind,
    FindNext,
    FindPrevious,
    QuickOpen,
    QuickOpenQueryChanged(String),
    QuickOpenNext,
    QuickOpenPrevious,
    QuickOpenActivate,
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
    LineHeightChanged(String),
    SyntaxTheme(cagents::settings::EditorSyntaxTheme),
    LanguageServerAssistance(bool),
    Theme(AppTheme),
    Scale(f32),
    SettingsSaved(Result<(), String>),
    FileRevealed(Result<cagents::file_manager::RevealOutcome, String>),
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
pub(crate) enum SettingsCategory {
    General,
    Editing,
    Appearance,
    Keyboard,
    LanguageServers,
}

impl SettingsCategory {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Editing,
        Self::Appearance,
        Self::Keyboard,
        Self::LanguageServers,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editor",
            Self::Appearance => "Appearance",
            Self::Keyboard => "Keyboard",
            Self::LanguageServers => "Language servers",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorShortcut {
    NewTab,
    OpenFile,
    OpenFolder,
    SaveFile,
    CloseTab,
    FindInFile,
    FilterFiles,
    QuickOpen,
    ToggleSidebar,
    ReviewToggleViewed,
    ReviewNextFile,
    ReviewPreviousFile,
    ReviewNextUnviewed,
    ReviewNextChange,
    ReviewPreviousChange,
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
            EditorShortcut::ReviewToggleViewed => self.review_toggle_viewed = value,
            EditorShortcut::ReviewNextFile => self.review_next_file = value,
            EditorShortcut::ReviewPreviousFile => self.review_previous_file = value,
            EditorShortcut::ReviewNextUnviewed => self.review_next_unviewed = value,
            EditorShortcut::ReviewNextChange => self.review_next_change = value,
            EditorShortcut::ReviewPreviousChange => self.review_previous_change = value,
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
            EditorShortcut::ReviewToggleViewed => &self.review_toggle_viewed,
            EditorShortcut::ReviewNextFile => &self.review_next_file,
            EditorShortcut::ReviewPreviousFile => &self.review_previous_file,
            EditorShortcut::ReviewNextUnviewed => &self.review_next_unviewed,
            EditorShortcut::ReviewNextChange => &self.review_next_change,
            EditorShortcut::ReviewPreviousChange => &self.review_previous_change,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let mut seen = Vec::new();
        for (value, unbindable) in [
            (&self.new_tab, false),
            (&self.open_file, false),
            (&self.open_folder, false),
            (&self.save_file, false),
            (&self.close_tab, false),
            (&self.find_in_file, false),
            (&self.filter_files, false),
            (&self.quick_open, false),
            (&self.toggle_sidebar, false),
            (&self.review_toggle_viewed, true),
            (&self.review_next_file, true),
            (&self.review_previous_file, true),
            (&self.review_next_unviewed, true),
            (&self.review_next_change, true),
            (&self.review_previous_change, true),
        ] {
            // A review action may stay unbound: its default is dropped when
            // an older table already used the chord for something else.
            if unbindable && value.trim().is_empty() {
                continue;
            }
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
    fn set_ui_scale(&mut self, scale: f32) {
        let ratio = self.settings.ui_scale / scale;
        self.window_size = iced::Size::new(
            self.window_size.width * ratio,
            self.window_size.height * ratio,
        );
        self.settings.ui_scale = scale;
    }
    fn select_tab(&mut self, id: CardId) -> Task<Message> {
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.active = Some(id);
        self.tab_mut(id).map_or_else(Task::none, |tab| {
            if tab.runtime.editor_text().is_some() {
                tab.pending_fragment
                    .take()
                    .map_or_else(Task::none, |fragment| {
                        repository_task(tab.runtime.preview_fragment(id, &fragment))
                    })
            } else {
                Task::none()
            }
        })
    }
    fn shortcut_pressed(&self, action: EditorShortcut, event: &keyboard::Event) -> bool {
        cagents::settings::parse_shortcut(self.shortcuts.value(action))
            .is_ok_and(|binding| workspace_widget::shortcut_event_matches(binding, event))
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
        for (shortcut, action) in [
            (
                EditorShortcut::ReviewToggleViewed,
                pull_request_review::Action::ToggleViewedAndAdvance,
            ),
            (
                EditorShortcut::ReviewNextFile,
                pull_request_review::Action::NextFile,
            ),
            (
                EditorShortcut::ReviewPreviousFile,
                pull_request_review::Action::PreviousFile,
            ),
            (
                EditorShortcut::ReviewNextUnviewed,
                pull_request_review::Action::NextUnviewed,
            ),
            (
                EditorShortcut::ReviewNextChange,
                pull_request_review::Action::NextChange,
            ),
            (
                EditorShortcut::ReviewPreviousChange,
                pull_request_review::Action::PreviousChange,
            ),
        ] {
            if self.shortcut_pressed(shortcut, event) {
                // The chords mean something only with a review on screen;
                // otherwise they stay with the editor, where an arrow chord
                // may be a line move.
                let reviewing = self.sidebar_open
                    && self.sidebar_panel == WorktreePanel::PullRequests
                    && self
                        .active
                        .and_then(|card| self.tab(card))
                        .is_some_and(|tab| tab.git && tab.review.is_open());
                return reviewing
                    .then_some(self.active)
                    .flatten()
                    .map(|card| self.update(Message::PullRequestReview(card, action)));
            }
        }
        None
    }
}

pub struct Editor {
    core: Core,
    tabs: Vec<Tab>,
    review_requests: pull_request_review::request_ui::monitor::Monitor,
    notifications: notification_ui::State,
    active: Option<CardId>,
    selection_epoch: u64,
    git_groups: BTreeMap<PathBuf, git_mutations::GroupQueue>,
    next_group_operation: u64,
    settings: AppSettings,
    shortcuts: crate::preferences::EditorShortcuts,
    shortcut_draft: crate::preferences::EditorShortcuts,
    settings_category: SettingsCategory,
    settings_search: String,
    line_height_draft: String,
    window_size: iced::Size,
    tab_history: cagents::tab_history::TabHistory<CardId>,
    settings_reset_confirm: bool,
    settings_path: PathBuf,
    settings_open: bool,
    settings_writable: bool,
    quick_open_files: Option<Vec<PathBuf>>,
    find_open: bool,
    find_query: String,
    find_match_index: usize,
    /// Occurrences of `find_query` in the active buffer, recorded by the
    /// search itself. Counting again in `view` re-materialized and
    /// re-lowercased the whole rope on every animation frame.
    find_match_total: usize,
    quick_open_generation: u64,
    quick_open: bool,
    quick_open_query: String,
    quick_open_index: usize,
    quick_open_cancel: CancellationToken,
    notice: Option<String>,
    notices_open: bool,
    file_dialog: Option<FileOperationDialog>,
    checkout_dialog: Option<checkout::Dialog>,
    checkout_journal: Option<checkout::Journal>,
    checkout_history: Vec<cagents::repository::WorktreeOperationReport>,
    checkout_history_open: bool,
    checkout_error: Option<String>,
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
    /// A shell theme change has landed that the open document pages were not
    /// rendered for. It is recorded rather than acted on, because the notice
    /// arrives on a path whose returned task libcosmic discards.
    document_tone_stale: bool,
    sidebar_panel: WorktreePanel,
    /// Newest generation written per preference file. Storage work runs on the
    /// blocking pool, so two captures of the same file can finish out of
    /// order; the loser is dropped instead of resurrecting stale values.
    preference_lane: Arc<std::sync::Mutex<[u64; PreferenceFile::COUNT]>>,
    preference_generation: u64,
}

/// One write-ordering lane per preference file ceditor owns.
#[derive(Clone, Copy)]
enum PreferenceFile {
    Settings,
    Assistance,
    Shortcuts,
    Session,
}

impl PreferenceFile {
    const COUNT: usize = 4;
}

/// Captured on the event loop; all storage work runs on the blocking pool.
///
/// `settings::save` takes a blocking `flock` shared with every other cagents
/// window and fsyncs twice. None of that belongs on the thread driving the
/// compositor frame.
enum PreferenceWrite {
    Settings(AppSettings),
    Assistance {
        settings: AppSettings,
        enabled: bool,
    },
    Shortcuts(crate::preferences::EditorShortcuts),
    Session(crate::preferences::Session),
}

impl PreferenceWrite {
    const fn file(&self) -> PreferenceFile {
        match self {
            Self::Settings(_) => PreferenceFile::Settings,
            Self::Assistance { .. } => PreferenceFile::Assistance,
            Self::Shortcuts(_) => PreferenceFile::Shortcuts,
            Self::Session(_) => PreferenceFile::Session,
        }
    }
}

struct PreferenceSave {
    path: PathBuf,
    generation: u64,
    lane: Arc<std::sync::Mutex<[u64; PreferenceFile::COUNT]>>,
    write: PreferenceWrite,
}

impl PreferenceSave {
    fn run(self) -> Result<(), String> {
        let file = self.write.file();
        let mut lane = self
            .lane
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let applied = &mut lane[file as usize];
        if *applied >= self.generation {
            return Ok(());
        }
        *applied = self.generation;
        // The lane stays locked across the write so two captures of one file
        // land in capture order rather than blocking-pool completion order.
        match self.write {
            PreferenceWrite::Settings(settings) => {
                settings::save(&self.path, &settings).map_err(|error| error.to_string())
            }
            PreferenceWrite::Assistance { settings, enabled } => {
                settings::save(&self.path, &settings)
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        crate::preferences::save_language_server_assistance(&self.path, enabled)
                    })
            }
            PreferenceWrite::Shortcuts(shortcuts) => {
                crate::preferences::save_shortcuts(&self.path, &shortcuts)
            }
            PreferenceWrite::Session(session) => {
                crate::preferences::save_session(&self.path, &session)
            }
        }
    }
}

const SIDEBAR_TRANSITION_DURATION: Duration = Duration::from_millis(160);

/// Quick open waits out a typing burst before walking the tree, matching the
/// sidebar's file filter over the same helper.
const QUICK_OPEN_SEARCH_DEBOUNCE: Duration = Duration::from_millis(80);

/// Hands one captured preference write to the blocking pool and reports the
/// outcome on a later frame.
fn preference_task(save: PreferenceSave) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || save.run())
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        Message::SettingsSaved(result)
    })
}

/// Whether an event is the fixed Ctrl+Space completion request.
///
/// The space bar has no `Named` variant in this toolkit, and a modifier-held
/// press usually carries no text, so the logical `Character(" ")` arm this
/// used to rely on alone could never fire. The physical code is the reliable
/// signal; the character form stays for backends that do report the text.
fn is_completion_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    modifiers.control()
        && (matches!(
            physical_key,
            keyboard::key::Physical::Code(keyboard::key::Code::Space)
        ) || matches!(key, keyboard::Key::Character(value) if value == " "))
}

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

/// Whether a repository action is allowed to land while a modal is up.
///
/// Input a reader could aim at the pane behind an overlay is refused, but a
/// completion is not input: dropping one strands whatever it completes. A
/// dropped page render leaves the document viewer reading "Rendering page"
/// for a page it will never draw, because accepting the result is the only
/// thing that clears the fence.
fn repository_action_allowed_while_modal(action: &RepositoryAction) -> bool {
    matches!(
        action,
        RepositoryAction::FilesLoaded { .. }
            | RepositoryAction::DirectoryListed { .. }
            | RepositoryAction::DirectorySearchLoaded { .. }
            | RepositoryAction::FileLoaded { .. }
            | RepositoryAction::DiffLoaded { .. }
            | RepositoryAction::DiffBasisResolved { .. }
            | RepositoryAction::WorkingDiffDue { .. }
            | RepositoryAction::WorkingDiffLoaded { .. }
            | RepositoryAction::FindLoaded { .. }
            | RepositoryAction::SaveFinished { .. }
            | RepositoryAction::PdfPageRendered { .. }
            | RepositoryAction::DiagramsRendered { .. }
            | RepositoryAction::MarkdownDue { .. }
            | RepositoryAction::AutoSaveDue { .. }
            | RepositoryAction::Save { .. }
            | RepositoryAction::Reload { .. }
    )
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
    let parent = path.parent().ok_or("File has no parent directory")?;
    let _protection =
        cagents::worktree_cleanup::protect(parent).map_err(|error| error.to_string())?;
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

/// The Pull requests panel body: the open pull requests of the reviewed
/// repository, or the review of the chosen one.
fn review_panel<'a>(
    review: &'a pull_request_review::State,
    modal_open: bool,
) -> Element<'a, pull_request_review::Action> {
    if !review.is_open() {
        return review.browser(modal_open);
    }
    let rows = vec![
        review.header(modal_open),
        design::horizontal_rule(),
        review.summary(modal_open),
        review.notice(modal_open),
        review.file_list(modal_open),
    ];
    widget::column::with_children(rows)
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn shortcut_setting<'a>(
    title: &'static str,
    detail: &'static str,
    action: EditorShortcut,
    value: &'a str,
) -> Setting<'a, Message> {
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

fn connected_dialog_surface<'a>(
    content: impl Into<Element<'a, Message>>,
    max_width: f32,
) -> Element<'a, Message> {
    cagents::interaction::opaque(
        widget::container(content)
            .class(cosmic::theme::Container::Dialog(false))
            .width(Length::Fill)
            .max_width(max_width),
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

fn find_bar(editor: &Editor) -> Element<'_, Message> {
    widget::container(
        widget::row::with_children(vec![
            cagents::ui_controls::passive_icon(Icon::Search, 16),
            widget::text_input("Find in file", &editor.find_query)
                .id(widget::Id::new("ceditor.find.query"))
                .on_input(Message::FindQueryChanged)
                .width(Length::Fill)
                .into(),
            design::caption(if editor.find_query.trim().is_empty() {
                "Type to find".to_owned()
            } else if editor.find_match_total == 0 {
                "No matches".to_owned()
            } else {
                format!(
                    "{} of {} matches",
                    editor.find_match_index + 1,
                    editor.find_match_total
                )
            })
            .into(),
            icon_button(Icon::ChevronUp, "Previous match (Shift+Enter)")
                .on_press(Message::FindPrevious)
                .into(),
            icon_button(Icon::ChevronDown, "Next match (Enter)")
                .on_press(Message::FindNext)
                .into(),
            icon_button(Icon::Close, "Close find")
                .on_press(Message::CloseFind)
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
    fn pending_discard(&self) -> Option<&sidebar::DiscardConfirmation> {
        self.current()?.navigation.pending_discard()
    }

    fn modal_open(&self) -> bool {
        self.settings_open
            || self.quick_open
            || self.closing.is_some()
            || self.file_dialog.is_some()
            || self.checkout_dialog.is_some()
            || self.checkout_history_open
            || self.notices_open
            || self.pending_discard().is_some()
    }

    fn settings_footer(&self) -> Element<'_, Message> {
        widget::row::with_children(vec![
            icon_button(Icon::Branch, "Worktrees")
                .selected(self.checkout_history_open)
                .on_press_maybe(
                    (!self.modal_open()).then_some(Message::Checkout(checkout::Action::History)),
                )
                .into(),
            icon_button(Icon::Settings, "Settings")
                .selected(self.settings_open)
                .on_press_maybe((!self.modal_open()).then_some(Message::Settings))
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
            tab.mutation.is_some()
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
        Self::resolve_open_with_limit(path, AppSettings::default().git_status_limit)
    }

    fn resolve_open_with_limit(path: PathBuf, status_limit: usize) -> Result<Opened, String> {
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
        let authority = if WorktreeAuthority::supported() {
            repository_root.as_deref().map(WorktreeAuthority::capture).transpose().map_err(|error| error.to_string())?
        } else { None };
        let snapshot = repository_root.as_ref().map(|root| read_display_snapshot(root, status_limit, authority.clone())).transpose()?;
        let root = repository_root.unwrap_or(directory);
        let file = (!path.is_dir())
            .then(|| path.strip_prefix(&root).map(Path::to_path_buf))
            .transpose()
            .map_err(|error| error.to_string())?;
        if let Some(authority) = &authority { authority.validate().map_err(|error| error.to_string())?; }
        Ok(Opened {
            root,
            file,
            snapshot,
            authority,
        })
    }

    fn open(path: PathBuf, selection_epoch: u64, status_limit: usize) -> Task<Message> {
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || Self::resolve_open_with_limit(path, status_limit))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::Opened(selection_epoch, result)
        })
    }

    fn restore_session(paths: Vec<(PathBuf, bool)>, status_limit: usize) -> Task<Message> {
        cosmic::task::future(async move {
            // Resolve in durable order, not background-task completion order.
            // An unavailable entry never prevents the remaining files opening.
            let opened = tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|(path, focus)| (focus, Self::resolve_open_with_limit(path, status_limit)))
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

    fn capture_preference(&mut self, write: PreferenceWrite) -> PreferenceSave {
        self.preference_generation += 1;
        PreferenceSave {
            path: self.settings_path.clone(),
            generation: self.preference_generation,
            lane: Arc::clone(&self.preference_lane),
            write,
        }
    }

    /// Captures the navigation state worth writing, or nothing when it has not
    /// changed since the last capture.
    ///
    /// The private configuration directory is created once, when `init` loads
    /// the settings file. Re-validating it here used to go through the shared
    /// settings writer, which read and *migrated* settings.toml on the way —
    /// so expanding a folder silently rewrote the user's preferences file.
    fn capture_session_save(&mut self) -> Option<PreferenceSave> {
        if self.exiting || self.restoring_session || !self.settings_writable {
            return None;
        }
        let session = self.session();
        if session == self.saved_session {
            return None;
        }
        // Recorded at capture time so a burst of navigation changes queues one
        // write per distinct state; the lane drops whichever loses the race.
        self.saved_session = session.clone();
        Some(self.capture_preference(PreferenceWrite::Session(session)))
    }

    fn save_session(&mut self) -> Task<Message> {
        self.capture_session_save()
            .map_or_else(Task::none, preference_task)
    }

    /// Shutdown has no later frame to deliver an acknowledgment on, so the
    /// final navigation write is the one place this blocks.
    fn save_session_now(&mut self) {
        if self.exiting || self.restoring_session || !self.settings_writable {
            return;
        }
        // A matching captured snapshot may still be waiting in the blocking
        // pool, or its write may have failed. A fresh generation fences both.
        let session = self.session();
        self.saved_session = session.clone();
        let save = self.capture_preference(PreferenceWrite::Session(session));
        if let Err(error) = save.run() {
            self.notice = Some(error);
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
        let descriptor_root = opened.root.clone();
        let descriptor = RepositoryView {
            selected_file: None,
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
        runtime.set_git_mutation_unavailable_reason(WorktreeAuthority::unsupported_reason().map(Arc::from));
        runtime.set_auto_save(self.settings.editor_auto_save);
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        runtime.set_line_height(self.settings.terminal_line_height);
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
        let mutation = self
            .tabs
            .iter()
            .filter(|tab| tab.runtime.descriptor.worktree == opened.root)
            .find_map(|tab| tab.mutation);
        if let Some(source) = sidebar.source_control.as_mut() {
            source.set_mutation(mutation);
            source.set_mutation_unavailable_reason(WorktreeAuthority::unsupported_reason().map(Arc::from));
        }
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
        let mut review = pull_request_review::State::new(
            Arc::new(GitHubService::system("gh")),
            RepositoryService::system("git"),
        )
        .with_cache(Some(Arc::new(pull_request_review::cache::JsonStore::new(
            self.settings_path.with_file_name("review.json"),
        ))));
        let review_task = review
            .set_source(review_source(&descriptor_root, opened.snapshot.as_ref()))
            .map(move |action| Message::PullRequestReview(id, action))
            .map(cosmic::Action::App);
        self.tabs.push(Tab {
            id,
            title,
            runtime,
            sidebar,
            navigation,
            pending_file,
            pending_fragment: None,
            owned_file: None,
            next_open: 1,
            files_loaded: false,
            git,
            diagnostics: Vec::new(),
            diagnostics_version: None,
            completions: Vec::new(),
            hover: None,
            lsp_error: None,
            mutation,
            snapshot_loading: false,
            snapshot_refresh_pending: false,
            change_source_revision: 1,
            worktree_authority: opened.authority,
            discard_authority: None,
            discard_queue_turn: None,
            marker_authority: None,
            mutation_operation: 0,
            mutation_cancel: None,
            git_mutation_revision: 0,
            discard_recovery: None,
            review,
        });
        if self.active.is_none() || (focus && !self.modal_open()) {
            self.active = Some(id);
        }
        let effects = self.apply_review_effects(id);
        Task::batch([task, review_task, effects])
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
                authority: None,
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
        Self::open(path, self.selection_epoch, self.settings.git_status_limit)
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
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        tab.pending_file = None;
        tab.pending_fragment = None;
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

    /// Opens the change Git already named, including a path that is no longer
    /// on disk. Canonicalizing that path fails, and the file never reaches
    /// the diff.
    fn open_working_change(
        &mut self,
        id: CardId,
        path: &RepoPath,
        area: cagents::repository::ChangeArea,
    ) -> Task<Message> {
        use cagents::repository::ChangeArea;
        // An open that was already in flight belongs to the previous
        // selection. The same fence `request_file` raises.
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        if self
            .tab(id)
            .is_some_and(|tab| tab.runtime.blocks_navigation())
        {
            self.notice =
                Some("Save or discard the current file before opening another file.".into());
            return Task::none();
        }
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let file = {
            let Some(source) = tab.sidebar.source_control.as_ref() else {
                return Task::none();
            };
            let find = |area| {
                source
                    .group(area)
                    .and_then(|group| group.rows().iter().find(|row| &row.entry.path == path))
            };
            if find(area).is_none() {
                return Task::none();
            }
            let staged = find(ChangeArea::Staged);
            let unstaged = find(ChangeArea::Unstaged)
                .or_else(|| find(ChangeArea::Untracked))
                .or_else(|| find(ChangeArea::Conflict));
            RepositoryFile {
                path: path.as_path().to_owned(),
                staged: staged.is_some(),
                unstaged: unstaged.is_some(),
                staged_old_path: staged
                    .and_then(|row| row.entry.old_path.as_ref())
                    .map(|old| old.as_path().to_owned()),
                unstaged_old_path: unstaged
                    .and_then(|row| row.entry.old_path.as_ref())
                    .map(|old| old.as_path().to_owned()),
                ..Default::default()
            }
        };
        let basis = if area == ChangeArea::Staged {
            DiffBasis::HeadToIndex
        } else {
            DiffBasis::IndexToWorktree
        };
        let file_name = path
            .as_path()
            .file_name()
            .unwrap_or(path.as_path().as_os_str())
            .to_string_lossy()
            .into_owned();
        tab.title = file_name;
        // The row may not exist on disk. Record it anyway, so the session and
        // the language-server document follow this change rather than the
        // file that was open before the click.
        tab.owned_file = Some(tab.runtime.descriptor.worktree.join(path.as_path()));
        tab.pending_file = None;
        tab.pending_fragment = None;
        repository_task(tab.runtime.update(RepositoryAction::OpenWorkingDiff {
            card: id,
            file,
            basis,
        }))
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
                    absolute_path,
                    worktree,
                    version,
                    text,
                } => {
                    // The intent names the file the bytes belong to. A link's
                    // buffer is the target, which is not the row that was clicked.
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
                        tab.diagnostics_version = None;
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
            tab.diagnostics_version = None;
            tab.completions.clear();
            tab.hover = None;
        }
    }

    /// Whether `id`'s diagnostics still describe the text the server holds.
    ///
    /// Both versions have to be known: an absent published version means no
    /// diagnostics have arrived for this document, and an absent document
    /// version means the manager is no longer tracking it, so nothing on
    /// screen can be vouched for.
    fn diagnostics_are_current(&self, id: CardId) -> bool {
        let Some(tab) = self.tab(id) else {
            return false;
        };
        match (
            tab.diagnostics_version,
            tab.runtime
                .lsp_document()
                .and_then(|doc| self.lsp.document_version(doc)),
        ) {
            (Some(published), Some(current)) => published == current,
            _ => false,
        }
    }

    fn request_close(&mut self, id: CardId) -> Task<Message> {
        let untitled_note = self.tab(id).is_some_and(|tab| self.tab_is_untitled(tab))
            && self.untitled_has_content(id)
            && !self.exiting;
        if self
            .tab(id)
            .is_some_and(|tab| tab.runtime.blocks_navigation() || tab.mutation.is_some())
            || (untitled_note && !self.close_confirmed)
        {
            self.active = Some(id);
            self.closing = Some(id);
            self.close_confirmed = false;
            let tab = self.tab(id).unwrap();
            let focus = if untitled_note || tab.runtime.can_save() {
                "ceditor.close.save"
            } else if tab.runtime.can_abandon_unsaved_changes() && tab.mutation.is_none() {
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
        let settings = self.settings.clone();
        // Preference changes are low frequency. One ordered atomic write avoids
        // competing snapshots overwriting newer values.
        preference_task(self.capture_preference(PreferenceWrite::Settings(settings)))
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
            let settings = self.settings.clone();
            tasks.push(preference_task(self.capture_preference(
                PreferenceWrite::Assistance { settings, enabled },
            )));
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
        let anchor = icon_button(
            if has_notices {
                Icon::Warning
            } else {
                Icon::Info
            },
            if has_notices {
                "Notices — errors need attention"
            } else {
                "Notices"
            },
        )
        .selected(self.notices_open)
        .on_press_maybe((self.notices_open || !self.modal_open()).then_some(Message::Notices));
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
                    icon_button(Icon::Close, "Close notices (Escape)")
                        .on_press(Message::CloseNotices)
                        .id(widget::Id::new("ceditor.notices.close"))
                        .into(),
                ])
                .align_y(iced::Alignment::Center)
                .into(),
                widget::scrollable(
                    widget::column::with_children(details)
                        .spacing(10)
                        .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                )
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

    fn appearance(&mut self) -> Task<Message> {
        let theme = self.settings.app_theme.resolve(self.core.system_theme());
        let documents = self.retone_documents(&theme);
        Task::batch([
            cosmic::command::set_theme(theme),
            documents,
            Task::done(cosmic::Action::Cosmic(cosmic::app::Action::ScaleFactor(
                self.settings.ui_scale,
            ))),
        ])
    }

    /// Re-renders every open document page for `theme`.
    ///
    /// A page's colours are rasterized into it, so the shell changing from
    /// light to dark leaves exactly the glaring white sheet the adapted tone
    /// exists to prevent until the page is rendered again. The theme is
    /// passed rather than read from the global, because the global is set by
    /// the command this is issued alongside and a page rendered for the
    /// outgoing theme would be no better than not rendering one at all.
    fn retone_documents(&mut self, theme: &cosmic::Theme) -> Task<Message> {
        Task::batch(
            self.tabs
                .iter_mut()
                .map(|tab| repository_task(tab.runtime.retone_document(tab.id, theme)))
                .collect::<Vec<_>>(),
        )
    }

    /// Renders the open document pages again for the theme now in force.
    ///
    /// The theme is read from the global here, which is the one case where
    /// that is the right source: the shell's change has already been applied
    /// to it by the time anything asks this.
    fn flush_document_tone(&mut self) -> Task<Message> {
        if !std::mem::take(&mut self.document_tone_stale) {
            return Task::none();
        }
        let theme = cosmic::theme::active();
        self.retone_documents(&theme)
    }

    /// Notices that the shell went from light to dark, or back.
    ///
    /// This watches the same configuration libcosmic watches, because the
    /// hook libcosmic offers for it hands back a task that libcosmic itself
    /// then discards on the path a light or dark flip actually takes.
    fn document_tone_subscription(&self) -> Subscription<Message> {
        self.core
            .watch_config::<cosmic::cosmic_theme::ThemeMode>(cosmic::cosmic_theme::THEME_MODE_ID)
            .map(|_| Message::DocumentToneStale)
    }

    fn can_refresh_visible(&self) -> bool {
        self.window_focused && !self.modal_open() && !self.exiting && self.current().is_some()
    }

    fn refresh_visible(&mut self) -> Task<Message> {
        // Reuse the existing refresh clock only while a bounded Undo receipt
        // is retained; expiry never performs a Git operation.
        for tab in &mut self.tabs {
            if tab.discard_recovery.as_ref().is_some_and(|recovery| !recovery.can_undo()) {
                tab.discard_recovery = None;
                if let Some(source) = tab.sidebar.source_control.as_mut() { source.undo_discard_until_unix_ms = None; }
            }
        }
        if !self.can_refresh_visible() {
            return Task::none();
        }
        let id = self.active.unwrap();
        let tab = self.tab_mut(id).unwrap();
        let files = repository_task(tab.runtime.refresh_visible(id));
        let review = if self.sidebar_open && self.sidebar_panel == WorktreePanel::PullRequests {
            self.handle_review(
                id,
                pull_request_review::Action::RefreshIfDue(std::time::Instant::now()),
            )
        } else {
            Task::none()
        };
        Task::batch([files, review, self.refresh_snapshot(id, false)])
    }

    fn refresh(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let files = repository_task(tab.runtime.refresh_after_mutation(id));
        Task::batch([files, self.refresh_snapshot(id, true)])
    }

    fn refresh_snapshot(&mut self, id: CardId, force: bool) -> Task<Message> {
        let status_limit = self.settings.git_status_limit;
        let queued = self.tab(id).is_some_and(|tab| self.git_groups.contains_key(&tab.runtime.descriptor.worktree));
        let Some(tab) = self.tab_mut(id).filter(|tab| tab.git) else {
            return Task::none();
        };
        if tab.mutation.is_some() || queued {
            tab.snapshot_refresh_pending = true;
            return Task::none();
        }
        if tab.snapshot_loading {
            tab.snapshot_refresh_pending |= force;
            return Task::none();
        }
        tab.snapshot_loading = true;
        tab.snapshot_refresh_pending = false;
        let root = tab.runtime.descriptor.worktree.clone();
        let key = tab.navigation.active_worktree().cloned();
        let requested = tab.navigation.files_reveal_demand().cloned();
        let worker_path = requested.clone();
        let authority = tab.worktree_authority.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut snapshot = read_display_snapshot(&root, status_limit, authority.clone())?;
                let target = worker_path
                    .as_ref()
                    .map(|path| {
                        cagents::repository::include_revealed_path(&root, &mut snapshot.files, path)
                    })
                    .transpose()?
                    .flatten();
                if let Some(authority) = authority { authority.validate().map_err(|error| error.to_string())?; }
                Ok((snapshot, target))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            match (key, requested) {
                (Some(key), Some(requested)) => Message::RevealedInFiles {
                    card: id,
                    key,
                    requested,
                    result,
                },
                _ => Message::Snapshot(id, result.map(|(snapshot, _)| snapshot)),
            }
        })
    }

    /// Hands a link to the desktop's opener. The exit status comes back as a
    /// notice because the shell has no browser of its own.
    fn open_external_link(uri: String) -> Task<Message> {
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let status = std::process::Command::new("xdg-open").arg(uri).status()?;
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
        })
    }

    /// Routes one review action to a tab and carries out what the review
    /// asks of the shell: which committed file the tab's runtime shows, and
    /// which link to open.
    fn handle_review(&mut self, id: CardId, action: pull_request_review::Action) -> Task<Message> {
        if self.modal_open() && !action.allowed_while_modal() {
            return Task::none();
        }
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let action = match action {
            pull_request_review::Action::Select(number) => match tab.review.listed_target(number) {
                Some(target) => pull_request_review::Action::Open(target),
                None => return Task::none(),
            },
            action => action,
        };
        let updated = tab
            .review
            .update(action)
            .map(move |action| Message::PullRequestReview(id, action))
            .map(cosmic::Action::App);
        Task::batch([updated, self.apply_review_effects(id)])
    }

    /// Carries out what a tab's review asked of the shell since the last
    /// drain. Every path that drives the review ends here, including the
    /// re-sourcing a snapshot triggers, so nothing waits in the outbox for
    /// the next keystroke.
    fn apply_review_effects(&mut self, id: CardId) -> Task<Message> {
        let Some(tab) = self.tab_mut(id) else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        let mut checkout_target = None;
        for effect in tab.review.take_effects() {
            match effect {
                pull_request_review::Effect::OpenFile {
                    path,
                    old_path,
                    basis,
                } => tasks.push(repository_task(tab.runtime.update(
                    RepositoryAction::OpenCommittedFile {
                        card: id,
                        path,
                        old_path,
                        basis,
                    },
                ))),
                pull_request_review::Effect::NextChange => tasks.push(repository_task(
                    tab.runtime
                        .update(RepositoryAction::NextChange { card: id }),
                )),
                pull_request_review::Effect::PreviousChange => tasks.push(repository_task(
                    tab.runtime
                        .update(RepositoryAction::PreviousChange { card: id }),
                )),
                pull_request_review::Effect::CloseFile => tasks.push(repository_task(
                    tab.runtime
                        .update(RepositoryAction::BackToFiles { card: id }),
                )),
                pull_request_review::Effect::RevealFileDiscussion => tasks.push(repository_task(
                    tab.runtime
                        .update(RepositoryAction::RevealDiffTop { card: id }),
                )),
                pull_request_review::Effect::OpenUrl(url) => {
                    tasks.push(Self::open_external_link(url.to_string()));
                }
                // The shell keeps no session state for the review, so the
                // selection has nothing to update here.
                pull_request_review::Effect::SelectionChanged(_) => {}
                pull_request_review::Effect::Checkout(target) => checkout_target = Some(target),
            }
        }
        if let Some(target) = checkout_target {
            self.begin_checkout(target);
        }
        Task::batch(tasks)
    }

    fn focus_active_editor(&self) -> Task<Message> {
        let Some(id) = self.active else {
            return Task::none();
        };
        self.tab(id).map_or_else(Task::none, |tab| {
            tab.runtime.focus_editor(id).map(cosmic::Action::App)
        })
    }

    fn focus_find_query(&self) -> Task<Message> {
        iced::widget::operation::focus(widget::Id::new("ceditor.find.query"))
            .map(cosmic::Action::App)
    }

    /// Releases the search hold on every tab.
    ///
    /// A tab in search mode ignores every editor action except scrolling, so a
    /// tab that kept the hold after find moved elsewhere swallowed typing,
    /// paste and IME commits with no indication and no way back.
    fn clear_search_mode(&mut self) {
        for tab in &mut self.tabs {
            tab.runtime.set_search_mode(false);
        }
    }

    /// Closes the find bar and releases the search hold.
    ///
    /// Find targets one buffer, so a tab switch closes it rather than silently
    /// re-pointing the query and the match index at a different file.
    fn close_find(&mut self) {
        self.find_open = false;
        self.find_match_index = 0;
        self.find_match_total = 0;
        self.clear_search_mode();
    }

    /// Dismisses the palette and abandons the tree walk it started.
    fn close_quick_open(&mut self) {
        self.quick_open = false;
        self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
        self.quick_open_files = None;
        self.quick_open_index = 0;
        self.quick_open_cancel.cancel();
    }

    /// Selects one occurrence in the active tab and records the occurrence
    /// count, which is the only place it is ever computed.
    fn select_find_match(&mut self, occurrence: usize) {
        let Some(id) = self.active else {
            return;
        };
        let query = self.find_query.clone();
        let total = self.tab_mut(id).and_then(|tab| {
            tab.runtime.set_search_mode(true);
            tab.runtime.select_text_match(&query, occurrence)
        });
        match total {
            Some(total) => {
                self.find_match_index = occurrence % total;
                self.find_match_total = total;
            }
            None => {
                self.find_match_index = 0;
                self.find_match_total = 0;
            }
        }
    }

    /// The keys the find bar itself owns.
    ///
    /// Everything else returns `None` and falls through to the shortcut table:
    /// find is deliberately not modal, so Ctrl+S must still save while it is
    /// open. Navigation is Enter, Shift+Enter, Up and Down only — Left and
    /// Right belong to the query input, where they move the caret.
    fn handle_find_key(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            repeat: false,
            ..
        } = event
        else {
            return None;
        };
        match key {
            keyboard::Key::Named(keyboard::key::Named::Escape) => {
                Some(self.update(Message::CloseFind))
            }
            keyboard::Key::Named(keyboard::key::Named::Enter) => Some(if modifiers.shift() {
                self.update(Message::FindPrevious)
            } else {
                self.update(Message::FindNext)
            }),
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                Some(self.update(Message::FindPrevious))
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                Some(self.update(Message::FindNext))
            }
            _ => None,
        }
    }

    /// The keys the quick-open palette owns.
    ///
    /// Quick open counts as a modal, which blocks the shortcut table, so this
    /// runs before that guard. Held arrows repeat, as in any result list.
    fn handle_quick_open_key(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        let keyboard::Event::KeyPressed { key, .. } = event else {
            return None;
        };
        match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                Some(self.update(Message::QuickOpenNext))
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                Some(self.update(Message::QuickOpenPrevious))
            }
            keyboard::Key::Named(keyboard::key::Named::Enter) => {
                Some(self.update(Message::QuickOpenActivate))
            }
            _ => None,
        }
    }

    /// The rows the palette is showing, in display order.
    fn quick_open_results(&self) -> Vec<PathBuf> {
        let query = self.quick_open_query.trim().to_ascii_lowercase();
        self.current()
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
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Clamps the selection to the rows currently listed, so a shrinking
    /// result set never leaves the highlight off the end.
    fn quick_open_selection(&self, total: usize) -> usize {
        if self.quick_open_index >= total {
            total.saturating_sub(1)
        } else {
            self.quick_open_index
        }
    }

    fn reveal_quick_open_selection(&self, total: usize) -> Task<Message> {
        cagents::interaction::reveal_list_selection(
            widget::Id::new("ceditor.quick-open.results"),
            self.quick_open_selection(total),
            QUICK_OPEN_ROW_HEIGHT,
            QUICK_OPEN_ROW_SPACING,
        )
    }

    fn quick_open_search(&mut self, query: String, generation: u64) -> Task<Message> {
        // Abandon the walk the new keystroke supersedes before starting another.
        self.quick_open_cancel.cancel();
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
        let token = CancellationToken::new();
        self.quick_open_cancel = token.clone();
        let search_query = query.clone();
        cosmic::task::future(async move {
            // A recursive walk per keystroke queued concurrent 50k-entry scans
            // on the pool that also carries auto-save and file loads.
            tokio::time::sleep(QUICK_OPEN_SEARCH_DEBOUNCE).await;
            let result = if token.is_cancelled() {
                Ok(Vec::new())
            } else {
                tokio::task::spawn_blocking(move || {
                    repository_widget::search_directory_files_for_quick_open_cancellable(
                        &root,
                        &search_query,
                        &token,
                    )
                    .map(|files| {
                        files
                            .into_iter()
                            .filter(|file| !file.directory)
                            .map(|file| file.path)
                            .collect()
                    })
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
            };
            Message::QuickOpenSearchFinished {
                card: id,
                generation,
                query,
                result,
            }
        })
    }

    fn refuse_git_mutation(&mut self, card: CardId) -> Task<Message> {
        self.notice = Some(self.tab(card).and_then(|tab| tab.sidebar.source_control.as_ref()).and_then(SourceControlModel::mutation_unavailable_reason).or_else(WorktreeAuthority::unsupported_reason).unwrap_or("The original worktree binding is unavailable. Reopen the worktree before changing Git state.").into());
        Task::none()
    }

    /// A worktree may have several file tabs. Guard and refresh all of them,
    /// including unsaved buffers that are not currently visible.
    fn begin_mutation(&mut self, id: CardId, kind: MutationKind) -> Option<PathBuf> {
        self.begin_mutation_with_buffer_policy(id, kind, false, None)
    }

    fn begin_mutation_with_buffer_policy(&mut self, id: CardId, kind: MutationKind, index_only_stage: bool, queue_turn: Option<u64>) -> Option<PathBuf> {
        let tab = self.tab(id)?;
        if !tab.git {
            return None;
        }
        if let Some(reason) = tab.sidebar.source_control.as_ref().and_then(SourceControlModel::mutation_unavailable_reason) {
            self.notice = Some(reason.into());
            return None;
        }
        if tab.worktree_authority.is_none() {
            self.notice = Some(WorktreeAuthority::unsupported_reason().unwrap_or("The original worktree binding is unavailable. Reopen the worktree before changing Git state.").into());
            return None;
        }
        let root = tab.runtime.descriptor.worktree.clone();
        if self.git_groups.get(&root).is_some_and(|queue| (queue.active.is_some() || !queue.pending.is_empty())
            && !queue_turn.is_some_and(|operation| queue.active == Some(operation))) {
            self.notice = Some("An earlier Git action is still being prepared. Wait for its result before submitting another mutation.".into());
            return None;
        }
        if self.tabs.iter().any(|tab| {
            tab.runtime.descriptor.worktree == root
                && (tab.mutation.is_some() || (if index_only_stage { tab.runtime.is_saving() || tab.runtime.has_durability_warning() } else { tab.runtime.blocks_navigation() })
                    || tab.sidebar.source_control.as_ref().is_some_and(|source| source.pending_marker_stage.is_some()))
        }) {
            self.notice = Some("Save open files and wait for pending Git operations before changing this worktree.".into());
            return None;
        }
        let operation = self.next_group_operation;
        self.next_group_operation = self.next_group_operation.wrapping_add(1);
        let cancellation = CancellationToken::default();
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == root {
                tab.mutation_cancel = Some(cancellation.clone());
                tab.mutation_operation = operation;
                tab.mutation = Some(kind);
                if let Some(source) = tab.sidebar.source_control.as_mut() {
                    source.set_mutation(Some(kind));
                }
            }
        }
        Some(root)
    }

    pub(super) fn mutation_cancellation(&self, id: CardId) -> Option<CancellationToken> {
        self.tab(id)?.mutation_cancel.clone()
    }

    fn finish_mutation(&mut self, id: CardId, notice: Option<String>) -> Task<Message> {
        let Some(root) = self.tab(id).map(|tab| tab.runtime.descriptor.worktree.clone()) else {
            self.notice = notice;
            return Task::none();
        };
        self.finish_mutation_root(root, notice)
    }

    fn finish_mutation_root(&mut self, root: PathBuf, notice: Option<String>) -> Task<Message> {
        self.notice = notice.clone();
        if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
        let mut affected = Vec::new();
        for tab in &mut self.tabs {
            if tab.runtime.descriptor.worktree == root {
                tab.mutation = None;
                tab.mutation_cancel = None;
                tab.git_mutation_revision = tab.git_mutation_revision.wrapping_add(1);
                if let Some(source) = tab.sidebar.source_control.as_mut() {
                    source.set_mutation(None);
                    source.set_error(notice.clone());
                }
                affected.push(tab.id);
            }
        }
        if let Some(next) = self.start_next_change_group(&root) {
            return next;
        }
        if self.tabs.iter().any(|tab| tab.runtime.descriptor.worktree == root
            && tab.sidebar.source_control.as_ref().is_some_and(|source| source.pending_marker_stage.is_some())) {
            return Task::none();
        }
        let mut tasks = affected
            .into_iter()
            .map(|id| self.refresh(id))
            .collect::<Vec<_>>();
        if self.close_confirmed
            && let Some(closing) = self.closing
            && self
                .tab(closing)
                .is_some_and(|tab| tab.mutation.is_none() && !tab.runtime.blocks_navigation())
        {
            tasks.push(self.request_close(closing));
        }
        Task::batch(tasks)
    }
    fn stage_selection(
        &mut self,
        id: CardId,
        stage: bool,
        selection: Vec<PathSelection>,
    ) -> Task<Message> {
        self.enqueue_explicit_stage(id, stage, selection)
    }

    fn stage_selection_with_approval(
        &mut self,
        id: CardId,
        stage: bool,
        selection: Vec<PathSelection>,
        approved: BTreeMap<RepoPath, cagents::repository::ConflictMarkerVersion>,
        queue_turn: Option<u64>,
    ) -> Task<Message> {
        if selection.is_empty() {
            return Task::none();
        }
        let Some(authority) = self.tab(id).and_then(|tab| tab.worktree_authority.clone()) else {
            return self.refuse_git_mutation(id);
        };
        let kind = if stage {
            MutationKind::Stage
        } else {
            MutationKind::Unstage
        };
        let Some(root) = self.begin_mutation_with_buffer_policy(id, kind, false, queue_turn) else {
            if queue_turn.is_some() {
                if let Some(root) = self.tab(id).map(|tab| tab.runtime.descriptor.worktree.clone()) {
                    if let Some(queue) = self.git_groups.get_mut(&root) { queue.active = None; }
                    return self.resume_change_groups();
                }
            }
            return Task::none();
        };
        let operation = self.tab(id).unwrap().mutation_operation;
        let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
        let marker_authority = authority.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git").with_worktree_authority(authority)?;
                let token = cancellation;
                if stage {
                    let request = StageRequest { worktree: root, selection };
                    if let Some(markers) = service.check_stage_markers(&request, &approved, &token)? {
                        return Ok(Err((request.selection, markers)));
                    }
                    service.stage(&request, &token).map(Ok)
                } else {
                    service.unstage(&UnstageRequest { worktree: root, selection }, &token).map(Ok)
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
            match result {
                Ok(Err((selection, markers))) => Message::StageMarkers { card: id, operation, authority: marker_authority, selection, markers },
                Ok(Ok(outcome)) => Message::Mutation(id, Ok(outcome)),
                Err(error) => Message::Mutation(id, Err(error)),
            }
        })
    }
}

fn tab_strip(editor: &Editor) -> Element<'_, Message> {
    let tabs: Vec<Element<'_, Message>> =
        editor
            .tabs
            .iter()
            .map(|tab| {
                let selected = editor.active == Some(tab.id);
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
                cagents::interaction::hover(cagents::tab_strip::middle_click_close(
                    widget::container(
                        widget::row::with_children(vec![
                            cagents::interaction::tooltip(
                                cagents::reorder::handle(
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
                                    .id(widget::Id::new(format!("ceditor.tab.{}.title", tab.id)))
                                    .name(title.clone())
                                    .class(tab_title_style())
                                    .padding([0, 8])
                                    .width(Length::Fill)
                                    .height(Length::Fill)
                                    .on_press(Message::Select(tab.id)),
                                    tab.id,
                                ),
                                title,
                            ),
                            icon_button(Icon::Close, "Close tab")
                                .on_press_maybe(
                                    (!editor.modal_open()).then_some(Message::CloseTab(tab.id)),
                                )
                                .id(widget::Id::new(format!("ceditor.tab.{}.close", tab.id)))
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
                    Message::CloseTab(tab.id),
                ))
            })
            .collect();
    // Keep the tab strip on the same fixed baseline as the app header.
    let mut tab_row = tabs;
    tab_row.push(
        icon_button(Icon::Plus, "New tab (Ctrl+T)")
            .on_press_maybe((!editor.modal_open()).then_some(Message::NewTab))
            .id(widget::Id::new("ceditor.tabs.new"))
            .into(),
    );
    cagents::tab_strip::reorderable(
        widget::Id::new("ceditor.tabs"),
        tab_row,
        design::PRIMARY_CONTROL_SIZE,
        design::HEADER_HEIGHT,
        None,
        cagents::tab_strip::Reordering {
            order: editor.tabs.iter().map(|tab| tab.id).collect(),
            tab_width: 184.0,
            enabled: !editor.modal_open(),
            select: Box::new(Message::Select),
            dropped: Box::new(|card, before| Message::ReorderTab { card, before }),
        },
    )
    .into()
}

impl Application for Editor {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = crate::APPLICATION_ID;

    fn init(mut core: Core, flags: Flags) -> (Self, Task<Message>) {
        core.window.border_padding = Some(0);
        // This load creates and validates the private configuration directory
        // that every later preference write assumes exists. It is the only
        // place that happens: doing it again from a navigation-state save went
        // through the shared settings writer, which migrated settings.toml as
        // a side effect of reading it.
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
        let protection_paths = startup_paths
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if let Err(error) =
            cagents::worktree_cleanup::complete_standalone_editor_protection(&protection_paths)
        {
            notice = Some(format!("Cannot safely open editor folders yet: {error}"));
            startup_paths.clear();
        }
        let lsp = LspManager::new(settings.language_servers.clone());
        let line_height_draft = settings.terminal_line_height.to_string();
        let sidebar_open = saved_session.sidebar_open;
        let mut app = Self {
            core,
            tabs: Vec::new(),
            review_requests: Default::default(),
            notifications: Default::default(),
            active: None,
            selection_epoch: 0,
            git_groups: BTreeMap::new(),
            next_group_operation: 1,
            settings,
            shortcuts,
            shortcut_draft,
            settings_category: SettingsCategory::Editing,
            settings_search: String::new(),
            line_height_draft,
            window_size: iced::Size::new(1200.0, 900.0),
            tab_history: cagents::tab_history::TabHistory::default(),
            settings_reset_confirm: false,
            settings_path: flags.settings_path,
            settings_open: false,
            settings_writable,
            quick_open_files: None,
            quick_open_generation: 0,
            find_open: false,
            find_query: String::new(),
            find_match_index: 0,
            find_match_total: 0,
            quick_open: false,
            quick_open_query: String::new(),
            quick_open_index: 0,
            quick_open_cancel: CancellationToken::default(),
            notice,
            notices_open: false,
            file_dialog: None,
            checkout_dialog: None,
            checkout_journal: None,
            checkout_history: Vec::new(),
            checkout_history_open: false,
            checkout_error: None,
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
            document_tone_stale: false,
            sidebar_panel: WorktreePanel::Files,
            preference_lane: Arc::new(std::sync::Mutex::new([0; PreferenceFile::COUNT])),
            preference_generation: 0,
        };
        match cagents::notifications::ui::Observer::bind_editor(flags.notification_window.clone()) {
            Ok(mut observer) => {
                if flags.notification_window.is_some()
                    && let Err(error) = observer.start()
                {
                    app.notice = Some(error);
                }
                app.notifications.observer = observer;
            }
            Err(error) => app.notice = Some(format!("Review activation unavailable: {error}")),
        }
        let mut tasks = vec![
            app.notifications
                .observer
                .next()
                .map(|action| Message::Notification(notification_ui::Action::Observed(action)))
                .map(cosmic::Action::App),
            app.appearance(),
            app.lsp_next(),
            app.load_checkout_journal(),
        ];
        if let Some(window) = app.core.main_window_id() {
            tasks.push(app.set_window_title("ceditor".to_owned(), window));
        }
        if !startup_paths.is_empty() {
            tasks.push(Self::restore_session(startup_paths, app.settings.git_status_limit));
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

    /// The shell's own theme changed under us.
    ///
    /// Only the staleness is recorded, never the render: libcosmic discards
    /// whatever this returns when the notice arrives as a theme change rather
    /// than a mode change, so dispatching from here would leave a page
    /// waiting on a render that was never started.
    fn system_theme_update(
        &mut self,
        _keys: &[&'static str],
        _theme: &cosmic::cosmic_theme::Theme,
    ) -> Task<Message> {
        self.document_tone_stale = self.settings.app_theme == AppTheme::System;
        Task::none()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        if let Some(notice) = cagents::diagnostics::take_notice() {
            self.notice = Some(match self.notice.take() {
                Some(previous) => format!("{previous}\n{notice}"),
                None => notice.to_owned(),
            });
        }
        self.tab_history.observe(self.active);
        let previous_tabs = (self.active, self.tabs.len());
        let was_refresh_visible = self.can_refresh_visible();
        let navigation_changed = matches!(
            &message,
            Message::Opened(..)
                | Message::DocumentOpened(..)
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
            if self
                .current()
                .is_some_and(|tab| tab.mutation == Some(MutationKind::Discard))
                && matches!(&message, Message::Completion(..))
            {
                return Task::none();
            }
            if self.settings_open
                && let Message::Key(keyboard::Event::KeyPressed { key, modifiers, .. }) = &message
                && modifiers.control()
                && !modifiers.shift()
                && !modifiers.alt()
                && !modifiers.logo()
                && matches!(key, keyboard::Key::Character(value) if value.eq_ignore_ascii_case("f"))
            {
                return iced::widget::operation::focus(cagents::settings_widgets::search_id())
                    .map(cosmic::Action::App);
            }
            if let Message::Key(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                ..
            }) = &message
            {
                if self.pending_discard().is_some() {
                    return self.update(Message::Sidebar(SidebarAction::CancelDiscard));
                }
                if self.notices_open {
                    return self.close_notices();
                }
                if self.file_dialog.is_some() {
                    return self.update(Message::CancelFileOperation);
                }
                if self.checkout_dialog.is_some() || self.checkout_history_open {
                    return self.checkout_action(checkout::Action::Cancel);
                }
                if self.quick_open {
                    return self.update(Message::CloseQuickOpen);
                }
                if self.find_open {
                    return self.update(Message::CloseFind);
                }
            }
            // Quick open counts as a modal, and the guard below drops every
            // key while a modal is up. A palette whose whole point is staying
            // on the keyboard has to read its own keys first.
            if self.quick_open
                && let Message::Key(event) = &message
                && let Some(task) = self.handle_quick_open_key(event)
            {
                return task;
            }
            if self.modal_open()
                && !matches!(
                    &message,
                    Message::Sidebar(
                        SidebarAction::ConfirmDiscard { .. } | SidebarAction::CancelDiscard
                    )
                )
                && matches!(
                    &message,
                    Message::Choose(_)
                        | Message::Select(_)
                        | Message::ReorderTab { .. }
                        | Message::CloseTab(_)
                        | Message::Complete
                        | Message::Hover
                        | Message::Completion(..)
                        | Message::Jump(..)
                        | Message::Sidebar(_)
                        | Message::ToggleSidebar
                        | Message::NewTab
                        | Message::Key(_)
                )
            {
                return Task::none();
            }
            match message {
                Message::Notification(action) => self.notification_action(action),
                Message::StartupWindowOpened => {
                    cagents::diagnostics::record(cagents::diagnostics::Event::Ready);
                    let ready = if self.notifications.observer.is_bound() {
                        Ok(())
                    } else {
                        Err("the editor activation listener could not be opened".to_owned())
                    };
                    let _ = cagents::instance::report_ui_startup(&ready);
                    Task::none()
                }
                Message::Checkout(action) => self.checkout_action(action),
                Message::WindowResized(size) => {
                    self.window_size = size;
                    Task::none()
                }
                Message::WindowFocused(window, focused) => {
                    if self.core.main_window_id() == Some(window) {
                        self.window_focused = focused;
                        if !focused {
                            self.tab_history.finish(self.active);
                        }
                    }
                    self.notification_focus()
                }
                Message::RefreshVisible => {
                    // A colour-only theme change reaches the hook but not the
                    // mode watch, so the periodic refresh catches that one.
                    Task::batch([self.refresh_visible(), self.flush_document_tone()])
                }
                Message::DocumentToneStale => {
                    self.document_tone_stale = self.settings.app_theme == AppTheme::System;
                    self.flush_document_tone()
                }
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
                Message::Chosen(epoch, Ok(Some(path))) => Self::open(path, epoch, self.settings.git_status_limit),
                Message::DocumentOpened(epoch, fragment, result) => {
                    let opened = match result {
                        Ok(opened) if !self.exiting => opened,
                        Ok(_) => return Task::none(),
                        Err(error) => {
                            self.notice = Some(error);
                            return Task::none();
                        }
                    };
                    let path = opened.file.as_ref().map(|file| opened.root.join(file));
                    let task = self.add(opened, epoch == self.selection_epoch);
                    if epoch != self.selection_epoch {
                        return task;
                    }
                    let owner = path.as_ref().and_then(|path| self.file_owner(path));
                    let visible = owner == self.active && !self.modal_open();
                    let navigation =
                        owner
                            .and_then(|id| self.tab_mut(id))
                            .map_or_else(Task::none, |tab| {
                                tab.pending_fragment = fragment;
                                if visible && tab.runtime.editor_text().is_some() {
                                    tab.pending_fragment.take().map_or_else(
                                        Task::none,
                                        |fragment| {
                                            repository_task(
                                                tab.runtime.preview_fragment(tab.id, &fragment),
                                            )
                                        },
                                    )
                                } else {
                                    Task::none()
                                }
                            });
                    Task::batch([task, navigation])
                }
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
                Message::Select(id) => self.select_tab(id),
                Message::ReorderTab { card, before } => {
                    let order: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
                    let Some(destination) = cagents::reorder::destination(&order, card, before)
                    else {
                        return Task::none();
                    };
                    let source = order.iter().position(|id| *id == card).unwrap();
                    if source == destination {
                        return Task::none();
                    }
                    let tab = self.tabs.remove(source);
                    self.tabs.insert(destination, tab);
                    self.save_session()
                }
                Message::CloseTab(id) => self.request_close(id),
                Message::CloseWindow => {
                    if self
                        .checkout_dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.busy())
                    {
                        self.notice =
                            Some("Cancel or finish the worktree operation before closing.".into());
                        return Task::none();
                    }
                    // Shutdown removes runtime tabs to honor dirty-buffer guards;
                    // keep the user's pre-shutdown tab order and selection.
                    self.save_session_now();
                    if self.file_dialog.as_ref().is_some_and(|dialog| dialog.busy) {
                        self.exiting = true;
                        return Task::none();
                    }
                    self.file_dialog = None;
                    for tab in &mut self.tabs {
                        tab.navigation.apply(&SidebarAction::CancelDiscard);
                    }
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
                                if tab.mutation.is_none() {
                                    let _ = tab.runtime.abandon_unsaved_changes();
                                }
                            }
                            self.close_confirmed = true;
                            return self.request_close(id);
                        }
                        if self.tab_mut(id).is_some_and(|tab| {
                            tab.mutation.is_none() && tab.runtime.abandon_unsaved_changes()
                        }) {
                            return self.request_close(id);
                        }
                    }
                    Task::none()
                }
                Message::ConfirmClose => {
                    if let Some(id) = self.closing.filter(|id| {
                        self.tab(*id).is_some_and(|tab| {
                            tab.mutation.is_none() && !tab.runtime.blocks_navigation()
                        })
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
                Message::Repository(RepositoryAction::ApplyDiffHunk { card, kind, request }) => {
                    self.start_diff_patch(card, kind, request)
                }
                Message::Repository(RepositoryAction::Review { card, action }) => {
                    self.handle_review(card, action)
                }
                Message::Repository(action) => {
                    if let RepositoryAction::DiffAnnotations {
                        identity,
                        review_revision,
                        ..
                    } = &action
                        && !self
                            .tab(identity.card)
                            .is_some_and(|tab| tab.review.annotation_revision() == *review_revision)
                    {
                        return Task::none();
                    }
                    let id = action.card();
                    if (self.modal_open()
                        || self
                            .tab(id)
                            .is_some_and(|tab| tab.mutation == Some(MutationKind::Discard)))
                        && !repository_action_allowed_while_modal(&action)
                    {
                        return Task::none();
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
                    if let RepositoryAction::StageSelected {
                        identity,
                        stage,
                        selection,
                    } = action
                    {
                        if identity.card != id
                            || !self.tab(id).is_some_and(|tab| {
                                tab.runtime.accepts_staging(&identity, stage, &selection)
                            })
                        {
                            return Task::none();
                        }
                        return self.stage_selection(id, stage, vec![selection]);
                    }
                    if let RepositoryAction::RefreshFiles { .. } = action {
                        return self.refresh(id);
                    }
                    if let RepositoryAction::MarkdownLink { uri, .. } = &action {
                        let Some(source) = self.tab(id).and_then(|tab| {
                            tab.runtime
                                .selected_path()
                                .map(|path| tab.runtime.descriptor.worktree.join(path))
                        }) else {
                            return Task::none();
                        };
                        let uri = match cagents::document_link::resolve(&source, uri) {
                            Ok(cagents::document_link::DocumentLink::External(url)) => {
                                url.to_string()
                            }
                            Ok(cagents::document_link::DocumentLink::Local { path, fragment }) => {
                                self.selection_epoch = self.selection_epoch.wrapping_add(1);
                                let epoch = self.selection_epoch;
                                // Markdown-to-Markdown navigation stays in the preview, including
                                // fragmentless links. New tabs preserve the source's unsaved edits.
                                let fragment = Some(fragment.unwrap_or_default());
                                let status_limit = self.settings.git_status_limit;
                                return cosmic::task::future(async move {
                                    let result = tokio::task::spawn_blocking(move || {
                                        let mut opened = Self::resolve_open_with_limit(path.clone(), status_limit)?;
                                        if path.is_dir() {
                                            let directory = path
                                                .canonicalize()
                                                .map_err(|error| error.to_string())?;
                                            if directory != opened.root {
                                                opened.root = directory;
                                                opened.snapshot = None;
                                                opened.authority = None;
                                            }
                                        }
                                        Ok(opened)
                                    })
                                    .await
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result);
                                    Message::DocumentOpened(epoch, fragment, result)
                                });
                            }
                            Err(error) => {
                                self.notice = Some(error);
                                return Task::none();
                            }
                        };
                        return Self::open_external_link(uri);
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
                    if focus_loaded
                        && tab.runtime.editor_text().is_some()
                        && let Some(fragment) = tab.pending_fragment.take()
                    {
                        tasks.push(repository_task(tab.runtime.preview_fragment(id, &fragment)));
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
                                tab.mutation.is_none() && !tab.runtime.blocks_navigation()
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
                    // Sidebar menus share command identities with the ADE,
                    // while this host owns its file mutations and preferences.
                    let action = match action {
                        SidebarAction::RunChangeGroup { key, area, command, selection_revision } => {
                            let resolved = self.current().and_then(|tab| sidebar::resolve_captured_change_group(
                                &tab.sidebar, &tab.navigation, &key, area, command, selection_revision, self.modal_open(),
                            ));
                            let Some(action) = resolved else {
                                self.notice = Some("The original change group is no longer available.".into());
                                return Task::none();
                            };
                            action
                        }
                        SidebarAction::RunChangeCommand {
                            command,
                            key,
                            row,
                            selection_revision,
                        } => {
                            let target = sidebar::commands::CommandTarget::Changes {
                                key,
                                row,
                                selection_revision,
                            };
                            let resolved = self.current().and_then(|tab| {
                                target.resolve_in(&tab.sidebar, &tab.navigation, command)
                            });
                            let Some(action) = resolved else {
                                self.notice = Some(
                                    "The original change selection is no longer available.".into(),
                                );
                                return Task::none();
                            };
                            action
                        }
                        SidebarAction::RunCommand { command, action } => {
                            let valid = cagents::sidebar::commands::command_id(&action)
                                == Some(command)
                                && self.current().is_some_and(|tab| {
                                    let SidebarAction::FileCommand { key, path, .. } =
                                        action.as_ref()
                                    else {
                                        return false;
                                    };
                                    tab.navigation.active_worktree() == Some(key)
                                        && tab.sidebar.source_control.as_ref().is_some_and(
                                            |source| {
                                                source.key == *key
                                                    && !source.is_reloading()
                                                    && !source.busy()
                                                    && cagents::sidebar::commands::file_rows(source)
                                                        .any(|row| row.path == *path)
                                            },
                                        )
                                });
                            if !valid {
                                self.notice = Some(
                                    "The original file menu target is no longer available.".into(),
                                );
                                return Task::none();
                            }
                            *action
                        }
                        action => action,
                    };
                    if let SidebarAction::ChangeGroup { key, area, command } = action {
                        return self.enqueue_change_group(key, area, command);
                    }
                    // Both the captured menu and inline row controls resolve
                    // selection synchronously, then retain this host's existing
                    // save, mutation and discard-confirmation checks.
                    let action =
                        if let SidebarAction::InvokeChangeRows { key, row, command } = action {
                            let resolved = self.current().and_then(|tab| {
                                (tab.navigation.active_worktree() == Some(&key)).then_some(())?;
                                let source = tab.sidebar.source_control.as_ref()?;
                                if source.is_reloading() || source.busy() && command == sidebar::ChangeGroupCommand::Discard {
                                    return None;
                                }
                                sidebar::resolve_change_command(
                                    source,
                                    &tab.navigation,
                                    &key,
                                    &row,
                                    command,
                                )
                            });
                            let Some(action) = resolved else {
                                self.notice = Some(
                                    "The original change selection is no longer available.".into(),
                                );
                                return Task::none();
                            };
                            action
                        } else {
                            action
                        };
                    if matches!(action, SidebarAction::OpenSettings) {
                        return self.update(Message::Settings);
                    }
                    if self.modal_open()
                        && !matches!(
                            action,
                            SidebarAction::ConfirmDiscard { .. } | SidebarAction::CancelDiscard
                        )
                    {
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
                                return self.open_working_change(id, path, *area);
                            }
                            SidebarAction::SelectChangeRows { key, row, intent }
                                if self.tab(id).is_some_and(|tab| {
                                    tab.navigation.active_worktree() == Some(key)
                                }) =>
                            {
                                let tab = self.tab_mut(id).unwrap();
                                let Some(source) = tab.sidebar.source_control.as_ref() else {
                                    return Task::none();
                                };
                                let order =
                                    sidebar::change_selection_order(source, &tab.navigation);
                                tab.navigation
                                    .select_change_rows(row.clone(), *intent, &order);
                                return iced::widget::operation::focus(
                                    sidebar::change_row_focus_id(key, row),
                                )
                                .map(cosmic::Action::App);
                            }
                            SidebarAction::NavigateChangeRow {
                                key,
                                row,
                                direction,
                                extend,
                            } if self.tab(id).is_some_and(|tab| {
                                tab.navigation.active_worktree() == Some(key)
                            }) =>
                            {
                                let tab = self.tab_mut(id).unwrap();
                                let Some(source) = tab.sidebar.source_control.as_ref() else {
                                    return Task::none();
                                };
                                let order =
                                    sidebar::change_selection_order(source, &tab.navigation);
                                let Some(position) =
                                    order.iter().position(|candidate| candidate == row)
                                else {
                                    return Task::none();
                                };
                                use iced::keyboard::key::Named;
                                let next = match direction {
                                    Named::ArrowUp => position.saturating_sub(1),
                                    Named::ArrowDown => {
                                        (position + 1).min(order.len().saturating_sub(1))
                                    }
                                    Named::Home => 0,
                                    Named::End => order.len().saturating_sub(1),
                                    _ => return Task::none(),
                                };
                                let target = order[next].clone();
                                tab.navigation.select_change_rows(
                                    target.clone(),
                                    if *extend {
                                        sidebar::SelectionIntent::Range { extend: false }
                                    } else {
                                        sidebar::SelectionIntent::Navigate
                                    },
                                    &order,
                                );
                                let scroll = tab
                                    .navigation
                                    .reveal_change_row(source, &target)
                                    .map(|(id, y)| {
                                        iced::widget::operation::scroll_to(
                                            id,
                                            iced::widget::operation::AbsoluteOffset { x: 0.0, y },
                                        )
                                        .map(cosmic::Action::App)
                                    })
                                    .unwrap_or_else(Task::none);
                                return Task::batch([
                                    scroll,
                                    iced::widget::operation::focus(sidebar::change_row_focus_id(
                                        key, &target,
                                    ))
                                    .map(cosmic::Action::App),
                                ]);
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
                        | SidebarAction::SelectChangeRows { key, .. }
                        | SidebarAction::ClearChangeSelection { key }
                        | SidebarAction::NavigateChangeRow { key, .. }
                        | SidebarAction::RevealChangeInFiles { key, .. }
                        | SidebarAction::IgnoreChangePath { key, .. }
                        | SidebarAction::FileCommand { key, .. }
                        | SidebarAction::RefreshWorktree { key }
                        | SidebarAction::FilesQueryChanged { key, .. }
                        | SidebarAction::ToggleFilesFilter { key }
                        | SidebarAction::ToggleDirectory { key, .. }
                        | SidebarAction::ToggleSection { key, .. }
                        | SidebarAction::RequestDiscard { key, .. }
                        | SidebarAction::ConfirmDiscard { key, .. }
                        | SidebarAction::ConfirmStageMarkers { key, .. }
                        | SidebarAction::UndoDiscard { key }
                        | SidebarAction::ToggleGitOutput { key }
                        | SidebarAction::CancelGitMutation { key }
                        | SidebarAction::CommitMessageChanged { key, .. }
                        | SidebarAction::Commit { key, .. }
                        | SidebarAction::AmendChanged { key, .. }
                        | SidebarAction::GitOperation { key, .. }
                        | SidebarAction::ToggleStashes { key }
                        | SidebarAction::RequestStashDrop { key, .. }
                        | SidebarAction::Push { key, .. }
                        | SidebarAction::OpenPublish { key, .. }
                        | SidebarAction::PublishRemoteChanged { key, .. }
                        | SidebarAction::PublishBranchChanged { key, .. } => Some(key),
                        _ => None,
                    };
                    if key.is_some_and(|key| tab.navigation.active_worktree() != Some(key)) {
                        return Task::none();
                    }
                    if let SidebarAction::ConfirmDiscard { key, request } = &action {
                        // Only the exact request displayed by the mounted dialog
                        // may destroy files; stale or forged confirmations do nothing.
                        if !request.confirmed
                            || !tab.navigation.pending_discard().is_some_and(|pending| {
                                pending.key == *key
                                    && pending.request.worktree == request.worktree
                                    && pending.request.tracked == request.tracked
                                    && pending.request.untracked == request.untracked
                            })
                        {
                            return Task::none();
                        }
                    }
                    if let SidebarAction::RequestDiscard { request, .. } = &action
                        && (request.worktree != tab.runtime.descriptor.worktree
                            || tab.mutation.is_some()
                            || tab.sidebar.source_control.as_ref().is_some_and(|source| source.busy()))
                    {
                        return Task::none();
                    }
                    tab.navigation.apply(&action);
                    let persist_expansion = matches!(action, SidebarAction::ToggleDirectory { .. });
                    let task = match action {
                        SidebarAction::CancelGitMutation { .. } => self.cancel_git_mutations(id),
                        SidebarAction::ToggleGitOutput { .. } => {
                            if let Some(source) = tab.sidebar.source_control.as_mut() { source.output_expanded = !source.output_expanded; }
                            Task::none()
                        }
                        SidebarAction::UndoDiscard { .. } => {
                            let Some(recovery) = tab.discard_recovery.clone().filter(|recovery| recovery.can_undo()) else { return Task::none(); };
                            let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(id); };
                            if self.begin_mutation(id, MutationKind::Discard).is_none() { return Task::none(); }
                            let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
                            let expires_unix_ms = recovery.expires_unix_ms;
                            let paths = recovery.entries.iter().map(|entry| entry.path.clone()).collect();
                            cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || RepositoryService::system("git").with_worktree_authority(authority)?.undo_discard(&recovery, &cancellation))
                                    .await.map_err(|error| error.to_string()).and_then(|result| result.map_err(|error| error.to_string()));
                                Message::UndoMutationFinished { card: id, expires_unix_ms, paths, result }
                            })
                        }
                        SidebarAction::ConfirmStageMarkers { confirmed, .. } => {
                            if tab.mutation.is_some() { return Task::none(); }
                            let Some(source) = tab.sidebar.source_control.as_mut() else { return Task::none(); };
                            let selection = source.pending_marker_stage.take();
                            let approved = std::mem::take(&mut source.pending_marker_approval);
                            let authority = tab.marker_authority.take();
                            if !confirmed { return self.resume_change_groups(); }
                            if authority.is_none() {
                                self.notice = Some("The original worktree was replaced; staging was refused.".into());
                                return self.resume_change_groups();
                            }
                            let Some(selection) = selection else { return Task::none(); };
                            let Some(turn) = self.reserve_marker_turn(id) else { return Task::none(); };
                            self.stage_selection_with_approval(id, true, selection, approved, Some(turn))
                        }
                        SidebarAction::RequestDiscard { .. } => {
                            tab.discard_authority = tab.worktree_authority.clone();
                            tab.discard_queue_turn = None;
                            widget::text_input::focus(sidebar::discard_cancel_id())
                                .map(cosmic::Action::App)
                        }
                        SidebarAction::CancelDiscard => {
                            tab.discard_authority = None;
                            let turn = tab.discard_queue_turn.take();
                            let root = tab.runtime.descriptor.worktree.clone();
                            if let Some(queue) = self.git_groups.get_mut(&root)
                                && queue.active == turn
                            { queue.active = None; }
                            Task::batch([self.resume_change_groups(), self.resume_pending_files()])
                        },
                        SidebarAction::ConfirmDiscard { request, .. } => {
                            let Some(authority) = tab.discard_authority.take() else { return Task::none(); };
                            let turn = tab.discard_queue_turn.take();
                            let root = tab.runtime.descriptor.worktree.clone();
                            if self.begin_mutation_with_buffer_policy(id, MutationKind::Discard, false, turn).is_none() {
                                if let Some(queue) = self.git_groups.get_mut(&root)
                                    && queue.active == turn
                                { queue.active = None; }
                                return self.resume_change_groups();
                            }
                            let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
                            cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    RepositoryService::system("git")
                                        .with_worktree_authority(authority)?
                                        .discard_recoverable(&request, &cancellation)
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                                Message::PatchMutationFinished { card: id, result: result.map(|result| (result.outcome, Some(result.recovery))) }
                            })
                        }
                        SidebarAction::Commit { message, amend, .. } => {
                            let Some(authority) = tab.worktree_authority.clone() else {
                                return self.refuse_git_mutation(id);
                            };
                            let Some(worktree) = self.begin_mutation(id, MutationKind::Commit)
                            else {
                                return Task::none();
                            };
                            let cancellation = self
                                .mutation_cancellation(id)
                                .expect("admitted mutation has cancellation");
                            cosmic::task::future(async move {
                                let submitted = message.clone();
                                let result = tokio::task::spawn_blocking(move || {
                                    RepositoryService::system("git")
                                        .with_worktree_authority(authority)?
                                        .commit_with_amend(
                                            &CommitRequest { worktree, message },
                                            amend,
                                            &cancellation,
                                        )
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                                Message::Committed(id, submitted, result)
                            })
                        }
                        SidebarAction::ToggleStashes { .. } => {
                            if let Some(source) = tab.sidebar.source_control.as_mut() {
                                source.stashes_expanded = !source.stashes_expanded;
                            }
                            Task::none()
                        }
                        SidebarAction::RequestStashDrop { entry, .. } => {
                            if let Some(source) = tab.sidebar.source_control.as_mut() {
                                if entry
                                    .as_ref()
                                    .is_none_or(|entry| source.stashes.contains(entry))
                                {
                                    source.pending_stash_drop = entry;
                                }
                            }
                            Task::none()
                        }
                        SidebarAction::GitOperation { operation, .. } => {
                            let Some(source) = tab.sidebar.source_control.as_ref() else {
                                return Task::none();
                            };
                            if source.is_reloading() || source.busy() {
                                return Task::none();
                            }
                            if let cagents::repository::GitOperation::StashDrop {
                                reference,
                                expected_oid,
                            } = &operation
                            {
                                if !source.pending_stash_drop.as_ref().is_some_and(|entry| {
                                    &entry.reference == reference && &entry.oid == expected_oid
                                }) {
                                    self.notice =
                                        Some("Confirm the exact stash before dropping it.".into());
                                    return Task::none();
                                }
                            }
                            let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(id); };
                            let Some(worktree) = self.begin_mutation(id, MutationKind::Other)
                            else {
                                return Task::none();
                            };
                            if let Some(source) = self
                                .tab_mut(id)
                                .and_then(|tab| tab.sidebar.source_control.as_mut())
                            {
                                source.pending_stash_drop = None;
                            }
                            let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
                            cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    RepositoryService::system("git").with_worktree_authority(authority)?.git_operation(
                                        &worktree,
                                        &operation,
                                        &cancellation,
                                    )
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                                Message::Mutation(id, result)
                            })
                        }
                        SidebarAction::Push { branch, target, .. } => {
                            let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(id); };
                            let Some(worktree) = self.begin_mutation(id, MutationKind::Push) else {
                                return Task::none();
                            };
                            let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
                            cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    RepositoryService::system("git").with_worktree_authority(authority)?.push(
                                        &PushRequest {
                                            worktree,
                                            branch,
                                            target,
                                        },
                                        &cancellation,
                                    )
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                                Message::Pushed(id, result)
                            })
                        }
                        SidebarAction::FileCommand { path, command, .. } => {
                            match command {
                                FileCommand::RevealInFileManager => {
                                    if cagents::device::is_remote() {
                                        self.notice = Some("Reveal in file manager is unavailable on remote devices.".into());
                                        return Task::none();
                                    }
                                    let path = tab.runtime.descriptor.worktree.join(path.as_path());
                                    cosmic::task::future(async move {
                                        Message::FileRevealed(
                                            cagents::file_manager::reveal(&path).await,
                                        )
                                    })
                                }
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
                            }
                        }
                        SidebarAction::RevealChangeInFiles { path, .. } => {
                            tab.navigation.reveal_in_files(path.clone());
                            self.sidebar_panel = WorktreePanel::Files;
                            self.refresh_snapshot(id, true)
                        }
                        SidebarAction::IgnoreChangePath { path, .. } => {
                            let eligible =
                                tab.sidebar.source_control.as_ref().is_some_and(|source| {
                                    source
                                        .group(cagents::repository::ChangeArea::Untracked)
                                        .is_some_and(|group| {
                                            group.rows().iter().any(|row| row.entry.path == path)
                                        })
                                });
                            if !eligible {
                                return Task::none();
                            }
                            let Some(authority) = tab.worktree_authority.clone() else { return self.refuse_git_mutation(id); };
                            let Some(root) = self.begin_mutation(id, MutationKind::Other) else {
                                return Task::none();
                            };
                            let cancellation = self.mutation_cancellation(id).expect("admitted mutation has cancellation");
                            cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    RepositoryService::system("git").with_worktree_authority(authority)?.git_operation(
                                        &root,
                                        &cagents::repository::GitOperation::IgnorePath { path },
                                        &cancellation,
                                    )
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result.map_err(|error| error.to_string()));
                                Message::Mutation(id, result)
                            })
                        }
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
                        return Task::batch([task, self.save_session()]);
                    }
                    task
                }
                Message::PullRequestReview(id, action) => self.handle_review(id, action),
                Message::ReviewRequests(action) => {
                    let repositories = self
                        .tabs
                        .iter()
                        .filter_map(|tab| tab.review.review_request_repository().cloned())
                        .collect::<Vec<_>>();
                    let task = self
                        .review_requests
                        .update(action, repositories)
                        .map(Message::ReviewRequests)
                        .map(cosmic::Action::App);
                    let report = self.report_review_requests();
                    for tab in &mut self.tabs {
                        tab.review.clear_external_review_requests();
                        for (repository, snapshot) in self.review_requests.snapshots() {
                            tab.review.apply_review_requests(repository, snapshot);
                        }
                    }
                    Task::batch([task, report])
                }
                Message::RevealedInFiles {
                    card,
                    key,
                    requested,
                    result,
                } => {
                    let reveal_visible = self.active == Some(card)
                        && self.sidebar_open
                        && self.sidebar_panel == WorktreePanel::Files
                        && !self.modal_open();
                    let Some(tab) = self.tab_mut(card) else {
                        return Task::none();
                    };
                    if !tab.snapshot_loading {
                        return Task::none();
                    }
                    tab.snapshot_loading = false;
                    if tab.mutation.is_some() {
                        tab.snapshot_refresh_pending = true;
                        return Task::none();
                    }
                    if std::mem::take(&mut tab.snapshot_refresh_pending) {
                        return self.refresh_snapshot(card, false);
                    }
                    if tab.navigation.active_worktree() != Some(&key)
                        || tab.navigation.files_reveal_demand() != Some(&requested)
                    {
                        return Task::none();
                    }
                    match result {
                        Ok((snapshot, target)) => {
                            let mut source = SourceControlModel::ready(key, &snapshot);
                            source.revealed_path = target;
                            if let Some(previous) = tab.sidebar.source_control.as_ref() {
                                previous.carry_mutation_into(&mut source);
                            }
                            tab.navigation.reconcile_change_rows(&source);
                            let focus = tab
                                .navigation
                                .finish_reveal_in_files(&source)
                                .filter(|_| reveal_visible);
                            tab.change_source_revision = tab.change_source_revision.wrapping_add(1);
                            tab.sidebar.source_control = Some(source);
                            let root = tab.runtime.descriptor.worktree.clone();
                            let sourced = tab
                                .review
                                .set_source(review_source(&root, Some(&snapshot)))
                                .map(move |action| Message::PullRequestReview(card, action))
                                .map(cosmic::Action::App);
                            Task::batch([
                                sourced,
                                self.apply_review_effects(card),
                                focus.map_or_else(Task::none, |id| {
                                    iced::widget::operation::focus(id).map(cosmic::Action::App)
                                }),
                            ])
                        }
                        Err(error) => {
                            self.notice =
                                Some(format!("Could not reveal the requested path: {error}"));
                            Task::none()
                        }
                    }
                }
                Message::Snapshot(id, result) => {
                    if let Some(tab) = self.tab_mut(id) {
                        if !tab.snapshot_loading {
                            return Task::none();
                        }
                        tab.snapshot_loading = false;
                        if tab.mutation.is_some() {
                            tab.snapshot_refresh_pending = true;
                            return Task::none();
                        }
                        if std::mem::take(&mut tab.snapshot_refresh_pending) {
                            // A mutation completed during this read. Discard its
                            // stale result and run exactly one fresh snapshot.
                            return self.refresh_snapshot(id, false);
                        }
                        match result {
                            Ok(snapshot) => {
                                if let Some(key) = tab.navigation.active_worktree().cloned() {
                                    let mut source = SourceControlModel::ready(key, &snapshot);
                                    if let Some(previous) = tab.sidebar.source_control.as_ref() {
                                        previous.carry_mutation_into(&mut source);
                                    }
                                    if let Some(reason) = WorktreeAuthority::unsupported_reason() { source.set_mutation_unavailable_reason(Some(Arc::from(reason))); }
                                    source.set_mutation(tab.mutation);
                                    tab.navigation.reconcile_change_rows(&source);
                                    tab.change_source_revision = tab.change_source_revision.wrapping_add(1);
                                    tab.sidebar.source_control = Some(source);
                                }
                                // A remote added or retargeted since the last
                                // read changes which repository is reviewed.
                                let root = tab.runtime.descriptor.worktree.clone();
                                let sourced = tab
                                    .review
                                    .set_source(review_source(&root, Some(&snapshot)))
                                    .map(move |action| Message::PullRequestReview(id, action))
                                    .map(cosmic::Action::App);
                                return Task::batch([sourced, self.apply_review_effects(id)]);
                            }
                            Err(error) => self.notice = Some(error),
                        }
                    }
                    Task::none()
                }
                Message::Discarded(id, paths, result) => {
                    // Status refresh deliberately preserves buffers. Discard is
                    // different: the selected files were restored or removed on
                    // disk, so reload their clean buffers in every sibling tab.
                    // Do this even for an uncertain outcome; disk is authoritative.
                    let root = self
                        .tab(id)
                        .map(|tab| tab.runtime.descriptor.worktree.clone());
                    let affected = self
                        .tabs
                        .iter()
                        .filter(|tab| {
                            Some(&tab.runtime.descriptor.worktree) == root.as_ref()
                                && !tab.runtime.blocks_navigation()
                                && tab.runtime.selected_path().is_some_and(|selected| {
                                    paths.iter().any(|path| path.as_path() == selected)
                                })
                        })
                        .map(|tab| tab.id)
                        .collect::<Vec<_>>();
                    let mut tasks = vec![self.update(Message::Mutation(id, result))];
                    for id in affected {
                        tasks.push(
                            self.update(Message::Repository(RepositoryAction::Reload { card: id })),
                        );
                    }
                    Task::batch(tasks)
                }
                Message::StageMarkers { card, operation, authority, selection, markers } => {
                    let Some(tab) = self.tab_mut(card).filter(|tab| tab.mutation_operation == operation && tab.mutation == Some(MutationKind::Stage)) else { return Task::none(); };
                    tab.marker_authority = Some(authority);
                    if let Some(source) = tab.sidebar.source_control.as_mut() {
                        if source.pending_marker_stage.is_some() { return self.finish_mutation(card, Some("An earlier marker confirmation is still pending.".into())); }
                        source.pending_marker_stage = Some(selection);
                        source.pending_marker_approval = markers;
                    }
                    self.finish_mutation(card, Some("Conflict markers remain; confirm Stage anyway or cancel.".into()))
                }
                Message::ChangeGroupResolved { request, result } => self.finish_change_group(request, result),
                Message::DiffPatchPrepared { origin, result } => self.finish_diff_patch(origin, result),
                Message::UndoMutationFinished { card, expires_unix_ms, paths, result } => {
                    if matches!(result, Ok(MutationOutcome::Applied { .. })) {
                        if let Some(root) = self.tab(card).map(|tab| tab.runtime.descriptor.worktree.clone()) {
                            for tab in &mut self.tabs {
                                if tab.runtime.descriptor.worktree == root && tab.discard_recovery.as_ref().is_some_and(|recovery| recovery.expires_unix_ms == expires_unix_ms) {
                                    tab.discard_recovery = None;
                                    if let Some(source) = tab.sidebar.source_control.as_mut() { source.undo_discard_until_unix_ms = None; }
                                }
                            }
                        }
                    }
                    self.update(Message::Discarded(card, paths, result))
                },
                Message::PatchMutationFinished { card, result } => {
                    match result {
                        Ok((outcome, recovery)) => {
                            let paths = recovery.as_ref().map(|recovery| recovery.entries.iter().map(|entry| entry.path.clone()).collect()).unwrap_or_default();
                            if let Some(recovery) = recovery.filter(|recovery| recovery.can_undo()) {
                                let root = recovery.worktree.clone();
                                let recovery = Arc::new(recovery);
                                for tab in &mut self.tabs {
                                    if tab.runtime.descriptor.worktree == root {
                                        tab.discard_recovery = Some(recovery.clone());
                                        if let Some(source) = tab.sidebar.source_control.as_mut() { source.undo_discard_until_unix_ms = Some(recovery.expires_unix_ms); }
                                    }
                                }
                            }
                            self.update(Message::Discarded(card, paths, Ok(outcome)))
                        }
                        Err(error) => self.update(Message::Mutation(card, Err(error))),
                    }
                },
                Message::Mutation(id, result) => {
                    let notice = match result {
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
                    self.finish_mutation(id, notice)
                }
                Message::Committed(id, submitted, result) => {
                    let notice = match result {
                        Ok(CommitOutcome::Created { warnings, .. }) => {
                            if let Some(tab) = self.tab_mut(id)
                                && tab.navigation.commit_message() == submitted
                            {
                                tab.navigation.clear_commit_message();
                            }
                            (!warnings.is_empty()).then(|| {
                                format!(
                                    "Commit created, but {}",
                                    warnings
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join("; ")
                                )
                            })
                        }
                        Ok(CommitOutcome::NotCreated { failure }) => {
                            Some(format!("Commit failed: {failure}"))
                        }
                        Ok(CommitOutcome::OutcomeUnknown { failure }) => Some(format!(
                            "Commit may or may not have been created: {failure}. Refresh to see what Git holds."
                        )),
                        Err(error) => Some(format!("Commit could not run: {error}")),
                    };
                    self.finish_mutation(id, notice)
                }
                Message::Pushed(id, result) => {
                    let notice = match result {
                        Ok(PushOutcome::Pushed { .. }) => None,
                        Ok(PushOutcome::NotPushed { failure }) => {
                            Some(format!("Push failed: {failure}"))
                        }
                        Ok(PushOutcome::OutcomeUnknown { failure }) => Some(format!(
                            "Push may or may not have reached the remote: {failure}. Refresh to see what Git holds."
                        )),
                        Err(error) => Some(format!("Push could not run: {error}")),
                    };
                    self.finish_mutation(id, notice)
                }
                Message::Settings => {
                    if self.closing.is_none()
                        && self.file_dialog.is_none()
                        && self.pending_discard().is_none()
                    {
                        self.notices_open = false;
                        self.settings_open = !self.settings_open;
                        if self.settings_open {
                            return self.save_settings();
                        }
                    }
                    self.resume_pending_files()
                }
                Message::SelectSettingsCategory(category) => {
                    self.settings_search.clear();
                    self.settings_reset_confirm = false;
                    self.settings_category = category;
                    cagents::settings_widgets::reveal_content_start()
                }
                Message::SettingsSearchChanged(query) => {
                    self.settings_search = query;
                    cagents::settings_widgets::reveal_content_start()
                }
                Message::ResetSettings => {
                    if !self.settings_writable {
                        return Task::none();
                    }
                    if !self.settings_reset_confirm {
                        self.settings_reset_confirm = true;
                        return Task::none();
                    }
                    self.settings_reset_confirm = false;
                    self.set_ui_scale(AppSettings::default().ui_scale);
                    self.settings = AppSettings::default();
                    self.shortcuts = crate::preferences::EditorShortcuts::default();
                    self.shortcut_draft = self.shortcuts.clone();
                    for tab in &mut self.tabs {
                        tab.runtime.set_auto_save(self.settings.editor_auto_save);
                        tab.runtime.set_wrap_lines(self.settings.editor_wrap_lines);
                        tab.runtime
                            .set_line_height(self.settings.terminal_line_height);
                        tab.runtime
                            .set_syntax_theme(self.settings.editor_syntax_theme);
                    }
                    let shortcuts =
                        self.capture_preference(PreferenceWrite::Shortcuts(self.shortcuts.clone()));
                    Task::batch([
                        self.set_language_server_assistance(false),
                        self.appearance(),
                        self.save_settings(),
                        preference_task(shortcuts),
                    ])
                }
                Message::ShortcutChanged(action, value) => {
                    self.shortcut_draft.set(action, value);
                    match self.shortcut_draft.validate() {
                        Ok(()) => {
                            self.shortcuts = self.shortcut_draft.clone();
                            if self.settings_writable {
                                let shortcuts = self.shortcuts.clone();
                                return preference_task(
                                    self.capture_preference(PreferenceWrite::Shortcuts(shortcuts)),
                                );
                            }
                        }
                        Err(error) => self.notice = Some(error),
                    }
                    Task::none()
                }
                Message::FindInFile => {
                    if self.find_open {
                        self.close_find();
                        return self.focus_active_editor();
                    }
                    self.find_open = true;
                    self.select_find_match(0);
                    self.focus_find_query()
                }
                Message::FindQueryChanged(query) => {
                    self.find_query = query;
                    self.select_find_match(0);
                    Task::none()
                }
                Message::FindNext => {
                    self.select_find_match(self.find_match_index.wrapping_add(1));
                    self.focus_find_query()
                }
                Message::FindPrevious => {
                    // Wrapping to `usize::MAX` and letting the search take it
                    // modulo the total only lands on the last match when the
                    // total divides 2^64: for 3 or 5 matches it re-selected the
                    // first, for 7 the second.
                    let previous = self
                        .find_match_index
                        .checked_sub(1)
                        .unwrap_or_else(|| self.find_match_total.saturating_sub(1));
                    self.select_find_match(previous);
                    self.focus_find_query()
                }
                Message::CloseFind => {
                    self.close_find();
                    self.focus_active_editor()
                }
                Message::QuickOpen => {
                    let opening = !self.quick_open;
                    self.close_quick_open();
                    if !opening {
                        return Task::none();
                    }
                    self.quick_open = true;
                    self.quick_open_query.clear();
                    iced::widget::operation::focus(widget::Id::new("ceditor.quick-open.query"))
                        .map(cosmic::Action::App)
                        .chain(self.reveal_quick_open_selection(self.quick_open_results().len()))
                }
                Message::QuickOpenQueryChanged(query) => {
                    self.quick_open_query = query.clone();
                    self.quick_open_files = None;
                    // A new query means a new result list: start at its top.
                    self.quick_open_index = 0;
                    self.quick_open_generation = self.quick_open_generation.wrapping_add(1);
                    Task::batch([
                        self.quick_open_search(query, self.quick_open_generation),
                        self.reveal_quick_open_selection(self.quick_open_results().len()),
                    ])
                }
                Message::QuickOpenNext => {
                    let total = self.quick_open_results().len();
                    if total > 0 {
                        self.quick_open_index = (self.quick_open_selection(total) + 1) % total;
                    }
                    self.reveal_quick_open_selection(total)
                }
                Message::QuickOpenPrevious => {
                    let total = self.quick_open_results().len();
                    if total > 0 {
                        self.quick_open_index = self
                            .quick_open_selection(total)
                            .checked_sub(1)
                            .unwrap_or(total - 1);
                    }
                    self.reveal_quick_open_selection(total)
                }
                Message::QuickOpenActivate => {
                    let results = self.quick_open_results();
                    let selected = self.quick_open_selection(results.len());
                    let Some(path) = results.into_iter().nth(selected) else {
                        return Task::none();
                    };
                    self.update(Message::OpenQuickFile(path))
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
                        self.reveal_quick_open_selection(self.quick_open_results().len())
                    } else {
                        Task::none()
                    }
                }
                Message::OpenQuickFile(path) => {
                    let Some(id) = self.active else {
                        return Task::none();
                    };
                    self.close_quick_open();
                    self.quick_open_query.clear();
                    self.request_file(id, path)
                }
                Message::CloseQuickOpen => {
                    self.close_quick_open();
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
                    self.save_session()
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
                Message::LineHeightChanged(value) => {
                    self.line_height_draft = value.clone();
                    let Ok(line_height) = value.parse::<f32>() else {
                        return Task::none();
                    };
                    if !(1.0..=2.0).contains(&line_height) {
                        return Task::none();
                    }
                    self.settings.terminal_line_height = line_height;
                    self.line_height_draft = line_height.to_string();
                    for tab in &mut self.tabs {
                        tab.runtime.set_line_height(line_height);
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
                    self.set_ui_scale(scale);
                    Task::batch([self.save_settings(), self.appearance()])
                }
                Message::FileRevealed(result) => {
                    self.notice = match result {
                        Ok(cagents::file_manager::RevealOutcome::Selected) => {
                            Some("Revealed in file manager".into())
                        }
                        Ok(cagents::file_manager::RevealOutcome::OpenedContainingFolder) => {
                            Some("Opened containing folder in file manager".into())
                        }
                        Err(error) => Some(error),
                    };
                    Task::none()
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
                                LspEvent::Diagnostics { items, version, .. } => {
                                    tab.diagnostics = items;
                                    tab.diagnostics_version = Some(version);
                                }
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
                                    tab.diagnostics_version = None;
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
                    // The diagnostics list is this message's only producer.
                    // Diagnostics are unsolicited and replace each other
                    // wholesale, so the rows on screen can describe text the
                    // buffer has moved past; jumping would land the caret on a
                    // line the diagnostic never named.
                    if !self.diagnostics_are_current(id) {
                        self.notice = Some(
                            "these diagnostics describe an earlier version of the file".into(),
                        );
                        return Task::none();
                    }
                    if let Some(tab) = self.tab_mut(id) {
                        tab.runtime.move_cursor_to(at);
                        return tab.runtime.focus_editor(id).map(cosmic::Action::App);
                    }
                    Task::none()
                }
                Message::Key(event) => {
                    if cagents::tab_history::released(&event) {
                        self.tab_history.finish(self.active);
                    }
                    if !self.modal_open()
                        && let Some(backwards) = cagents::tab_history::step_direction(&event)
                    {
                        let available: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
                        self.tab_history.retain(|id| available.contains(&id));
                        if let Some(id) = self.tab_history.step(&available, self.active, backwards)
                        {
                            let select = self.select_tab(id);
                            return Task::batch([select, self.focus_active_editor()]);
                        }
                        return Task::none();
                    }
                    if self.find_open
                        && let Some(task) = self.handle_find_key(&event)
                    {
                        return task;
                    }
                    if let Some(task) = self.handle_shortcut(&event) {
                        return task;
                    }
                    if let keyboard::Event::KeyPressed {
                        key,
                        physical_key,
                        modifiers,
                        repeat: false,
                        ..
                    } = &event
                        && !self.modal_open()
                        && is_completion_chord(key, *physical_key, *modifiers)
                    {
                        return self.update(Message::Complete);
                    }
                    Task::none()
                }
            }
        })();
        // Find owns one buffer. If the active tab changed under it, the
        // outgoing tab would keep the search hold that makes its editor ignore
        // every edit, recoverable only by closing the tab.
        let find_closed = previous_tabs.0 != self.active && self.find_open;
        if find_closed {
            self.close_find();
        }
        let session = if navigation_changed || previous_tabs != (self.active, self.tabs.len()) {
            self.save_session()
        } else {
            Task::none()
        };
        let tabs_changed = previous_tabs != (self.active, self.tabs.len());
        let refresh =
            self.can_refresh_visible() && (previous_tabs.0 != self.active || !was_refresh_visible);
        Task::batch([
            task,
            self.resume_change_groups(),
            self.notification_focus(),
            self.open_requested_review(),
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
            session,
            if find_closed {
                // The find input just vanished; hand the keyboard to the
                // buffer the user switched to.
                self.focus_active_editor()
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
            iced::Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized(size)),
            iced::Event::Window(window::Event::Unfocused) => {
                Some(Message::WindowFocused(window, false))
            }
            _ => None,
        });
        let refresh = if self.can_refresh_visible() || self.tabs.iter().any(|tab| tab.discard_recovery.is_some()) {
            iced::time::every(std::time::Duration::from_secs(10)).map(|_| Message::RefreshVisible)
        } else {
            Subscription::none()
        };
        let frames = if self.sidebar_transition.is_some() {
            window::frames().map(|(_, now)| Message::Frame(now))
        } else {
            Subscription::none()
        };
        let review_requests = if self
            .tabs
            .iter()
            .any(|tab| tab.review.review_request_repository().is_some())
        {
            iced::time::every(Duration::from_secs(10)).map(|_| {
                Message::ReviewRequests(pull_request_review::request_ui::monitor::Action::Tick)
            })
        } else {
            Subscription::none()
        };
        Subscription::batch([
            events,
            refresh,
            frames,
            review_requests,
            self.document_tone_subscription(),
        ])
    }

    fn header_start(&self) -> Vec<Element<'_, Message>> {
        vec![
            icon_button(
                Icon::Sidebar,
                if self.sidebar_open {
                    "Hide sidebar"
                } else {
                    "Show sidebar"
                },
            )
            .selected(self.sidebar_open)
            .on_press_maybe((!self.modal_open()).then_some(Message::ToggleSidebar))
            .into(),
            icon_button(Icon::OpenFile, "Open file (Ctrl+O)")
                .on_press_maybe((!self.modal_open()).then_some(Message::Choose(false)))
                .into(),
            icon_button(Icon::OpenFolder, "Open folder (Ctrl+Shift+O)")
                .on_press_maybe((!self.modal_open()).then_some(Message::Choose(true)))
                .into(),
        ]
    }
    fn header_end(&self) -> Vec<Element<'_, Message>> {
        self.notices_control().into_iter().collect()
    }
    fn header_center(&self) -> Vec<Element<'_, Message>> {
        vec![tab_strip(self)]
    }
    fn view(&self) -> Element<'_, Message> {
        let mut body = Vec::new();
        // The COSMIC header above has no separator of its own: always rule
        // off the content below it, tabs or placeholder alike.
        body.push(design::horizontal_rule().into());
        if let Some(tab) = self.current() {
            let mut editor = Vec::new();
            let mut toolbar_controls = Vec::new();
            if let Some(toolbar) = repository_widget::selected_file_toolbar(tab.id, &tab.runtime) {
                toolbar_controls.push(
                    widget::container(toolbar.map(Message::Repository))
                        .width(Length::FillPortion(1))
                        .into(),
                );
            } else {
                toolbar_controls.push(iced::widget::Space::new().width(Length::Fill).into());
            }
            if tab.review.is_open()
                && let Some(control) = tab
                    .review
                    .file_comment_control(self.modal_open(), tab.runtime.can_show_file_discussion())
            {
                toolbar_controls.push(
                    toolbar_actions(vec![control])
                        .map(move |action| Message::PullRequestReview(tab.id, action)),
                );
            }
            if tab.runtime.lsp_document().is_some() {
                toolbar_controls.push(
                    toolbar_actions(vec![
                        icon_button(Icon::InsertText, "Complete (Ctrl+Space)")
                            .on_press_maybe((!self.modal_open()).then_some(Message::Complete))
                            .into(),
                        icon_button(Icon::Info, "Show symbol information")
                            .on_press_maybe((!self.modal_open()).then_some(Message::Hover))
                            .into(),
                    ])
                    .into(),
                );
            }
            toolbar_controls.push(
                icon_button(Icon::Search, "Find in file (Ctrl+F)")
                    .selected(self.find_open)
                    .on_press_maybe((!self.modal_open()).then_some(Message::FindInFile))
                    .into(),
            );
            if !toolbar_controls.is_empty() {
                editor.push(
                    widget::container(
                        widget::row::with_children(toolbar_controls)
                            .spacing(design::TOOLBAR_GROUP_SPACING)
                            .align_y(iced::Alignment::Center)
                            .width(Length::Fill),
                    )
                    // The file toolbar carries the header's leading inset
                    // itself, so this row adds none: a second inset here put
                    // the file name at 20px while everything below it started
                    // at 12. Only the trailing edge needs one, because the
                    // controls this shell appends past the toolbar have no
                    // container of their own to provide it.
                    .padding(iced::Padding {
                        right: design::TOOLBAR_INSET,
                        ..iced::Padding::ZERO
                    })
                    .center_y(Length::Fill)
                    .height(design::HEADER_CONTENT_HEIGHT)
                    .width(Length::Fill)
                    .into(),
                );
                editor.push(design::horizontal_rule().into());
            }
            if self.find_open {
                editor.push(find_bar(self));
                editor.push(design::horizontal_rule().into());
            }
            editor.push(
                repository_widget::view_with_review(
                    tab.id,
                    &tab.runtime,
                    tab.review.is_open().then_some(&tab.review),
                )
                .map(Message::Repository),
            );
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
                let presented = cagents::inspector::diagnostic_rows(
                    &tab.diagnostics,
                    cagents::inspector::LSP_ROW_LIMIT,
                );
                let stale = !presented.rows.is_empty() && !self.diagnostics_are_current(tab.id);
                let withheld = presented.withheld;
                for row in presented.rows {
                    results.push(
                        widget::button::custom(
                            widget::column::with_children(vec![
                                design::caption(row.label).into(),
                                design::body(row.message)
                                    .wrapping(iced::widget::text::Wrapping::Word)
                                    .into(),
                            ])
                            .spacing(2),
                        )
                        .class(cosmic::theme::Button::Text)
                        .width(Length::Fill)
                        .on_press(Message::Jump(tab.id, row.at))
                        .into(),
                    );
                }
                if withheld > 0 {
                    results.push(
                        design::caption(cagents::inspector::withheld_diagnostics_label(withheld))
                            .into(),
                    );
                }
                if stale {
                    // Say so where the rows are, rather than only refusing the
                    // jump once the user has already clicked one.
                    results.push(
                        design::caption(
                            "these diagnostics describe an earlier version of the file",
                        )
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                    );
                }
                editor.push(design::horizontal_rule().into());
                editor.push(
                    widget::scrollable(
                        widget::column::with_children(results)
                            .spacing(4)
                            .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                    )
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .height(150)
                    .into(),
                );
            }
            let mut workspace = Vec::new();
            let progress = self.sidebar_progress();
            if progress > 0.0 {
                let width = self.sidebar_width();
                let panel = if tab.git {
                    self.sidebar_panel
                } else {
                    WorktreePanel::Files
                };
                let sidebar = if panel == WorktreePanel::PullRequests {
                    let id = tab.id;
                    sidebar::worktree_sidebar_with_body(
                        &tab.sidebar,
                        &tab.navigation,
                        width,
                        self.modal_open(),
                        panel,
                        tab.git,
                        review_panel(&tab.review, self.modal_open())
                            .map(move |action| Message::PullRequestReview(id, action)),
                        self.settings_footer(),
                        Message::Sidebar,
                    )
                } else {
                    sidebar::worktree_sidebar_with_footer(
                        &tab.sidebar,
                        &tab.navigation,
                        width,
                        self.modal_open(),
                        panel,
                        tab.git,
                        self.settings_footer(),
                        Message::Sidebar,
                    )
                };
                workspace.push(cagents::interaction::reveal_leading_width(
                    sidebar, width, progress,
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
                    design::horizontal_rule().into(),
                    self.settings_footer(),
                ])
                .height(Length::Fill)
                .into(),
            );
        }
        cagents::interaction::reserve_tab_history(
            widget::column::with_children(body).height(Length::Fill),
            !self.modal_open(),
        )
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        if self.checkout_dialog.is_some() || self.checkout_history_open {
            return self.checkout_view();
        }
        if let Some(pending) = self.pending_discard() {
            return Some(dialog_overlay(
                sidebar::discard_confirmation_dialog(pending).map(Message::Sidebar),
                Message::Sidebar(SidebarAction::CancelDiscard),
            ));
        }
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
            let ready =
                !tab.runtime.blocks_navigation() && tab.mutation.is_none() && !untitled_note;
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
                if (tab.runtime.can_abandon_unsaved_changes() || untitled_note)
                    && tab.mutation.is_none()
                {
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
                        widget::text::body(tab.runtime.save_error().unwrap_or(
                            if tab.mutation.is_some() {
                                "A Git operation is still running."
                            } else if untitled_note {
                                "This note has not been saved to a file yet."
                            } else if ready {
                                "The tab is ready to close."
                            } else {
                                "This file has unsaved changes or an unfinished save."
                            },
                        ))
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
            let files = self.quick_open_results();
            let selected = self.quick_open_selection(files.len());
            let mut rows: Vec<Element<'_, Message>> = files
                .into_iter()
                .enumerate()
                .map(|(index, path)| {
                    let label = path.to_string_lossy().into_owned();
                    let highlighted = index == selected;
                    widget::button::text(label)
                        .height(QUICK_OPEN_ROW_HEIGHT)
                        .class(if highlighted {
                            cosmic::theme::Button::Suggested
                        } else {
                            cosmic::theme::Button::Text
                        })
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
                        widget::scrollable(
                            widget::column::with_children(rows)
                                .spacing(QUICK_OPEN_ROW_SPACING)
                                // Every result button fills the row, so the
                                // thumb would sit on top of its background.
                                .padding(design::with_vertical_rail(iced::Padding::ZERO)),
                        )
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .id(widget::Id::new("ceditor.quick-open.results"))
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
        Some(self.settings_dialog(self.window_size))
    }
}

impl Editor {
    fn settings_dialog(&self, size: iced::Size) -> Element<'_, Message> {
        let categories: Vec<_> = if self.settings_search.trim().is_empty() {
            vec![self.settings_category]
        } else {
            SettingsCategory::ALL.to_vec()
        };
        let mut results = Vec::new();
        for category in categories {
            let controls = match category {
                SettingsCategory::General => vec![settings_group(
                    "General",
                    "Preferences for the standalone editor.",
                    vec![
                        settings_field(
                            "Settings file",
                            "Your editor preferences are independent from workbench settings.",
                            design::body(self.settings_path.to_string_lossy()).into(),
                        ),
                        settings_field(
                            "Reset all settings",
                            "Restore default editor preferences and keyboard shortcuts. Open files and saved tabs are preserved.",
                            widget::button::standard(if self.settings_reset_confirm {
                                "Confirm reset all settings"
                            } else {
                                "Reset all settings"
                            })
                            .on_press(Message::ResetSettings)
                            .into(),
                        ),
                    ],
                )],
                SettingsCategory::Editing => {
                    let mut controls = cagents::settings_widgets::editor_options(
                        self.settings.editor_auto_save,
                        self.settings.editor_wrap_lines,
                        self.settings.editor_syntax_theme,
                        Message::AutoSave,
                        Message::WrapLines,
                        Message::SyntaxTheme,
                    );
                    controls.push(settings_field(
                        "Line height",
                        "Relative spacing between editor lines. Use a value from 1.0 to 2.0.",
                        widget::text_input("Line height", &self.line_height_draft)
                            .on_input(Message::LineHeightChanged)
                            .into(),
                    ));
                    vec![settings_group(
                        "Editor",
                        "Shared native editor and diff preferences.",
                        controls,
                    )]
                }
                SettingsCategory::Appearance => vec![settings_group(
                    "Appearance",
                    "Application colors and interface scale.",
                    vec![
                        settings_field(
                            "App theme",
                            "Colors for the entire application. Syntax palettes are independent.",
                            widget::dropdown(
                                AppTheme::ALL
                                    .iter()
                                    .map(|theme| theme.label().to_owned())
                                    .collect::<Vec<_>>(),
                                AppTheme::ALL
                                    .iter()
                                    .position(|theme| *theme == self.settings.app_theme),
                                |index| Message::Theme(AppTheme::ALL[index]),
                            )
                            .into(),
                        ),
                        settings_field(
                            "Interface scale",
                            "Size of text and controls.",
                            widget::row::with_children(vec![
                                icon_button(Icon::Minus, "Decrease scale")
                                    .on_press_maybe((self.settings.ui_scale > 0.75).then_some(
                                        Message::Scale((self.settings.ui_scale - 0.1).max(0.75)),
                                    ))
                                    .into(),
                                widget::container(design::body(format!(
                                    "{:.0}%",
                                    self.settings.ui_scale * 100.0
                                )))
                                .center_x(64)
                                .center_y(design::PRIMARY_CONTROL_SIZE)
                                .into(),
                                icon_button(Icon::Plus, "Increase scale")
                                    .on_press_maybe((self.settings.ui_scale < 2.0).then_some(
                                        Message::Scale((self.settings.ui_scale + 0.1).min(2.0)),
                                    ))
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
                        shortcut_setting(
                            "Mark reviewed file viewed",
                            "Toggles the open pull request file's viewed mark and moves to the next file. Leave empty to unbind.",
                            EditorShortcut::ReviewToggleViewed,
                            &self.shortcut_draft.review_toggle_viewed,
                        ),
                        shortcut_setting(
                            "Next reviewed file",
                            "Opens the next changed file of the open pull request.",
                            EditorShortcut::ReviewNextFile,
                            &self.shortcut_draft.review_next_file,
                        ),
                        shortcut_setting(
                            "Previous reviewed file",
                            "Opens the previous changed file of the open pull request.",
                            EditorShortcut::ReviewPreviousFile,
                            &self.shortcut_draft.review_previous_file,
                        ),
                        shortcut_setting(
                            "Next unviewed file",
                            "Opens the next unviewed file of the open pull request.",
                            EditorShortcut::ReviewNextUnviewed,
                            &self.shortcut_draft.review_next_unviewed,
                        ),
                        shortcut_setting(
                            "Next changed hunk",
                            "Opens the next changed hunk of the open pull request.",
                            EditorShortcut::ReviewNextChange,
                            &self.shortcut_draft.review_next_change,
                        ),
                        shortcut_setting(
                            "Previous changed hunk",
                            "Opens the previous changed hunk of the open pull request.",
                            EditorShortcut::ReviewPreviousChange,
                            &self.shortcut_draft.review_previous_change,
                        ),
                        // Fixed, not configurable: the completion request is wired
                        // to the chord directly rather than through the shortcut
                        // table, so list it read-only instead of leaving it
                        // undiscoverable.
                        settings_field(
                            "Request completions",
                            "Ask the language server for completions at the caret.",
                            design::body("Ctrl+Space").into(),
                        ),
                    ],
                )],
                SettingsCategory::LanguageServers => vec![settings_group(
                    "Language servers",
                    "Semantic diagnostics, completion, and hover. Off by default.",
                    vec![settings_field(
                        "Language-server assistance",
                        "Semantic diagnostics, completion, and hover. Configure language_servers in the preferences file, then restart ceditor.",
                        widget::toggler(self.language_server_assistance)
                            .label("Enable language servers".to_owned())
                            .on_toggle(Message::LanguageServerAssistance)
                            .into(),
                    )],
                )],
            };
            let query =
                if cagents::settings_widgets::matches(category.label(), &self.settings_search) {
                    ""
                } else {
                    &self.settings_search
                };
            results.extend(
                controls
                    .into_iter()
                    .filter_map(|group| group.view(category.label(), query)),
            );
        }
        if results.is_empty() {
            results.push(design::body("No matching settings").into());
        }
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
        let navigation = widget::column::with_children(vec![
            widget::text_input("Search", &self.settings_search)
                .id(cagents::settings_widgets::search_id())
                .on_input(Message::SettingsSearchChanged)
                .into(),
            navigation.into(),
        ])
        .padding(8)
        .width(200)
        .height(Length::Fill);
        let body = widget::row::with_children(vec![
            navigation.into(),
            design::vertical_rule().into(),
            widget::scrollable(
                widget::container(widget::column::with_children(results).spacing(20))
                    .padding([4.0 * design::GRID_UNIT, 5.0 * design::GRID_UNIT])
                    .width(Length::Fill)
                    .max_width(620),
            )
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .id(cagents::settings_widgets::content_id())
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        ])
        .width(Length::Fill)
        .height(Length::Fill);
        let header = widget::container(widget::row::with_children(vec![
            widget::text::title3("Editor settings")
                .width(Length::Fill)
                .into(),
            icon_button(Icon::Close, "Close settings")
                .on_press(Message::Settings)
                .into(),
        ]))
        .padding([2.0 * design::GRID_UNIT, 4.0 * design::GRID_UNIT])
        .align_y(iced::Alignment::Center)
        .height(design::HEADER_HEIGHT);
        dialog_overlay(
            connected_dialog_surface(
                widget::column::with_children(vec![
                    header.into(),
                    design::horizontal_rule().into(),
                    body.into(),
                ])
                .spacing(0)
                // Keep a margin around the editor's modal at every window size.
                .height(Length::Fixed(size.height * 0.87)),
                900.0,
            ),
            Message::Settings,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cagents::repository_widget::AsyncIdentity;

    fn fixture_git_command(root: &Path, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new("git");
        for (name, _) in std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_")) { command.env_remove(name); }
        command.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
            .current_dir(root).args(args);
        command
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = fixture_git_command(root, args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git_fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        git(
            root,
            &[
                "config",
                "core.hooksPath",
                root.join("hooks").to_str().unwrap(),
            ],
        );
        std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "Initial"]);
        directory
    }

    #[test]
    fn pr_checkout_confirms_a_pinned_commit_and_keeps_the_original_worktree() {
        use cagents::pull_request_review::CheckoutTarget;
        let repository = git_fixture();
        let root = repository.path();
        let data = tempfile::tempdir().unwrap();
        let destination = data.path().join("checkouts/review");
        let original = git(root, &["rev-parse", "HEAD"]);
        let mut app = editor();
        app.settings_path = data.path().join("editor/settings.toml");
        let task = app.load_checkout_journal();
        run_tasks(&mut app, task);
        assert!(app.checkout_journal.is_some(), "{:?}", app.notice);
        app.begin_checkout(CheckoutTarget {
            repository: root.to_path_buf(),
            number: 17,
            head: cagents::repository::Oid::parse(&original).unwrap(),
        });
        drop(app.update(Message::Checkout(checkout::Action::Destination(
            destination.display().to_string(),
        ))));
        let task = app.update(Message::Checkout(checkout::Action::Resolve));
        run_tasks(&mut app, task);
        assert!(!destination.exists(), "confirmation is read-only");
        std::fs::write(root.join("tracked.txt"), "new head\n").unwrap();
        git(root, &["commit", "-am", "Move main after confirmation"]);
        let moved = git(root, &["rev-parse", "HEAD"]);
        let task = app.update(Message::Checkout(checkout::Action::Create));
        run_tasks(&mut app, task);
        assert_eq!(git(&destination, &["rev-parse", "HEAD"]), original);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), moved);
        assert_eq!(
            std::fs::read_to_string(destination.join("tracked.txt")).unwrap(),
            "original\n"
        );
        let task = app.update(Message::Checkout(checkout::Action::Create));
        run_tasks(&mut app, task);
        assert_eq!(git(&destination, &["rev-parse", "HEAD"]), original);
        // A crash can lose the completion UI after Git and the durable journal
        // have committed. Reopening exposes the existing result without replay.
        let mut reopened = editor();
        reopened.settings_path = app.settings_path.clone();
        let task = reopened.load_checkout_journal();
        run_tasks(&mut reopened, task);
        let report = reopened
            .checkout_history
            .iter()
            .find(|r| {
                r.state == cagents::repository::WorktreeOperationState::Created
                    && r.request
                        .as_ref()
                        .is_some_and(|r| r.destination == destination)
            })
            .expect("completed checkout survives restart");
        let id = report.operation_id;
        drop(reopened.update(Message::Checkout(checkout::Action::History)));
        assert!(reopened.checkout_history_open);
        let task = reopened.update(Message::Checkout(checkout::Action::OpenRecorded(id)));
        run_tasks(&mut reopened, task);
        assert!(!reopened.checkout_history_open);
        assert!(
            reopened
                .tabs
                .iter()
                .any(|tab| tab.runtime.descriptor.worktree == destination)
        );
        assert_eq!(git(&destination, &["rev-parse", "HEAD"]), original);
    }

    #[test]
    fn checkout_journal_failure_remains_visible_in_the_dialog() {
        let mut app = editor();
        drop(
            app.update(Message::Checkout(checkout::Action::JournalLoaded(Err(
                "journal path is not writable".into(),
            )))),
        );
        app.begin_checkout(cagents::pull_request_review::CheckoutTarget {
            repository: PathBuf::from("/review"),
            number: 1,
            head: cagents::repository::Oid::parse(&"a".repeat(40)).unwrap(),
        });
        assert_eq!(
            app.checkout_error.as_deref(),
            Some("journal path is not writable")
        );
        assert!(app.checkout_view().is_some());
        assert!(app.checkout_journal.is_none());
    }

    /// Execute actual application tasks, including mutation completion and
    /// subsequent snapshot/file refreshes, without a window or network remote.
    fn run_tasks(app: &mut Editor, task: Task<Message>) {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut pending = vec![task];
                while let Some(task) = pending.pop() {
                    if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                        while let Some(action) = stream.next().await {
                            if let iced::runtime::Action::Output(cosmic::Action::App(message)) =
                                action
                            {
                                pending.push(app.update(message));
                            }
                        }
                    }
                }
            });
    }

    fn open_real_git(app: &mut Editor, root: &Path, file: Option<&str>) -> CardId {
        let snapshot = RepositoryService::system("git")
            .snapshot(root, &CancellationToken::default())
            .unwrap();
        let task = app.add(
            Opened {
                root: root.to_path_buf(),
                file: file.map(PathBuf::from),
                snapshot: Some(snapshot),
                authority: WorktreeAuthority::capture(root).ok(),
            },
            true,
        );
        run_tasks(app, task);
        app.active.unwrap()
    }

    fn sidebar_action(app: &mut Editor, action: SidebarAction) {
        let task = app.update(Message::Sidebar(action));
        run_tasks(app, task);
    }

    #[test]
    fn git_actions_commit_publish_and_push_through_the_sidebar() {
        let directory = git_fixture();
        let root = directory.path();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);
        git(
            root,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::CommitMessageChanged {
                key: key.clone(),
                message: "Keep my message".into(),
            },
        );
        sidebar_action(
            &mut app,
            SidebarAction::Commit {
                key: key.clone(),
                message: "Keep my message".into(),
                amend: false,
            },
        );
        assert!(app.notice.as_deref().unwrap().contains("Commit failed"));
        assert_eq!(
            app.tab(id).unwrap().navigation.commit_message(),
            "Keep my message"
        );

        std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        sidebar_action(
            &mut app,
            SidebarAction::Stage {
                key: key.clone(),
                selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
            },
        );
        let task = app.update(Message::Sidebar(SidebarAction::Commit {
            key: key.clone(),
            message: "Keep my message".into(),
            amend: false,
        }));
        assert!(app.tab(id).unwrap().mutation.is_some());
        assert!(
            app.tab(id)
                .unwrap()
                .sidebar
                .source_control
                .as_ref()
                .unwrap()
                .busy()
        );
        run_tasks(&mut app, task);
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(git(root, &["log", "-1", "--format=%s"]), "Keep my message");
        assert!(app.tab(id).unwrap().navigation.commit_message().is_empty());
        assert!(app.tab(id).unwrap().mutation.is_none());

        let branch = cagents::repository::ValidatedBranchName::parse("main").unwrap();
        sidebar_action(
            &mut app,
            SidebarAction::OpenPublish {
                key: key.clone(),
                branch: branch.clone(),
                remotes: vec!["origin".into()],
            },
        );
        let publish = app
            .tab(id)
            .unwrap()
            .navigation
            .publish_draft()
            .unwrap()
            .push_action()
            .unwrap();
        sidebar_action(&mut app, publish);
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(
            git(remote.path(), &["rev-parse", "refs/heads/main"]),
            git(root, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
            "origin/main"
        );

        std::fs::write(root.join("tracked.txt"), "pushed again\n").unwrap();
        sidebar_action(
            &mut app,
            SidebarAction::Stage {
                key: key.clone(),
                selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
            },
        );
        sidebar_action(
            &mut app,
            SidebarAction::Commit {
                key: key.clone(),
                message: "Second".into(),
                amend: false,
            },
        );
        let target = app
            .tab(id)
            .unwrap()
            .sidebar
            .source_control
            .as_ref()
            .unwrap()
            .upstream
            .clone()
            .unwrap();
        sidebar_action(
            &mut app,
            SidebarAction::Push {
                key,
                branch,
                target,
            },
        );
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(
            git(remote.path(), &["rev-parse", "refs/heads/main"]),
            git(root, &["rev-parse", "HEAD"])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn standalone_commit_and_amend_cancel_the_running_hook() {
        use std::os::unix::fs::PermissionsExt;

        for amend in [false, true] {
            let directory = git_fixture();
            let root = directory.path();
            std::fs::write(root.join("tracked.txt"), "staged change\n").unwrap();
            git(root, &["add", "tracked.txt"]);
            let before_head = git(root, &["rev-parse", "HEAD"]);
            let before_index = git(root, &["diff", "--cached", "--binary"]);
            let hooks = root.join("hooks");
            std::fs::create_dir_all(&hooks).unwrap();
            let hook = hooks.join("pre-commit");
            std::fs::write(
                &hook,
                b"#!/bin/sh\n: > \"$0.started\"\nwhile [ ! -e \"$0.release\" ]; do sleep 0.01; done\n",
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            let started = hooks.join("pre-commit.started");
            let release = hooks.join("pre-commit.release");
            let mut app = editor();
            let id = open_real_git(&mut app, root, None);
            let key = app
                .tab(id)
                .unwrap()
                .navigation
                .active_worktree()
                .unwrap()
                .clone();
            let message = "Keep cancelled commit draft";
            sidebar_action(
                &mut app,
                SidebarAction::CommitMessageChanged {
                    key: key.clone(),
                    message: message.into(),
                },
            );
            let task = app.update(Message::Sidebar(SidebarAction::Commit {
                key: key.clone(),
                message: message.into(),
                amend,
            }));
            let cancellation = app.mutation_cancellation(id).expect("commit was admitted");
            let worker = std::thread::spawn(move || task_messages(task));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !started.exists() && !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let hook_started = started.exists();
            drop(app.update(Message::Sidebar(SidebarAction::CancelGitMutation { key })));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let settled_before_release = worker.is_finished();
            // Release even a broken adapter's hook before asserting, so the
            // regression cannot leave its worker or Git process waiting.
            std::fs::write(release, b"release\n").unwrap();
            let messages = worker.join().unwrap();
            for message in messages {
                let task = app.update(message);
                run_tasks(&mut app, task);
            }
            assert!(hook_started, "the actual commit must reach its hook");
            assert!(
                cancellation.is_cancelled(),
                "Cancel uses the admitted token"
            );
            assert!(
                settled_before_release,
                "Cancel must stop Git while its hook is blocked"
            );
            assert_eq!(git(root, &["rev-parse", "HEAD"]), before_head);
            assert_eq!(git(root, &["diff", "--cached", "--binary"]), before_index);
            assert_eq!(app.tab(id).unwrap().navigation.commit_message(), message);
            assert!(app.tab(id).unwrap().mutation.is_none());
            assert!(app.notice.is_some(), "the cancelled result remains visible");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn standalone_commit_and_amend_reject_a_replaced_root_before_worker_start() {
        for amend in [false, true] {
            let directory = git_fixture();
            let replacement = git_fixture();
            let root = directory.path();
            std::fs::write(root.join("tracked.txt"), "original staged change\n").unwrap();
            std::fs::write(
                replacement.path().join("tracked.txt"),
                "replacement staged change\n",
            )
            .unwrap();
            git(root, &["add", "tracked.txt"]);
            git(replacement.path(), &["add", "tracked.txt"]);
            let original_head = git(root, &["rev-parse", "HEAD"]);
            let original_index = git(root, &["diff", "--cached", "--binary"]);
            let replacement_head = git(replacement.path(), &["rev-parse", "HEAD"]);
            let replacement_index = git(replacement.path(), &["diff", "--cached", "--binary"]);
            let mut app = editor();
            let id = open_real_git(&mut app, root, None);
            let key = app
                .tab(id)
                .unwrap()
                .navigation
                .active_worktree()
                .unwrap()
                .clone();
            let message = "Keep replaced-root draft";
            sidebar_action(
                &mut app,
                SidebarAction::CommitMessageChanged {
                    key: key.clone(),
                    message: message.into(),
                },
            );
            let delayed = app.update(Message::Sidebar(SidebarAction::Commit {
                key,
                message: message.into(),
                amend,
            }));
            assert!(app.tab(id).unwrap().mutation.is_some());
            let parked = root.with_extension("commit-original");
            std::fs::rename(root, &parked).unwrap();
            std::fs::rename(replacement.path(), root).unwrap();
            run_tasks(&mut app, delayed);
            let replacement_after_head = git(root, &["rev-parse", "HEAD"]);
            let replacement_after_index = git(root, &["diff", "--cached", "--binary"]);
            std::fs::remove_dir_all(root).unwrap();
            std::fs::rename(parked, root).unwrap();
            assert_eq!(replacement_after_head, replacement_head);
            assert_eq!(replacement_after_index, replacement_index);
            assert_eq!(git(root, &["rev-parse", "HEAD"]), original_head);
            assert_eq!(git(root, &["diff", "--cached", "--binary"]), original_index);
            assert_eq!(app.tab(id).unwrap().navigation.commit_message(), message);
            assert!(app.tab(id).unwrap().mutation.is_none());
            assert!(
                app.notice.is_some(),
                "the replaced-root refusal remains visible"
            );
        }
    }

    #[test]
    fn shared_git_controls_execute_stash_and_amend_with_exact_worktree_and_confirmation() {
        use cagents::repository::GitOperation;
        let directory = git_fixture();
        let root = directory.path();
        let original_head = git(root, &["rev-parse", "HEAD"]);
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::Commit {
                key: key.clone(),
                message: "Amended through shared controls".into(),
                amend: true,
            },
        );
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(
            git(root, &["log", "-1", "--format=%s"]),
            "Amended through shared controls"
        );
        assert_ne!(git(root, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(
            git(root, &["rev-list", "--count", "HEAD"]),
            "1",
            "amend replaces the commit"
        );

        std::fs::write(root.join("tracked.txt"), "stash me\n").unwrap();
        std::fs::write(root.join("new.txt"), "new file\n").unwrap();
        sidebar_action(
            &mut app,
            SidebarAction::GitOperation {
                key: key.clone(),
                operation: GitOperation::StashPush {
                    message: Some("shared control".into()),
                    include_untracked: true,
                },
            },
        );
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert!(!root.join("new.txt").exists());
        let stash = app
            .tab(id)
            .unwrap()
            .sidebar
            .source_control
            .as_ref()
            .unwrap()
            .stashes[0]
            .clone();
        let mut stale = key.clone();
        stale.canonical_path = directory.path().join("replacement");
        sidebar_action(
            &mut app,
            SidebarAction::GitOperation {
                key: stale,
                operation: GitOperation::StashApply {
                    reference: stash.reference.clone(),
                    expected_oid: stash.oid.clone(),
                    pop: false,
                },
            },
        );
        assert!(
            !root.join("new.txt").exists(),
            "stale worktree cannot apply a stash"
        );
        sidebar_action(
            &mut app,
            SidebarAction::GitOperation {
                key: key.clone(),
                operation: GitOperation::StashApply {
                    reference: stash.reference.clone(),
                    expected_oid: stash.oid.clone(),
                    pop: false,
                },
            },
        );
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "new file\n"
        );
        sidebar_action(
            &mut app,
            SidebarAction::GitOperation {
                key: key.clone(),
                operation: GitOperation::StashDrop {
                    reference: stash.reference.clone(),
                    expected_oid: stash.oid.clone(),
                },
            },
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Confirm the exact stash")
        );
        assert_eq!(
            git(root, &["stash", "list", "--format=%H"]),
            stash.oid.to_string()
        );
        sidebar_action(
            &mut app,
            SidebarAction::RequestStashDrop {
                key: key.clone(),
                entry: Some(stash.clone()),
            },
        );
        sidebar_action(
            &mut app,
            SidebarAction::GitOperation {
                key,
                operation: GitOperation::StashDrop {
                    reference: stash.reference,
                    expected_oid: stash.oid,
                },
            },
        );
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert!(git(root, &["stash", "list"]).is_empty());
    }

    #[test]
    fn git_actions_discard_requires_the_mounted_confirmation_and_escape_cancels() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        let request = cagents::repository::DiscardRequest {
            worktree: root.to_path_buf(),
            tracked: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
            untracked: vec![RepoPath::parse("new.txt").unwrap()],
            confirmed: false,
        };
        sidebar_action(
            &mut app,
            SidebarAction::ConfirmDiscard {
                key: key.clone(),
                request: cagents::repository::DiscardRequest {
                    confirmed: true,
                    ..request.clone()
                },
            },
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        sidebar_action(
            &mut app,
            SidebarAction::RequestDiscard {
                key: key.clone(),
                request: request.clone(),
            },
        );
        assert!(app.modal_open());
        assert!(app.dialog().is_some());
        drop(app.update(Message::NewTab));
        drop(app.update(Message::Settings));
        assert_eq!(app.tabs.len(), 1);
        assert!(!app.settings_open);
        drop(app.update(Message::Key(named_key(
            keyboard::key::Named::Escape,
            keyboard::Modifiers::empty(),
        ))));
        assert!(!app.modal_open());
        assert!(root.join("new.txt").exists());

        sidebar_action(&mut app, SidebarAction::RequestDiscard { key, request });
        let confirm = app.pending_discard().unwrap().confirm_action();
        sidebar_action(&mut app, confirm);
        assert!(!app.modal_open());
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "original\n"
        );
        assert!(!root.join("new.txt").exists());
        assert!(
            !app.tab(id)
                .unwrap()
                .sidebar
                .source_control
                .as_ref()
                .unwrap()
                .has_changes()
        );
    }

    #[test]
    fn git_actions_reject_stale_keys_and_dirty_buffers_in_other_tabs() {
        let directory = git_fixture();
        let root = directory.path();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let dirty = open_real_git(&mut app, root, Some("tracked.txt"));
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: dirty,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("unsaved".into()),
                )),
            })),
        );
        assert!(app.tab(dirty).unwrap().runtime.is_dirty());
        let other = open_real_git(&mut app, root, None);
        assert_ne!(dirty, other);
        let stale = app
            .tab(dirty)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        let key = app
            .tab(other)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::CommitMessageChanged {
                key: stale,
                message: "wrong tab".into(),
            },
        );
        assert!(
            app.tab(other)
                .unwrap()
                .navigation
                .commit_message()
                .is_empty()
        );
        sidebar_action(
            &mut app,
            SidebarAction::Commit {
                key,
                message: "blocked".into(),
                amend: false,
            },
        );
        assert!(app.notice.as_deref().unwrap().contains("Save open files"));
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
        assert_eq!(git(root, &["log", "-1", "--format=%s"]), "Initial");
        assert!(app.tab(dirty).unwrap().runtime.is_dirty());
        let key = app
            .tab(other)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::RequestDiscard {
                key,
                request: cagents::repository::DiscardRequest {
                    worktree: root.to_path_buf(),
                    tracked: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
                    untracked: Vec::new(),
                    confirmed: false,
                },
            },
        );
        let confirm = app.pending_discard().unwrap().confirm_action();
        sidebar_action(&mut app, confirm);
        assert!(app.notice.as_deref().unwrap().contains("Save open files"));
        assert!(app.tab(dirty).unwrap().runtime.is_dirty());
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "original\n"
        );
    }

    #[test]
    fn git_actions_discard_serializes_sibling_tabs_and_refreshes_their_buffers() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let file = open_real_git(&mut app, root, Some("tracked.txt"));
        let folder = open_real_git(&mut app, root, None);
        let key = app
            .tab(folder)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        sidebar_action(
            &mut app,
            SidebarAction::RequestDiscard {
                key: key.clone(),
                request: cagents::repository::DiscardRequest {
                    worktree: root.to_path_buf(),
                    tracked: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
                    untracked: Vec::new(),
                    confirmed: false,
                },
            },
        );
        let confirm = app.pending_discard().unwrap().confirm_action();
        let task = app.update(Message::Sidebar(confirm));
        assert!(
            app.tabs
                .iter()
                .all(|tab| tab.mutation == Some(MutationKind::Discard))
        );
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: file,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("racing edit".into()),
                )),
            })),
        );
        assert!(!app.tab(file).unwrap().runtime.is_dirty());
        sidebar_action(
            &mut app,
            SidebarAction::Stage {
                key,
                selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())],
            },
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("pending Git operations")
        );
        let arriving = open_real_git(&mut app, root, None);
        assert_eq!(
            app.tab(arriving).unwrap().mutation,
            Some(MutationKind::Discard)
        );
        run_tasks(&mut app, task);
        assert!(app.notice.is_none(), "{:?}", app.notice);
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
        assert_eq!(
            app.tab(file).unwrap().runtime.editor_text().as_deref(),
            Some("original\n")
        );
        assert!(
            app.tabs
                .iter()
                .all(|tab| !tab.sidebar.source_control.as_ref().unwrap().has_changes())
        );
    }

    fn editor() -> Editor {
        Editor {
            core: Core::default(),
            tabs: Vec::new(),
            review_requests: Default::default(),
            notifications: Default::default(),
            active: None,
            selection_epoch: 0,
            git_groups: BTreeMap::new(),
            next_group_operation: 1,
            settings: AppSettings::default(),
            shortcuts: crate::preferences::EditorShortcuts::default(),
            shortcut_draft: crate::preferences::EditorShortcuts::default(),
            settings_category: SettingsCategory::Editing,
            settings_search: String::new(),
            line_height_draft: AppSettings::default().terminal_line_height.to_string(),
            window_size: iced::Size::new(1200.0, 900.0),
            tab_history: cagents::tab_history::TabHistory::default(),
            settings_reset_confirm: false,
            settings_path: PathBuf::new(),
            settings_open: false,
            find_open: false,
            find_match_index: 0,
            find_match_total: 0,
            find_query: String::new(),
            quick_open: false,
            quick_open_query: String::new(),
            quick_open_index: 0,
            quick_open_cancel: CancellationToken::default(),
            settings_writable: false,
            quick_open_files: None,
            quick_open_generation: 0,
            notice: None,
            notices_open: false,
            file_dialog: None,
            checkout_dialog: None,
            checkout_journal: None,
            checkout_history: Vec::new(),
            checkout_history_open: false,
            checkout_error: None,
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
            document_tone_stale: false,
            sidebar_panel: WorktreePanel::Files,
            preference_lane: Arc::new(std::sync::Mutex::new([0; PreferenceFile::COUNT])),
            preference_generation: 0,
        }
    }

    fn character_key(character: &str, code: keyboard::key::Code) -> keyboard::Event {
        let key = keyboard::Key::Character(character.into());
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        }
    }

    fn named_key(named: keyboard::key::Named, modifiers: keyboard::Modifiers) -> keyboard::Event {
        let key = keyboard::Key::Named(named);
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    }

    /// Opens `file` from `root` into tab `id` as an editable buffer.
    fn file_loaded(app: &mut Editor, id: CardId, root: &Path, file: &str) {
        files_loaded(app, id);
        drop(
            app.update(Message::Repository(RepositoryAction::FileLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: root.to_path_buf(),
                    path: file.into(),
                    generation: 1,
                },
                result: cagents::editor::load_file(
                    root,
                    Path::new(file),
                    cagents::editor::PdfTone::Original,
                ),
            })),
        );
        assert!(
            app.tab(id).unwrap().runtime.editor_text().is_some(),
            "the buffer for {file} never opened",
        );
    }

    #[test]
    fn control_tab_navigates_history_and_focus_loss_commits_it() {
        let mut app = editor();
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        let first = add(&mut app, "/workspace", Some("a.rs"));
        let second = add(&mut app, "/workspace", Some("b.rs"));
        let third = add(&mut app, "/workspace", Some("c.rs"));
        for id in [first, second, third] {
            drop(app.update(Message::Select(id)));
        }
        let next = || {
            Message::Key(named_key(
                keyboard::key::Named::Tab,
                keyboard::Modifiers::CTRL,
            ))
        };
        drop(app.update(next()));
        assert_eq!(app.active, Some(second));
        drop(app.update(next()));
        assert_eq!(app.active, Some(first));
        drop(app.update(Message::Key(named_key(
            keyboard::key::Named::Tab,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        ))));
        assert_eq!(app.active, Some(second));
        drop(app.update(Message::WindowFocused(window::Id::RESERVED, false)));
        drop(app.update(next()));
        assert_eq!(app.active, Some(third));
        app.settings_open = true;
        drop(app.update(next()));
        assert_eq!(app.active, Some(third));
    }

    #[test]
    fn reordered_tabs_preserve_dirty_documents_focus_and_saved_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut app = configured(&directory);
        app.settings_writable = true;
        app.settings.editor_auto_save = false;
        let mut ids = Vec::new();
        for file in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join(file), file).unwrap();
            let id = add(&mut app, root.to_str().unwrap(), Some(file));
            load_selected(&mut app, id);
            ids.push(id);
        }
        let active = ids[2];
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: active,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("unsaved".into()),
                )),
            })),
        );
        let text = app.tab(active).unwrap().runtime.editor_text();
        drop(app.update(Message::ReorderTab {
            card: active,
            before: Some(ids[0]),
        }));
        assert_eq!(
            app.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            [ids[2], ids[0], ids[1]]
        );
        assert_eq!(app.active, Some(active));
        assert_eq!(app.tab(active).unwrap().runtime.editor_text(), text);
        assert!(app.tab(active).unwrap().runtime.is_dirty());
        flush_session(&mut app);
        let restored = crate::preferences::load_session(&app.settings_path).unwrap();
        assert_eq!(
            restored.paths,
            [root.join("c.rs"), root.join("a.rs"), root.join("b.rs")]
        );
        assert_eq!(restored.active, Some(0));
        drop(app.update(Message::CloseTab(ids[0])));
        assert_eq!(
            app.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            [ids[2], ids[1]]
        );
        let order = app.session();
        drop(app.update(Message::ReorderTab {
            card: ids[1],
            before: Some(ids[0]),
        }));
        assert_eq!(
            app.session(),
            order,
            "a removed destination cannot turn into an append"
        );
    }

    #[test]
    fn a_tab_left_behind_while_find_was_open_still_accepts_edits() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("a.rs"), "alpha\n").unwrap();
        std::fs::write(root.join("b.rs"), "beta\n").unwrap();
        let mut app = editor();
        let first = add(&mut app, root.to_str().unwrap(), Some("a.rs"));
        file_loaded(&mut app, first, root, "a.rs");
        let second = add(&mut app, root.to_str().unwrap(), Some("b.rs"));
        file_loaded(&mut app, second, root, "b.rs");
        drop(app.update(Message::Select(first)));
        drop(app.update(Message::FindInFile));
        assert!(app.find_open);
        // Clicking another tab's title while find is open used to leave the
        // outgoing tab holding search mode, where it drops every edit.
        drop(app.update(Message::Select(second)));
        assert!(!app.find_open);
        drop(app.update(Message::Key(named_key(
            keyboard::key::Named::Escape,
            keyboard::Modifiers::empty(),
        ))));
        drop(app.update(Message::Select(first)));
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: first,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("typed".into()),
                )),
            })),
        );
        assert!(
            app.tab(first)
                .unwrap()
                .runtime
                .editor_text()
                .unwrap()
                .contains("typed"),
            "the tab find was opened on silently discarded the edit",
        );
    }

    #[test]
    fn find_previous_from_the_first_match_wraps_to_the_last() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for total in [3usize, 5, 7] {
            let file = format!("matches{total}.txt");
            std::fs::write(root.join(&file), "hit\n".repeat(total)).unwrap();
            let mut app = editor();
            let id = add(&mut app, root.to_str().unwrap(), Some(&file));
            file_loaded(&mut app, id, root, &file);
            drop(app.update(Message::FindInFile));
            drop(app.update(Message::FindQueryChanged("hit".into())));
            assert_eq!(app.find_match_total, total);
            assert_eq!(app.find_match_index, 0);
            drop(app.update(Message::FindPrevious));
            assert_eq!(
                app.find_match_index,
                total - 1,
                "previous from the first match did not wrap for {total} matches",
            );
            drop(app.update(Message::FindNext));
            assert_eq!(app.find_match_index, 0);
        }
    }

    #[test]
    fn the_find_bar_does_not_swallow_other_shortcuts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("a.rs"), "alpha\n").unwrap();
        let mut app = editor();
        let id = add(&mut app, root.to_str().unwrap(), Some("a.rs"));
        file_loaded(&mut app, id, root, "a.rs");
        drop(app.update(Message::FindInFile));
        let save = character_key("s", keyboard::key::Code::KeyS);
        assert!(
            app.handle_find_key(&save).is_none(),
            "find claimed Ctrl+S, which reads as a data-loss risk",
        );
        // Left and Right belong to the query input's caret.
        for arrow in [
            keyboard::key::Named::ArrowLeft,
            keyboard::key::Named::ArrowRight,
        ] {
            assert!(
                app.handle_find_key(&named_key(arrow, keyboard::Modifiers::empty()))
                    .is_none(),
            );
        }
        let sidebar_open = app.sidebar_open;
        drop(app.update(Message::Key(character_key("b", keyboard::key::Code::KeyB))));
        assert_ne!(
            app.sidebar_open, sidebar_open,
            "no shortcut reached the shortcut table while the find bar was open",
        );
        assert!(app.find_open, "an unrelated chord closed the find bar");
    }

    #[test]
    fn digit_chords_fire_from_the_physical_digit_row() {
        let mut app = editor();
        app.shortcuts.toggle_sidebar = "Ctrl+1".into();
        let sidebar_open = app.sidebar_open;
        // Layouts that shift the digit row report a symbol as the logical key,
        // which is why the canonical matcher consults the physical digit.
        let key = keyboard::Key::Character("!".into());
        drop(app.update(Message::Key(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Digit1),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        })));
        assert_ne!(
            app.sidebar_open, sidebar_open,
            "the configured digit chord never fired",
        );
    }

    fn dialog_scroll_step(
        app: &Editor,
        tree: &mut iced::advanced::widget::Tree,
        id: widget::Id,
        task: Task<Message>,
    ) -> f32 {
        use iced::advanced::{Layout, layout, renderer::Headless, widget::Operation};
        use iced::{Size, futures::StreamExt};
        struct Probe(widget::Id, Option<f32>);
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn iced::advanced::widget::operation::Scrollable,
            ) {
                if id == Some(&self.0) {
                    self.1 = Some(translation.y);
                }
            }
        }
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let renderer = executor
            .block_on(<cosmic::Renderer as Headless>::new(
                iced::Font::DEFAULT,
                iced::Pixels::from(16),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut view = app.dialog().unwrap();
        tree.diff(view.as_widget_mut());
        let node = view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1000.0, 800.0)),
        );
        executor.block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Widget(mut operation) = action {
                        view.as_widget_mut().operate(
                            tree,
                            Layout::new(&node),
                            &renderer,
                            operation.as_mut(),
                        );
                    }
                }
            }
        });
        let mut probe = Probe(id, None);
        view.as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut probe);
        probe.1.expect("dialog result scrollable is mounted")
    }

    #[test]
    fn quick_open_keyboard_selection_stays_visible_and_queries_reset_scroll() {
        use iced::advanced::widget::Tree;
        let mut app = editor();
        let directory = tempfile::tempdir().unwrap();
        add(&mut app, directory.path().to_str().unwrap(), None);
        app.quick_open = true;
        app.quick_open_files = Some(
            (0..40)
                .map(|index| PathBuf::from(format!("file-{index}.rs")))
                .collect(),
        );
        let mut tree = Tree::new(app.dialog().unwrap().as_widget());
        let id = widget::Id::new("ceditor.quick-open.results");
        // Up wraps from the first to the last result, far beyond the viewport.
        let previous = app.update(Message::QuickOpenPrevious);
        assert!(dialog_scroll_step(&app, &mut tree, id.clone(), previous) > 1000.0);
        let next = app.update(Message::QuickOpenNext);
        assert_eq!(dialog_scroll_step(&app, &mut tree, id.clone(), next), 0.0);
        let previous = app.update(Message::QuickOpenPrevious);
        assert!(dialog_scroll_step(&app, &mut tree, id.clone(), previous) > 1000.0);
        // Retain many rows while applying the query handler's task, preventing
        // shorter-content clamping from disguising a missing reveal operation.
        let files = app.quick_open_files.clone();
        let query = app.update(Message::QuickOpenQueryChanged("file".into()));
        app.quick_open_files = files;
        assert_eq!(dialog_scroll_step(&app, &mut tree, id, query), 0.0);
    }

    #[test]
    fn editor_settings_navigation_reveals_the_content_start() {
        use iced::advanced::widget::Tree;
        let mut app = editor();
        app.settings_open = true;
        app.settings_category = SettingsCategory::Keyboard;
        let mut tree = Tree::new(app.dialog().unwrap().as_widget());
        let id = cagents::settings_widgets::content_id();
        let scroll = || {
            iced::widget::operation::scroll_to(
                id.clone(),
                iced::widget::operation::AbsoluteOffset { x: 0.0, y: 300.0 },
            )
        };
        assert!(dialog_scroll_step(&app, &mut tree, id.clone(), scroll()) > 0.0);
        let category = app.update(Message::SelectSettingsCategory(SettingsCategory::Keyboard));
        assert_eq!(
            dialog_scroll_step(&app, &mut tree, id.clone(), category),
            0.0
        );
        assert!(dialog_scroll_step(&app, &mut tree, id.clone(), scroll()) > 0.0);
        let query = app.update(Message::SettingsSearchChanged("keyboard".into()));
        assert_eq!(dialog_scroll_step(&app, &mut tree, id, query), 0.0);
    }

    #[test]
    fn quick_open_is_driveable_from_the_keyboard() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("a.rs"), "alpha\n").unwrap();
        std::fs::write(root.join("b.rs"), "beta\n").unwrap();
        let mut app = editor();
        let id = add(&mut app, root.to_str().unwrap(), None);
        drop(
            app.update(Message::Repository(RepositoryAction::FilesLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: root.to_path_buf(),
                    path: ".".into(),
                    generation: 1,
                },
                result: repository_widget::load_directory_files(root),
            })),
        );
        drop(app.update(Message::QuickOpen));
        assert!(app.quick_open);
        let results = app.quick_open_results();
        assert_eq!(results.len(), 2, "quick open listed {results:?}");
        assert_eq!(app.quick_open_selection(results.len()), 0);
        let down = named_key(
            keyboard::key::Named::ArrowDown,
            keyboard::Modifiers::empty(),
        );
        drop(app.update(Message::Key(down.clone())));
        assert_eq!(app.quick_open_selection(results.len()), 1);
        // Down from the last row wraps, and Up from the first row wraps back.
        drop(app.update(Message::Key(down)));
        assert_eq!(app.quick_open_selection(results.len()), 0);
        drop(app.update(Message::Key(named_key(
            keyboard::key::Named::ArrowUp,
            keyboard::Modifiers::empty(),
        ))));
        assert_eq!(app.quick_open_selection(results.len()), 1);
        let selected = results[1].clone();
        drop(app.update(Message::Key(named_key(
            keyboard::key::Named::Enter,
            keyboard::Modifiers::empty(),
        ))));
        assert!(!app.quick_open, "Enter did not open the selected result");
        assert_eq!(
            app.tab(id)
                .unwrap()
                .pending_file
                .as_ref()
                .map(|pending| pending.path.clone()),
            Some(selected),
        );
    }

    #[test]
    fn control_space_is_the_completion_chord() {
        use keyboard::{
            Key, Modifiers,
            key::{Code, Named, Physical},
        };

        // A modifier-held space carries no text, so the physical code is the
        // only reliable signal; the character form is the fallback.
        assert!(is_completion_chord(
            &Key::Character("x".into()),
            Physical::Code(Code::Space),
            Modifiers::CTRL
        ));
        assert!(is_completion_chord(
            &Key::Character(" ".into()),
            Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            Modifiers::CTRL
        ));
        assert!(!is_completion_chord(
            &Key::Character(" ".into()),
            Physical::Code(Code::Space),
            Modifiers::empty()
        ));
        assert!(!is_completion_chord(
            &Key::Named(Named::Enter),
            Physical::Code(Code::Enter),
            Modifiers::CTRL
        ));
    }

    #[test]
    fn settings_controls_remain_interactive_over_the_overlay() {
        let mut app = editor();
        app.settings_open = true;
        drop(app.update(Message::AutoSave(true)));
        assert!(app.settings.editor_auto_save);
    }

    #[test]
    fn document_links_deduplicate_tabs_and_keep_the_latest_pending_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("target.md");
        std::fs::write(&path, "# First\n\n# Second\n").unwrap();
        let mut app = configured(&directory);
        app.selection_epoch = 2;
        drop(app.update(Message::DocumentOpened(
            2,
            Some("second".into()),
            Editor::resolve_open(path.clone()),
        )));
        let id = app.active.unwrap();
        drop(app.update(Message::DocumentOpened(
            1,
            Some("first".into()),
            Editor::resolve_open(path.clone()),
        )));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(
            app.tab(id).unwrap().pending_fragment.as_deref(),
            Some("second")
        );
        drop(
            app.update(Message::Repository(RepositoryAction::FileLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: directory.path().to_path_buf(),
                    path: PathBuf::from("target.md"),
                    generation: 999,
                },
                result: cagents::editor::load_file(
                    directory.path(),
                    Path::new("target.md"),
                    cagents::editor::PdfTone::Original,
                ),
            })),
        );
        assert_eq!(
            app.tab(id).unwrap().pending_fragment.as_deref(),
            Some("second")
        );
        load_selected(&mut app, id);
        assert!(app.tab(id).unwrap().pending_fragment.is_none());
        app.settings_open = true;
        drop(app.update(Message::DocumentOpened(
            2,
            Some("first".into()),
            Editor::resolve_open(path),
        )));
        assert_eq!(
            app.tab(id).unwrap().pending_fragment.as_deref(),
            Some("first")
        );
        app.settings_open = false;
        drop(app.update(Message::Select(id)));
        assert!(app.tab(id).unwrap().pending_fragment.is_none());
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
    fn settings_category_selection_works_while_settings_is_open() {
        let mut app = editor();
        app.settings_open = true;
        drop(app.update(Message::SelectSettingsCategory(SettingsCategory::Keyboard)));
        assert_eq!(app.settings_category, SettingsCategory::Keyboard);
    }

    /// Runs the preference write `update` handed to the blocking pool. Unit
    /// tests have no executor to drain a `Task`, and the persisted bytes are
    /// the contract under test.
    fn flush_settings(app: &mut Editor) {
        let settings = app.settings.clone();
        app.capture_preference(PreferenceWrite::Settings(settings))
            .run()
            .unwrap();
    }

    fn flush_assistance(app: &mut Editor) {
        let settings = app.settings.clone();
        let enabled = app.language_server_assistance;
        app.capture_preference(PreferenceWrite::Assistance { settings, enabled })
            .run()
            .unwrap();
    }

    fn flush_session(app: &mut Editor) {
        let session = app.session();
        app.capture_preference(PreferenceWrite::Session(session))
            .run()
            .unwrap();
    }

    #[test]
    fn a_jump_into_stale_diagnostics_is_refused_and_explained() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", Some("a.rs"));
        app.tab_mut(id)
            .unwrap()
            .runtime
            .set_lsp_document(Some(LspDocumentId(1)));
        app.tab_mut(id).unwrap().diagnostics_version = Some(3);
        // The manager holds no version for that document, so nothing on
        // screen can be vouched for and the caret must not move.
        assert!(!app.diagnostics_are_current(id));
        drop(app.update(Message::Jump(id, LspPosition::new(4, 0))));
        assert_eq!(
            app.notice.as_deref(),
            Some("these diagnostics describe an earlier version of the file"),
        );
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
        flush_settings(&mut app);
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
                result: cagents::editor::load_file(
                    directory.path(),
                    Path::new("a.rs"),
                    cagents::editor::PdfTone::Original,
                ),
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
        flush_assistance(&mut app);
        assert!(crate::preferences::load_language_server_assistance(&app.settings_path).unwrap());
        drop(app.update(Message::LanguageServerAssistance(false)));
        assert!(app.tab(id).unwrap().runtime.lsp_document().is_none());
        flush_assistance(&mut app);
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
    fn opened_raw_byte_tabs_persist_and_restore_real_session_files() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let normal = root.join("normal.txt");
        let raw = root.join(OsString::from_vec(b"raw-\xff.txt".to_vec()));
        std::fs::write(&normal, "normal contents\n").unwrap();
        std::fs::write(&raw, "raw contents\n").unwrap();
        let mut app = configured(&directory);
        for (path, contents) in [(&normal, "normal contents\n"), (&raw, "raw contents\n")] {
            drop(app.update(Message::DocumentOpened(
                0,
                None,
                Editor::resolve_open(path.clone()),
            )));
            let id = app.active.unwrap();
            load_selected(&mut app, id);
            assert_eq!(
                app.tab(id).unwrap().runtime.editor_text().as_deref(),
                Some(contents)
            );
            flush_session(&mut app);
        }
        let saved = crate::preferences::load_session(&app.settings_path).unwrap();
        assert_eq!(saved.paths, vec![normal, raw]);
        assert_eq!(saved.active, Some(1));
        let opened = saved
            .startup_paths(Vec::new())
            .into_iter()
            .map(|(path, focus)| (focus, Editor::resolve_open(path)))
            .collect();
        let mut restored = configured(&directory);
        restored.restoring_session = true;
        drop(restored.update(Message::SessionOpened(0, opened)));
        assert_eq!(restored.session(), saved);
        let id = restored.active.unwrap();
        load_selected(&mut restored, id);
        assert_eq!(
            restored.tab(id).unwrap().runtime.editor_text().as_deref(),
            Some("raw contents\n")
        );
        flush_session(&mut restored);
        assert_eq!(
            crate::preferences::load_session(&restored.settings_path).unwrap(),
            saved
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
                authority: None,
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
        assert_eq!(
            app.session().paths,
            vec![
                PathBuf::from("/workspace/a.rs"),
                PathBuf::from("/workspace/b.rs")
            ]
        );
        drop(app.update(Message::CloseTab(second)));
        let remaining = app.session();
        assert_eq!(remaining.paths, vec![PathBuf::from("/workspace/a.rs")]);
        assert_eq!(remaining.active, Some(0));
        flush_session(&mut app);
        drop(app.update(Message::CloseWindow));
        assert!(app.tabs.is_empty());
        assert_eq!(
            crate::preferences::load_session(&app.settings_path).unwrap(),
            remaining
        );
    }

    #[test]
    fn window_shutdown_retries_a_session_that_was_captured_but_not_written() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = editor();
        app.settings_path = directory.path().join("missing/settings.toml");
        app.settings_writable = true;
        let _ = add(&mut app, "/workspace", Some("a.rs"));
        let failed = app.capture_session_save().unwrap();
        assert!(failed.run().is_err());
        std::fs::create_dir(directory.path().join("missing")).unwrap();

        drop(app.update(Message::CloseWindow));

        assert_eq!(
            crate::preferences::load_session(&app.settings_path)
                .unwrap()
                .paths,
            vec![PathBuf::from("/workspace/a.rs")]
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
                authority: None,
            },
            true,
        ));
        app.active.unwrap()
    }

    #[test]
    fn review_alert_waits_for_its_repository_and_ignores_superseded_verification() {
        use cagents::notifications::{Target, review::ReviewRequest, ui::Action as Observed};
        use notification_ui::Action;
        let mut app = editor();
        let request = ReviewRequest {
            host: "github.com".into(),
            port: None,
            owner: "o".into(),
            repository: "r".into(),
            number: 51,
            viewer: "viewer".into(),
            request: "first".into(),
        };
        drop(
            app.notification_action(Action::Observed(Observed::Activated(Target::PullRequest(
                request.clone(),
            )))),
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Open this review's repository")
        );
        let other = add(&mut app, "/other", None);
        let wanted = add(&mut app, "/wanted", None);
        drop(app.tab_mut(wanted).unwrap().review.set_source(Some(
            pull_request_review::ReviewSource {
                repository: request.repository_key(),
                worktree: "/wanted".into(),
            },
        )));
        drop(app.select_tab(other));
        let newer = ReviewRequest {
            request: "second".into(),
            ..request.clone()
        };
        drop(
            app.notification_action(Action::Observed(Observed::Activated(Target::PullRequest(
                newer.clone(),
            )))),
        );
        drop(app.notification_action(Action::Verified {
            request,
            result: Ok(()),
        }));
        assert_eq!(
            app.active,
            Some(other),
            "older async verification cannot open an obsolete request"
        );
        drop(app.notification_action(Action::Verified {
            request: newer,
            result: Ok(()),
        }));
        assert_eq!(app.active, Some(wanted));
        assert!(app.sidebar_open);
        assert_eq!(app.sidebar_panel, WorktreePanel::PullRequests);
    }

    /// Alt+Down and its siblings are ordinary editing chords elsewhere, so
    /// they act on a review only while one is open and on screen.
    #[test]
    fn review_chords_stay_with_the_editor_unless_a_review_is_on_screen() {
        let mut app = editor();
        let id = add(&mut app, "/workspace", None);
        app.tab_mut(id).unwrap().git = true;
        let next = named_key(keyboard::key::Named::ArrowDown, keyboard::Modifiers::ALT);
        assert!(app.handle_shortcut(&next).is_none(), "no review is open");

        app.sidebar_panel = WorktreePanel::PullRequests;
        assert!(app.handle_shortcut(&next).is_none(), "still none open");

        let target = pull_request_review::ReviewTarget {
            repository: cagents::github::GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
            worktree: "/workspace".into(),
            pull_request: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "number": 1,
                    "title": "Change",
                    "state": "OPEN",
                    "headRefName": "feature",
                    "baseRefName": "main",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "url": "https://github.com/o/r/pull/1",
                }))
                .unwrap(),
            ),
        };
        drop(
            app.tab_mut(id)
                .unwrap()
                .review
                .update(pull_request_review::Action::Open(target)),
        );
        assert!(app.handle_shortcut(&next).is_some(), "review on screen");

        app.sidebar_panel = WorktreePanel::Files;
        assert!(app.handle_shortcut(&next).is_none(), "review panel hidden");
        app.sidebar_panel = WorktreePanel::PullRequests;
        app.sidebar_open = false;
        assert!(app.handle_shortcut(&next).is_none(), "sidebar closed");
    }

    #[test]
    fn the_review_lists_the_origin_repository_before_other_github_remotes() {
        let remote = |name: &str, url: &str| cagents::repository::RemoteRecord {
            name: name.into(),
            fetch_url: Some(url.into()),
            push_url: None,
        };
        let root = Path::new("/repo");
        let source = review_source_for_remotes(
            root,
            &[
                remote("upstream", "git@github.com:acme/widgets.git"),
                remote("origin", "https://github.com/someone/widgets.git"),
            ],
        )
        .expect("a GitHub remote names a repository to review");
        assert_eq!(source.repository.owner, "someone");
        assert_eq!(source.worktree, root);

        let source = review_source_for_remotes(
            root,
            &[remote("upstream", "git@github.com:acme/widgets.git")],
        )
        .expect("any GitHub remote will do without origin");
        assert_eq!(source.repository.owner, "acme");

        assert!(
            review_source_for_remotes(root, &[remote("origin", "https://example.org/x.git")])
                .is_none(),
            "a remote off GitHub has no pull requests to list"
        );
        assert!(review_source(root, None).is_none());
    }

    #[test]
    fn the_pull_requests_panel_is_a_sidebar_panel() {
        let mut app = editor();
        drop(
            app.update(Message::Sidebar(SidebarAction::SetWorktreePanel {
                panel: WorktreePanel::PullRequests,
            })),
        );
        assert_eq!(app.sidebar_panel, WorktreePanel::PullRequests);
        let id = add(&mut app, "/repo", None);
        // A tab without a repository has no review to select from; the
        // choice is dropped rather than reaching the runtime.
        drop(app.update(Message::PullRequestReview(
            id,
            pull_request_review::Action::Select(7),
        )));
        assert!(app.tab(id).unwrap().review.target().is_none());
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
    fn standalone_group_actions_resolve_all_paths_and_keep_discard_confirmation_exact() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        let directory = git_fixture();
        let root = directory.path();
        for number in 0..6 {
            std::fs::write(root.join(format!("new-{number}.txt")), "new\n").unwrap();
        }
        let mut app = editor();
        app.settings.git_status_limit = 2;
        let task = app.update(Message::Chosen(app.selection_epoch, Ok(Some(root.to_path_buf()))));
        run_tasks(&mut app, task);
        let id = app.active.unwrap();
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let invoke = |app: &mut Editor, area, command| {
            let revision = app.tab(id).unwrap().navigation.change_selection_revision();
            sidebar_action(app, SidebarAction::RunChangeGroup {
                key: key.clone(), area, command, selection_revision: revision,
            });
        };
        invoke(&mut app, ChangeArea::Untracked, Group::Stage);
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]).lines().count(), 6);
        invoke(&mut app, ChangeArea::Staged, Group::Unstage);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        invoke(&mut app, ChangeArea::Untracked, Group::Discard);
        let pending = app.pending_discard().expect("full group opens the real confirmation");
        assert_eq!(pending.request.untracked.len(), 6);
        assert!(root.join("new-0.txt").exists(), "reading paths cannot discard them");
        let confirmation = pending.confirm_action();
        std::fs::write(root.join("later.txt"), "keep newer work\n").unwrap();
        sidebar_action(&mut app, confirmation);
        assert!(!root.join("new-0.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("later.txt")).unwrap(), "keep newer work\n");
        assert!(app.git_groups.is_empty());
    }

    #[test]
    fn standalone_group_selection_and_delayed_read_guards_preserve_original_intent() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        let directory = git_fixture();
        let root = directory.path();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let row = |name| sidebar::ChangeSelectionRow { area: ChangeArea::Untracked, path: RepoPath::parse(name).unwrap() };
        sidebar_action(&mut app, SidebarAction::SelectChangeRows {
            key: key.clone(), row: row("a.txt"), intent: sidebar::SelectionIntent::Toggle,
        });
        sidebar_action(&mut app, SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage });
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "a.txt");
        let stale_revision = app.tab(id).unwrap().navigation.change_selection_revision();
        let task = app.update(Message::Sidebar(SidebarAction::ChangeGroup {
            key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage,
        }));
        sidebar_action(&mut app, SidebarAction::SelectChangeRows {
            key: key.clone(), row: row("b.txt"), intent: sidebar::SelectionIntent::Toggle,
        });
        run_tasks(&mut app, task);
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "a.txt", "a delayed read cannot adopt a replacement selection");
        sidebar_action(&mut app, SidebarAction::RunChangeGroup {
            key, area: ChangeArea::Untracked, command: Group::Stage, selection_revision: stale_revision,
        });
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "a.txt", "a captured header cannot retarget a later selection");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn standalone_staging_rejects_a_stale_file_callback_and_fresh_actions_target_only_the_new_file()
    {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("other.txt"), "other original\n").unwrap();
        git(root, &["add", "other.txt"]);
        git(root, &["commit", "-m", "Add another file"]);
        std::fs::write(root.join("tracked.txt"), "tracked change\n").unwrap();
        std::fs::write(root.join("other.txt"), "other change\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, Some("tracked.txt"));
        let stale = app
            .tab(id)
            .unwrap()
            .runtime
            .selected_staging_action(id, true)
            .expect("the original selected file offers Stage");
        let task = app.update(Message::Repository(RepositoryAction::OpenFile {
            card: id,
            path: "other.txt".into(),
        }));
        run_tasks(&mut app, task);
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("other.txt"))
        );
        let task = app.update(Message::Repository(stale));
        run_tasks(&mut app, task);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert!(app.git_groups.is_empty());
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));

        let stage = app
            .tab(id)
            .unwrap()
            .runtime
            .selected_staging_action(id, true)
            .expect("the new selected file offers its own Stage action");
        let task = app.update(Message::Repository(stage));
        run_tasks(&mut app, task);
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "other.txt");
        assert_eq!(git(root, &["show", ":tracked.txt"]), "original");
        assert_eq!(git(root, &["show", ":other.txt"]), "other change");
        let unstage = app
            .tab(id)
            .unwrap()
            .runtime
            .selected_staging_action(id, false)
            .expect("the new selected file offers its own Unstage action");
        let task = app.update(Message::Repository(unstage));
        run_tasks(&mut app, task);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(git(root, &["show", ":tracked.txt"]), "original");
        assert_eq!(git(root, &["show", ":other.txt"]), "other original");
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "tracked change\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("other.txt")).unwrap(),
            "other change\n"
        );
        assert!(app.git_groups.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn standalone_admitted_staging_keeps_its_captured_file_after_navigation() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("other.txt"), "other original\n").unwrap();
        git(root, &["add", "other.txt"]);
        git(root, &["commit", "-m", "Add another file"]);
        std::fs::write(root.join("tracked.txt"), "tracked change\n").unwrap();
        std::fs::write(root.join("other.txt"), "other change\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, Some("tracked.txt"));
        let captured = app
            .tab(id)
            .unwrap()
            .runtime
            .selected_staging_action(id, true)
            .expect("the selected file offers Stage");
        let admitted = app.update(Message::Repository(captured));
        assert!(app.git_groups.get(root).unwrap().active.is_some());
        let navigation = app.update(Message::Repository(RepositoryAction::OpenFile {
            card: id,
            path: "other.txt".into(),
        }));
        run_tasks(&mut app, navigation);
        assert_eq!(
            app.tab(id).unwrap().runtime.selected_path(),
            Some(Path::new("other.txt"))
        );
        assert!(
            git(root, &["diff", "--cached", "--name-only"]).is_empty(),
            "the admitted worker has not been polled yet"
        );
        run_tasks(&mut app, admitted);
        assert_eq!(
            git(root, &["diff", "--cached", "--name-only"]),
            "tracked.txt"
        );
        assert_eq!(git(root, &["show", ":tracked.txt"]), "tracked change");
        assert_eq!(git(root, &["show", ":other.txt"]), "other original");
        assert!(app.git_groups.is_empty());
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
    }

    #[test]
    fn standalone_group_queue_reads_complete_areas_after_preceding_mutations() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "staged before group\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        for number in 0..6 {
            std::fs::write(root.join(format!("new-{number}.txt")), "new\n").unwrap();
        }
        let mut app = editor();
        app.settings.git_status_limit = 2;
        let task = app.update(Message::Chosen(app.selection_epoch, Ok(Some(root.to_path_buf()))));
        run_tasks(&mut app, task);
        let id = app.active.unwrap();
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        assert!(app.begin_mutation(id, MutationKind::Other).is_some());
        drop(app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage })));
        drop(app.update(Message::Sidebar(SidebarAction::ChangeGroup { key, area: ChangeArea::Staged, command: Group::Unstage })));
        assert_eq!(app.git_groups[root].pending_len(), 2);
        std::fs::write(root.join("late.txt"), "arrived before the queued read\n").unwrap();
        let task = app.update(Message::Mutation(id, Ok(MutationOutcome::Applied { paths: Vec::new() })));
        run_tasks(&mut app, task);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty(), "the second fresh read sees all paths staged by the first queued group");
        assert!(app.git_groups.is_empty());
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
    }

    #[test]
    fn standalone_group_read_refuses_a_replaced_physical_root() {
        use cagents::repository::ChangeArea;
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("new.txt"), "original work\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let task = app.update(Message::Sidebar(SidebarAction::ChangeGroup {
            key, area: ChangeArea::Untracked, command: sidebar::ChangeGroupCommand::Stage,
        }));
        let parked = root.with_extension("original-worktree");
        assert!(!parked.exists());
        std::fs::rename(root, &parked).unwrap();
        std::fs::create_dir(root).unwrap();
        run_tasks(&mut app, task);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
        std::fs::rename(&parked, root).unwrap();
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "original work\n");
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
    }

    fn task_messages(task: Task<Message>) -> Vec<Message> {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut messages = Vec::new();
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                        messages.push(message);
                    }
                }
            }
            messages
        })
    }

    #[test]
    fn standalone_marker_confirmation_holds_the_next_queued_group_turn() {
        let directory = git_fixture();
        let root = directory.path();
        git(root, &["checkout", "-b", "other"]);
        std::fs::write(root.join("tracked.txt"), "other\n").unwrap();
        git(root, &["commit", "-am", "Other"]);
        git(root, &["checkout", "main"]);
        std::fs::write(root.join("tracked.txt"), "main\n").unwrap();
        git(root, &["commit", "-am", "Main"]);
        let merged = fixture_git_command(root, &["merge", "other"]).output().unwrap();
        assert!(!merged.status.success());
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let action = || Message::Sidebar(SidebarAction::ChangeGroup {
            key: key.clone(), area: cagents::repository::ChangeArea::Conflict, command: sidebar::ChangeGroupCommand::Stage,
        });
        let first = app.update(action());
        drop(app.update(action()));
        assert_eq!(app.git_groups.get(root).unwrap().pending_len(), 1);
        run_tasks(&mut app, first);
        let first_approval = app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().pending_marker_approval.clone();
        assert!(!first_approval.is_empty());
        assert_eq!(app.git_groups.get(root).unwrap().pending_len(), 1, "pending warning must hold the queue");
        assert!(task_messages(app.resume_change_groups()).is_empty());
        assert_eq!(app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().pending_marker_approval, first_approval);
        assert!(!git(root, &["ls-files", "--unmerged"]).is_empty());
        sidebar_action(&mut app, SidebarAction::ConfirmStageMarkers { key, confirmed: false });
        assert_eq!(app.git_groups.get(root).unwrap().pending_len(), 0, "cancel releases the next read turn");
        assert!(app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().pending_marker_stage.is_some(), "second warning is a new confirmation");
        assert!(!git(root, &["ls-files", "--unmerged"]).is_empty());
    }

    #[test]
    fn standalone_group_stage_retains_root_authority_after_read_before_mutation_worker() {
        let directory = git_fixture();
        let replacement = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("new.txt"), "original work\n").unwrap();
        std::fs::write(replacement.path().join("new.txt"), "replacement work\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let read = app.update(Message::Sidebar(SidebarAction::ChangeGroup {
            key, area: cagents::repository::ChangeArea::Untracked, command: sidebar::ChangeGroupCommand::Stage,
        }));
        let resolved = task_messages(read).into_iter().find(|message| matches!(message, Message::ChangeGroupResolved { .. })).unwrap();
        let delayed_mutation = app.update(resolved);
        let parked = root.with_extension("delayed-original");
        std::fs::rename(root, &parked).unwrap();
        std::fs::rename(replacement.path(), root).unwrap();
        run_tasks(&mut app, delayed_mutation);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "replacement work\n");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::rename(parked, root).unwrap();
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn standalone_group_discard_confirmation_keeps_original_root_authority() {
        let directory = git_fixture();
        let replacement = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("new.txt"), "original work\n").unwrap();
        std::fs::write(replacement.path().join("new.txt"), "replacement work\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        sidebar_action(&mut app, SidebarAction::ChangeGroup { key, area: cagents::repository::ChangeArea::Untracked, command: sidebar::ChangeGroupCommand::Discard });
        let confirm = app.pending_discard().unwrap().confirm_action();
        let parked = root.with_extension("confirmed-original");
        std::fs::rename(root, &parked).unwrap();
        std::fs::rename(replacement.path(), root).unwrap();
        sidebar_action(&mut app, confirm);
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "replacement work\n");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::rename(parked, root).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "original work\n");
    }

    fn open_real_change(app: &mut Editor, id: CardId, path: &str, area: cagents::repository::ChangeArea) {
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        sidebar_action(app, SidebarAction::OpenChange { key, path: RepoPath::parse(path).unwrap(), area });
    }

    fn apply_real_hunk(app: &mut Editor, id: CardId, kind: cagents::git_patch::Kind) {
        let action = app.tab(id).unwrap().runtime.diff_hunk_mutation_action(id, kind).expect("actual Ready diff must emit a hunk action");
        let task = app.update(Message::Repository(action));
        run_tasks(app, task);
    }

    #[test]
    fn standalone_hunk_dispatch_stages_unstages_and_reverts_with_one_undo() {
        use cagents::{git_patch::Kind, repository::ChangeArea};
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, None);
        open_real_change(&mut app, id, "tracked.txt", ChangeArea::Unstaged);
        apply_real_hunk(&mut app, id, Kind::Stage);
        assert_eq!(git(root, &["show", ":tracked.txt"]), "changed");
        open_real_change(&mut app, id, "tracked.txt", ChangeArea::Staged);
        apply_real_hunk(&mut app, id, Kind::Unstage);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        open_real_change(&mut app, id, "tracked.txt", ChangeArea::Unstaged);
        apply_real_hunk(&mut app, id, Kind::Revert);
        assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "changed\n", "buffer Revert does not silently persist");
        let undo = app.update(Message::Repository(RepositoryAction::Undo { card: id }));
        run_tasks(&mut app, undo);
        assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "changed\n");
        assert!(!app.tab(id).unwrap().runtime.can_step_editor_history(false), "one Undo restores the whole native transaction");
        // Actual selected-line producer uses the same host adapter.
        let request = app.tab(id).unwrap().runtime.diff_mutation_request(id, 0..1).unwrap();
        let selection = app.update(Message::Repository(RepositoryAction::SelectDiffRow {
            identity: request.identity.clone(), basis: request.basis.clone(), mode: request.mode,
            revision: request.layout_revision, row: 0, side: cagents::diff_interaction::Side::New,
            extend: false, context: false, drag: false,
        }));
        run_tasks(&mut app, selection);
        let extension = app.update(Message::Repository(RepositoryAction::SelectDiffRow {
            identity: request.identity.clone(), basis: request.basis.clone(), mode: request.mode,
            revision: request.layout_revision, row: 1, side: cagents::diff_interaction::Side::New,
            extend: true, context: false, drag: false,
        }));
        run_tasks(&mut app, extension);
        let selected = app.tab(id).unwrap().runtime.diff_selection_mutation_action(id, Kind::Stage).unwrap();
        let task = app.update(Message::Repository(selected));
        run_tasks(&mut app, task);
        assert_eq!(git(root, &["show", ":tracked.txt"]), "changed");
    }

    #[test]
    fn standalone_group_discard_confirmation_keeps_its_turn_ahead_of_later_stage() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("old.txt"), "old\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        let first = app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Discard }));
        drop(app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage })));
        run_tasks(&mut app, first);
        let confirmation = app.pending_discard().unwrap().confirm_action();
        std::fs::write(root.join("later.txt"), "later\n").unwrap();
        sidebar_action(&mut app, confirmation);
        assert!(!root.join("old.txt").exists());
        assert!(root.join("later.txt").exists());
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "later.txt");
        assert!(app.git_groups.is_empty());
        assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
    }

    #[test]
    fn standalone_cancel_after_group_read_success_fences_stage_unstage_and_discard_handoff() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        for command in [Group::Stage, Group::Unstage, Group::Discard] {
            let directory = git_fixture();
            let root = directory.path();
            std::fs::write(root.join("new.txt"), "original work\n").unwrap();
            if command == Group::Unstage { git(root, &["add", "new.txt"]); }
            let before_index = git(root, &["diff", "--cached", "--binary"]);
            let mut app = editor();
            let id = open_real_git(&mut app, root, None);
            let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
            let area = if command == Group::Unstage { ChangeArea::Staged } else { ChangeArea::Untracked };
            let read = app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area, command }));
            let completed = task_messages(read).into_iter().find(|message| matches!(message, Message::ChangeGroupResolved { result: Ok(_), .. })).expect("status really completed successfully before cancellation");
            sidebar_action(&mut app, SidebarAction::CancelGitMutation { key });
            let task = app.update(completed);
            run_tasks(&mut app, task);
            assert_eq!(git(root, &["diff", "--cached", "--binary"]), before_index);
            assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "original work\n");
            assert!(app.pending_discard().is_none());
            assert!(app.git_groups.is_empty());
            assert!(app.tabs.iter().all(|tab| tab.mutation.is_none()));
        }
    }

    #[test]
    fn standalone_new_capability_refusal_survives_active_completion_and_retires_queued_groups() {
        use cagents::repository::ChangeArea;
        use sidebar::ChangeGroupCommand as Group;
        for failed in [false, true] {
            for queued in [false, true] {
                let directory = git_fixture();
                let root = directory.path();
                std::fs::write(root.join("new.txt"), "preserved work\n").unwrap();
                let mut app = editor();
                let id = open_real_git(&mut app, root, None);
                let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
                let read = app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage }));
                let completed = task_messages(read).into_iter().find(|message| matches!(message, Message::ChangeGroupResolved { result: Ok(_), .. })).unwrap();
                if queued {
                    drop(app.update(Message::Sidebar(SidebarAction::ChangeGroup { key: key.clone(), area: ChangeArea::Untracked, command: Group::Stage })));
                    assert_eq!(app.git_groups.get(root).unwrap().pending_len(), 1);
                }
                let reason: Arc<str> = Arc::from("Git writes disabled after this read started.");
                {
                    let tab = app.tab_mut(id).unwrap();
                    tab.sidebar.source_control.as_mut().unwrap().set_mutation_unavailable_reason(Some(reason.clone()));
                    tab.runtime.set_git_mutation_unavailable_reason(Some(reason.clone()));
                }
                let completed = if failed {
                    let Message::ChangeGroupResolved { request, .. } = completed else { unreachable!() };
                    Message::ChangeGroupResolved { request, result: Err("fixture status failure".into()) }
                } else { completed };
                let followup = app.update(completed);
                run_tasks(&mut app, followup);
                assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
                assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "preserved work\n");
                assert!(app.git_groups.is_empty(), "not-yet-started groups retire on the new refusal");
                let source = app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap();
                assert_eq!(source.mutation_unavailable_reason(), Some(reason.as_ref()));
                assert!(!source.busy());
                assert!(app.tab(id).unwrap().mutation.is_none());
                sidebar_action(&mut app, SidebarAction::Stage { key, selection: vec![PathSelection::new(RepoPath::parse("new.txt").unwrap())] });
                assert_eq!(app.notice.as_deref(), Some(reason.as_ref()));
            }
        }
    }

    #[test]
    fn standalone_read_only_git_keeps_buffer_hunk_revert_and_one_native_undo() {
        use cagents::{git_patch::Kind, repository::ChangeArea};
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "working text\n").unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, None);
        open_real_change(&mut app, id, "tracked.txt", ChangeArea::Unstaged);
        {
            let tab = app.tab_mut(id).unwrap();
            tab.worktree_authority = None;
            let reason: Arc<str> = Arc::from("Git writes unavailable; native buffer edits remain enabled.");
            tab.runtime.set_git_mutation_unavailable_reason(Some(reason.clone()));
            tab.sidebar.source_control.as_mut().unwrap().set_mutation_unavailable_reason(Some(reason));
        }
        assert!(app.tab(id).unwrap().runtime.diff_hunk_mutation_action(id, Kind::Stage).is_none());
        apply_real_hunk(&mut app, id, Kind::Revert);
        assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "working text\n");
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        let undo = app.update(Message::Repository(RepositoryAction::Undo { card: id }));
        run_tasks(&mut app, undo);
        assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "working text\n");
        assert!(!app.tab(id).unwrap().runtime.can_step_editor_history(false));
        assert!(app.git_groups.is_empty());
    }

    #[test]
    fn standalone_cancel_preparation_releases_reservation_and_late_completion_cannot_mutate() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "working hunk\n").unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        open_real_change(&mut app, id, "tracked.txt", cagents::repository::ChangeArea::Unstaged);
        let action = app.tab(id).unwrap().runtime.diff_hunk_mutation_action(id, cagents::git_patch::Kind::Stage).unwrap();
        let parked = app.update(Message::Repository(action));
        assert!(app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().busy());
        sidebar_action(&mut app, SidebarAction::CancelGitMutation { key: key.clone() });
        assert!(app.git_groups.is_empty());
        assert!(!app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().busy());
        run_tasks(&mut app, parked);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        sidebar_action(&mut app, SidebarAction::Stage { key, selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] });
        assert_eq!(git(root, &["show", ":tracked.txt"]), "working hunk", "a new intention owns a fresh cancellation token");
    }

    #[test]
    fn standalone_preparing_patch_blocks_later_commit_and_preserves_selected_queue_order() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "already staged\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "later working hunk\n").unwrap();
        let original_head = git(root, &["rev-parse", "HEAD"]);
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, None);
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        sidebar_action(&mut app, SidebarAction::CommitMessageChanged { key: key.clone(), message: "later commit".into() });
        open_real_change(&mut app, id, "tracked.txt", cagents::repository::ChangeArea::Unstaged);
        let action = app.tab(id).unwrap().runtime.diff_hunk_mutation_action(id, cagents::git_patch::Kind::Stage).unwrap();
        let parked_preparation = app.update(Message::Repository(action));
        assert!(app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().busy(), "composer sees the preparation reservation");
        let commit = app.update(Message::Sidebar(SidebarAction::Commit { key: key.clone(), message: "later commit".into(), amend: false }));
        run_tasks(&mut app, commit);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(git(root, &["show", ":tracked.txt"]), "already staged");
        assert_eq!(app.tab(id).unwrap().navigation.commit_message(), "later commit");
        // A later selected Unstage is queueable and must read the index after
        // the earlier hunk Stage, rather than overtaking its preparation.
        let later = app.update(Message::Sidebar(SidebarAction::Unstage { key, selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] }));
        run_tasks(&mut app, later);
        assert_eq!(git(root, &["show", ":tracked.txt"]), "already staged");
        run_tasks(&mut app, parked_preparation);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(git(root, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "later working hunk\n");
        assert!(app.git_groups.is_empty());
        assert!(!app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap().busy());
    }

    #[test]
    fn standalone_partial_stage_accepts_dirty_live_hunks_and_selected_lines_without_saving() {
        use cagents::{git_patch::Kind, repository::ChangeArea};
        for selected in [false, true] {
            let directory = git_fixture();
            let root = directory.path();
            std::fs::write(root.join("tracked.txt"), "disk version\n").unwrap();
            let mut app = editor();
            app.settings.editor_auto_save = false;
            let id = open_real_git(&mut app, root, None);
            open_real_change(&mut app, id, "tracked.txt", ChangeArea::Unstaged);
            let edit_surface = app.update(Message::Repository(RepositoryAction::ApplySourceSurface { card: id, diff: false }));
            run_tasks(&mut app, edit_surface);
            let select = app.update(Message::Repository(RepositoryAction::EditorAction { card: id, action: EditorAction::SelectAll }));
            run_tasks(&mut app, select);
            let edit = app.update(Message::Repository(RepositoryAction::EditorAction { card: id, action: EditorAction::Edit(EditorEdit::Paste(Arc::new("dirty live text\n".into()))) }));
            run_tasks(&mut app, edit);
            let diff_surface = app.update(Message::Repository(RepositoryAction::ApplySourceSurface { card: id, diff: true }));
            run_tasks(&mut app, diff_surface);
            assert!(app.tab(id).unwrap().runtime.is_dirty());
            if selected {
                let request = app.tab(id).unwrap().runtime.diff_mutation_request(id, 0..2).unwrap();
                for (row, extend) in [(0, false), (1, true)] {
                    let task = app.update(Message::Repository(RepositoryAction::SelectDiffRow {
                        identity: request.identity.clone(), basis: request.basis.clone(), mode: request.mode,
                        revision: request.layout_revision, row, side: cagents::diff_interaction::Side::New,
                        extend, context: false, drag: false,
                    }));
                    run_tasks(&mut app, task);
                }
                let action = app.tab(id).unwrap().runtime.diff_selection_mutation_action(id, Kind::Stage).expect("dirty live selected lines are advertised");
                let task = app.update(Message::Repository(action));
                run_tasks(&mut app, task);
            } else { apply_real_hunk(&mut app, id, Kind::Stage); }
            assert_eq!(git(root, &["show", ":tracked.txt"]), "dirty live text");
            assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "disk version\n");
            assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "dirty live text\n");
            assert!(app.tab(id).unwrap().runtime.is_dirty());
            let undo = app.update(Message::Repository(RepositoryAction::Undo { card: id }));
            run_tasks(&mut app, undo);
            assert_eq!(app.tab(id).unwrap().runtime.editor_text().unwrap(), "disk version\n", "Stage does not consume native Undo");
            assert_eq!(git(root, &["show", ":tracked.txt"]), "dirty live text");
        }
    }

    #[test]
    fn standalone_partial_revert_new_and_deleted_files_keep_native_or_recovery_undo() {
        use cagents::{git_patch::Kind, repository::ChangeArea};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("new.sh"), "new script\n").unwrap();
        std::fs::set_permissions(root.join("new.sh"), std::fs::Permissions::from_mode(0o751)).unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, None);
        open_real_change(&mut app, id, "new.sh", ChangeArea::Untracked);
        apply_real_hunk(&mut app, id, Kind::Revert);
        let save = app.update(Message::Repository(RepositoryAction::Save { card: id, overwrite: false }));
        run_tasks(&mut app, save);
        assert!(!root.join("new.sh").exists());
        let undo = app.update(Message::Repository(RepositoryAction::Undo { card: id }));
        run_tasks(&mut app, undo);
        let save = app.update(Message::Repository(RepositoryAction::Save { card: id, overwrite: false }));
        run_tasks(&mut app, save);
        assert_eq!(std::fs::read_to_string(root.join("new.sh")).unwrap(), "new script\n");
        assert_eq!(std::fs::metadata(root.join("new.sh")).unwrap().mode() & 0o777, 0o751);
        std::fs::remove_file(root.join("tracked.txt")).unwrap();
        let refresh = app.refresh_snapshot(id, true);
        run_tasks(&mut app, refresh);
        open_real_change(&mut app, id, "tracked.txt", ChangeArea::Unstaged);
        apply_real_hunk(&mut app, id, Kind::Revert);
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "original\n");
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        assert!(app.tab(id).unwrap().discard_recovery.as_ref().unwrap().can_undo());
        sidebar_action(&mut app, SidebarAction::UndoDiscard { key });
        assert!(!root.join("tracked.txt").exists(), "recovery Undo restores the original absence");
        assert!(app.tab(id).unwrap().discard_recovery.is_none());
    }

    #[test]
    fn standalone_prepared_hunk_refuses_a_replaced_root_before_queue_turn() {
        let directory = git_fixture();
        let replacement = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(replacement.path().join("tracked.txt"), "replacement\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        open_real_change(&mut app, id, "tracked.txt", cagents::repository::ChangeArea::Unstaged);
        let action = app.tab(id).unwrap().runtime.diff_hunk_mutation_action(id, cagents::git_patch::Kind::Stage).unwrap();
        let preparation = app.update(Message::Repository(action));
        let prepared = task_messages(preparation).into_iter().find(|message| matches!(message, Message::DiffPatchPrepared { .. })).unwrap();
        let parked = root.with_extension("prepared-original");
        std::fs::rename(root, &parked).unwrap();
        std::fs::rename(replacement.path(), root).unwrap();
        let task = app.update(prepared);
        run_tasks(&mut app, task);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(std::fs::read_to_string(root.join("tracked.txt")).unwrap(), "replacement\n");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::rename(parked, root).unwrap();
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn portable_git_snapshot_read_keeps_caps_and_supported_binding_errors_do_not_fallback() {
        let directory = git_fixture();
        let root = directory.path();
        for name in ["first.txt", "second.txt"] { std::fs::write(root.join(name), name).unwrap(); }
        let snapshot = read_portable_display_snapshot(root, 1).unwrap();
        assert!(snapshot.status.listing_truncated);
        assert_eq!(snapshot.status.entries.len(), 1);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        if WorktreeAuthority::supported() {
            assert!(read_display_snapshot(root, 1, None).is_err(), "missing supported binding never selects portable fallback");
            let authority = WorktreeAuthority::capture(root).unwrap();
            let original = root.join(".git-original");
            std::fs::rename(root.join(".git"), &original).unwrap();
            git(root, &["init", "-b", "foreign"]);
            assert!(read_display_snapshot(root, 1, Some(authority)).is_err(), "replaced metadata never retries through unpinned reads");
        }
    }

    #[test]
    fn read_only_git_capability_keeps_native_file_editing_and_refuses_stale_mutation_messages() {
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("tracked.txt"), "working text\n").unwrap();
        let mut app = editor();
        app.settings.editor_auto_save = false;
        let id = open_real_git(&mut app, root, Some("tracked.txt"));
        let reason: Arc<str> = Arc::from("Git mutations are unavailable in this fixture; file editing and Git reads remain enabled.");
        let key = app.tab(id).unwrap().navigation.active_worktree().unwrap().clone();
        {
            let tab = app.tab_mut(id).unwrap();
            tab.worktree_authority = None;
            tab.runtime.set_git_mutation_unavailable_reason(Some(reason.clone()));
            tab.sidebar.source_control.as_mut().unwrap().set_mutation_unavailable_reason(Some(reason.clone()));
        }
        assert!(app.tab(id).unwrap().git, "read-only Git remains a Git source");
        let source = app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap();
        assert!(!source.mutations_available());
        assert!(!source.can_commit("message"));
        assert!(!sidebar::whole_change_group_available(&app.tab(id).unwrap().sidebar, &app.tab(id).unwrap().navigation, cagents::repository::ChangeArea::Unstaged, sidebar::ChangeGroupCommand::Stage, false));
        sidebar_action(&mut app, SidebarAction::Stage { key: key.clone(), selection: vec![PathSelection::new(RepoPath::parse("tracked.txt").unwrap())] });
        assert_eq!(app.notice.as_deref(), Some(reason.as_ref()));
        sidebar_action(&mut app, SidebarAction::Commit { key, message: "must refuse".into(), amend: false });
        assert_eq!(app.notice.as_deref(), Some(reason.as_ref()));
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        let edit = app.update(Message::Repository(RepositoryAction::EditorAction { card: id, action: EditorAction::Edit(EditorEdit::Paste(Arc::new("editable ".into()))) }));
        run_tasks(&mut app, edit);
        assert!(app.tab(id).unwrap().runtime.editor_text().unwrap().contains("editable"));
        let save = app.update(Message::Repository(RepositoryAction::Save { card: id, overwrite: false }));
        run_tasks(&mut app, save);
        assert!(std::fs::read_to_string(root.join("tracked.txt")).unwrap().contains("editable"));
    }

    #[test]
    fn standalone_display_snapshots_keep_the_configured_cap_on_open_and_refresh() {
        let directory = git_fixture();
        for number in 0..6 {
            std::fs::write(directory.path().join(format!("new-{number}.txt")), "new\n").unwrap();
        }
        let mut app = editor();
        app.settings.git_status_limit = 2;
        let epoch = app.selection_epoch;
        let task = app.update(Message::Chosen(epoch, Ok(Some(directory.path().to_path_buf()))));
        run_tasks(&mut app, task);
        let id = app.active.unwrap();
        let shown = |app: &Editor| app.tab(id).unwrap().sidebar.source_control.as_ref().unwrap()
            .groups().iter().map(|group| group.rows().len()).sum::<usize>();
        assert_eq!(shown(&app), 2);
        let task = app.update(Message::RefreshVisible);
        run_tasks(&mut app, task);
        assert_eq!(shown(&app), 2);
        assert_eq!(RepositoryService::system("git").status(directory.path(), &CancellationToken::default())
            .unwrap().entries.len(), 6, "display limits never affect authoritative status");
    }

    #[test]
    fn registered_change_navigation_and_ignore_keep_their_exact_host_path() {
        use cagents::workspace_widget::GlobalShortcut as Command;
        let directory = git_fixture();
        let root = directory.path();
        std::fs::create_dir_all(root.join("nested/deep")).unwrap();
        std::fs::write(root.join("nested/deep/a.txt"), "first\n").unwrap();
        std::fs::write(root.join("other.txt"), "second\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        let path = RepoPath::parse("nested/deep/a.txt").unwrap();
        let row = sidebar::ChangeSelectionRow {
            area: cagents::repository::ChangeArea::Untracked,
            path: path.clone(),
        };
        let capture = |app: &Editor, command| SidebarAction::RunChangeCommand {
            command,
            key: key.clone(),
            row: row.clone(),
            selection_revision: app.tab(id).unwrap().navigation.change_selection_revision(),
        };
        let reveal = capture(&app, Command::RevealInFiles);
        sidebar_action(&mut app, reveal);
        assert_eq!(app.sidebar_panel, WorktreePanel::Files);
        assert_eq!(app.tab(id).unwrap().navigation.selected_file(), Some(&path));
        assert!(
            app.tab(id)
                .unwrap()
                .navigation
                .directory_is_expanded(&RepoPath::parse("nested/deep").unwrap())
        );
        let ignore = capture(&app, Command::IgnoreChangePath);
        sidebar_action(&mut app, ignore);
        assert!(
            git(
                root,
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "--",
                    "nested/deep/a.txt"
                ]
            )
            .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("other.txt")).unwrap(),
            "second\n"
        );
        assert!(
            std::fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains("a.txt")
        );
    }

    #[test]
    fn registered_change_menus_preserve_host_selection_and_discard_confirmation() {
        use cagents::repository::ChangeArea;
        use cagents::workspace_widget::GlobalShortcut as Command;
        let directory = git_fixture();
        let root = directory.path();
        std::fs::write(root.join("a.txt"), "first\n").unwrap();
        std::fs::write(root.join("b.txt"), "second\n").unwrap();
        let mut app = editor();
        let id = open_real_git(&mut app, root, None);
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        let row = |area, path| sidebar::ChangeSelectionRow {
            area,
            path: RepoPath::parse(path).unwrap(),
        };
        let a = row(ChangeArea::Untracked, "a.txt");
        let b = row(ChangeArea::Untracked, "b.txt");
        let capture = |app: &Editor, row: sidebar::ChangeSelectionRow, command| {
            SidebarAction::RunChangeCommand {
                command,
                key: key.clone(),
                row,
                selection_revision: app.tab(id).unwrap().navigation.change_selection_revision(),
            }
        };
        sidebar_action(
            &mut app,
            SidebarAction::SelectChangeRows {
                key: key.clone(),
                row: a.clone(),
                intent: sidebar::SelectionIntent::Toggle,
            },
        );
        let stale = capture(&app, a.clone(), Command::StageSelectedChanges);
        sidebar_action(
            &mut app,
            SidebarAction::SelectChangeRows {
                key: key.clone(),
                row: a.clone(),
                intent: sidebar::SelectionIntent::Toggle,
            },
        );
        sidebar_action(
            &mut app,
            SidebarAction::SelectChangeRows {
                key: key.clone(),
                row: b.clone(),
                intent: sidebar::SelectionIntent::Toggle,
            },
        );
        sidebar_action(&mut app, stale);
        assert!(
            git(root, &["diff", "--cached", "--name-only"]).is_empty(),
            "a replacement selection cannot retarget a captured menu"
        );
        sidebar_action(
            &mut app,
            SidebarAction::ClearChangeSelection { key: key.clone() },
        );
        let stage = capture(&app, a.clone(), Command::StageSelectedChanges);
        sidebar_action(&mut app, stage);
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "a.txt");
        let unstage = capture(
            &app,
            row(ChangeArea::Staged, "a.txt"),
            Command::UnstageSelectedChanges,
        );
        sidebar_action(&mut app, unstage);
        assert!(git(root, &["diff", "--cached", "--name-only"]).is_empty());
        let discard = capture(&app, a, Command::DiscardSelectedChanges);
        sidebar_action(&mut app, discard);
        assert!(
            root.join("a.txt").exists(),
            "opening discard must not mutate disk"
        );
        let mut request = app
            .tab(id)
            .unwrap()
            .navigation
            .pending_discard()
            .expect("existing host confirmation")
            .request
            .clone();
        assert_eq!(request.untracked, [RepoPath::parse("a.txt").unwrap()]);
        request.confirmed = true;
        sidebar_action(&mut app, SidebarAction::ConfirmDiscard { key, request });
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "second\n"
        );
    }

    #[test]
    fn registered_file_menus_keep_editor_host_mutations_and_reject_stale_targets() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("a.txt"), "saved").unwrap();
        let mut app = editor();
        let id = add(&mut app, root.to_str().unwrap(), None);
        drop(
            app.update(Message::Repository(RepositoryAction::FilesLoaded {
                identity: AsyncIdentity {
                    card: id,
                    worktree: root.to_path_buf(),
                    path: ".".into(),
                    generation: 1,
                },
                result: repository_widget::load_directory_files(root),
            })),
        );
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .unwrap()
            .clone();
        let path = cagents::repository::RepoPath::parse("a.txt").unwrap();
        for command in [FileCommand::Rename, FileCommand::CopyFile] {
            let action = SidebarAction::FileCommand {
                key: key.clone(),
                path: path.clone(),
                command,
            };
            drop(app.update(Message::Sidebar(sidebar::commands::registered(action))));
            let dialog = app
                .file_dialog
                .as_ref()
                .expect("the host file operation opens");
            assert_eq!(dialog.source, Path::new("a.txt"));
            assert_eq!(dialog.duplicate, command == FileCommand::CopyFile);
            drop(app.update(Message::CancelFileOperation));
        }
        let action = SidebarAction::FileCommand {
            key: key.clone(),
            path: path.clone(),
            command: FileCommand::Rename,
        };
        drop(app.update(Message::Sidebar(SidebarAction::RunCommand {
            command: workspace_widget::GlobalShortcut::DuplicateFile,
            action: Box::new(action.clone()),
        })));
        assert!(
            app.file_dialog.is_none(),
            "a mismatched identity cannot invoke a mutation"
        );
        let mut stale_key = key;
        stale_key.canonical_path = root.join("replaced-worktree");
        drop(app.update(Message::Sidebar(sidebar::commands::registered(
            SidebarAction::FileCommand {
                key: stale_key,
                path,
                command: FileCommand::Rename,
            },
        ))));
        assert!(
            app.file_dialog.is_none(),
            "the original worktree binding is required"
        );
        app.tab_mut(id)
            .unwrap()
            .sidebar
            .source_control
            .as_mut()
            .unwrap()
            .files
            .clear();
        drop(app.update(Message::Sidebar(sidebar::commands::registered(action))));
        assert!(
            app.file_dialog.is_none(),
            "a removed row cannot invoke its old menu"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "saved"
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
                result: cagents::editor::load_file(
                    directory.path(),
                    Path::new("a.txt"),
                    cagents::editor::PdfTone::Original,
                ),
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
                    authority: None,
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
                authority: None,
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
        app.tab_mut(id).unwrap().mutation = Some(MutationKind::Stage);
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
                result: cagents::editor::load_file(
                    &worktree,
                    &path,
                    cagents::editor::PdfTone::Original,
                ),
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
    fn a_modal_refuses_editor_input_but_never_an_async_completion() {
        let card = CardId::new();
        assert!(repository_action_allowed_while_modal(
            &RepositoryAction::FindLoaded {
                card,
                ticket: card.as_uuid(),
                result: Ok(Some(Vec::new())),
            }
        ));
        assert!(!repository_action_allowed_while_modal(
            &RepositoryAction::EditorAction {
                card,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("blocked".into()),
                )),
            }
        ));
        // A dropped page render leaves the document viewer reading "Rendering
        // page" for a page it never draws, because accepting the result is
        // the only thing that clears the fence.
        assert!(repository_action_allowed_while_modal(
            &RepositoryAction::PdfPageRendered {
                card,
                request: 1,
                result: Err("fixture".into()),
            }
        ));
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
        // The navigation write went to the blocking pool; flush it so the
        // assertion below reads real bytes.
        flush_session(&mut app);
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

    fn changed_path(
        path: &str,
        old: Option<&str>,
        area: cagents::repository::ChangeArea,
        kind: cagents::repository::ChangeKind,
    ) -> cagents::repository::ChangedPath {
        use cagents::repository::{ChangedPath, RepoPath};
        ChangedPath {
            path: RepoPath::parse(path).unwrap(),
            old_path: old.map(|path| RepoPath::parse(path).unwrap()),
            area,
            kind,
            conflict: None,
            submodule: false,
            added_lines: Some(1),
            removed_lines: Some(0),
        }
    }

    fn changes_snapshot(
        entries: Vec<cagents::repository::ChangedPath>,
    ) -> cagents::repository::RepositorySnapshot {
        use cagents::repository::{RepositorySnapshot, StatusSnapshot, WorkingFiles};
        RepositorySnapshot {
            status: StatusSnapshot {
                entries,
                ..StatusSnapshot::default()
            },
            files: WorkingFiles::default(),
            remotes: Vec::new(),
            local_refs: Vec::new(),
            remote_refs: Vec::new(),
            remote_heads: Vec::new(),
        }
    }

    fn open_git(
        app: &mut Editor,
        root: &Path,
        file: Option<&str>,
        entries: Vec<cagents::repository::ChangedPath>,
    ) -> CardId {
        drop(app.add(
            Opened {
                root: root.to_path_buf(),
                file: file.map(PathBuf::from),
                snapshot: Some(changes_snapshot(entries)),
                authority: WorktreeAuthority::capture(root).ok(),
            },
            true,
        ));
        app.sidebar_panel = WorktreePanel::Changes;
        app.active.unwrap()
    }

    fn open_change(
        app: &mut Editor,
        id: CardId,
        path: &str,
        area: cagents::repository::ChangeArea,
    ) {
        let key = app
            .tab(id)
            .unwrap()
            .navigation
            .active_worktree()
            .cloned()
            .unwrap();
        drop(app.update(Message::Sidebar(SidebarAction::OpenChange {
            key,
            path: RepoPath::parse(path).unwrap(),
            area,
        })));
    }

    /// Custom widget ids that name more than one node. A repeated custom id
    /// aborts the process on the next draw.
    fn duplicate_custom_ids(element: &mut Element<'_, Message>) -> Vec<String> {
        use cosmic::iced::advanced::widget::Tree;
        use cosmic::iced::core::id::Internal;
        use std::collections::HashMap;

        fn tally(tree: &Tree, out: &mut HashMap<String, usize>) {
            if let Some(cosmic::iced::core::id::Id(Internal::Custom(_, name))) = &tree.id {
                *out.entry(name.to_string()).or_default() += 1;
            }
            for child in &tree.children {
                tally(child, out);
            }
        }

        let mut counts = HashMap::new();
        tally(&Tree::new(element.as_widget_mut()), &mut counts);
        let mut repeated = counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        repeated.sort();
        repeated
    }

    #[test]
    fn clicking_a_deleted_change_opens_its_diff() {
        use cagents::repository::{ChangeArea, ChangeKind};
        let directory = tempfile::tempdir().unwrap();
        let mut app = editor();
        let id = open_git(
            &mut app,
            directory.path(),
            None,
            vec![changed_path(
                "gone.txt",
                None,
                ChangeArea::Unstaged,
                ChangeKind::Deleted,
            )],
        );
        open_change(&mut app, id, "gone.txt", ChangeArea::Unstaged);
        let runtime = &app.tab(id).unwrap().runtime;
        let context = runtime.inspector_context();
        assert_eq!(context.surface, "Diff");
        assert_eq!(
            context.selected_path.as_deref(),
            Some(Path::new("gone.txt"))
        );
        assert_eq!(context.diff_basis, Some("Unstaged"));
        assert_eq!(context.diff_state, Some("Loading"));
        assert!(runtime.comparison_old_path().is_none());
        assert!(app.tab(id).unwrap().pending_file.is_none());
        assert!(app.notice.is_none());
        let repeated = duplicate_custom_ids(&mut app.view());
        assert!(
            repeated.is_empty(),
            "the changes-to-diff frame mounts {repeated:?} more than once"
        );
    }

    #[test]
    fn open_tabs_mount_once_in_the_header_center() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
        let mut app = editor();
        let id = open_git(&mut app, root, Some("notes.txt"), Vec::new());
        file_loaded(&mut app, id, root, "notes.txt");
        assert_eq!(app.tab(id).unwrap().title, "notes.txt");
        assert_eq!(
            app.header_center().len(),
            1,
            "the open tab strip lives in the header center"
        );
        let repeated = duplicate_custom_ids(&mut app.view());
        assert!(
            repeated.is_empty(),
            "the header tab strip mounts {repeated:?} more than once"
        );
    }

    #[test]
    fn line_height_setting_applies_to_open_buffers_within_range() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
        let mut app = editor();
        let id = open_git(&mut app, root, Some("notes.txt"), Vec::new());
        file_loaded(&mut app, id, root, "notes.txt");
        drop(app.update(Message::LineHeightChanged("1.8".into())));
        assert_eq!(app.settings.terminal_line_height, 1.8);
        assert_eq!(app.line_height_draft, "1.8");
        drop(app.update(Message::LineHeightChanged("wide".into())));
        drop(app.update(Message::LineHeightChanged("5.0".into())));
        assert_eq!(app.settings.terminal_line_height, 1.8);
        assert_eq!(app.line_height_draft, "5.0");
    }

    #[test]
    fn clicking_a_renamed_change_opens_the_diff_against_its_old_path() {
        use cagents::repository::{ChangeArea, ChangeKind};
        let directory = tempfile::tempdir().unwrap();
        let mut app = editor();
        let id = open_git(
            &mut app,
            directory.path(),
            None,
            vec![changed_path(
                "renamed.txt",
                Some("original.txt"),
                ChangeArea::Staged,
                ChangeKind::Renamed,
            )],
        );
        open_change(&mut app, id, "renamed.txt", ChangeArea::Staged);
        let runtime = &app.tab(id).unwrap().runtime;
        let context = runtime.inspector_context();
        assert_eq!(context.surface, "Diff");
        assert_eq!(
            context.selected_path.as_deref(),
            Some(Path::new("renamed.txt"))
        );
        assert_eq!(context.diff_basis, Some("Staged"));
        assert_eq!(
            runtime.comparison_old_path().as_deref(),
            Some(Path::new("original.txt"))
        );
        assert!(app.notice.is_none());
    }

    #[test]
    fn a_dirty_buffer_refuses_a_change_row_and_stays_up() {
        use cagents::repository::{ChangeArea, ChangeKind};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
        let mut app = editor();
        let id = open_git(
            &mut app,
            root,
            Some("notes.txt"),
            vec![changed_path(
                "gone.txt",
                None,
                ChangeArea::Unstaged,
                ChangeKind::Deleted,
            )],
        );
        file_loaded(&mut app, id, root, "notes.txt");
        drop(
            app.update(Message::Repository(RepositoryAction::EditorAction {
                card: id,
                action: widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(
                    Arc::new("typed".into()),
                )),
            })),
        );
        assert!(app.tab(id).unwrap().runtime.blocks_navigation());
        open_change(&mut app, id, "gone.txt", ChangeArea::Unstaged);
        let runtime = &app.tab(id).unwrap().runtime;
        assert_eq!(runtime.selected_path(), Some(Path::new("notes.txt")));
        assert_eq!(runtime.inspector_context().surface, "Editor");
        assert_eq!(
            app.notice.as_deref(),
            Some("Save or discard the current file before opening another file.")
        );
    }

    #[test]
    fn opening_a_change_tracks_that_file_instead_of_the_one_already_open() {
        use cagents::repository::{ChangeArea, ChangeKind};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
        let mut app = editor();
        let id = open_git(
            &mut app,
            root,
            Some("notes.txt"),
            vec![changed_path(
                "gone.txt",
                None,
                ChangeArea::Unstaged,
                ChangeKind::Deleted,
            )],
        );
        file_loaded(&mut app, id, root, "notes.txt");
        assert_eq!(app.tab(id).unwrap().title, "notes.txt");
        let epoch = app.selection_epoch;
        open_change(&mut app, id, "gone.txt", ChangeArea::Unstaged);
        let tab = app.tab(id).unwrap();
        assert!(app.selection_epoch != epoch);
        assert_eq!(tab.title, "gone.txt");
        assert_eq!(
            tab.owned_file.as_deref(),
            Some(root.join("gone.txt").as_path())
        );
        assert_eq!(
            app.session().paths,
            vec![root.join("gone.txt")],
            "a restart would otherwise reopen the file that was open before the click"
        );
        drop(app.request_file(id, "notes.txt".into()));
        assert_eq!(
            app.tab(id)
                .unwrap()
                .pending_file
                .as_ref()
                .map(|pending| pending.path.as_path()),
            Some(Path::new("notes.txt")),
            "the previous file is no longer considered the one this tab has open"
        );
    }
}
