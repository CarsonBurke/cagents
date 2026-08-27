use std::{
    any::Any,
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use cosmic::{
    Application, Element,
    app::{ApplicationExt, Core, Task},
    dialog::file_chooser,
    iced::{
        self, Alignment, Length, Padding, Subscription, advanced::text::EllipsizeHeightLimit,
        event, keyboard, window,
    },
    widget,
};
use url::Url;

use crate::{
    browser::{
        BrowserEvent, BrowserHandle, BrowserLifecycle, BrowserRestoreState, BrowserWidget,
        DelegateRequest, DelegateRequestId, InputMethodState, PhysicalViewport,
        RawWaylandSubsurfaceFactory, WaylandSubsurfaceFactory,
    },
    github::{
        CreatePullRequestRequest, GitHubRepoKey, GitHubService, ListPullRequestsRequest,
        PullRequest, PullRequestCheck, PullRequestCreationOutcome, PullRequestStateFilter,
    },
    lifecycle::{LifecycleAction, LifecycleBudget, LifecycleManager},
    model::{
        BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState, Existence,
        Exposure, HostSessionProof, LaunchRecipe, ProjectId, ProjectState, RepositoryView,
        Residency, RuntimeError, TerminalContinuity, TerminalExit, TerminalId, TerminalSession,
        WorkspaceState, WorktreeState,
    },
    repository::{
        BaseRevision, CancellationToken, CreateWorktreeRequest, RepositoryId, RepositoryInfo,
        RepositoryService, StatusSnapshot, ValidatedBranchName, WorktreeCreationOutcome,
        WorktreeRecord,
    },
    session::SessionRuntime,
    settings::{self, AppSettings, KeyboardShortcuts, LinkOpenDestination, LinkOpenGesture},
    sidebar::{
        self, SidebarAction, SidebarDataState, SidebarMode, SidebarModel, SidebarProject,
        SidebarPullRequest, SidebarState, SidebarWorktree,
    },
    terminal::{
        HostedTerminalHandle, TerminalConfig, TerminalEvent, TerminalSurface, TerminalWidget,
    },
    workspace::{
        CardWidth, ViewportUpdate, VisibilityClass, VisibilityTransition, WheelDelta, Workspace,
        WorkspaceCard, WorkspaceConfig,
    },
    workspace_widget::{self, FrameClock, PanGesture, WorkspaceViewportEvent},
};

const APPLICATION_ICON_PNG: &[u8] = include_bytes!("../packaging/dev.cagents.Cagents.png");
const APPLICATION_ICON_SIZE: u32 = 128;

/// The name shown to a person, as opposed to `APP_ID`, which only matches the
/// window to its desktop entry.
const APPLICATION_NAME: &str = "cagents";

/// Separates what the window is showing from the application name, following
/// the desktop convention of "<document> — <application>".
const WINDOW_TITLE_SEPARATOR: &str = " — ";

fn application_window_icon() -> window::Icon {
    let rgba = image::load_from_memory_with_format(APPLICATION_ICON_PNG, image::ImageFormat::Png)
        .expect("embedded application icon must be a valid PNG")
        .into_rgba8();
    window::icon::from_rgba(
        rgba.into_raw(),
        APPLICATION_ICON_SIZE,
        APPLICATION_ICON_SIZE,
    )
    .expect("embedded application icon dimensions must match its RGBA pixels")
}

const INITIAL_HYDRATION_EPOCH: u64 = 1;

#[derive(Debug)]
struct ParentWindowLifetime {
    window_id: window::Id,
    active: AtomicBool,
}

impl ParentWindowLifetime {
    fn new(window_id: window::Id) -> Self {
        Self {
            window_id,
            active: AtomicBool::new(true),
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub struct BrowserFactoryBinding {
    factory: Arc<RawWaylandSubsurfaceFactory>,
    parent_lifetime: Arc<ParentWindowLifetime>,
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalStartMode {
    Attach(HostSessionProof),
    Spawn(LaunchRecipe),
}

const TOOLBAR_FOCUS_ID: &str = "cagents-workspace-toolbar";
const COMMAND_BAR_COMPACT_BREAKPOINT: f32 = 1_200.0;
const APP_HEADER_HEIGHT: f32 = 48.0;
const NOTICE_ROW_HEIGHT: f32 = 36.0;
const CARD_HEADER_HEIGHT: f32 = 40.0;
const BROWSER_TOOLBAR_HEIGHT: f32 = 36.0;
const BROWSER_PROGRESS_HEIGHT: f32 = 2.0;
const FOCUS_RAIL_HEIGHT: f32 = 3.0;
const ICON_CONTROL_SIZE: f32 = 32.0;
const DIALOG_TRANSITION_DURATION: Duration = Duration::from_millis(160);
const CONTENT_MAX_WIDTH: f32 = 560.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeSeverity {
    Success,
    Warning,
    Error,
}

impl NoticeSeverity {
    fn icon(self) -> &'static str {
        match self {
            Self::Success => "emblem-ok-symbolic",
            Self::Warning => "dialog-warning-symbolic",
            Self::Error => "dialog-error-symbolic",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransientNotice {
    severity: NoticeSeverity,
    message: String,
}

impl TransientNotice {
    fn new(severity: NoticeSeverity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
        }
    }

    fn success(message: impl Into<String>) -> Self {
        Self::new(NoticeSeverity::Success, message)
    }

    fn warning(message: impl Into<String>) -> Self {
        Self::new(NoticeSeverity::Warning, message)
    }

    fn error(message: impl Into<String>) -> Self {
        Self::new(NoticeSeverity::Error, message)
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalWakeup {
    card: CardId,
    generation: u64,
}

#[derive(Clone)]
struct TerminalWakeSource(async_channel::Receiver<TerminalWakeup>);

impl PartialEq for TerminalWakeSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for TerminalWakeSource {}

impl std::hash::Hash for TerminalWakeSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-terminal-wakes", state);
    }
}

fn terminal_wake_stream(
    source: &TerminalWakeSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = TerminalWakeup> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|wake| (wake, receiver)) },
    ))
}

fn restored_terminal_failure(session: &TerminalSession) -> Option<String> {
    let TerminalContinuity::RelaunchAvailable {
        last_exit: Some(last_exit),
        ..
    } = &session.continuity
    else {
        return None;
    };
    let outcome = if let Some(signal) = last_exit.signal {
        format!("terminal was terminated by signal {signal}")
    } else if let Some(code) = last_exit.code {
        format!("terminal exited with status {code}")
    } else {
        "terminal exited".into()
    };
    Some(format!("{outcome}; Retry starts a new terminal"))
}

fn restored_runtime_residency(
    kind: &CardKind,
    _exposure: Exposure,
    durable_residency: &Residency,
) -> Residency {
    match kind {
        CardKind::Terminal(session) => restored_terminal_failure(session).map_or_else(
            || Residency::Hibernated,
            |message| Residency::Failed {
                typed_error: RuntimeError {
                    code: "terminal_exited".into(),
                    message,
                },
                retryable: true,
            },
        ),
        CardKind::Browser(_) => Residency::Hibernated,
        CardKind::Repository(_) | CardKind::PullRequests(_) => durable_residency.clone(),
    }
}

fn restored_terminal_residency(
    kind: &CardKind,
    exposure: Exposure,
    durable_residency: &Residency,
) -> Option<Residency> {
    matches!(kind, CardKind::Terminal(_))
        .then(|| restored_runtime_residency(kind, exposure, durable_residency))
}

/// Applies only the durable part of a viewport update. Camera offset is
/// transient, so callers use the result to avoid queueing an identical,
/// potentially large workspace snapshot for every animation frame.
fn apply_durable_viewport_update(
    state: &mut WorkspaceState,
    focused: Option<CardId>,
    transitions: &[VisibilityTransition],
) -> bool {
    let mut changed = false;
    for transition in transitions {
        let exposure = transition.to.into();
        if let Some(card) = state.cards.iter_mut().find(|card| card.id == transition.id)
            && card.exposure != exposure
        {
            card.exposure = exposure;
            changed = true;
        }
    }
    if state.focused != focused {
        state.focused = focused;
        changed = true;
    }
    changed
}

/// Whether [`apply_durable_viewport_update`] would change anything.
///
/// Callers hold the workspace behind an `Arc`, and taking a mutable reference
/// copies it while the writer thread still holds the last snapshot. Camera-only
/// updates arrive every frame, so they must not reach that copy.
fn durable_viewport_update_pending(
    state: &WorkspaceState,
    focused: Option<CardId>,
    transitions: &[VisibilityTransition],
) -> bool {
    state.focused != focused
        || transitions.iter().any(|transition| {
            let exposure = Exposure::from(transition.to);
            state
                .cards
                .iter()
                .any(|card| card.id == transition.id && card.exposure != exposure)
        })
}

fn deferred_terminal_session(
    state: Option<&CardState>,
    runtime_is_loading: bool,
) -> Option<TerminalSession> {
    if !runtime_is_loading {
        return None;
    }
    state
        .filter(|state| state.existence.is_open())
        .and_then(|state| match &state.kind {
            CardKind::Terminal(session) => Some(session.clone()),
            _ => None,
        })
}

fn terminal_start_visibility(state: Option<&CardState>) -> Option<bool> {
    state
        .filter(|state| state.existence.is_open())
        .filter(|state| matches!(&state.kind, CardKind::Terminal(_)))
        .map(|state| state.exposure == Exposure::Visible)
}

fn can_register_deferred_browser(
    state: &CardState,
    lifecycle_registered: bool,
    runtime_is_loading: bool,
) -> bool {
    runtime_is_loading
        && !lifecycle_registered
        && state.existence.is_open()
        && matches!(&state.kind, CardKind::Browser(_))
}

fn can_accept_terminal_start(
    state: Option<&CardState>,
    lifecycle_registered: bool,
    runtime_is_loading: bool,
) -> bool {
    lifecycle_registered
        && runtime_is_loading
        && state.is_some_and(|state| {
            state.existence.is_open() && matches!(&state.kind, CardKind::Terminal(_))
        })
}

fn offscreen_hot(lifecycle: &LifecycleManager, card: CardId) -> bool {
    matches!(
        lifecycle.state(card),
        Some((Exposure::Offscreen, Residency::Hot))
    )
}

fn rollback_failed_hibernation(
    lifecycle: &mut LifecycleManager,
    card: CardId,
    epoch: u64,
    now: Instant,
) -> Option<Residency> {
    let (exposure, residency) = lifecycle
        .state(card)
        .map(|(exposure, residency)| (*exposure, residency.clone()))?;
    if residency != (Residency::Evicting { epoch }) {
        return None;
    }
    let residency = Residency::Hot;
    lifecycle.register(card, exposure, residency.clone(), now);
    Some(residency)
}

fn lifecycle_sweep_timer(delay: Duration) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(delay).await;
        Message::LifecycleSweepElapsed
    })
}

fn card_control_id(id: CardId, control: &str) -> widget::Id {
    widget::Id::new(format!("card.{id}.{control}"))
}

fn card_accessible_name(type_label: &str, title: &str, caption: &str, focused: bool) -> String {
    format!(
        "{type_label} column {title}, {caption}, {}",
        if focused { "focused" } else { "not focused" }
    )
}

fn card_close_name(type_label: &str, title: &str) -> String {
    format!("Close {type_label} column {title}")
}

fn card_kind_labels(kind: Option<&CardKind>) -> CardLabels {
    match kind {
        Some(CardKind::Terminal(_)) => CardLabels {
            type_label: "Terminal",
            icon: "utilities-terminal-symbolic",
            starting: "Starting terminal…",
            unavailable: "Terminal unavailable",
        },
        Some(CardKind::Browser(_)) => CardLabels {
            type_label: "Browser",
            icon: "internet-web-browser-symbolic",
            starting: "Restoring browser…",
            unavailable: "Browser unavailable",
        },
        Some(CardKind::Repository(_)) => CardLabels {
            type_label: "Repository",
            icon: "folder-symbolic",
            starting: "Loading repository…",
            unavailable: "Repository unavailable",
        },
        Some(CardKind::PullRequests(_)) => CardLabels {
            type_label: "Pull requests",
            icon: "vcs-merge-request-symbolic",
            starting: "Loading pull requests…",
            unavailable: "Pull requests unavailable",
        },
        None => CardLabels {
            type_label: "Column",
            icon: "view-grid-symbolic",
            starting: "Starting column…",
            unavailable: "Column unavailable",
        },
    }
}

fn card_kind_caption(kind: &CardKind) -> Cow<'_, str> {
    match kind {
        CardKind::Terminal(session) => display_path(&session.launch.working_directory),
        CardKind::Browser(session) => Cow::Borrowed(session.current_url.as_str()),
        CardKind::PullRequests(view) => display_path(&view.repository),
        CardKind::Repository(view) => display_path(&view.worktree),
    }
}

/// A path as a person would write it, with the home directory as `~`.
///
/// Every path this application shows lives under the user's home, so the
/// absolute prefix is noise that costs the readable tail its space when the
/// label has to ellipsize.
fn display_path(path: &Path) -> Cow<'_, str> {
    static HOME: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned()));
    let relative = HOME
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    match relative {
        Some(relative) => Cow::Owned(format!("~/{}", relative.display())),
        None => path.to_string_lossy(),
    }
}

/// Whether `title` is the same path as `caption`, only written shorter.
///
/// Prompt themes commonly report the working directory with every component but
/// the last shortened to its first letters, so a card whose shell does that
/// stacks `~/D/r/cagents` above `~/Documents/repositories/cagents` and says one
/// thing twice, the readable copy second. Compared component by component
/// rather than as substrings, so a title naming anything else - a running
/// command, an edited file - always survives.
fn title_abbreviates_path(title: &str, caption: &str) -> bool {
    let mut components = caption.split('/');
    let mut abbreviated = title.split('/');
    loop {
        match (abbreviated.next(), components.next()) {
            (Some(short), Some(full)) if full.starts_with(short) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn terminal_transcript_path(id: TerminalId) -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "cagents", "cagents").map(|dirs| {
        dirs.data_dir()
            .join("transcripts")
            .join(format!("{id}.log"))
    })
}

fn terminal_exit(exit_code: u32) -> TerminalExit {
    TerminalExit {
        code: i32::try_from(exit_code).ok(),
        signal: None,
        occurred_at_unix_ms: unix_millis(),
    }
}

fn transition_terminal_to_relaunchable(session: &mut TerminalSession, last_exit: TerminalExit) {
    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: session.launch.clone(),
        last_exit: Some(last_exit),
    };
}

fn terminal_continuity(state: Option<&CardState>) -> Option<&TerminalContinuity> {
    match state.map(|state| &state.kind) {
        Some(CardKind::Terminal(session)) => Some(&session.continuity),
        _ => None,
    }
}

fn fresh_terminal_relaunch(session: &TerminalSession) -> TerminalSession {
    let (recipe, last_exit) = match &session.continuity {
        TerminalContinuity::RelaunchAvailable { recipe, last_exit } => {
            (recipe.clone(), last_exit.clone())
        }
        TerminalContinuity::Exited { last_exit } => {
            (session.launch.clone(), Some(last_exit.clone()))
        }
        TerminalContinuity::Live { .. }
        | TerminalContinuity::DetachedSameBoot { .. }
        | TerminalContinuity::Failed { .. } => (session.launch.clone(), None),
    };
    let id = TerminalId::new();
    TerminalSession {
        id,
        launch: recipe.clone(),
        transcript_path: terminal_transcript_path(id),
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable { recipe, last_exit },
    }
}

fn repository_refresh_targets(projects: &[SidebarProject]) -> Vec<(PathBuf, ProjectId)> {
    projects
        .iter()
        .map(|project| (project.repository.clone(), project.id))
        .collect()
}

fn finish_repository_load_failure(data_state: &mut SidebarDataState, error: String) {
    *data_state = SidebarDataState::Error(error);
}

fn update_workspace_viewport(
    workspace: &mut Workspace,
    event: WorkspaceViewportEvent,
) -> (ViewportUpdate, bool) {
    match event {
        WorkspaceViewportEvent::Scrolled(sample) => {
            (workspace.native_scroll(sample.offset, sample.width), false)
        }
        WorkspaceViewportEvent::Resized(width) => (workspace.resize_viewport(width), true),
    }
}

#[derive(Debug, Clone)]
pub struct LoadedRepository {
    project: ProjectState,
    repository: RepositoryInfo,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    github_repository: Option<GitHubRepoKey>,
    pull_requests: Vec<(PullRequest, Vec<PullRequestCheck>)>,
}

type BootstrapResult = (
    WorkspaceState,
    Option<SessionRuntime>,
    Vec<ProjectState>,
    Vec<WorktreeState>,
    AppSettings,
    Option<PathBuf>,
    bool,
    Option<String>,
);

#[derive(Debug, Clone)]
struct WorktreeDialog {
    project_id: ProjectId,
    repository: PathBuf,
    branch: String,
    base: String,
    destination: String,
}
#[derive(Debug, Clone)]
struct PullRequestDialog {
    repository: GitHubRepoKey,
    base: String,
    head: String,
    title: String,
    body: String,
    draft: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inspector {
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    General,
    Appearance,
    Terminal,
    Links,
    Keyboard,
}

impl SettingsCategory {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Appearance,
        Self::Terminal,
        Self::Links,
        Self::Keyboard,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Links => "Browser & links",
            Self::Keyboard => "Keyboard",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InspectorTransition {
    started_at: Instant,
    from: f32,
    to: f32,
}

impl InspectorTransition {
    fn duration(self) -> Duration {
        DIALOG_TRANSITION_DURATION.mul_f32((self.to - self.from).abs())
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

fn is_escape_press(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            repeat: false,
            ..
        }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    MaxHotCards,
    HibernationDelayMs,
    TerminalFontSize,
    TerminalFontFamily,
    TerminalScrollbackLines,
    SidebarWidth,
    DefaultColumnFraction,
    LinkOpenDestination,
    LinkOpenGesture,
    ShortcutNewTerminal,
    ShortcutCloseFocusedColumn,
    ShortcutSearchProjects,
    ShortcutPreviousColumn,
    ShortcutNextColumn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingsFeedback {
    Saved,
    Invalid(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsDraft {
    schema_version: String,
    max_hot_cards: String,
    hibernation_delay_ms: String,
    terminal_font_size: String,
    terminal_font_family: String,
    terminal_scrollback_lines: String,
    sidebar_width: String,
    default_column_fraction: String,
    link_open_destination: LinkOpenDestination,
    link_open_gesture: LinkOpenGesture,
    keyboard_shortcuts: KeyboardShortcuts,
}

impl SettingsDraft {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            schema_version: settings.schema_version.to_string(),
            max_hot_cards: settings.max_hot_cards.to_string(),
            hibernation_delay_ms: settings.hibernation_delay_ms.to_string(),
            terminal_font_size: settings.terminal_font_size.to_string(),
            terminal_font_family: settings.terminal_font_family.clone(),
            terminal_scrollback_lines: settings.terminal_scrollback_lines.to_string(),
            sidebar_width: settings.sidebar_width.to_string(),
            default_column_fraction: settings.default_column_fraction.to_string(),
            link_open_destination: settings.link_open_destination,
            link_open_gesture: settings.link_open_gesture,
            keyboard_shortcuts: settings.keyboard_shortcuts.clone(),
        }
    }

    fn set(&mut self, field: SettingsField, value: String) {
        match field {
            SettingsField::MaxHotCards => self.max_hot_cards = value,
            SettingsField::HibernationDelayMs => self.hibernation_delay_ms = value,
            SettingsField::TerminalFontSize => self.terminal_font_size = value,
            SettingsField::TerminalFontFamily => self.terminal_font_family = value,
            SettingsField::TerminalScrollbackLines => self.terminal_scrollback_lines = value,
            SettingsField::SidebarWidth => self.sidebar_width = value,
            SettingsField::DefaultColumnFraction => self.default_column_fraction = value,
            SettingsField::LinkOpenDestination => {
                self.link_open_destination = if value == LinkOpenDestination::SystemDefault.as_str()
                {
                    LinkOpenDestination::SystemDefault
                } else {
                    LinkOpenDestination::IntegratedBrowser
                };
            }
            SettingsField::LinkOpenGesture => {
                self.link_open_gesture = if value == LinkOpenGesture::ControlShiftClick.as_str() {
                    LinkOpenGesture::ControlShiftClick
                } else {
                    LinkOpenGesture::ControlClick
                };
            }
            SettingsField::ShortcutNewTerminal => self.keyboard_shortcuts.new_terminal = value,
            SettingsField::ShortcutCloseFocusedColumn => {
                self.keyboard_shortcuts.close_focused_column = value;
            }
            SettingsField::ShortcutSearchProjects => {
                self.keyboard_shortcuts.search_projects = value;
            }
            SettingsField::ShortcutPreviousColumn => {
                self.keyboard_shortcuts.previous_column = value;
            }
            SettingsField::ShortcutNextColumn => self.keyboard_shortcuts.next_column = value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SettingsDecision {
    Invalid(String),
    Unchanged,
    Persist(AppSettings),
}

/// The accepted sidebar widths, worded for a person.
///
/// The bounds belong to the pane that renders at them, so the validation error
/// and the settings dialog's help text both derive from its constants rather
/// than restating numbers that would silently drift.
fn sidebar_width_range() -> String {
    format!(
        "between {} and {} pixels",
        sidebar::EXPANDED_WIDTH_MIN,
        sidebar::EXPANDED_WIDTH_MAX
    )
}

fn settings_update(
    current: &AppSettings,
    field: SettingsField,
    raw_value: &str,
) -> SettingsDecision {
    let value = raw_value.trim();
    let mut next = current.clone();
    let parsed = match field {
        SettingsField::MaxHotCards => value
            .parse::<usize>()
            .map(|value| next.max_hot_cards = value)
            .map_err(|_| "Maximum hot columns must be a whole number.".to_owned()),
        SettingsField::HibernationDelayMs => value
            .parse::<u64>()
            .map(|value| next.hibernation_delay_ms = value)
            .map_err(|_| "Hibernation delay must be milliseconds as a whole number.".to_owned()),
        SettingsField::TerminalFontSize => value
            .parse::<f32>()
            .map(|value| next.terminal_font_size = value)
            .map_err(|_| "Terminal font size must be a number.".to_owned()),
        SettingsField::TerminalFontFamily => {
            next.terminal_font_family = value.to_owned();
            Ok(())
        }
        SettingsField::TerminalScrollbackLines => value
            .parse::<usize>()
            .map(|value| next.terminal_scrollback_lines = value)
            .map_err(|_| "Terminal scrollback must be a whole number.".to_owned()),
        SettingsField::SidebarWidth => value
            .parse::<f32>()
            .map_err(|_| ())
            .and_then(|value| {
                if (sidebar::EXPANDED_WIDTH_MIN..=sidebar::EXPANDED_WIDTH_MAX).contains(&value) {
                    Ok(value)
                } else {
                    Err(())
                }
            })
            .map(|value| next.sidebar_width = value)
            .map_err(|_| format!("Sidebar width must be {}.", sidebar_width_range())),
        SettingsField::DefaultColumnFraction => value
            .parse::<f32>()
            .map(|value| next.default_column_fraction = value)
            .map_err(|_| "Default column fraction must be a number.".to_owned()),
        SettingsField::LinkOpenDestination => match value {
            "integrated_browser" => {
                next.link_open_destination = LinkOpenDestination::IntegratedBrowser;
                Ok(())
            }
            "system_default" => {
                next.link_open_destination = LinkOpenDestination::SystemDefault;
                Ok(())
            }
            _ => Err("Choose where links should open.".to_owned()),
        },
        SettingsField::LinkOpenGesture => match value {
            "control_click" => {
                next.link_open_gesture = LinkOpenGesture::ControlClick;
                Ok(())
            }
            "control_shift_click" => {
                next.link_open_gesture = LinkOpenGesture::ControlShiftClick;
                Ok(())
            }
            _ => Err("Choose a link-opening gesture.".to_owned()),
        },
        SettingsField::ShortcutNewTerminal => {
            next.keyboard_shortcuts.new_terminal = value.to_owned();
            Ok(())
        }
        SettingsField::ShortcutCloseFocusedColumn => {
            next.keyboard_shortcuts.close_focused_column = value.to_owned();
            Ok(())
        }
        SettingsField::ShortcutSearchProjects => {
            next.keyboard_shortcuts.search_projects = value.to_owned();
            Ok(())
        }
        SettingsField::ShortcutPreviousColumn => {
            next.keyboard_shortcuts.previous_column = value.to_owned();
            Ok(())
        }
        SettingsField::ShortcutNextColumn => {
            next.keyboard_shortcuts.next_column = value.to_owned();
            Ok(())
        }
    };
    if let Err(message) = parsed {
        return SettingsDecision::Invalid(message);
    }
    let next = match next.validate() {
        Ok(next) => next,
        Err(error) => return SettingsDecision::Invalid(error.to_string()),
    };
    if next == *current {
        SettingsDecision::Unchanged
    } else {
        SettingsDecision::Persist(next)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    NewTerminal,
    TerminalStarted {
        card: CardId,
        result: Result<TerminalSurface, String>,
    },
    TerminalWake {
        card: CardId,
        generation: u64,
    },
    TerminalError {
        card: CardId,
        message: String,
    },
    OpenRepository,
    RepositorySelected(Result<PathBuf, String>),
    RepositoryLoaded {
        open_cards: bool,
        result: Result<Box<LoadedRepository>, String>,
    },
    Sidebar(SidebarAction),
    FocusCard(CardId),
    CloseCard(CardId),
    CommitClose(CardId),
    Viewport(WorkspaceViewportEvent),
    Pan(PanGesture),
    Wheel(WheelDelta),
    Frame(Instant),
    LifecycleSweep,
    /// The debounced sweep timer fired; sweeps only if the deadline has passed.
    LifecycleSweepElapsed,
    InitializeBrowserFactory,
    BrowserFactoryReady(Result<BrowserFactoryBinding, String>),
    WindowScaleFactorChanged(f32),
    WindowResized(f32),
    OpenBrowser(Url),
    ExternalLinkOpened(Result<(), String>),
    NewBrowser,
    BrowserBack(CardId),
    BrowserForward(CardId),
    BrowserReload(CardId),
    RetryCard(CardId),
    BrowserAddressChanged {
        card: CardId,
        value: String,
    },
    BrowserNavigate(CardId),
    BrowserEvent {
        card: CardId,
        event: BrowserEvent,
    },
    BrowserError {
        card: CardId,
        message: String,
    },
    ClipboardRead {
        card: CardId,
        request: DelegateRequestId,
        value: Option<String>,
    },
    FilePickerResult {
        card: CardId,
        request: DelegateRequestId,
        result: Result<Option<Vec<PathBuf>>, String>,
    },
    WorktreeBranchChanged(String),
    WorktreeBaseChanged(String),
    WorktreeDestinationChanged(String),
    SubmitWorktree,
    WorktreeCreated {
        project_id: ProjectId,
        repository: PathBuf,
        result: Result<WorktreeCreationOutcome, String>,
    },
    PullRequestBaseChanged(String),
    PullRequestHeadChanged(String),
    PullRequestTitleChanged(String),
    PullRequestBodyChanged(String),
    PullRequestDraftChanged(bool),
    SubmitPullRequest,
    PullRequestCreated(Result<PullRequestCreationOutcome, String>),
    CancelDialog,
    KeyEvent(keyboard::Event),
    FocusAdjacent(crate::workspace::FocusDirection),
    FocusProjectSearch,
    CloseInspector,
    SelectSettingsCategory(SettingsCategory),
    SettingsChanged {
        field: SettingsField,
        value: String,
    },
    ResetSettings,
    DismissStatus,
}

struct BrowserRuntime {
    handle: BrowserHandle,
    ime: Option<InputMethodState>,
    lifecycle: BrowserLifecycle,
    address: String,
    load_state: Option<String>,
}

enum CardRuntime {
    Loading,
    Failed(String),
    Terminal(TerminalSurface),
    Browser(BrowserRuntime),
    Repository(RepositoryView),
    PullRequests(PathBuf),
}

/// Static chrome strings for a card kind.
struct CardLabels {
    type_label: &'static str,
    icon: &'static str,
    starting: &'static str,
    unavailable: &'static str,
}

/// Widget ids and accessible names for one card.
///
/// Both embed the card id and title, so they are built when a card appears or
/// is renamed instead of once per rendered frame. Reusing an id also keeps the
/// card's accessibility node stable across frames.
struct CardChrome {
    focus_id: widget::Id,
    container_id: widget::Id,
    close_id: widget::Id,
    browser_back_id: widget::Id,
    browser_forward_id: widget::Id,
    browser_reload_id: widget::Id,
    browser_address_id: widget::Id,
    type_label: &'static str,
    title: String,
    focused_name: String,
    caption: String,
    unfocused_name: String,
    close_name: String,
    close_tooltip: String,
}

impl CardChrome {
    fn new(
        id: CardId,
        type_label: &'static str,
        title: &str,
        caption: &str,
        shortcuts: &KeyboardShortcuts,
    ) -> Self {
        Self {
            focus_id: workspace_widget::card_focus_id(id),
            container_id: workspace_widget::card_widget_id(id),
            close_id: card_control_id(id, "close"),
            browser_back_id: card_control_id(id, "browser.back"),
            browser_forward_id: card_control_id(id, "browser.forward"),
            browser_reload_id: card_control_id(id, "browser.reload"),
            browser_address_id: card_control_id(id, "browser.address"),
            type_label,
            title: title.to_owned(),
            caption: caption.to_owned(),
            focused_name: card_accessible_name(type_label, title, caption, true),
            unfocused_name: card_accessible_name(type_label, title, caption, false),
            close_name: format!(
                "{}, {}",
                card_close_name(type_label, title),
                workspace_widget::shortcut_keys(
                    workspace_widget::GlobalShortcut::CloseFocusedCard,
                    shortcuts,
                ),
            ),
            close_tooltip: workspace_widget::shortcut_tooltip(
                workspace_widget::GlobalShortcut::CloseFocusedCard,
                shortcuts,
            ),
        }
    }

    fn describes(&self, type_label: &str, title: &str, caption: &str) -> bool {
        self.type_label == type_label && self.title == title && self.caption == caption
    }

    fn accessible_name(&self, focused: bool) -> &str {
        if focused {
            &self.focused_name
        } else {
            &self.unfocused_name
        }
    }
}

pub struct Cagents {
    workspace: Workspace,
    durable: Arc<WorkspaceState>,
    cards: HashMap<CardId, CardRuntime>,
    card_chrome: HashMap<CardId, CardChrome>,
    lifecycle: LifecycleManager,
    // A sweep is a full lifecycle reconcile plus a view rebuild, and scrolling
    // requests one per visibility transition, so requests are debounced onto a
    // single in-flight timer.
    lifecycle_sweep_deadline: Option<Instant>,
    lifecycle_sweep_armed: bool,
    terminal_wake_tx: async_channel::Sender<TerminalWakeup>,
    terminal_wake_rx: async_channel::Receiver<TerminalWakeup>,
    session: Option<SessionRuntime>,
    settings: AppSettings,
    // These owners are declared before `core`: application Drop empties and
    // joins them explicitly, and field drop order is the final backstop.
    browser_teardowns: Vec<JoinHandle<()>>,
    terminal_teardowns: Vec<JoinHandle<()>>,
    browser_factory: Option<Arc<RawWaylandSubsurfaceFactory>>,
    browser_parent_lifetime: Option<Arc<ParentWindowLifetime>>,
    window_scale_factor: f32,
    window_width: f32,
    core: Core,
    settings_path: Option<PathBuf>,
    settings_write_blocked: bool,
    sidebar_model: SidebarModel,
    sidebar_state: SidebarState,
    github_repositories: HashMap<ProjectId, GitHubRepoKey>,
    worktree_dialog: Option<WorktreeDialog>,
    pull_request_dialog: Option<PullRequestDialog>,
    frame_clock: FrameClock,
    inspector: Option<Inspector>,
    inspector_transition: Option<InspectorTransition>,
    settings_category: SettingsCategory,
    keyboard_modifiers: keyboard::Modifiers,
    settings_draft: SettingsDraft,
    settings_feedback: Option<SettingsFeedback>,
    status: Option<TransientNotice>,
}

impl Cagents {
    fn bootstrap() -> BootstrapResult {
        let (settings, settings_path, settings_write_blocked, mut status) =
            match settings::default_path() {
                Ok(path) => match settings::load(&path) {
                    Ok(settings) => (settings, Some(path), false, None),
                    Err(error) => (
                        AppSettings::default(),
                        Some(path),
                        true,
                        Some(error.to_string()),
                    ),
                },
                Err(error) => (AppSettings::default(), None, true, Some(error.to_string())),
            };
        let (session, restored, projects, worktrees) = match SessionRuntime::open_default() {
            Ok((mut session, restored)) => {
                let (projects, worktrees) = session.take_catalog();
                (Some(session), restored, projects, worktrees)
            }
            Err(error) => {
                status = Some(error.to_string());
                (None, None, Vec::new(), Vec::new())
            }
        };
        (
            restored.unwrap_or_default(),
            session,
            projects,
            worktrees,
            settings,
            settings_path,
            settings_write_blocked,
            status,
        )
    }

    fn rebuild_workspace(state: &WorkspaceState) -> Workspace {
        let mut workspace = Workspace::new(WorkspaceConfig::default()).unwrap_or_default();
        for card in state.cards.iter().filter(|card| card.existence.is_open()) {
            let width = CardWidth::Fraction(card.width_fraction.clamp(0.25, 1.0));
            let index = workspace.cards().len();
            let _ = workspace.add_card(
                WorkspaceCard::new(card.id, card.title.clone(), width).unwrap_or_else(|_| {
                    WorkspaceCard {
                        id: card.id,
                        accessible_name: "Workspace card".into(),
                        width: CardWidth::Fraction(0.7),
                    }
                }),
                index,
                false,
            );
        }
        if let Some(focused) = state.focused {
            let _ = workspace.focus(focused);
        }
        workspace
    }

    fn repository_load_task(
        path: PathBuf,
        project_id: Option<ProjectId>,
        open_cards: bool,
    ) -> Task<Message> {
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || load_repository(&path, project_id))
                .await
                .map_err(|error| format!("repository loader join failed: {error}"))
                .and_then(|result| result)
                .map(Box::new);
            Message::RepositoryLoaded { open_cards, result }
        })
    }

    fn browser_factory_task(&self) -> Task<Message> {
        let Some(window_id) = self.core.main_window_id() else {
            return cosmic::task::message(Message::BrowserFactoryReady(Err(
                "main Wayland window is unavailable".into(),
            )));
        };
        window::run(window_id, move |parent| {
            let display = parent
                .display_handle()
                .map_err(|error| format!("parent display handle: {error}"))?
                .as_raw();
            let window = parent
                .window_handle()
                .map_err(|error| format!("parent window handle: {error}"))?
                .as_raw();
            let parent_lifetime = Arc::new(ParentWindowLifetime::new(window_id));
            debug_assert_eq!(parent_lifetime.window_id, window_id);
            let lifetime_guard: Arc<dyn Any + Send + Sync> = parent_lifetime.clone();
            // SAFETY: this non-inert guard is retained both by Cagents and the
            // factory. Cagents synchronously joins active/reaping browser
            // workers and drops the factory before deactivating the guard;
            // `core` (the actual parent owner) is declared after all of them.
            let factory = unsafe {
                RawWaylandSubsurfaceFactory::from_raw_handles(display, window, lifetime_guard)
            }
            .map(Arc::new)
            .map_err(|error| error.to_string())?;
            Ok(BrowserFactoryBinding {
                factory,
                parent_lifetime,
            })
        })
        .map(Message::BrowserFactoryReady)
        .map(cosmic::Action::App)
    }

    fn register_deferred_browsers(&mut self) {
        let pending = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                can_register_deferred_browser(
                    card,
                    self.lifecycle.state(card.id).is_some(),
                    matches!(self.cards.get(&card.id), Some(CardRuntime::Loading)),
                )
            })
            .map(|card| (card.id, card.exposure))
            .collect::<Vec<_>>();
        let now = Instant::now();
        for (card, exposure) in pending {
            self.lifecycle
                .register(card, exposure, Residency::Hibernated, now);
            if let Some(state) = self.card_state_mut(card) {
                state.residency = Residency::Hibernated;
            }
        }
    }

    fn initialize_runtimes(&mut self) -> Task<Message> {
        let initially_visible = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && card.exposure == Exposure::Visible
                    && matches!(&card.kind, CardKind::Terminal(_))
            })
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        let initially_prefetch = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && card.exposure == Exposure::Prefetch
                    && matches!(&card.kind, CardKind::Terminal(_))
            })
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        for card in self.durable.cards.clone() {
            if !card.existence.is_open() {
                continue;
            }
            let terminal_residency =
                restored_terminal_residency(&card.kind, card.exposure, &card.residency);
            let runtime_residency =
                restored_runtime_residency(&card.kind, card.exposure, &card.residency);
            match card.kind {
                CardKind::Terminal(session) => {
                    let terminal_residency =
                        terminal_residency.expect("terminal restore has lifecycle residency");
                    self.lifecycle.register(
                        card.id,
                        card.exposure,
                        terminal_residency.clone(),
                        Instant::now(),
                    );
                    if let Some(state) = self.card_state_mut(card.id) {
                        state.residency = terminal_residency;
                    }
                    if let Some(message) = restored_terminal_failure(&session) {
                        self.cards
                            .insert(card.id, CardRuntime::Failed(message.clone()));
                        self.status
                            .get_or_insert_with(|| TransientNotice::error(message));
                    } else {
                        // Restored terminals remain detached until lifecycle
                        // reconciliation admits them under the terminal startup
                        // budget.
                        self.cards.insert(card.id, CardRuntime::Loading);
                    }
                }
                CardKind::Browser(_) => {
                    if let Some(state) = self.card_state_mut(card.id) {
                        state.residency = runtime_residency;
                    }
                    self.cards.insert(card.id, CardRuntime::Loading);
                }
                CardKind::Repository(view) => {
                    self.cards.insert(card.id, CardRuntime::Repository(view));
                }
                CardKind::PullRequests(view) => {
                    self.cards
                        .insert(card.id, CardRuntime::PullRequests(view.repository));
                }
            }
        }

        // Admit restored terminals first. Restored browsers remain unregistered
        // and hibernated until the rendering factory is ready; the resulting
        // lifecycle sweep admits them under the same hot-card budget.
        self.reconcile_lifecycle(&initially_visible, &initially_prefetch)
    }

    fn start_terminal(
        &self,
        card: CardId,
        session: TerminalSession,
        visible: bool,
    ) -> Task<Message> {
        let max_scrollback = self.settings.terminal_scrollback_lines;
        let sender = self.terminal_wake_tx.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let config = TerminalConfig {
                    initially_visible: visible,
                    max_scrollback,
                    ..TerminalConfig::default()
                };
                let mode = terminal_start_mode(&session)?;
                let wake = move |wake: crate::terminal::TerminalWake| {
                    // Each terminal source coalesces visual and semantic edges
                    // until the app acknowledges them. The unbounded app queue
                    // is therefore O(live cards), not O(terminal event volume),
                    // and cannot strand a source edge because a fixed queue filled.
                    let _ = sender.try_send(TerminalWakeup {
                        card,
                        generation: wake.generation,
                    });
                };
                let handle = match mode {
                    TerminalStartMode::Attach(proof) => {
                        HostedTerminalHandle::connect(session.id, Some(proof), wake)
                    }
                    TerminalStartMode::Spawn(recipe) => {
                        let mut session = session;
                        session.launch = recipe;
                        HostedTerminalHandle::attach_or_spawn(session, config, None, wake)
                    }
                }
                .map_err(|error| error.to_string())?;
                let surface = TerminalSurface::from(handle);
                if !visible {
                    surface.hide().map_err(|error| error.to_string())?;
                }
                Ok(surface)
            })
            .await
            .map_err(|error| format!("terminal worker join failed: {error}"))
            .and_then(|result| result);
            Message::TerminalStarted { card, result }
        })
    }

    fn add_terminal(&mut self, working_directory: PathBuf) -> Task<Message> {
        let launch = match LaunchRecipe::shell(working_directory.clone()) {
            Ok(launch) => launch,
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "could not create terminal: {error}"
                )));
                return Task::none();
            }
        };
        let card_id = CardId::new();
        let terminal_id = TerminalId::new();
        let transcript_path = terminal_transcript_path(terminal_id);
        let session = TerminalSession {
            id: terminal_id,
            launch: launch.clone(),
            transcript_path,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        };
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: working_directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Terminal".into()),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating { epoch: 1 },
            kind: CardKind::Terminal(session.clone()),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card.clone());
        durable.focused = Some(card_id);
        let index = self.workspace.cards().len();
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    card.title,
                    CardWidth::Fraction(card.width_fraction),
                )
                .expect("new terminal card is valid"),
                index,
                true,
            )
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                ViewportUpdate::unchanged()
            });
        self.cards.insert(card_id, CardRuntime::Loading);
        self.lifecycle.register(
            card_id,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        self.persist();
        Task::batch([
            self.start_terminal(card_id, session, true),
            self.apply_viewport(update, true, false),
        ])
    }

    fn add_repository_card(
        &mut self,
        project: ProjectId,
        repository: PathBuf,
        worktree: PathBuf,
    ) -> Task<Message> {
        let card_id = CardId::new();
        let view = RepositoryView {
            project,
            repository,
            worktree: worktree.clone(),
        };
        let title = worktree
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Repository".into());
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: title.clone(),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(view.clone()),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card);
        durable.focused = Some(card_id);
        self.cards.insert(card_id, CardRuntime::Repository(view));
        let index = self.workspace.cards().len();
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    title,
                    CardWidth::Fraction(self.settings.default_column_fraction),
                )
                .expect("repository card is valid"),
                index,
                true,
            )
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                ViewportUpdate::unchanged()
            });
        self.persist();
        self.apply_viewport(update, true, false)
    }
    fn open_link(&mut self, url: Url) -> Task<Message> {
        self.show_workspace_for_action();
        match self.settings.link_open_destination {
            LinkOpenDestination::IntegratedBrowser => self.add_browser(url),
            LinkOpenDestination::SystemDefault => cosmic::task::future(async move {
                let result = async {
                    let request = ashpd::desktop::open_uri::OpenFileRequest::default()
                        .send_uri(&url)
                        .await
                        .map_err(|error| error.to_string())?;
                    request.response().map_err(|error| error.to_string())?;
                    Ok(())
                }
                .await;
                Message::ExternalLinkOpened(result)
            }),
        }
    }

    fn configured_link_gesture_is_pressed(&self) -> bool {
        self.keyboard_modifiers.control()
            && match self.settings.link_open_gesture {
                LinkOpenGesture::ControlClick => !self.keyboard_modifiers.shift(),
                LinkOpenGesture::ControlShiftClick => self.keyboard_modifiers.shift(),
            }
    }

    fn open_clicked_link(&mut self, url: Url) -> Task<Message> {
        if self.configured_link_gesture_is_pressed() {
            self.open_link(url)
        } else {
            self.add_browser(url)
        }
    }

    fn add_browser(&mut self, url: Url) -> Task<Message> {
        let card_id = CardId::new();
        let session = BrowserSession {
            id: BrowserId::new(),
            current_url: url.clone(),
            history: vec![url.clone()],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::ExactLive,
        };
        let mut startup_error = None;
        if let Some(factory) = self.browser_factory.clone() {
            match spawn_browser(&session, true, self.window_scale_factor, Some(factory)) {
                Ok(runtime) => {
                    self.cards.insert(card_id, CardRuntime::Browser(runtime));
                }
                Err(error) => {
                    startup_error = Some(error.clone());
                    self.status = Some(TransientNotice::error(error));
                    self.cards.insert(card_id, CardRuntime::Loading);
                }
            }
        } else {
            self.cards.insert(card_id, CardRuntime::Loading);
        }
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(CardState {
            id: card_id,
            generation: durable.generation,
            title: url.host_str().unwrap_or("Browser").into(),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating {
                epoch: INITIAL_HYDRATION_EPOCH,
            },
            kind: CardKind::Browser(session),
        });
        durable.focused = Some(card_id);
        self.lifecycle.register(
            card_id,
            Exposure::Visible,
            Residency::Hydrating {
                epoch: INITIAL_HYDRATION_EPOCH,
            },
            Instant::now(),
        );
        if let Some(error) = startup_error {
            self.fail_runtime(card_id, error);
        }
        let index = self.workspace.cards().len();
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    "Browser",
                    CardWidth::Fraction(self.settings.default_column_fraction),
                )
                .expect("browser card is valid"),
                index,
                true,
            )
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                ViewportUpdate::unchanged()
            });
        self.persist();
        self.apply_viewport(update, true, false)
    }

    fn persist(&self) {
        if let Some(session) = &self.session {
            session.snapshot(Arc::clone(&self.durable));
        }
    }

    fn card_state(&self, id: CardId) -> Option<&CardState> {
        self.durable.cards.iter().find(|card| card.id == id)
    }

    /// Copy-on-write access; callers must confirm a change is needed first, as
    /// this clones the workspace while the writer thread holds the last snapshot.
    fn card_state_mut(&mut self, id: CardId) -> Option<&mut CardState> {
        Arc::make_mut(&mut self.durable)
            .cards
            .iter_mut()
            .find(|card| card.id == id)
    }

    /// Renames a card to the title its own runtime reported, and answers whether
    /// the durable state changed.
    ///
    /// A shell that emits a bare `OSC 0` or a page with no `<title>` reports an
    /// empty string. Storing it would blank the card header, the card's
    /// accessible name and the window title all at once, so the card keeps the
    /// name it already had.
    fn apply_reported_title(&mut self, id: CardId, title: String) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        let _ = self.workspace.rename_card(id, title.to_owned());
        if self
            .card_state(id)
            .is_some_and(|state| state.title != title)
            && let Some(state) = self.card_state_mut(id)
        {
            state.title = title.to_owned();
            return true;
        }
        false
    }

    /// Builds chrome for cards that appeared or were renamed, drops entries for
    /// cards the workspace no longer holds, and renames the window.
    fn sync_chrome(&mut self) -> Task<Message> {
        for card in &self.durable.cards {
            let type_label = card_kind_labels(Some(&card.kind)).type_label;
            let caption = card_kind_caption(&card.kind);
            if self
                .card_chrome
                .get(&card.id)
                .is_none_or(|chrome| !chrome.describes(type_label, &card.title, &caption))
            {
                self.card_chrome.insert(
                    card.id,
                    CardChrome::new(
                        card.id,
                        type_label,
                        &card.title,
                        &caption,
                        &self.settings.keyboard_shortcuts,
                    ),
                );
            }
        }
        if self.card_chrome.len() != self.durable.cards.len() {
            self.card_chrome
                .retain(|id, _| self.durable.cards.iter().any(|card| card.id == *id));
        }
        self.sync_window_title()
    }

    /// Names the window and its header bar after whatever the window is showing.
    ///
    /// `APP_ID` only matches the window to its desktop entry, which supplies the
    /// icon; the title is the sole per-window name a task switcher, window list
    /// or screen reader can announce, and an empty one leaves the app nameless.
    /// Terminal cards carry the title their shell reports over OSC, so this
    /// tracks the focused shell the way a terminal emulator does.
    ///
    /// The header bar shows only what is being shown, because the application
    /// name is already the window's identity everywhere the header is visible;
    /// the window title appends it, because a task switcher shows nothing else.
    /// `header_title` doubles as the record of what was last applied, so an
    /// unchanged title costs one string comparison and no allocation.
    fn sync_window_title(&mut self) -> Task<Message> {
        let header = {
            let showing = self
                .durable
                .focused
                .and_then(|id| self.card_state(id))
                .map(|card| card.title.trim())
                .filter(|title| !title.is_empty())
                .unwrap_or(APPLICATION_NAME);
            if self.core.window.header_title == showing {
                return Task::none();
            }
            showing.to_owned()
        };
        let window_title = if header == APPLICATION_NAME {
            header.clone()
        } else {
            format!("{header}{WINDOW_TITLE_SEPARATOR}{APPLICATION_NAME}")
        };
        self.set_header_title(header);
        match self.core.main_window_id() {
            Some(id) => self.set_window_title(window_title, id),
            None => Task::none(),
        }
    }

    fn finish_initial_hydration(&mut self, card: CardId, result: Result<(), String>) -> bool {
        let epoch = match self.lifecycle.state(card).map(|(_, residency)| residency) {
            Some(Residency::Hydrating { epoch }) => *epoch,
            _ => return false,
        };
        let succeeded = result.is_ok();
        let result = result.map_err(|message| RuntimeError {
            code: "runtime_hydration_failed".into(),
            message,
        });
        if !self.lifecycle.hydration_finished(card, epoch, result) {
            return false;
        }
        if let Some(residency) = self
            .lifecycle
            .state(card)
            .map(|(_, residency)| residency.clone())
            && let Some(state) = self.card_state_mut(card)
        {
            state.residency = residency;
        }
        succeeded && offscreen_hot(&self.lifecycle, card)
    }

    fn mark_runtime_failed(&mut self, card: CardId, message: String) {
        if matches!(
            self.lifecycle.state(card).map(|(_, residency)| residency),
            Some(Residency::Hydrating { .. })
        ) {
            self.finish_initial_hydration(card, Err(message));
            return;
        }
        let Some(exposure) = self.lifecycle.state(card).map(|(exposure, _)| *exposure) else {
            return;
        };
        let residency = Residency::Failed {
            typed_error: RuntimeError {
                code: "runtime_failed".into(),
                message,
            },
            retryable: true,
        };
        self.lifecycle
            .register(card, exposure, residency.clone(), Instant::now());
        if let Some(state) = self.card_state_mut(card) {
            state.residency = residency;
        }
    }
    fn fail_runtime(&mut self, card: CardId, message: String) {
        match self.cards.remove(&card) {
            Some(CardRuntime::Terminal(handle)) => {
                self.begin_terminal_teardown(handle);
            }
            Some(CardRuntime::Browser(browser)) => {
                let _ = browser.handle.set_visible(false);
                self.begin_browser_teardown(browser.handle);
            }
            Some(
                CardRuntime::Loading
                | CardRuntime::Failed(_)
                | CardRuntime::Repository(_)
                | CardRuntime::PullRequests(_),
            )
            | None => {}
        }
        self.cards
            .insert(card, CardRuntime::Failed(message.clone()));
        self.mark_runtime_failed(card, message);
    }

    fn retry_card(&mut self, card: CardId) -> Task<Message> {
        let Some(mut state) = self
            .durable
            .cards
            .iter()
            .find(|state| state.id == card && state.existence.is_open())
            .cloned()
        else {
            return Task::none();
        };
        let terminal_replacement = if let CardKind::Terminal(session) = &mut state.kind {
            let expected_terminal_id = session.id;
            let replacement = fresh_terminal_relaunch(session);
            *session = replacement.clone();
            Some((expected_terminal_id, replacement))
        } else {
            None
        };
        let epoch = unix_millis().max(1) as u64;
        let residency = Residency::Hydrating { epoch };
        self.lifecycle
            .register(card, state.exposure, residency.clone(), Instant::now());
        if let Some(durable_state) = self.card_state_mut(card) {
            durable_state.kind = state.kind.clone();
            durable_state.residency = residency;
        }
        self.status = None;
        if let Some((expected_terminal_id, replacement)) = terminal_replacement
            && let Some(session) = &self.session
        {
            session.replace_terminal_identity(card, expected_terminal_id, replacement);
        }
        self.persist();

        let surfaces_visible = self.inspector.is_none();
        match state.kind {
            CardKind::Terminal(session) => {
                self.cards.insert(card, CardRuntime::Loading);
                self.start_terminal(
                    card,
                    session,
                    state.exposure == Exposure::Visible && surfaces_visible,
                )
            }
            CardKind::Browser(session) => {
                self.cards.insert(card, CardRuntime::Loading);
                let Some(factory) = self.browser_factory.clone() else {
                    return cosmic::task::message(Message::InitializeBrowserFactory);
                };
                match spawn_browser(
                    &session,
                    state.exposure == Exposure::Visible && surfaces_visible,
                    self.window_scale_factor,
                    Some(factory),
                ) {
                    Ok(runtime) => {
                        self.cards.insert(card, CardRuntime::Browser(runtime));
                    }
                    Err(error) => {
                        self.fail_runtime(card, error.clone());
                        self.status = Some(TransientNotice::error(error));
                    }
                }
                Task::none()
            }
            CardKind::Repository(view) => {
                self.cards.insert(card, CardRuntime::Repository(view));
                Task::none()
            }
            CardKind::PullRequests(view) => {
                self.cards
                    .insert(card, CardRuntime::PullRequests(view.repository));
                Task::none()
            }
        }
    }

    fn begin_terminal_teardown(&mut self, handle: TerminalSurface) {
        let mut index = 0;
        while index < self.terminal_teardowns.len() {
            if self.terminal_teardowns[index].is_finished() {
                let finished = self.terminal_teardowns.swap_remove(index);
                let _ = finished.join();
            } else {
                index += 1;
            }
        }

        let fallback = handle.clone();
        match std::thread::Builder::new()
            .name("cagents-terminal-reaper".into())
            .spawn(move || {
                let _ = handle.shutdown();
            }) {
            Ok(join) => self.terminal_teardowns.push(join),
            Err(error) => {
                if let Err(shutdown_error) = fallback.shutdown() {
                    self.status = Some(TransientNotice::error(format!(
                        "could not stop terminal after reaper failure ({error}): {shutdown_error}"
                    )));
                }
            }
        }
    }

    fn begin_browser_teardown(&mut self, mut handle: BrowserHandle) {
        let mut index = 0;
        while index < self.browser_teardowns.len() {
            if self.browser_teardowns[index].is_finished() {
                let finished = self.browser_teardowns.swap_remove(index);
                let _ = finished.join();
            } else {
                index += 1;
            }
        }

        handle.request_shutdown();
        match std::thread::Builder::new()
            .name("cagents-browser-reaper".into())
            .spawn(move || handle.shutdown())
        {
            Ok(join) => self.browser_teardowns.push(join),
            Err(error) => {
                // The failed spawn drops the closure and therefore the handle;
                // BrowserHandle::drop synchronously joins the worker.
                self.status = Some(TransientNotice::error(format!(
                    "could not start browser teardown worker: {error}"
                )));
            }
        }
    }

    fn synchronize_workspace_scroll(&mut self) -> Task<Message> {
        let offset = self.workspace.camera().offset();
        self.workspace.record_programmatic_scroll(offset);
        workspace_widget::scroll_to(workspace_widget::workspace_scroll_id(), offset)
            .map(cosmic::Action::App)
    }

    fn apply_viewport(
        &mut self,
        update: ViewportUpdate,
        synchronize_scroll: bool,
        additional_durable_change: bool,
    ) -> Task<Message> {
        let focused = self.workspace.focused();
        let viewport_durable_changed =
            durable_viewport_update_pending(&self.durable, focused, &update.transitions)
                && apply_durable_viewport_update(
                    Arc::make_mut(&mut self.durable),
                    focused,
                    &update.transitions,
                );
        let durable_changed = additional_durable_change || viewport_durable_changed;
        let mut tasks = Vec::new();
        let mut schedule_delayed_sweep = false;
        let mut reconcile_now = false;
        let surfaces_visible = self.inspector.is_none();
        for transition in update.transitions {
            let browser_card = self
                .durable
                .cards
                .iter()
                .find(|card| card.id == transition.id)
                .is_some_and(|card| matches!(&card.kind, CardKind::Browser(_)));
            match self.cards.get(&transition.id) {
                Some(CardRuntime::Terminal(handle)) => {
                    if transition.to == VisibilityClass::Visible && surfaces_visible {
                        let _ = handle.reveal();
                    } else {
                        let _ = handle.hide();
                    }
                }
                Some(CardRuntime::Browser(browser)) => {
                    let _ = browser
                        .handle
                        .set_visible(transition.to == VisibilityClass::Visible && surfaces_visible);
                }
                _ => {}
            }
            match transition.to {
                VisibilityClass::Offscreen => schedule_delayed_sweep = true,
                VisibilityClass::Overscan if browser_card => {
                    reconcile_now = true;
                    schedule_delayed_sweep = true;
                }
                VisibilityClass::Visible | VisibilityClass::Overscan => reconcile_now = true,
            }
        }
        if synchronize_scroll && update.offset_changed {
            tasks.push(self.synchronize_workspace_scroll());
        }
        if let Some(card) = self.workspace.take_widget_focus_request() {
            tasks.push(workspace_widget::focus_card(card).map(cosmic::Action::App));
        }
        if reconcile_now {
            tasks.push(cosmic::task::message(Message::LifecycleSweep));
        }
        if schedule_delayed_sweep {
            let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
            tasks.push(self.request_lifecycle_sweep(delay));
        }
        if durable_changed {
            self.persist();
        }
        Task::batch(tasks)
    }

    /// Debounces delayed sweeps onto one in-flight timer.
    ///
    /// The deadline only ever moves later, so a burst of requests collapses to
    /// a single reconcile roughly `delay` after the last request.
    fn request_lifecycle_sweep(&mut self, delay: Duration) -> Task<Message> {
        let Some(requested) = Instant::now().checked_add(delay) else {
            // A configured delay this large can never elapse anyway.
            return Task::none();
        };
        let deadline = self.lifecycle_sweep_deadline.get_or_insert(requested);
        if requested > *deadline {
            *deadline = requested;
        }
        let sleep = deadline.saturating_duration_since(Instant::now());
        if self.lifecycle_sweep_armed {
            return Task::none();
        }
        self.lifecycle_sweep_armed = true;
        lifecycle_sweep_timer(sleep)
    }

    fn lifecycle_sweep_elapsed(&mut self) -> Task<Message> {
        self.lifecycle_sweep_armed = false;
        let Some(deadline) = self.lifecycle_sweep_deadline else {
            return Task::none();
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            // Later requests pushed the deadline out; wait for the remainder
            // rather than sweeping early.
            self.lifecycle_sweep_armed = true;
            return lifecycle_sweep_timer(remaining);
        }
        self.lifecycle_sweep_deadline = None;
        self.lifecycle_sweep()
    }

    fn lifecycle_sweep(&mut self) -> Task<Message> {
        let browser_ids = self
            .durable
            .cards
            .iter()
            .filter_map(|card| matches!(&card.kind, CardKind::Browser(_)).then_some(card.id))
            .collect::<HashSet<_>>();
        let visible = self
            .workspace
            .order()
            .filter(|id| self.workspace.visibility(*id) == Some(VisibilityClass::Visible))
            .collect::<HashSet<_>>();
        let prefetch = self
            .workspace
            .order()
            .filter(|id| {
                self.workspace.visibility(*id) == Some(VisibilityClass::Overscan)
                    && !browser_ids.contains(id)
            })
            .collect::<HashSet<_>>();
        self.reconcile_lifecycle(&visible, &prefetch)
    }

    fn reconcile_lifecycle(
        &mut self,
        visible: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
    ) -> Task<Message> {
        let now = Instant::now();
        let surfaces_visible = self.inspector.is_none();
        let actions = self.lifecycle.reconcile(visible, prefetch, now);
        // Actions are only emitted for real transitions, so an empty sweep left
        // the durable state untouched and needs no snapshot.
        let durable_changed = !actions.is_empty();
        let mut tasks = Vec::new();
        let mut schedule_sweep = false;
        for action in actions {
            match action {
                LifecycleAction::SetExposure { card, exposure } => {
                    if let Some(state) = self.card_state_mut(card) {
                        state.exposure = exposure;
                    }
                }
                LifecycleAction::Hydrate { card, epoch } => {
                    if let Some(state) = self.card_state_mut(card) {
                        state.residency = Residency::Hydrating { epoch };
                    }
                    let deferred_session = deferred_terminal_session(
                        self.durable.cards.iter().find(|state| state.id == card),
                        matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
                    );
                    if let Some(session) = deferred_session {
                        tasks.push(self.start_terminal(
                            card,
                            session,
                            visible.contains(&card) && surfaces_visible,
                        ));
                        continue;
                    }
                    let mut deferred_browser_started = false;
                    let deferred_browser = self
                        .durable
                        .cards
                        .iter()
                        .find(|state| state.id == card)
                        .and_then(|state| match &state.kind {
                            CardKind::Browser(session)
                                if matches!(self.cards.get(&card), Some(CardRuntime::Loading)) =>
                            {
                                Some(session.clone())
                            }
                            _ => None,
                        });
                    if let Some(session) = deferred_browser {
                        let Some(factory) = self.browser_factory.clone() else {
                            tasks.push(cosmic::task::message(Message::InitializeBrowserFactory));
                            continue;
                        };
                        match spawn_browser(
                            &session,
                            visible.contains(&card) && surfaces_visible,
                            self.window_scale_factor,
                            Some(factory),
                        ) {
                            Ok(runtime) => {
                                self.cards.insert(card, CardRuntime::Browser(runtime));
                                deferred_browser_started = true;
                            }
                            Err(message) => {
                                self.fail_runtime(card, message.clone());
                                self.status = Some(TransientNotice::error(message));
                                tasks.push(cosmic::task::message(Message::LifecycleSweep));
                                continue;
                            }
                        }
                    }
                    let result = if deferred_browser_started {
                        Some(Ok(()))
                    } else {
                        match self.cards.get(&card) {
                            Some(CardRuntime::Terminal(handle)) => Some(
                                handle
                                    .restore(visible.contains(&card) && surfaces_visible)
                                    .map_err(|error| error.to_string()),
                            ),
                            Some(CardRuntime::Browser(browser)) => {
                                match browser.handle.restore().and_then(|()| {
                                    browser
                                        .handle
                                        .set_visible(visible.contains(&card) && surfaces_visible)
                                }) {
                                    Ok(()) => None,
                                    Err(error) => Some(Err(error.to_string())),
                                }
                            }
                            Some(CardRuntime::Loading) => None,
                            _ => Some(Ok(())),
                        }
                    };
                    let Some(result) = result else {
                        continue;
                    };
                    match result {
                        Ok(()) => {
                            self.lifecycle.hydration_finished(card, epoch, Ok(()));
                            if let Some(state) = self.card_state_mut(card) {
                                state.residency = Residency::Hot;
                            }
                            schedule_sweep |= offscreen_hot(&self.lifecycle, card);
                        }
                        Err(message) => {
                            self.fail_runtime(card, message.clone());
                            self.status = Some(TransientNotice::error(message));
                            tasks.push(cosmic::task::message(Message::LifecycleSweep));
                        }
                    }
                }
                LifecycleAction::Hibernate { card, epoch } => {
                    let result = match self.cards.get(&card) {
                        Some(CardRuntime::Terminal(handle)) => {
                            handle.hibernate().map_err(|error| error.to_string())
                        }
                        Some(CardRuntime::Browser(browser)) => browser
                            .handle
                            .hibernate()
                            .map_err(|error| error.to_string()),
                        _ => Ok(()),
                    };
                    match result {
                        Ok(()) => {
                            self.lifecycle.hibernation_finished(card, epoch);
                            if let Some(state) = self.card_state_mut(card) {
                                state.residency = Residency::Hibernated;
                            }
                        }
                        Err(message) => {
                            if let Some(residency) =
                                rollback_failed_hibernation(&mut self.lifecycle, card, epoch, now)
                            {
                                if let Some(state) = self.card_state_mut(card) {
                                    state.residency = residency;
                                }
                                schedule_sweep = true;
                            }
                            self.status = Some(TransientNotice::error(message));
                        }
                    }
                }
            }
        }
        if durable_changed {
            self.persist();
        }
        if schedule_sweep {
            let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
            tasks.push(self.request_lifecycle_sweep(delay));
        }
        Task::batch(tasks)
    }

    fn drain_terminal(&mut self, card: CardId, _generation: u64) {
        let (proof, collected) = {
            let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                return;
            };
            let events = handle.events();
            (handle.host_proof(), events.try_iter().collect::<Vec<_>>())
        };
        let mut durable_changed = false;
        if let Some(proof) = proof {
            let live = TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            };
            // The host republishes its proof on every wake, so this is reached at
            // PTY-output rate. Only a genuine identity change is durable, and
            // `card_state_mut` copies the snapshot the writer thread still holds.
            if let Some(current) = terminal_continuity(self.card_state(card))
                && *current != live
                && let Some(state) = self.card_state_mut(card)
                && let CardKind::Terminal(session) = &mut state.kind
            {
                session.continuity = live;
                durable_changed = true;
            }
        }
        for event in collected {
            match event {
                TerminalEvent::TitleChanged(title) => {
                    durable_changed |= self.apply_reported_title(card, title);
                }
                TerminalEvent::Bell => {}
                TerminalEvent::Exited { exit_code, signal } => {
                    let message = match signal {
                        Some(signal) => format!("terminal exited with {signal}"),
                        None => format!("terminal exited with status {exit_code}"),
                    };
                    if let Some(state) = self.card_state_mut(card)
                        && let CardKind::Terminal(session) = &mut state.kind
                    {
                        transition_terminal_to_relaunchable(session, terminal_exit(exit_code));
                    }
                    self.fail_runtime(card, message.clone());
                    durable_changed = true;
                    self.status = Some(TransientNotice::error(message));
                }
                TerminalEvent::Error { operation, message } => {
                    let message = format!("terminal {operation}: {message}");
                    if operation == "connect to cagents daemon"
                        && let Some(state) = self.card_state_mut(card)
                        && let CardKind::Terminal(session) = &mut state.kind
                        && matches!(
                            &session.continuity,
                            TerminalContinuity::Live { .. }
                                | TerminalContinuity::DetachedSameBoot { .. }
                        )
                    {
                        session.continuity = TerminalContinuity::RelaunchAvailable {
                            recipe: session.launch.clone(),
                            last_exit: None,
                        };
                    }
                    self.fail_runtime(card, message.clone());
                    durable_changed = true;
                    self.status = Some(TransientNotice::error(message));
                }
            }
        }
        if durable_changed {
            self.persist();
        }
    }

    fn handle_browser_event(&mut self, id: CardId, event: BrowserEvent) -> Task<Message> {
        let mut tasks = Vec::new();
        let mut durable_changed = false;
        match event {
            BrowserEvent::UrlChanged(url) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&id) {
                    browser.address = url.as_str().to_owned();
                }
                if let Some(card) = self.card_state_mut(id)
                    && let CardKind::Browser(session) = &mut card.kind
                {
                    session.current_url = url;
                    durable_changed = true;
                }
            }
            BrowserEvent::HistoryChanged { entries, current } => {
                if let Some(card) = self.card_state_mut(id)
                    && let CardKind::Browser(session) = &mut card.kind
                {
                    session.history = entries;
                    session.history_index = current;
                    durable_changed = true;
                }
            }
            BrowserEvent::TitleChanged(Some(title)) => {
                durable_changed |= self.apply_reported_title(id, title);
            }
            BrowserEvent::InputMethod(ime) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&id) {
                    browser.ime = ime;
                }
            }
            BrowserEvent::Delegate(delegate) => match delegate {
                DelegateRequest::ClipboardRead { id: request } => {
                    tasks.push(
                        iced::clipboard::read()
                            .map(move |value| Message::ClipboardRead {
                                card: id,
                                request,
                                value,
                            })
                            .map(cosmic::Action::App),
                    );
                }
                DelegateRequest::ClipboardWrite { text } => {
                    tasks.push(iced::clipboard::write(text).map(cosmic::Action::App));
                }
                DelegateRequest::ClipboardClear => {
                    tasks.push(iced::clipboard::write(String::new()).map(cosmic::Action::App));
                }
                DelegateRequest::NewWindow { url: Some(url) } => {
                    tasks.push(cosmic::task::message(Message::OpenBrowser(url)));
                }
                DelegateRequest::NewWindow { url: None } => {
                    let url = Url::parse("about:blank").expect("about:blank is a valid URL");
                    tasks.push(cosmic::task::message(Message::OpenBrowser(url)));
                }
                DelegateRequest::FilePicker {
                    id: request,
                    multiple,
                    current_paths,
                    filters,
                } => {
                    tasks.push(cosmic::task::future(async move {
                        let mut dialog = file_chooser::open::Dialog::new().title("Choose file");
                        if let Some(current) = current_paths.first() {
                            let directory = if current.is_dir() {
                                current.clone()
                            } else {
                                current
                                    .parent()
                                    .map(Path::to_path_buf)
                                    .unwrap_or_else(|| current.clone())
                            };
                            dialog = dialog.directory(directory);
                        }
                        if !filters.is_empty() {
                            let mut filter = file_chooser::FileFilter::new("Accepted files");
                            for pattern in filters {
                                filter = if pattern.contains('/') {
                                    filter.mimetype(&pattern)
                                } else {
                                    filter.glob(&pattern)
                                };
                            }
                            dialog = dialog.filter(filter);
                        }
                        let result = if multiple {
                            dialog.open_files().await.map(|response| {
                                response
                                    .urls()
                                    .iter()
                                    .filter_map(|url| url.to_file_path().ok())
                                    .collect::<Vec<_>>()
                            })
                        } else {
                            dialog.open_file().await.map(|response| {
                                response
                                    .url()
                                    .to_file_path()
                                    .ok()
                                    .into_iter()
                                    .collect::<Vec<_>>()
                            })
                        };
                        let result = match result {
                            Ok(paths) => Ok(Some(paths)),
                            Err(file_chooser::Error::Cancelled) => Ok(None),
                            Err(error) => Err(error.to_string()),
                        };
                        Message::FilePickerResult {
                            card: id,
                            request,
                            result,
                        }
                    }));
                }
            },
            BrowserEvent::LifecycleChanged(lifecycle) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&id) {
                    browser.lifecycle = lifecycle;
                }
            }
            BrowserEvent::Error(error) => {
                let message = error.to_string();
                self.status = Some(TransientNotice::error(message));
            }
            BrowserEvent::Crashed { reason, .. } => {
                self.fail_runtime(id, reason.clone());
                durable_changed = true;
                self.status = Some(TransientNotice::error(format!("Browser crashed: {reason}")));
                tasks.push(cosmic::task::message(Message::LifecycleSweep));
            }
            BrowserEvent::ClosedByContent => {
                tasks.push(cosmic::task::message(Message::CloseCard(id)));
            }
            BrowserEvent::Ready { .. } => {
                if self.finish_initial_hydration(id, Ok(())) {
                    let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
                    tasks.push(self.request_lifecycle_sweep(delay));
                }
                durable_changed = true;
            }
            BrowserEvent::LoadStateChanged(state) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&id) {
                    browser.load_state = (!state.eq_ignore_ascii_case("complete")).then_some(state);
                }
            }
            BrowserEvent::FocusChanged(_)
            | BrowserEvent::InputHandled { .. }
            | BrowserEvent::TitleChanged(None) => {}
        }
        if durable_changed {
            self.persist();
        }
        Task::batch(tasks)
    }

    fn card_view<'a>(&'a self, id: CardId, focused: bool) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let card = self.card_state(id);
        let title = card.map_or("Card", |card| card.title.as_str());
        let labels = card_kind_labels(card.map(|card| &card.kind));
        // `sync_chrome` keeps an entry for every durable card; a miss can
        // only be a card that is not durable, which renders degenerate chrome.
        let chrome = self.card_chrome.get(&id);
        let browser_runtime = match self.cards.get(&id) {
            Some(CardRuntime::Browser(browser)) => Some(browser),
            _ => None,
        };
        let browser_session = card.and_then(|card| match &card.kind {
            CardKind::Browser(session) => Some(session),
            _ => None,
        });
        let caption = card.map(|card| card_kind_caption(&card.kind));

        let mut title_lines: Vec<Element<'a, Message>> = vec![
            widget::text::body(title)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ];
        if let Some(caption) = caption
            .clone()
            .filter(|caption| !title_abbreviates_path(title, caption))
        {
            title_lines.push(
                widget::text::caption(caption)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            );
        }
        let title_button = widget::button::custom(
            widget::row::with_children(vec![
                widget::icon::from_name(labels.icon).size(16).into(),
                widget::column::with_children(title_lines)
                    .spacing(0)
                    .width(Length::Fill)
                    .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
        )
        .id(chrome.map_or_else(
            || workspace_widget::card_focus_id(id),
            |chrome| chrome.focus_id.clone(),
        ))
        .name(chrome.map_or_else(
            || {
                Cow::Owned(card_accessible_name(
                    labels.type_label,
                    title,
                    caption.as_deref().unwrap_or(""),
                    focused,
                ))
            },
            |chrome| Cow::Borrowed(chrome.accessible_name(focused)),
        ))
        .padding([0, spacing.space_xxs])
        .height(Length::Fill)
        .width(Length::Fill)
        .class(cosmic::theme::Button::Text)
        .selected(focused)
        .on_press(Message::FocusCard(id));
        let mut title_tooltip: Vec<Element<'a, Message>> =
            vec![widget::text::caption(title).into()];
        if let Some(caption) = caption.clone() {
            title_tooltip.push(widget::text::caption(caption).into());
        }
        let title_region: Element<'a, Message> = widget::tooltip::tooltip(
            title_button,
            widget::column::with_children(title_tooltip).spacing(0),
            widget::tooltip::Position::Bottom,
        )
        .into();

        let mut header_children = vec![title_region];
        let runtime_status = match self.cards.get(&id) {
            Some(CardRuntime::Loading) | None => Some((labels.starting, true)),
            Some(CardRuntime::Failed(_)) => Some(("Unavailable", false)),
            Some(CardRuntime::Browser(browser))
                if browser.lifecycle == BrowserLifecycle::Restoring =>
            {
                Some(("Restoring…", true))
            }
            Some(CardRuntime::Browser(browser)) if browser.load_state.is_some() => {
                Some(("Loading…", true))
            }
            _ => None,
        };
        if let Some((label, busy)) = runtime_status {
            if busy {
                header_children.push(
                    widget::progress_bar::circular::Circular::new()
                        .size(16.0)
                        .into(),
                );
            }
            header_children.push(widget::text::caption(label).into());
        }
        header_children.push(
            widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                .id(chrome.map_or_else(
                    || card_control_id(id, "close"),
                    |chrome| chrome.close_id.clone(),
                ))
                .name(chrome.map_or_else(
                    || {
                        Cow::Owned(format!(
                            "{}, {}",
                            card_close_name(labels.type_label, title),
                            workspace_widget::shortcut_keys(
                                workspace_widget::GlobalShortcut::CloseFocusedCard,
                                &self.settings.keyboard_shortcuts,
                            ),
                        ))
                    },
                    |chrome| Cow::Borrowed(chrome.close_name.as_str()),
                ))
                .tooltip(chrome.map_or_else(
                    || {
                        Cow::Owned(workspace_widget::shortcut_tooltip(
                            workspace_widget::GlobalShortcut::CloseFocusedCard,
                            &self.settings.keyboard_shortcuts,
                        ))
                    },
                    |chrome| Cow::Borrowed(chrome.close_tooltip.as_str()),
                ))
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press(Message::CloseCard(id))
                .into(),
        );

        let rail = widget::container(iced::widget::Space::new())
            .height(Length::Fixed(FOCUS_RAIL_HEIGHT))
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(move |theme| {
                let color = if focused {
                    *theme.cosmic().accent_color()
                } else {
                    *theme.cosmic().bg_divider()
                };
                cosmic::iced::widget::container::Style {
                    background: Some(iced::Background::Color(color.into())),
                    ..Default::default()
                }
            }));
        let header_row = widget::container(
            widget::row::with_children(header_children)
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxxs)
                .width(Length::Fill),
        )
        .height(Length::Fixed(CARD_HEADER_HEIGHT - FOCUS_RAIL_HEIGHT))
        .padding([0, spacing.space_xxs])
        .class(if focused {
            cosmic::theme::Container::Secondary
        } else {
            cosmic::theme::Container::Background
        });
        let header = widget::column::with_children(vec![rail.into(), header_row.into()])
            .spacing(0)
            .height(Length::Fixed(CARD_HEADER_HEIGHT));

        let browser_toolbar: Option<Element<'a, Message>> = browser_session.map(|session| {
            let address = browser_runtime
                .map(|browser| browser.address.as_str())
                .unwrap_or_else(|| session.current_url.as_str());
            let active = browser_runtime.is_some();
            let can_go_back = active && session.history_index > 0;
            let can_go_forward =
                active && session.history_index.saturating_add(1) < session.history.len();
            let mut address_input = widget::text_input("URL", address)
                .id(chrome.map_or_else(
                    || card_control_id(id, "browser.address"),
                    |chrome| chrome.browser_address_id.clone(),
                ))
                .width(Length::Fill);
            if active {
                address_input = address_input
                    .on_input(move |value| Message::BrowserAddressChanged { card: id, value })
                    .on_submit(move |_| Message::BrowserNavigate(id));
            }
            let toolbar = widget::container(
                widget::row::with_children(vec![
                    widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                        .id(chrome.map_or_else(
                            || card_control_id(id, "browser.back"),
                            |chrome| chrome.browser_back_id.clone(),
                        ))
                        .name("Back")
                        .tooltip("Back")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press_maybe(can_go_back.then_some(Message::BrowserBack(id)))
                        .into(),
                    widget::button::icon(widget::icon::from_name("go-next-symbolic"))
                        .id(chrome.map_or_else(
                            || card_control_id(id, "browser.forward"),
                            |chrome| chrome.browser_forward_id.clone(),
                        ))
                        .name("Forward")
                        .tooltip("Forward")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press_maybe(can_go_forward.then_some(Message::BrowserForward(id)))
                        .into(),
                    widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
                        .id(chrome.map_or_else(
                            || card_control_id(id, "browser.reload"),
                            |chrome| chrome.browser_reload_id.clone(),
                        ))
                        .name("Reload")
                        .tooltip("Reload")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press_maybe(active.then_some(Message::BrowserReload(id)))
                        .into(),
                    address_input.into(),
                ])
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxxs)
                .width(Length::Fill),
            )
            .height(Length::Fixed(BROWSER_TOOLBAR_HEIGHT))
            .padding([0, spacing.space_xxs])
            .class(cosmic::theme::Container::Background);
            let browser_loading = match self.cards.get(&id) {
                Some(CardRuntime::Browser(browser)) => {
                    browser.lifecycle == BrowserLifecycle::Restoring || browser.load_state.is_some()
                }
                Some(CardRuntime::Loading) | None => true,
                _ => false,
            };
            let mut rows: Vec<Element<'a, Message>> =
                vec![widget::divider::horizontal::light().into(), toolbar.into()];
            if browser_loading {
                rows.push(
                    widget::container(iced::widget::Space::new())
                        .height(Length::Fixed(BROWSER_PROGRESS_HEIGHT))
                        .width(Length::Fill)
                        .class(cosmic::theme::Container::custom(|theme| {
                            cosmic::iced::widget::container::Style {
                                background: Some(iced::Background::Color(
                                    (*theme.cosmic().accent_color()).into(),
                                )),
                                ..Default::default()
                            }
                        }))
                        .into(),
                );
            }
            widget::column::with_children(rows).spacing(0).into()
        });

        let content: Element<'a, Message> = match self.cards.get(&id) {
            Some(CardRuntime::Terminal(handle)) => TerminalWidget::new(handle.clone())
                .font_size(self.settings.terminal_font_size)
                .font_family(&self.settings.terminal_font_family)
                .scale_factor(self.window_scale_factor)
                .model_focused(focused)
                .on_focus(Message::FocusCard(id))
                .on_command_error(move |error| Message::TerminalError {
                    card: id,
                    message: error.to_string(),
                })
                .into(),
            Some(CardRuntime::Browser(browser)) => BrowserWidget::new(&browser.handle)
                .model_focused(focused)
                .on_focus(Message::FocusCard(id))
                .scale_factor(self.window_scale_factor)
                .virtual_offset(self.workspace.camera().offset())
                .input_method(browser.ime.clone())
                .on_error(move |error| Message::BrowserError {
                    card: id,
                    message: error.to_string(),
                })
                .into(),
            Some(CardRuntime::Repository(view)) => repository_card(view),
            Some(CardRuntime::PullRequests(repository)) => widget::container(
                widget::column::with_children(vec![
                    widget::icon::from_name("vcs-merge-request-symbolic")
                        .size(24)
                        .into(),
                    widget::text::title3("No pull request selected").into(),
                    widget::text::body(
                        "Choose a pull request from Projects to open it in a browser column.",
                    )
                    .into(),
                    widget::text::caption(repository.to_string_lossy()).into(),
                    widget::button::standard("Search projects")
                        .on_press(Message::FocusProjectSearch)
                        .into(),
                ])
                .align_x(Alignment::Center)
                .spacing(spacing.space_xxs),
            )
            .center(Length::Fill)
            .padding(spacing.space_m)
            .into(),
            Some(CardRuntime::Failed(message)) => widget::container(
                widget::container(
                    widget::column::with_children(vec![
                        widget::row::with_children(vec![
                            widget::icon::from_name("dialog-warning-symbolic")
                                .size(24)
                                .into(),
                            widget::text::title3(labels.unavailable).into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_xxs)
                        .into(),
                        widget::selectable_text(message)
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                            .width(Length::Fill)
                            .into(),
                        widget::row::with_children(vec![
                            widget::button::suggested("Retry")
                                .id(card_control_id(id, "retry"))
                                .name(format!("Retry {}", labels.type_label))
                                .on_press(Message::RetryCard(id))
                                .into(),
                            widget::button::standard("Close")
                                .id(card_control_id(id, "failed.close"))
                                .name(format!("Close {}", labels.type_label))
                                .on_press(Message::CloseCard(id))
                                .into(),
                        ])
                        .spacing(spacing.space_xxs)
                        .into(),
                    ])
                    .spacing(spacing.space_s),
                )
                .class(cosmic::theme::Container::Secondary)
                .max_width(520)
                .padding(spacing.space_m),
            )
            .center(Length::Fill)
            .into(),
            Some(CardRuntime::Loading) | None => widget::container(
                widget::column::with_children(vec![
                    widget::progress_bar::circular::Circular::new()
                        .size(24.0)
                        .into(),
                    widget::text::title3(labels.starting).into(),
                ])
                .align_x(Alignment::Center)
                .spacing(spacing.space_xxs),
            )
            .center(Length::Fill)
            .into(),
        };

        let mut rows: Vec<Element<'a, Message>> = vec![header.into()];
        if let Some(toolbar) = browser_toolbar {
            rows.push(toolbar);
        } else {
            rows.push(widget::divider::horizontal::default().into());
        }
        rows.push(content);
        widget::container(
            widget::column::with_children(rows)
                .spacing(0)
                .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn synchronize_sidebar_selection(&mut self) {
        let Some(card) = self.workspace.focused().and_then(|id| self.card_state(id)) else {
            return;
        };
        let action = match &card.kind {
            CardKind::Repository(view) => self
                .sidebar_model
                .projects
                .iter()
                .find(|project| project.id == view.project)
                .and_then(|project| {
                    project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.path == view.worktree)
                        .map(|worktree| SidebarAction::OpenWorktree {
                            project_id: project.id,
                            worktree_id: worktree.id,
                            path: worktree.path.clone(),
                        })
                        .or_else(|| {
                            Some(SidebarAction::OpenRepository {
                                project_id: project.id,
                                repository: project.repository.clone(),
                            })
                        })
                }),
            CardKind::Terminal(session) => self.sidebar_model.projects.iter().find_map(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == session.launch.working_directory)
                    .map(|worktree| SidebarAction::OpenWorktree {
                        project_id: project.id,
                        worktree_id: worktree.id,
                        path: worktree.path.clone(),
                    })
            }),
            CardKind::Browser(session) => self.sidebar_model.projects.iter().find_map(|project| {
                project
                    .pull_requests
                    .iter()
                    .find(|pull_request| pull_request.url == session.current_url)
                    .map(|pull_request| SidebarAction::OpenPullRequest {
                        project_id: project.id,
                        number: pull_request.number,
                        url: pull_request.url.clone(),
                    })
            }),
            CardKind::PullRequests(view) => view.selected_number.and_then(|number| {
                self.sidebar_model
                    .projects
                    .iter()
                    .find(|project| project.repository == view.repository)
                    .and_then(|project| {
                        project
                            .pull_requests
                            .iter()
                            .find(|pull_request| pull_request.number == number)
                            .map(|pull_request| SidebarAction::OpenPullRequest {
                                project_id: project.id,
                                number,
                                url: pull_request.url.clone(),
                            })
                    })
            }),
        };
        if let Some(action) = action {
            self.sidebar_state.apply(&action);
        }
    }

    fn set_workspace_surfaces_visible(&mut self, workspace_visible: bool) {
        let visible_cards = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && card.exposure == Exposure::Visible)
            .map(|card| card.id)
            .collect::<HashSet<_>>();
        let mut failure = None;
        for (id, runtime) in &self.cards {
            let visible = workspace_visible && visible_cards.contains(id);
            let result = match runtime {
                CardRuntime::Terminal(surface) => if visible {
                    surface.reveal()
                } else {
                    surface.hide()
                }
                .map_err(|error| error.to_string()),
                CardRuntime::Browser(browser) => browser
                    .handle
                    .set_visible(visible)
                    .map_err(|error| error.to_string()),
                CardRuntime::Loading
                | CardRuntime::Failed(_)
                | CardRuntime::Repository(_)
                | CardRuntime::PullRequests(_) => Ok(()),
            };
            if failure.is_none() {
                failure = result.err();
            }
        }
        if let Some(error) = failure {
            self.status = Some(TransientNotice::error(format!(
                "could not update workspace visibility: {error}"
            )));
        }
    }

    fn show_workspace_for_action(&mut self) {
        let was_open = self.inspector.take().is_some();
        self.inspector_transition = None;
        if was_open {
            self.set_workspace_surfaces_visible(true);
        }
    }

    fn open_inspector(&mut self, inspector: Inspector) {
        let now = Instant::now();
        let from = self.inspector_transition.map_or_else(
            || if self.inspector.is_some() { 1.0 } else { 0.0 },
            |transition| transition.progress(now),
        );
        let was_closed = self.inspector.replace(inspector).is_none();
        self.inspector_transition = Some(InspectorTransition {
            started_at: now,
            from,
            to: 1.0,
        });
        if was_closed {
            self.set_workspace_surfaces_visible(false);
        }
        self.frame_clock.reset();
    }

    fn close_inspector(&mut self) {
        if self.inspector.is_some() {
            let now = Instant::now();
            let from = self
                .inspector_transition
                .map_or(1.0, |transition| transition.progress(now));
            self.inspector_transition = Some(InspectorTransition {
                started_at: now,
                from,
                to: 0.0,
            });
            self.frame_clock.reset();
        }
    }

    fn advance_inspector_transition(&mut self, now: Instant) {
        let Some(transition) = self.inspector_transition else {
            return;
        };
        if transition.is_complete(now) {
            if transition.to == 0.0 {
                self.inspector = None;
                self.set_workspace_surfaces_visible(true);
            }
            self.inspector_transition = None;
        }
    }

    fn save_settings_now(&mut self) -> Result<(), String> {
        if self.settings_write_blocked {
            return Err(
                "settings were not loaded successfully; use Reset to defaults to replace them"
                    .to_owned(),
            );
        }
        let path = match self.settings_path.clone() {
            Some(path) => path,
            None => settings::default_path().map_err(|error| error.to_string())?,
        };
        settings::save(&path, &self.settings).map_err(|error| error.to_string())?;
        self.settings_path = Some(path);
        self.settings_write_blocked = false;
        Ok(())
    }

    fn synchronize_lifecycle_budget(&mut self) {
        self.lifecycle.update_budget(LifecycleBudget {
            max_hot_cards: self.settings.max_hot_cards,
            offscreen_grace: Duration::from_millis(self.settings.hibernation_delay_ms),
        });
    }

    fn reschedule_offscreen_lifecycle_sweep(&mut self) -> Task<Message> {
        self.lifecycle_sweep_deadline = None;
        self.lifecycle_sweep_armed = false;
        let Some(delay) = self.lifecycle.next_offscreen_sweep_delay(Instant::now()) else {
            return Task::none();
        };
        self.request_lifecycle_sweep(delay)
    }

    fn apply_setting_input(&mut self, field: SettingsField, value: String) -> Task<Message> {
        self.settings_draft.set(field, value.clone());
        match settings_update(&self.settings, field, &value) {
            SettingsDecision::Invalid(message) => {
                self.settings_feedback = Some(SettingsFeedback::Invalid(message));
                Task::none()
            }
            SettingsDecision::Unchanged => {
                self.settings_feedback = None;
                Task::none()
            }
            SettingsDecision::Persist(settings) => {
                self.settings = settings;
                self.synchronize_lifecycle_budget();
                self.settings_feedback = Some(match self.save_settings_now() {
                    Ok(()) => SettingsFeedback::Saved,
                    Err(error) => SettingsFeedback::Error(error),
                });
                if matches!(
                    field,
                    SettingsField::ShortcutNewTerminal
                        | SettingsField::ShortcutCloseFocusedColumn
                        | SettingsField::ShortcutSearchProjects
                        | SettingsField::ShortcutPreviousColumn
                        | SettingsField::ShortcutNextColumn
                ) {
                    self.card_chrome.clear();
                }
                match field {
                    SettingsField::MaxHotCards => cosmic::task::message(Message::LifecycleSweep),
                    SettingsField::HibernationDelayMs => Task::batch([
                        cosmic::task::message(Message::LifecycleSweep),
                        self.reschedule_offscreen_lifecycle_sweep(),
                    ]),
                    SettingsField::TerminalFontSize
                    | SettingsField::TerminalFontFamily
                    | SettingsField::TerminalScrollbackLines
                    | SettingsField::SidebarWidth
                    | SettingsField::DefaultColumnFraction
                    | SettingsField::LinkOpenDestination
                    | SettingsField::LinkOpenGesture
                    | SettingsField::ShortcutNewTerminal
                    | SettingsField::ShortcutCloseFocusedColumn
                    | SettingsField::ShortcutSearchProjects
                    | SettingsField::ShortcutPreviousColumn
                    | SettingsField::ShortcutNextColumn => Task::none(),
                }
            }
        }
    }

    fn reset_settings(&mut self) -> Task<Message> {
        self.settings = AppSettings::default();
        self.settings_draft = SettingsDraft::from_settings(&self.settings);
        self.card_chrome.clear();
        self.synchronize_lifecycle_budget();
        self.settings_write_blocked = false;
        self.settings_feedback = Some(match self.save_settings_now() {
            Ok(()) => SettingsFeedback::Saved,
            Err(error) => SettingsFeedback::Error(error),
        });
        Task::batch([
            cosmic::task::message(Message::LifecycleSweep),
            self.reschedule_offscreen_lifecycle_sweep(),
        ])
    }
}

impl Application for Cagents {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "dev.cagents.Cagents";

    fn init(core: Core, (): ()) -> (Self, Task<Message>) {
        let (
            durable,
            session,
            projects,
            worktrees,
            settings,
            settings_path,
            settings_write_blocked,
            status,
        ) = Self::bootstrap();
        let sidebar_projects = projects
            .iter()
            .filter(|project| project.existence.is_open())
            .map(|project| {
                SidebarProject::new(
                    project.id,
                    RepositoryId(project.id.as_uuid()),
                    project.name.clone(),
                    project.repository.clone(),
                    worktrees
                        .iter()
                        .filter(|worktree| {
                            worktree.project_id == project.id && worktree.existence.is_open()
                        })
                        .map(|worktree| {
                            SidebarWorktree::from_durable(
                                worktree.id,
                                worktree.path.clone(),
                                worktree.branch.clone(),
                            )
                        })
                        .collect(),
                    Vec::new(),
                )
            })
            .collect();
        let workspace = Self::rebuild_workspace(&durable);
        let (terminal_wake_tx, terminal_wake_rx) = async_channel::unbounded();
        let window_scale_factor = core.scale_factor().max(0.01);
        let settings_draft = SettingsDraft::from_settings(&settings);
        let mut app = Self {
            core,
            workspace,
            durable: Arc::new(durable),
            cards: HashMap::new(),
            card_chrome: HashMap::new(),
            lifecycle: LifecycleManager::new(LifecycleBudget {
                max_hot_cards: settings.max_hot_cards,
                offscreen_grace: Duration::from_millis(settings.hibernation_delay_ms),
            }),
            terminal_wake_tx,
            terminal_wake_rx,
            session,
            settings,
            browser_teardowns: Vec::new(),
            terminal_teardowns: Vec::new(),
            browser_factory: None,
            browser_parent_lifetime: None,
            window_scale_factor,
            window_width: 1_600.0,
            settings_path,
            settings_write_blocked,
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, sidebar_projects),
            sidebar_state: SidebarState::default(),
            github_repositories: HashMap::new(),
            worktree_dialog: None,
            pull_request_dialog: None,
            frame_clock: FrameClock::default(),
            inspector: None,
            inspector_transition: None,
            settings_category: SettingsCategory::General,
            keyboard_modifiers: keyboard::Modifiers::empty(),
            settings_draft,
            settings_feedback: None,
            status: status.map(TransientNotice::error),
            lifecycle_sweep_deadline: None,
            lifecycle_sweep_armed: false,
        };
        let mut tasks = vec![app.initialize_runtimes()];
        tasks.push(cosmic::task::message(Message::InitializeBrowserFactory));
        if let Some(window_id) = app.core.main_window_id() {
            tasks.push(
                window::set_icon(window_id, application_window_icon()).map(cosmic::Action::App),
            );
        }
        tasks.extend(
            projects
                .into_iter()
                .filter(|project| project.existence.is_open())
                .map(|project| {
                    Self::repository_load_task(project.repository, Some(project.id), false)
                }),
        );
        let mut task = Task::batch(tasks);
        if app.durable.cards.is_empty() {
            task = Task::batch([
                task,
                app.add_terminal(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
            ]);
        }
        let chrome = app.sync_chrome();
        (app, Task::batch([task, chrome]))
    }

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = match message {
            Message::NewTerminal => {
                self.show_workspace_for_action();
                let directory = self
                    .workspace
                    .focused()
                    .and_then(|id| self.cards.get(&id))
                    .and_then(|runtime| match runtime {
                        CardRuntime::Repository(view) => Some(view.worktree.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                    });
                self.add_terminal(directory)
            }
            Message::TerminalStarted { card, result } => {
                let accept = can_accept_terminal_start(
                    self.durable.cards.iter().find(|state| state.id == card),
                    self.lifecycle.state(card).is_some(),
                    matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
                );
                if !accept {
                    if let Ok(handle) = result {
                        self.begin_terminal_teardown(handle);
                    }
                    return Task::none();
                }

                let mut tasks = Vec::new();
                match result {
                    Ok(handle) => {
                        let visible = terminal_start_visibility(
                            self.durable.cards.iter().find(|state| state.id == card),
                        )
                        .expect("accepted terminal start has an open terminal card")
                            && self.inspector.is_none();
                        let visibility = if visible {
                            handle.reveal()
                        } else {
                            handle.hide()
                        };
                        match visibility {
                            Ok(()) => {
                                self.cards.insert(card, CardRuntime::Terminal(handle));
                                if self.finish_initial_hydration(card, Ok(())) {
                                    let delay =
                                        Duration::from_millis(self.settings.hibernation_delay_ms);
                                    tasks.push(self.request_lifecycle_sweep(delay));
                                }
                            }
                            Err(error) => {
                                self.begin_terminal_teardown(handle);
                                let error = format!(
                                    "could not apply restored terminal visibility: {error}"
                                );
                                self.fail_runtime(card, error.clone());
                                self.status = Some(TransientNotice::error(error));
                                tasks.push(cosmic::task::message(Message::LifecycleSweep));
                            }
                        }
                    }
                    Err(error) => {
                        self.fail_runtime(card, error.clone());
                        self.status = Some(TransientNotice::error(error));
                        tasks.push(cosmic::task::message(Message::LifecycleSweep));
                    }
                }
                self.persist();
                Task::batch(tasks)
            }
            Message::TerminalWake { card, generation } => {
                self.drain_terminal(card, generation);
                Task::none()
            }
            Message::TerminalError { card: _, message }
            | Message::BrowserError { card: _, message } => {
                self.status = Some(TransientNotice::error(message));
                Task::none()
            }
            Message::OpenRepository => {
                self.show_workspace_for_action();
                cosmic::task::future(async {
                    let dialog = file_chooser::open::Dialog::new().title("Open Git repository");
                    let result = dialog
                        .open_folder()
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|response| {
                            response
                                .url()
                                .to_file_path()
                                .map_err(|()| "selected folder is not a local path".into())
                        });
                    Message::RepositorySelected(result)
                })
            }
            Message::RepositorySelected(result) => match result {
                Ok(path) => {
                    self.sidebar_model.data_state = SidebarDataState::Loading;
                    let project_id = self
                        .sidebar_model
                        .projects
                        .iter()
                        .find(|project| project.repository == path)
                        .map(|project| project.id);
                    Self::repository_load_task(path, project_id, true)
                }
                Err(error) => {
                    if !error.to_ascii_lowercase().contains("cancel") {
                        self.status = Some(TransientNotice::error(error));
                    }
                    Task::none()
                }
            },
            Message::RepositoryLoaded { open_cards, result } => {
                let mut task = Task::none();
                match result {
                    Ok(loaded) => {
                        if let Some(session) = &self.session {
                            session.save_project(loaded.project.clone());
                            session.save_worktrees(loaded.worktrees.iter().map(|(record, _)| {
                                WorktreeState {
                                    id: record.id,
                                    project_id: loaded.project.id,
                                    generation: 0,
                                    path: record.path.clone(),
                                    branch: record.branch.as_deref().map(|branch| {
                                        branch
                                            .strip_prefix("refs/heads/")
                                            .unwrap_or(branch)
                                            .to_owned()
                                    }),
                                    existence: Existence::Open,
                                }
                            }));
                        }
                        let worktrees = loaded
                            .worktrees
                            .iter()
                            .map(|(record, status)| {
                                SidebarWorktree::from_repository(record, status.as_ref())
                            })
                            .collect();
                        let pull_requests = loaded
                            .pull_requests
                            .iter()
                            .map(|(pull_request, checks)| {
                                SidebarPullRequest::from_github(
                                    loaded.project.id,
                                    pull_request,
                                    checks,
                                )
                            })
                            .collect();
                        let sidebar_project = SidebarProject::from_repository(
                            &loaded.project,
                            &loaded.repository,
                            worktrees,
                            pull_requests,
                        );
                        if let Some(repository) = loaded.github_repository.clone() {
                            self.github_repositories
                                .insert(loaded.project.id, repository);
                        }
                        self.sidebar_model
                            .projects
                            .retain(|project| project.id != loaded.project.id);
                        self.sidebar_model.projects.push(sidebar_project);
                        self.sidebar_model.data_state = SidebarDataState::Ready;
                        let worktree = loaded
                            .worktrees
                            .first()
                            .map(|(record, _)| record.path.clone())
                            .unwrap_or_else(|| loaded.repository.selected_path.clone());
                        if open_cards {
                            task = Task::batch([
                                self.add_repository_card(
                                    loaded.project.id,
                                    loaded.repository.common_dir.clone(),
                                    worktree.clone(),
                                ),
                                self.add_terminal(worktree),
                            ]);
                        }
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.clone()));
                        finish_repository_load_failure(&mut self.sidebar_model.data_state, error);
                    }
                }
                task
            }
            Message::Sidebar(action) => {
                self.sidebar_state.apply(&action);
                match action {
                    SidebarAction::OpenRepositoryPicker => {
                        cosmic::task::message(Message::OpenRepository)
                    }
                    SidebarAction::OpenRepository {
                        project_id,
                        repository,
                    } => {
                        self.show_workspace_for_action();
                        self.add_repository_card(project_id, repository.clone(), repository)
                    }
                    SidebarAction::OpenWorktree {
                        project_id, path, ..
                    } => {
                        self.show_workspace_for_action();
                        Task::batch([
                            self.add_repository_card(project_id, path.clone(), path.clone()),
                            self.add_terminal(path),
                        ])
                    }
                    SidebarAction::OpenPullRequest { url, .. } => self.open_clicked_link(url),
                    SidebarAction::NewWorktree {
                        project_id,
                        repository,
                    } => {
                        let root = repository
                            .parent()
                            .unwrap_or_else(|| Path::new("/"))
                            .join("worktrees");
                        self.worktree_dialog = Some(WorktreeDialog {
                            project_id,
                            repository,
                            branch: String::new(),
                            base: "main".into(),
                            destination: root.display().to_string(),
                        });
                        Task::none()
                    }
                    SidebarAction::Refresh => {
                        let targets = repository_refresh_targets(&self.sidebar_model.projects);
                        if targets.is_empty() {
                            return cosmic::task::message(Message::OpenRepository);
                        }
                        self.sidebar_model.data_state = SidebarDataState::Loading;
                        Task::batch(targets.into_iter().map(|(repository, id)| {
                            Self::repository_load_task(repository, Some(id), false)
                        }))
                    }
                    SidebarAction::CreatePullRequest { project_id, .. } => {
                        let Some(repository) = self.github_repositories.get(&project_id).cloned()
                        else {
                            self.status = Some(TransientNotice::warning(
                                "This repository is not associated with an authenticated GitHub remote",
                            ));
                            return Task::none();
                        };
                        let head = self
                            .sidebar_model
                            .projects
                            .iter()
                            .find(|project| project.id == project_id)
                            .and_then(|project| {
                                project
                                    .worktrees
                                    .iter()
                                    .find_map(|worktree| worktree.branch.clone())
                            })
                            .unwrap_or_default();
                        self.pull_request_dialog = Some(PullRequestDialog {
                            repository,
                            base: "main".into(),
                            head,
                            title: String::new(),
                            body: String::new(),
                            draft: false,
                        });
                        Task::none()
                    }
                    SidebarAction::OpenSettings => {
                        self.settings_category = SettingsCategory::General;
                        self.settings_draft = SettingsDraft::from_settings(&self.settings);
                        self.settings_feedback = None;
                        self.open_inspector(Inspector::Settings);
                        Task::none()
                    }
                    // The handle clamps to its own rendered bounds, and the
                    // settings file accepts a narrower range, so the width is
                    // clamped again before it can reach a rejected save.
                    SidebarAction::Resize { width } => {
                        self.settings.sidebar_width =
                            width.clamp(sidebar::EXPANDED_WIDTH_MIN, sidebar::EXPANDED_WIDTH_MAX);
                        self.settings_draft.sidebar_width = self.settings.sidebar_width.to_string();
                        Task::none()
                    }
                    SidebarAction::ResizeFinished => {
                        self.settings_feedback = Some(match self.save_settings_now() {
                            Ok(()) => SettingsFeedback::Saved,
                            Err(error) => SettingsFeedback::Error(error),
                        });
                        Task::none()
                    }
                    SidebarAction::SearchChanged(_)
                    | SidebarAction::SetFilter(_)
                    | SidebarAction::ToggleControls
                    | SidebarAction::ToggleProject(_)
                    | SidebarAction::TogglePullRequests(_) => Task::none(),
                }
            }
            Message::FocusCard(id) => match self.workspace.focus(id) {
                Ok(update) => self.apply_viewport(update, true, false),
                Err(error) => {
                    self.status = Some(TransientNotice::error(error.to_string()));
                    Task::none()
                }
            },
            Message::CloseCard(id) => match self.workspace.request_close(id) {
                Ok(handoff) => workspace_widget::focus_handoff_then(
                    handoff,
                    widget::Id::new(TOOLBAR_FOCUS_ID),
                    Message::CommitClose(id),
                )
                .map(cosmic::Action::App),
                Err(error) => {
                    self.status = Some(TransientNotice::error(error.to_string()));
                    Task::none()
                }
            },
            Message::CommitClose(id) => {
                if let Some(runtime) = self.cards.remove(&id) {
                    match runtime {
                        CardRuntime::Terminal(handle) => {
                            self.begin_terminal_teardown(handle);
                        }
                        CardRuntime::Browser(browser) => {
                            self.begin_browser_teardown(browser.handle);
                        }
                        CardRuntime::Loading
                        | CardRuntime::Failed(_)
                        | CardRuntime::Repository(_)
                        | CardRuntime::PullRequests(_) => {}
                    }
                }
                self.lifecycle.remove(id);
                if let Some(card) = self.card_state_mut(id) {
                    card.existence = Existence::Closed {
                        tombstone: crate::model::Tombstone {
                            operation_id: None,
                            closed_at_unix_ms: unix_millis(),
                            reason: Some("closed by user".into()),
                        },
                    };
                }
                match self.workspace.commit_close(id) {
                    Ok(update) => self.apply_viewport(update, true, true),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::Viewport(event) => {
                let (update, synchronize_scroll) =
                    update_workspace_viewport(&mut self.workspace, event);
                let task = self.apply_viewport(update, false, false);
                if synchronize_scroll {
                    Task::batch([task, self.synchronize_workspace_scroll()])
                } else {
                    task
                }
            }
            Message::Pan(gesture) => match gesture {
                PanGesture::Begin { pointer_x } => {
                    self.workspace.begin_pan(pointer_x);
                    Task::none()
                }
                PanGesture::Update { pointer_x } => {
                    let update = self.workspace.update_pan(pointer_x);
                    self.apply_viewport(update, true, false)
                }
                PanGesture::End => {
                    self.workspace.end_pan();
                    Task::none()
                }
            },
            Message::Wheel(delta) => {
                let update = self.workspace.scroll_wheel(delta);
                self.apply_viewport(update, true, false)
            }
            Message::Frame(frame) => {
                self.advance_inspector_transition(frame);
                let Some(elapsed) = self.frame_clock.elapsed(frame) else {
                    return Task::none();
                };
                let update = self.workspace.tick(elapsed);
                self.apply_viewport(update, true, false)
            }
            Message::LifecycleSweep => self.lifecycle_sweep(),
            Message::LifecycleSweepElapsed => self.lifecycle_sweep_elapsed(),
            Message::InitializeBrowserFactory => self.browser_factory_task(),
            Message::BrowserFactoryReady(result) => match result {
                Ok(binding) => {
                    self.browser_parent_lifetime = Some(binding.parent_lifetime);
                    self.browser_factory = Some(binding.factory);
                    self.register_deferred_browsers();
                    self.lifecycle_sweep()
                }
                Err(error) => {
                    let message = format!("GPU-only Servo embedding unavailable: {error}");
                    let failed = self
                        .durable
                        .cards
                        .iter()
                        .filter(|card| matches!(&card.kind, CardKind::Browser(_)))
                        .filter_map(|card| {
                            matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                                .then_some(card.id)
                        })
                        .collect::<Vec<_>>();
                    for card in failed {
                        self.fail_runtime(card, message.clone());
                    }
                    self.status = Some(TransientNotice::error(message));
                    Task::none()
                }
            },
            Message::WindowScaleFactorChanged(scale_factor) => {
                if scale_factor.is_finite() && scale_factor > 0.0 {
                    self.window_scale_factor = scale_factor;
                    for runtime in self.cards.values() {
                        if let CardRuntime::Browser(browser) = runtime {
                            let _ = browser.handle.set_scale_factor(scale_factor);
                        }
                    }
                }
                Task::none()
            }
            Message::WindowResized(width) => {
                if width.is_finite() && width > 0.0 {
                    self.window_width = width;
                }
                Task::none()
            }
            Message::OpenBrowser(url) => {
                self.show_workspace_for_action();
                self.add_browser(url)
            }
            Message::ExternalLinkOpened(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(format!(
                        "could not open link in the system browser: {error}"
                    )));
                }
                Task::none()
            }
            Message::NewBrowser => {
                self.show_workspace_for_action();
                self.add_browser(
                    Url::parse("https://github.com/").expect("default browser URL is valid"),
                )
            }
            Message::BrowserBack(card) => {
                browser_command(&self.cards, card, |handle| handle.back(), &mut self.status)
            }
            Message::BrowserForward(card) => browser_command(
                &self.cards,
                card,
                |handle| handle.forward(),
                &mut self.status,
            ),
            Message::BrowserReload(card) => browser_command(
                &self.cards,
                card,
                |handle| handle.reload(),
                &mut self.status,
            ),
            Message::RetryCard(card) => self.retry_card(card),
            Message::BrowserAddressChanged { card, value } => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.address = value;
                }
                Task::none()
            }
            Message::BrowserNavigate(card) => {
                let address = match self.cards.get(&card) {
                    Some(CardRuntime::Browser(browser)) => browser.address.trim().to_owned(),
                    _ => return Task::none(),
                };
                let url =
                    Url::parse(&address).or_else(|_| Url::parse(&format!("https://{address}")));
                match url {
                    Ok(url) => browser_command(
                        &self.cards,
                        card,
                        |handle| handle.navigate(url),
                        &mut self.status,
                    ),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "invalid browser URL: {error}"
                        )));
                        Task::none()
                    }
                }
            }
            Message::BrowserEvent { card, event } => self.handle_browser_event(card, event),
            Message::ClipboardRead {
                card,
                request,
                value,
            } => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get(&card) {
                    let _ = browser.handle.respond_clipboard(
                        request,
                        value.ok_or_else(|| "clipboard has no text".into()),
                    );
                }
                Task::none()
            }
            Message::FilePickerResult {
                card,
                request,
                result,
            } => {
                match result {
                    Ok(paths) => {
                        if let Some(CardRuntime::Browser(browser)) = self.cards.get(&card)
                            && let Err(error) = browser.handle.respond_file_picker(request, paths)
                        {
                            self.status = Some(TransientNotice::error(error.to_string()));
                        }
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "file chooser failed: {error}"
                        )));
                        if let Some(CardRuntime::Browser(browser)) = self.cards.get(&card) {
                            let _ = browser.handle.respond_file_picker(request, None);
                        }
                    }
                }
                Task::none()
            }
            Message::WorktreeBranchChanged(value) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.branch = value;
                }
                Task::none()
            }
            Message::WorktreeBaseChanged(value) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.base = value;
                }
                Task::none()
            }
            Message::WorktreeDestinationChanged(value) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.destination = value;
                }
                Task::none()
            }
            Message::SubmitWorktree => {
                let Some(dialog) = self.worktree_dialog.take() else {
                    return Task::none();
                };
                let request =
                    ValidatedBranchName::parse(dialog.branch.clone()).and_then(|branch| {
                        let base = ValidatedBranchName::parse(dialog.base.clone())
                            .map(BaseRevision::LocalBranch)?;
                        let destination = PathBuf::from(&dialog.destination)
                            .join(branch_destination_component(branch.as_str()));
                        Ok(CreateWorktreeRequest {
                            repository: dialog.repository.clone(),
                            managed_root: PathBuf::from(&dialog.destination),
                            destination,
                            branch,
                            base,
                            track: false,
                            operation_id: crate::model::OperationId::new(),
                        })
                    });
                match request {
                    Ok(request) => {
                        let project_id = dialog.project_id;
                        let repository = dialog.repository;
                        cosmic::task::future(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                let _ = std::fs::create_dir_all(&request.managed_root);
                                RepositoryService::system("git")
                                    .create_worktree(&request, &CancellationToken::default())
                            })
                            .await
                            .map_err(|error| format!("worktree creator join failed: {error}"))
                            .and_then(|result| result.map_err(|error| error.to_string()));
                            Message::WorktreeCreated {
                                project_id,
                                repository,
                                result,
                            }
                        })
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        self.worktree_dialog = Some(dialog);
                        Task::none()
                    }
                }
            }
            Message::WorktreeCreated {
                project_id,
                repository,
                result,
            } => {
                match result {
                    Ok(WorktreeCreationOutcome::Created { worktree, .. }) => {
                        if let Some(session) = &self.session {
                            session.save_worktrees([WorktreeState {
                                id: worktree.id,
                                project_id,
                                generation: 0,
                                path: worktree.path.clone(),
                                branch: worktree.branch.as_deref().map(|branch| {
                                    branch
                                        .strip_prefix("refs/heads/")
                                        .unwrap_or(branch)
                                        .to_owned()
                                }),
                                existence: Existence::Open,
                            }]);
                        }
                        self.status = Some(TransientNotice::success(format!(
                            "created worktree {}",
                            worktree.path.display()
                        )));
                        let terminal = self.add_terminal(worktree.path);
                        // `add_terminal` pushed a durable card, and this arm
                        // returns without reaching the `sync_chrome` at the
                        // tail of `update`, so refresh the cache here to keep
                        // the "an entry for every durable card" invariant true.
                        let chrome = self.sync_chrome();
                        return Task::batch([
                            terminal,
                            chrome,
                            Self::repository_load_task(repository, Some(project_id), false),
                        ]);
                    }
                    Ok(other) => {
                        self.status = Some(TransientNotice::warning(format!(
                            "worktree outcome: {other:?}"
                        )));
                    }
                    Err(error) => self.status = Some(TransientNotice::error(error)),
                }
                Task::none()
            }
            Message::PullRequestBaseChanged(value) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.base = value;
                }
                Task::none()
            }
            Message::PullRequestHeadChanged(value) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.head = value;
                }
                Task::none()
            }
            Message::PullRequestTitleChanged(value) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.title = value;
                }
                Task::none()
            }
            Message::PullRequestBodyChanged(value) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.body = value;
                }
                Task::none()
            }
            Message::PullRequestDraftChanged(value) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.draft = value;
                }
                Task::none()
            }
            Message::SubmitPullRequest => {
                let Some(dialog) = self.pull_request_dialog.take() else {
                    return Task::none();
                };
                let request = CreatePullRequestRequest {
                    repository: dialog.repository,
                    base: dialog.base,
                    head: dialog.head,
                    title: dialog.title,
                    body: dialog.body,
                    draft: dialog.draft,
                    labels: Vec::new(),
                    reviewers: Vec::new(),
                };
                if let Err(error) = request.validate() {
                    self.status = Some(TransientNotice::error(error.to_string()));
                    return Task::none();
                }
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        GitHubService::system("gh")
                            .create(&request, &CancellationToken::default())
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| format!("pull request creator join failed: {error}"))
                    .and_then(|result| result);
                    Message::PullRequestCreated(result)
                })
            }
            Message::PullRequestCreated(result) => match result {
                Ok(PullRequestCreationOutcome::Created { url, .. }) => {
                    self.status = Some(TransientNotice::success(format!(
                        "created pull request {url}"
                    )));
                    self.open_link(url)
                }
                Ok(PullRequestCreationOutcome::NotCreated { failure }) => {
                    self.status = Some(TransientNotice::error(failure.to_string()));
                    Task::none()
                }
                Ok(PullRequestCreationOutcome::OutcomeUnknown { failure }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "pull request outcome is unknown; reconcile before retrying: {failure}"
                    )));
                    Task::none()
                }
                Err(error) => {
                    self.status = Some(TransientNotice::error(error));
                    Task::none()
                }
            },
            Message::CancelDialog => {
                self.worktree_dialog = None;
                self.pull_request_dialog = None;
                Task::none()
            }
            Message::KeyEvent(event) => {
                if let keyboard::Event::ModifiersChanged(modifiers) = &event {
                    self.keyboard_modifiers = *modifiers;
                }
                if is_escape_press(&event) {
                    if self.inspector.is_some() {
                        return cosmic::task::message(Message::CloseInspector);
                    }
                    if self.worktree_dialog.is_some() || self.pull_request_dialog.is_some() {
                        return cosmic::task::message(Message::CancelDialog);
                    }
                }
                if self.inspector.is_some() {
                    return Task::none();
                }
                match workspace_widget::global_shortcut(&event, &self.settings.keyboard_shortcuts) {
                    Some(workspace_widget::GlobalShortcut::NewTerminal) => {
                        cosmic::task::message(Message::NewTerminal)
                    }
                    Some(workspace_widget::GlobalShortcut::CloseFocusedCard) => {
                        match close_focused_message(&self.workspace) {
                            Some(message) => cosmic::task::message(message),
                            None => Task::none(),
                        }
                    }
                    Some(workspace_widget::GlobalShortcut::FocusProjectSearch) => {
                        cosmic::task::message(Message::FocusProjectSearch)
                    }
                    Some(workspace_widget::GlobalShortcut::FocusAdjacent(direction)) => {
                        cosmic::task::message(Message::FocusAdjacent(direction))
                    }
                    None => Task::none(),
                }
            }
            Message::FocusAdjacent(direction) => {
                self.show_workspace_for_action();
                match self.workspace.focus_adjacent(direction) {
                    Ok(update) => self.apply_viewport(update, true, false),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::FocusProjectSearch => {
                if !self.sidebar_state.controls_expanded() {
                    self.sidebar_state.apply(&SidebarAction::ToggleControls);
                }
                let id = sidebar::search_focus_id();
                iced::widget::operation::focus(id.clone())
                    .chain(iced::widget::operation::select_all(id))
                    .map(cosmic::Action::App)
            }
            Message::CloseInspector => {
                self.close_inspector();
                Task::none()
            }
            Message::SelectSettingsCategory(category) => {
                self.settings_category = category;
                Task::none()
            }
            Message::SettingsChanged { field, value } => self.apply_setting_input(field, value),
            Message::ResetSettings => self.reset_settings(),
            Message::DismissStatus => {
                self.status = None;
                Task::none()
            }
        };
        self.synchronize_sidebar_selection();
        // `view` cannot rebuild the chrome cache itself, so refresh it here.
        let chrome = self.sync_chrome();
        Task::batch([task, chrome])
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            Subscription::run_with(
                TerminalWakeSource(self.terminal_wake_rx.clone()),
                terminal_wake_stream,
            )
            .map(|wake| Message::TerminalWake {
                card: wake.card,
                generation: wake.generation,
            }),
            event::listen_with(|event, _status, _window| match event {
                iced::Event::Keyboard(event) => Some(Message::KeyEvent(event)),
                iced::Event::Window(window::Event::Rescaled(scale_factor)) => {
                    Some(Message::WindowScaleFactorChanged(scale_factor))
                }
                iced::Event::Window(window::Event::Resized(size)) => {
                    Some(Message::WindowResized(size.width))
                }
                _ => None,
            }),
        ];
        if let Some(window) = self.core.main_window_id()
            && (self.workspace.camera().is_animating() || self.inspector_transition.is_some())
        {
            subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
        }
        for (&card, runtime) in &self.cards {
            if let CardRuntime::Browser(browser) = runtime {
                subscriptions.push(
                    browser
                        .handle
                        .subscription()
                        .with(card)
                        .map(|(card, event)| Message::BrowserEvent { card, event }),
                );
            }
        }
        Subscription::batch(subscriptions)
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        let spacing = cosmic::theme::spacing();
        if let Some(dialog) = self.worktree_dialog.as_ref() {
            return Some(
                widget::container(
                    widget::column::with_children(vec![
                        widget::text::title3("Create worktree").into(),
                        widget::column::with_children(vec![
                            widget::text::caption("Branch").into(),
                            widget::text_input("Branch", &dialog.branch)
                                .id(widget::Id::new("dialog.worktree.branch"))
                                .on_input(Message::WorktreeBranchChanged)
                                .into(),
                        ])
                        .spacing(spacing.space_xxxs)
                        .into(),
                        widget::column::with_children(vec![
                            widget::text::caption("Base branch").into(),
                            widget::text_input("Base branch", &dialog.base)
                                .id(widget::Id::new("dialog.worktree.base"))
                                .on_input(Message::WorktreeBaseChanged)
                                .into(),
                        ])
                        .spacing(spacing.space_xxxs)
                        .into(),
                        widget::column::with_children(vec![
                            widget::text::caption("Managed directory").into(),
                            widget::text_input("Managed directory", &dialog.destination)
                                .id(widget::Id::new("dialog.worktree.destination"))
                                .on_input(Message::WorktreeDestinationChanged)
                                .into(),
                        ])
                        .spacing(spacing.space_xxxs)
                        .into(),
                        widget::row::with_children(vec![
                            widget::button::standard("Cancel")
                                .on_press(Message::CancelDialog)
                                .into(),
                            widget::button::suggested("Create")
                                .on_press(Message::SubmitWorktree)
                                .into(),
                        ])
                        .spacing(spacing.space_xxs)
                        .into(),
                    ])
                    .spacing(spacing.space_xs),
                )
                .class(cosmic::theme::Container::Dialog(false))
                .padding(spacing.space_m)
                .max_width(520)
                .into(),
            );
        }

        if let Some(inspector) = self.inspector {
            let progress = self
                .inspector_transition
                .map_or(1.0, |transition| transition.progress(Instant::now()));
            return Some(match inspector {
                Inspector::Settings => settings_dialog(
                    self.settings_category,
                    &self.settings_draft,
                    self.settings_feedback.as_ref(),
                    progress,
                ),
            });
        }

        let dialog = self.pull_request_dialog.as_ref()?;
        Some(
            widget::container(
                widget::column::with_children(vec![
                    widget::text::title3("Create pull request").into(),
                    widget::text::caption(dialog.repository.gh_selector()).into(),
                    widget::column::with_children(vec![
                        widget::text::caption("Title").into(),
                        widget::text_input("Title", &dialog.title)
                            .id(widget::Id::new("dialog.pull-request.title"))
                            .on_input(Message::PullRequestTitleChanged)
                            .into(),
                    ])
                    .spacing(spacing.space_xxxs)
                    .into(),
                    widget::column::with_children(vec![
                        widget::text::caption("Head branch").into(),
                        widget::text_input("Head branch", &dialog.head)
                            .id(widget::Id::new("dialog.pull-request.head"))
                            .on_input(Message::PullRequestHeadChanged)
                            .into(),
                    ])
                    .spacing(spacing.space_xxxs)
                    .into(),
                    widget::column::with_children(vec![
                        widget::text::caption("Base branch").into(),
                        widget::text_input("Base branch", &dialog.base)
                            .id(widget::Id::new("dialog.pull-request.base"))
                            .on_input(Message::PullRequestBaseChanged)
                            .into(),
                    ])
                    .spacing(spacing.space_xxxs)
                    .into(),
                    widget::column::with_children(vec![
                        widget::text::caption("Description").into(),
                        widget::text_input("Description", &dialog.body)
                            .id(widget::Id::new("dialog.pull-request.description"))
                            .on_input(Message::PullRequestBodyChanged)
                            .into(),
                    ])
                    .spacing(spacing.space_xxxs)
                    .into(),
                    widget::checkbox(dialog.draft)
                        .label("Create as draft")
                        .on_toggle(Message::PullRequestDraftChanged)
                        .into(),
                    widget::row::with_children(vec![
                        widget::button::standard("Cancel")
                            .on_press(Message::CancelDialog)
                            .into(),
                        widget::button::suggested("Create")
                            .on_press(Message::SubmitPullRequest)
                            .into(),
                    ])
                    .spacing(spacing.space_xxs)
                    .into(),
                ])
                .spacing(spacing.space_xs),
            )
            .class(cosmic::theme::Container::Dialog(false))
            .padding(spacing.space_m)
            .max_width(620)
            .into(),
        )
    }

    /// The workspace commands, drawn in the window's own header bar.
    ///
    /// They used to occupy a second full-width row directly beneath it, which
    /// duplicated the focused card's name and cost a row of workspace height for
    /// a bar that was otherwise empty apart from the window controls.
    fn header_end(&self) -> Vec<Element<'_, Message>> {
        let compact_commands = self.window_width < COMMAND_BAR_COMPACT_BREAKPOINT;
        let total = self.workspace.cards().len();
        let current = self
            .workspace
            .focused()
            .and_then(|focused| self.workspace.order().position(|id| id == focused))
            .map_or(0, |index| index + 1);
        let previous = workspace_widget::shortcut_descriptor(
            workspace_widget::GlobalShortcut::FocusAdjacent(crate::workspace::FocusDirection::Left),
        );
        let next =
            workspace_widget::shortcut_descriptor(workspace_widget::GlobalShortcut::FocusAdjacent(
                crate::workspace::FocusDirection::Right,
            ));
        let navigator: Element<'_, Message> = widget::row::with_children(vec![
            widget::button::icon(widget::icon::from_name("go-previous-symbolic"))
                .id(widget::Id::new("command.previous-column"))
                .name(format!(
                    "{}, {}",
                    previous.label,
                    workspace_widget::shortcut_keys(
                        previous.action,
                        &self.settings.keyboard_shortcuts,
                    )
                ))
                .tooltip(workspace_widget::shortcut_tooltip(
                    previous.action,
                    &self.settings.keyboard_shortcuts,
                ))
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe((current > 1).then_some(Message::FocusAdjacent(
                    crate::workspace::FocusDirection::Left,
                )))
                .into(),
            widget::container(widget::text::caption(format!("{current} / {total}")))
                .center_x(Length::Fixed(APP_HEADER_HEIGHT))
                .align_y(Alignment::Center)
                .into(),
            widget::button::icon(widget::icon::from_name("go-next-symbolic"))
                .id(widget::Id::new("command.next-column"))
                .name(format!(
                    "{}, {}",
                    next.label,
                    workspace_widget::shortcut_keys(next.action, &self.settings.keyboard_shortcuts,)
                ))
                .tooltip(workspace_widget::shortcut_tooltip(
                    next.action,
                    &self.settings.keyboard_shortcuts,
                ))
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(
                    (current > 0 && current < total).then_some(Message::FocusAdjacent(
                        crate::workspace::FocusDirection::Right,
                    )),
                )
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(0)
        .into();

        let failed = self
            .cards
            .values()
            .filter(|runtime| matches!(runtime, CardRuntime::Failed(_)))
            .count();
        let busy = self
            .cards
            .values()
            .filter(|runtime| match runtime {
                CardRuntime::Loading => true,
                CardRuntime::Browser(browser) => {
                    browser.lifecycle == BrowserLifecycle::Restoring || browser.load_state.is_some()
                }
                _ => false,
            })
            .count();
        let runtime_exception: Option<Element<'_, Message>> = if failed > 0 {
            let label = format!(
                "{failed} runtime{} unavailable",
                if failed == 1 { "" } else { "s" }
            );
            // The indicator is the only place a failure off-screen is visible,
            // so it reveals the column whose recovery buttons live in the card.
            let first_failed = self
                .workspace
                .order()
                .find(|id| matches!(self.cards.get(id), Some(CardRuntime::Failed(_))));
            Some(
                widget::button::icon(widget::icon::from_name("dialog-warning-symbolic"))
                    .id(widget::Id::new("command.runtime-exception"))
                    .name(label.clone())
                    .tooltip(label)
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press_maybe(first_failed.map(Message::FocusCard))
                    .into(),
            )
        } else if busy > 0 {
            let label = format!(
                "{busy} runtime{} starting",
                if busy == 1 { "" } else { "s" }
            );
            Some(
                widget::tooltip::tooltip(
                    widget::container(widget::progress_bar::circular::Circular::new().size(16.0))
                        .center(Length::Fixed(ICON_CONTROL_SIZE)),
                    widget::text::caption(label),
                    widget::tooltip::Position::Bottom,
                )
                .into(),
            )
        } else {
            None
        };

        let new_terminal =
            workspace_widget::shortcut_descriptor(workspace_widget::GlobalShortcut::NewTerminal);
        let actions: Vec<Element<'_, Message>> = if compact_commands {
            vec![
                widget::button::icon(widget::icon::from_name("utilities-terminal-symbolic"))
                    .id(widget::Id::new(TOOLBAR_FOCUS_ID))
                    .name(format!(
                        "{}, {}",
                        new_terminal.label,
                        workspace_widget::shortcut_keys(
                            new_terminal.action,
                            &self.settings.keyboard_shortcuts,
                        )
                    ))
                    .tooltip(workspace_widget::shortcut_tooltip(
                        new_terminal.action,
                        &self.settings.keyboard_shortcuts,
                    ))
                    .class(cosmic::theme::Button::Suggested)
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::NewTerminal)
                    .into(),
                widget::button::icon(widget::icon::from_name("folder-open-symbolic"))
                    .id(widget::Id::new("command.open-repository"))
                    .name("Open repository")
                    .tooltip("Open repository")
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::OpenRepository)
                    .into(),
                widget::button::icon(widget::icon::from_name("internet-web-browser-symbolic"))
                    .id(widget::Id::new("command.new-browser"))
                    .name("New browser")
                    .tooltip("New browser")
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::NewBrowser)
                    .into(),
            ]
        } else {
            vec![
                widget::button::suggested("New terminal")
                    .id(widget::Id::new(TOOLBAR_FOCUS_ID))
                    .name(format!(
                        "{}, {}",
                        new_terminal.label,
                        workspace_widget::shortcut_keys(
                            new_terminal.action,
                            &self.settings.keyboard_shortcuts,
                        )
                    ))
                    .tooltip(workspace_widget::shortcut_tooltip(
                        new_terminal.action,
                        &self.settings.keyboard_shortcuts,
                    ))
                    .leading_icon(widget::icon::from_name("utilities-terminal-symbolic").size(16))
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::NewTerminal)
                    .into(),
                widget::button::standard("Open repository")
                    .id(widget::Id::new("command.open-repository"))
                    .name("Open repository")
                    .tooltip("Open a repository and add its worktrees")
                    .leading_icon(widget::icon::from_name("folder-open-symbolic").size(16))
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::OpenRepository)
                    .into(),
                widget::button::standard("New browser")
                    .id(widget::Id::new("command.new-browser"))
                    .name("New browser")
                    .tooltip("Open a browser column")
                    .leading_icon(widget::icon::from_name("internet-web-browser-symbolic").size(16))
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::NewBrowser)
                    .into(),
            ]
        };
        let mut header_children = Vec::with_capacity(actions.len() + 2);
        if let Some(exception) = runtime_exception {
            header_children.push(exception);
        }
        header_children.push(navigator);
        header_children.extend(actions);
        header_children
    }

    fn view(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let window_width = self.window_width;
        let sidebar_mode = SidebarMode::for_width(window_width);
        let sidebar = sidebar::view(
            &self.sidebar_model,
            &self.sidebar_state,
            sidebar_mode,
            self.settings.sidebar_width,
        )
        .map(Message::Sidebar);

        // The focused card's own name and path are the card header's job, and
        // the window's header bar already carries the name; see
        // `sync_window_title`.

        let notice: Option<Element<'_, Message>> = self.status.as_ref().map(|notice| {
            let severity = notice.severity;
            widget::container(
                widget::row::with_children(vec![
                    widget::icon::from_name(severity.icon()).size(16).into(),
                    widget::text::body(&notice.message)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                    widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                        .id(widget::Id::new("status.dismiss"))
                        .name(format!("Dismiss {} notice", severity.label()))
                        .tooltip("Dismiss notice")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press(Message::DismissStatus)
                        .into(),
                ])
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxs),
            )
            .class(cosmic::theme::Container::custom(move |theme| {
                let cosmic = theme.cosmic();
                let component = match severity {
                    NoticeSeverity::Success => &cosmic.success,
                    NoticeSeverity::Warning => &cosmic.warning,
                    NoticeSeverity::Error => &cosmic.destructive,
                };
                cosmic::iced::widget::container::Style {
                    icon_color: Some(component.on.into()),
                    text_color: Some(component.on.into()),
                    background: Some(iced::Background::Color(component.base.into())),
                    border: iced::Border {
                        width: 1.0,
                        color: component.border.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }))
            .height(Length::Fixed(NOTICE_ROW_HEIGHT))
            .padding([0, spacing.space_xs])
            .width(Length::Fill)
            .into()
        });

        let workspace: Element<'_, Message> = if self.workspace.cards().is_empty() {
            let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            widget::container(
                widget::column::with_children(vec![
                    widget::text::title2("Start a workspace").into(),
                    widget::text::body(
                        "Open a repository for project context, or start a standalone terminal.",
                    )
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into(),
                    widget::text::caption(format!(
                        "Starting directory: {}",
                        display_path(&directory)
                    ))
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into(),
                    widget::row::with_children(vec![
                        widget::button::suggested("New terminal")
                            .id(widget::Id::new("empty.new-terminal"))
                            .name("New terminal")
                            .leading_icon(
                                widget::icon::from_name("utilities-terminal-symbolic").size(16),
                            )
                            .on_press(Message::NewTerminal)
                            .into(),
                        widget::button::standard("Open repository")
                            .id(widget::Id::new("empty.open-repository"))
                            .name("Open repository")
                            .leading_icon(widget::icon::from_name("folder-open-symbolic").size(16))
                            .on_press(Message::OpenRepository)
                            .into(),
                        widget::button::standard("New browser")
                            .id(widget::Id::new("empty.new-browser"))
                            .name("New browser")
                            .leading_icon(
                                widget::icon::from_name("internet-web-browser-symbolic").size(16),
                            )
                            .on_press(Message::NewBrowser)
                            .into(),
                    ])
                    .spacing(spacing.space_xxs)
                    .into(),
                ])
                .spacing(spacing.space_s)
                .max_width(CONTENT_MAX_WIDTH),
            )
            .center(Length::Fill)
            .padding(spacing.space_m)
            .into()
        } else {
            workspace_widget::view(
                &self.workspace,
                &self.settings.keyboard_shortcuts,
                self.workspace.viewport_width(),
                workspace_widget::workspace_scroll_id(),
                |context| self.card_view(context.id, context.focused),
                // `sync_chrome` keeps an entry for every durable card, so
                // the fallback is only reachable for a card the workspace holds
                // but the durable model does not.
                |id| {
                    self.card_chrome.get(&id).map_or_else(
                        || workspace_widget::card_widget_id(id),
                        |chrome| chrome.container_id.clone(),
                    )
                },
                workspace_widget::Events {
                    viewport: Message::Viewport,
                    pan: Message::Pan,
                    wheel: Message::Wheel,
                },
            )
        };

        let mut main_children: Vec<Element<'_, Message>> = Vec::with_capacity(3);
        if let Some(notice) = notice {
            main_children.push(notice);
            main_children.push(widget::divider::horizontal::default().into());
        }
        main_children.push(workspace);
        let main = widget::column::with_children(main_children)
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill);

        widget::row::with_children(vec![sidebar, main.into()])
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }
}

impl Drop for Cagents {
    fn drop(&mut self) {
        self.persist();
        if !self.settings_write_blocked
            && let Some(path) = &self.settings_path
            && let Err(error) = settings::save(path, &self.settings)
        {
            tracing::error!(%error, "failed to persist settings");
        }
        if let Some(session) = &mut self.session {
            session.shutdown();
        }

        // Exact Wayland parent invariant:
        // 1. request and join every browser still owned by a card;
        // 2. join every off-UI-path card-close reaper;
        // 3. drop the raw-handle factory/Wayland host;
        // 4. deactivate/drop the parent lifetime guard;
        // 5. only then may Rust drop `core` and its parent window.
        for runtime in std::mem::take(&mut self.cards).into_values() {
            if let CardRuntime::Browser(mut browser) = runtime {
                browser.handle.shutdown();
            }
        }
        for teardown in self.terminal_teardowns.drain(..) {
            let _ = teardown.join();
        }
        for teardown in self.browser_teardowns.drain(..) {
            let _ = teardown.join();
        }
        self.browser_factory.take();
        if let Some(parent_lifetime) = self.browser_parent_lifetime.take() {
            parent_lifetime.deactivate();
        }
    }
}

fn setting_input<'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>> + 'a,
    value: &'a str,
    field: SettingsField,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    widget::column::with_children(vec![
        widget::text::caption_heading(label).into(),
        widget::text_input(label, value)
            .on_input(move |value| Message::SettingsChanged { field, value })
            .width(Length::Fill)
            .into(),
        widget::text::caption(detail)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
    ])
    .spacing(spacing.space_xxxs)
    .width(Length::Fill)
    .into()
}

fn settings_choice<'a>(
    label: &'static str,
    detail: &'static str,
    field: SettingsField,
    choices: impl IntoIterator<Item = (&'static str, &'static str, bool)>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let buttons: Vec<Element<'a, Message>> = choices
        .into_iter()
        .map(|(choice, value, selected)| -> Element<'a, Message> {
            let button = widget::button::standard(choice).on_press(Message::SettingsChanged {
                field,
                value: value.to_owned(),
            });
            if selected {
                button.class(cosmic::theme::Button::Suggested).into()
            } else {
                button.into()
            }
        })
        .collect();
    widget::column::with_children(vec![
        widget::text::caption_heading(label).into(),
        widget::row::with_children(buttons)
            .spacing(spacing.space_xxs)
            .into(),
        widget::text::caption(detail)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
    ])
    .spacing(spacing.space_xxxs)
    .width(Length::Fill)
    .into()
}

fn settings_group<'a>(
    title: &'static str,
    description: &'static str,
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let mut children = Vec::with_capacity(controls.len() + 2);
    children.push(widget::text::title4(title).into());
    children.push(
        widget::text::caption(description)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into(),
    );
    children.extend(controls);
    widget::container(
        widget::column::with_children(children)
            .spacing(spacing.space_s)
            .width(Length::Fill),
    )
    .class(cosmic::theme::Container::Card)
    .padding(spacing.space_s)
    .width(Length::Fill)
    .into()
}

fn settings_category_controls<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
) -> Vec<Element<'a, Message>> {
    let spacing = cosmic::theme::spacing();
    match category {
        SettingsCategory::General => vec![
            settings_group(
                "Runtime",
                "Control how many live columns cagents keeps active and when background work is suspended.",
                vec![
                    setting_input(
                        "Maximum hot columns",
                        "Applies immediately; extra background columns may hibernate.",
                        &draft.max_hot_cards,
                        SettingsField::MaxHotCards,
                    ),
                    setting_input(
                        "Hibernation delay (ms)",
                        "Applies immediately to background columns.",
                        &draft.hibernation_delay_ms,
                        SettingsField::HibernationDelayMs,
                    ),
                ],
            ),
            settings_group(
                "Configuration",
                "The on-disk settings format is managed by cagents.",
                vec![
                    widget::container(widget::text::monotext(&draft.schema_version))
                        .class(cosmic::theme::Container::Secondary)
                        .padding([spacing.space_xxs, spacing.space_xs])
                        .width(Length::Fill)
                        .into(),
                ],
            ),
        ],
        SettingsCategory::Appearance => vec![settings_group(
            "Workspace layout",
            "Tune the persistent navigation and the starting size of newly created columns.",
            vec![
                setting_input(
                    "Sidebar width (px)",
                    format!(
                        "Applies immediately, {}. Dragging the pane's edge sets it too.",
                        sidebar_width_range()
                    ),
                    &draft.sidebar_width,
                    SettingsField::SidebarWidth,
                ),
                setting_input(
                    "Default column fraction",
                    "Applies to new columns only; existing widths are preserved.",
                    &draft.default_column_fraction,
                    SettingsField::DefaultColumnFraction,
                ),
            ],
        )],
        SettingsCategory::Terminal => vec![settings_group(
            "Integrated terminal",
            "Rendering changes apply immediately. Scrollback capacity applies to terminals started after the change.",
            vec![
                setting_input(
                    "Font size",
                    "A value from 8 to 40 pixels.",
                    &draft.terminal_font_size,
                    SettingsField::TerminalFontSize,
                ),
                setting_input(
                    "Font family",
                    "Leave empty for the system monospace font. Powerline prompts need a font that includes their glyphs.",
                    &draft.terminal_font_family,
                    SettingsField::TerminalFontFamily,
                ),
                setting_input(
                    "Scrollback lines",
                    "From 0 to 1,000,000 lines per newly started terminal.",
                    &draft.terminal_scrollback_lines,
                    SettingsField::TerminalScrollbackLines,
                ),
            ],
        )],
        SettingsCategory::Links => vec![settings_group(
            "Link handling",
            "Choose where modified clicks on known links open. Regular clicks stay inside the workspace.",
            vec![
                settings_choice(
                    "Open modified links in",
                    "Applies to pull request links and other links whose destination is known to cagents.",
                    SettingsField::LinkOpenDestination,
                    [
                        (
                            "Integrated browser",
                            LinkOpenDestination::IntegratedBrowser.as_str(),
                            draft.link_open_destination == LinkOpenDestination::IntegratedBrowser,
                        ),
                        (
                            "System default",
                            LinkOpenDestination::SystemDefault.as_str(),
                            draft.link_open_destination == LinkOpenDestination::SystemDefault,
                        ),
                    ],
                ),
                settings_choice(
                    "Modified-click gesture",
                    "This gesture opens the link in the destination above.",
                    SettingsField::LinkOpenGesture,
                    [
                        (
                            "Ctrl+Click",
                            LinkOpenGesture::ControlClick.as_str(),
                            draft.link_open_gesture == LinkOpenGesture::ControlClick,
                        ),
                        (
                            "Ctrl+Shift+Click",
                            LinkOpenGesture::ControlShiftClick.as_str(),
                            draft.link_open_gesture == LinkOpenGesture::ControlShiftClick,
                        ),
                    ],
                ),
            ],
        )],
        SettingsCategory::Keyboard => vec![settings_group(
            "Keyboard shortcuts",
            "Use Ctrl, Alt, Shift, or Super plus A-Z, 0-9, Left, or Right. Every shortcut must be unique.",
            vec![
                setting_input(
                    "New terminal",
                    "Creates a terminal column.",
                    &draft.keyboard_shortcuts.new_terminal,
                    SettingsField::ShortcutNewTerminal,
                ),
                setting_input(
                    "Close focused column",
                    "Closes the active workspace column.",
                    &draft.keyboard_shortcuts.close_focused_column,
                    SettingsField::ShortcutCloseFocusedColumn,
                ),
                setting_input(
                    "Search projects",
                    "Moves keyboard focus to project search.",
                    &draft.keyboard_shortcuts.search_projects,
                    SettingsField::ShortcutSearchProjects,
                ),
                setting_input(
                    "Previous column",
                    "Moves focus left and may repeat while held.",
                    &draft.keyboard_shortcuts.previous_column,
                    SettingsField::ShortcutPreviousColumn,
                ),
                setting_input(
                    "Next column",
                    "Moves focus right and may repeat while held.",
                    &draft.keyboard_shortcuts.next_column,
                    SettingsField::ShortcutNextColumn,
                ),
            ],
        )],
    }
}

fn settings_dialog<'a>(
    category: SettingsCategory,
    draft: &'a SettingsDraft,
    feedback: Option<&'a SettingsFeedback>,
    progress: f32,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let navigation: Vec<Element<'a, Message>> = SettingsCategory::ALL
        .into_iter()
        .map(|item| -> Element<'a, Message> {
            let button = widget::button::standard(item.label())
                .on_press(Message::SelectSettingsCategory(item))
                .width(Length::Fill);
            if item == category {
                button.class(cosmic::theme::Button::Suggested).into()
            } else {
                button.into()
            }
        })
        .collect();
    let navigation: Element<'a, Message> = widget::container(
        widget::column::with_children(navigation)
            .spacing(spacing.space_xxs)
            .width(Length::Fill),
    )
    .class(cosmic::theme::Container::Secondary)
    .padding(spacing.space_s)
    .width(Length::Fixed(220.0))
    .height(Length::Fill)
    .into();

    let controls = settings_category_controls(category, draft);
    let content: Element<'a, Message> = widget::container(
        widget::scrollable(
            widget::column::with_children(controls)
                .spacing(spacing.space_s)
                .width(Length::Fill),
        )
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .class(cosmic::theme::Container::Primary)
    .padding(spacing.space_m)
    .width(Length::Fill)
    .height(Length::Fill)
    .into();

    let feedback: Element<'a, Message> = if let Some(feedback) = feedback {
        let (icon, message) = match feedback {
            SettingsFeedback::Saved => ("emblem-ok-symbolic", "Saved to settings.toml".to_owned()),
            SettingsFeedback::Invalid(message) => ("dialog-warning-symbolic", message.clone()),
            SettingsFeedback::Error(message) => (
                "dialog-error-symbolic",
                format!("Could not save settings: {message}"),
            ),
        };
        widget::row::with_children(vec![
            widget::icon::from_name(icon).size(16).into(),
            widget::text::caption(message)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs)
        .into()
    } else {
        iced::widget::Space::new().width(Length::Fill).into()
    };

    let header = widget::row::with_children(vec![
        widget::column::with_children(vec![
            widget::text::title2("Settings").into(),
            widget::text::caption(category.label()).into(),
        ])
        .spacing(spacing.space_xxxs)
        .width(Length::Fill)
        .into(),
        widget::button::icon(widget::icon::from_name("window-close-symbolic"))
            .id(widget::Id::new("dialog.settings.close"))
            .name("Close settings")
            .on_press(Message::CloseInspector)
            .into(),
    ])
    .align_y(Alignment::Center);
    let footer = widget::row::with_children(vec![
        widget::button::standard("Reset to defaults")
            .leading_icon(widget::icon::from_name("edit-undo-symbolic").size(16))
            .on_press(Message::ResetSettings)
            .into(),
        feedback,
        widget::button::suggested("Done")
            .on_press(Message::CloseInspector)
            .into(),
    ])
    .align_y(Alignment::Center)
    .spacing(spacing.space_xs);
    let surface = widget::container(
        widget::column::with_children(vec![
            header.into(),
            widget::divider::horizontal::light().into(),
            widget::row::with_children(vec![navigation, content])
                .spacing(spacing.space_s)
                .height(Length::Fill)
                .into(),
            widget::divider::horizontal::light().into(),
            footer.into(),
        ])
        .spacing(spacing.space_s)
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .class(cosmic::theme::Container::Dialog(false))
    .padding(spacing.space_m)
    .width(Length::Fill)
    .height(Length::Fill)
    .max_width(1_100.0)
    .max_height(820.0);

    let progress = progress.clamp(0.0, 1.0);
    let offset = 18.0 * (1.0 - progress);
    widget::container(surface)
        .center(Length::Fill)
        .padding(Padding {
            top: 28.0 + offset,
            right: 28.0,
            bottom: 28.0 - offset,
            left: 28.0,
        })
        .class(cosmic::theme::Container::custom(move |_| {
            cosmic::iced::widget::container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgba(
                    0.0,
                    0.0,
                    0.0,
                    0.56 * progress,
                ))),
                ..Default::default()
            }
        }))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn spawn_browser(
    session: &BrowserSession,
    visible: bool,
    scale_factor: f32,
    fallback: Option<Arc<dyn WaylandSubsurfaceFactory>>,
) -> Result<BrowserRuntime, String> {
    let restore = BrowserRestoreState {
        url: session.current_url.clone(),
        history: session.history.clone(),
        history_index: session.history_index,
        scroll_x: session.scroll_x,
        scroll_y: session.scroll_y,
        scale_factor: scale_factor.max(0.01),
        viewport: PhysicalViewport::new(1280, 800),
    };
    BrowserHandle::spawn(restore, visible, fallback)
        .map(|handle| BrowserRuntime {
            handle,
            ime: None,
            lifecycle: if visible {
                BrowserLifecycle::Visible
            } else {
                BrowserLifecycle::Hibernated
            },
            address: session.current_url.as_str().to_owned(),
            load_state: None,
        })
        .map_err(|error| error.to_string())
}

fn repository_card<'a>(view: &'a RepositoryView) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let name = view
        .worktree
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Repository".into());
    widget::container(
        widget::column::with_children(vec![
            widget::row::with_children(vec![
                widget::icon::from_name("folder-symbolic").size(20).into(),
                widget::text::title3(name).into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs)
            .into(),
            widget::text::body(view.worktree.display().to_string())
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            widget::text::caption(format!("Git directory: {}", view.repository.display()))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            widget::row::with_children(vec![
                widget::button::suggested("New terminal here")
                    .leading_icon(widget::icon::from_name("utilities-terminal-symbolic").size(16))
                    .on_press(Message::NewTerminal)
                    .into(),
            ])
            .spacing(spacing.space_xxs)
            .into(),
        ])
        .align_x(Alignment::Start)
        .spacing(spacing.space_s)
        .max_width(CONTENT_MAX_WIDTH),
    )
    .padding(spacing.space_m)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn browser_command(
    cards: &HashMap<CardId, CardRuntime>,
    card: CardId,
    command: impl FnOnce(&BrowserHandle) -> Result<(), crate::browser::BrowserError>,
    status: &mut Option<TransientNotice>,
) -> Task<Message> {
    if let Some(CardRuntime::Browser(browser)) = cards.get(&card)
        && let Err(error) = command(&browser.handle)
    {
        *status = Some(TransientNotice::error(error.to_string()));
    }
    Task::none()
}

fn load_repository(path: &Path, project_id: Option<ProjectId>) -> Result<LoadedRepository, String> {
    let service = RepositoryService::system("git");
    let cancellation = CancellationToken::default();
    let repository = service
        .discover(path, &cancellation)
        .map_err(|error| error.to_string())?;
    let worktrees = service
        .list_worktrees(&repository.common_dir, &cancellation)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|worktree| {
            let status = (!worktree.bare)
                .then(|| service.status(&worktree.path, &cancellation).ok())
                .flatten();
            (worktree, status)
        })
        .collect::<Vec<_>>();
    let github = GitHubService::system("gh");
    let github_repository = github
        .resolve_repository(&repository.selected_path, &cancellation)
        .ok();
    let pull_requests = github_repository
        .as_ref()
        .and_then(|repository| {
            github
                .list(
                    &ListPullRequestsRequest {
                        repository: repository.clone(),
                        state: PullRequestStateFilter::Open,
                        limit: 50,
                    },
                    &cancellation,
                )
                .ok()
                .map(|pull_requests| {
                    let checks =
                        github.checks_for_pull_requests(repository, &pull_requests, &cancellation);
                    pull_requests.into_iter().zip(checks).collect::<Vec<_>>()
                })
        })
        .unwrap_or_default();
    let name = repository
        .common_dir
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repository.common_dir.display().to_string());
    Ok(LoadedRepository {
        project: ProjectState {
            id: project_id.unwrap_or_default(),
            generation: 0,
            name,
            repository: repository.common_dir.clone(),
            existence: Existence::Open,
        },
        repository,
        worktrees,
        github_repository,
        pull_requests,
    })
}

fn terminal_start_mode(session: &TerminalSession) -> Result<TerminalStartMode, String> {
    match &session.continuity {
        TerminalContinuity::Live { .. } | TerminalContinuity::DetachedSameBoot { .. } => {
            let proof = session
                .continuity
                .live_proof()
                .expect("proof-backed continuity has a host proof");
            if proof.is_complete() {
                Ok(TerminalStartMode::Attach(proof))
            } else {
                Err(format!(
                    "terminal {} has incomplete host continuity proof; explicit relaunch is required",
                    session.id
                ))
            }
        }
        TerminalContinuity::RelaunchAvailable { recipe, .. } => {
            Ok(TerminalStartMode::Spawn(recipe.clone()))
        }
        TerminalContinuity::Exited { .. } => Err(format!(
            "terminal {} has exited; explicit relaunch is required",
            session.id
        )),
        TerminalContinuity::Failed { typed_error, .. } => Err(format!(
            "terminal {} cannot resume: {}; explicit relaunch is required",
            session.id, typed_error.message
        )),
    }
}

fn branch_destination_component(branch: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut component = String::with_capacity(branch.len());
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            component.push(char::from(byte));
        } else {
            component.push('%');
            component.push(char::from(HEX[usize::from(byte >> 4)]));
            component.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    component
}

fn close_focused_message(workspace: &Workspace) -> Option<Message> {
    workspace.focused().map(Message::CloseCard)
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn launch_recipe(working_directory: impl Into<PathBuf>) -> LaunchRecipe {
        LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: working_directory.into(),
            environment: Default::default(),
        }
    }
    #[test]
    fn embedded_application_icon_is_valid_rgba() {
        let (rgba, size) = application_window_icon().into_raw();
        assert_eq!(size.width, APPLICATION_ICON_SIZE);
        assert_eq!(size.height, APPLICATION_ICON_SIZE);
        assert_eq!(
            rgba.len(),
            (APPLICATION_ICON_SIZE * APPLICATION_ICON_SIZE * 4) as usize
        );
    }

    #[test]
    fn branch_destination_is_one_safe_component() {
        let component = branch_destination_component("feature/security/fix%2F");
        assert_eq!(component, "feature%2Fsecurity%2Ffix%252F");
        assert_eq!(Path::new(&component).components().count(), 1);
    }

    #[test]
    fn proof_backed_continuity_is_attach_only() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let launch = launch_recipe("/tmp");
        let mut session = TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("host"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        };
        assert!(matches!(
            terminal_start_mode(&session),
            Ok(TerminalStartMode::Attach(_))
        ));

        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        };
        assert!(matches!(
            terminal_start_mode(&session),
            Ok(TerminalStartMode::Spawn(_))
        ));
        assert!(restored_terminal_failure(&session).is_none());
        assert_eq!(
            restored_runtime_residency(
                &CardKind::Terminal(session),
                Exposure::Visible,
                &Residency::Cold,
            ),
            Residency::Hibernated
        );
    }

    #[test]
    fn exited_terminal_retries_with_fresh_spawn_identity_and_exit_metadata() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let launch = launch_recipe("/worktree");
        let mut session = TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: Some("stale-provider-session".into()),
            continuity: TerminalContinuity::Live {
                host_session_id: HostSessionId::new("host"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        };
        let exited_id = session.id;
        let last_exit = TerminalExit {
            code: Some(17),
            signal: None,
            occurred_at_unix_ms: 42,
        };

        transition_terminal_to_relaunchable(&mut session, last_exit.clone());
        assert_eq!(
            session.continuity,
            TerminalContinuity::RelaunchAvailable {
                recipe: launch.clone(),
                last_exit: Some(last_exit.clone()),
            }
        );

        let relaunched = fresh_terminal_relaunch(&session);
        assert_ne!(relaunched.id, exited_id);
        assert_eq!(relaunched.launch, launch.clone());
        assert_eq!(relaunched.resume_hint, None);
        assert!(matches!(
            terminal_start_mode(&relaunched),
            Ok(TerminalStartMode::Spawn(recipe)) if recipe == launch
        ));
        assert!(matches!(
            relaunched.continuity,
            TerminalContinuity::RelaunchAvailable {
                last_exit: Some(exit),
                ..
            } if exit == last_exit
        ));
    }

    #[test]
    fn restored_child_exit_stays_retryable_without_starting_the_stale_identity() {
        let launch = launch_recipe("/worktree");
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: Some(TerminalExit {
                    code: Some(17),
                    signal: None,
                    occurred_at_unix_ms: 42,
                }),
            },
        };

        assert_eq!(
            restored_terminal_failure(&session).as_deref(),
            Some("terminal exited with status 17; Retry starts a new terminal")
        );
        assert!(matches!(
            restored_runtime_residency(
                &CardKind::Terminal(session),
                Exposure::Visible,
                &Residency::Hot,
            ),
            Residency::Failed {
                typed_error: RuntimeError {
                    code,
                    message,
                },
                retryable: true,
            } if code == "terminal_exited"
                && message == "terminal exited with status 17; Retry starts a new terminal"
        ));
    }

    #[test]
    fn sidebar_refresh_targets_every_existing_project_without_opening_cards() {
        let first = ProjectId::new();
        let second = ProjectId::new();
        let projects = vec![
            SidebarProject::new(
                first,
                RepositoryId::new(),
                "first".into(),
                PathBuf::from("/repositories/first"),
                Vec::new(),
                Vec::new(),
            ),
            SidebarProject::new(
                second,
                RepositoryId::new(),
                "second".into(),
                PathBuf::from("/repositories/second"),
                Vec::new(),
                Vec::new(),
            ),
        ];

        assert_eq!(
            repository_refresh_targets(&projects),
            vec![
                (PathBuf::from("/repositories/first"), first),
                (PathBuf::from("/repositories/second"), second),
            ]
        );
    }

    #[test]
    fn failed_sidebar_refresh_finishes_loading_with_an_error() {
        let mut data_state = SidebarDataState::Loading;

        finish_repository_load_failure(&mut data_state, "refresh failed".into());

        assert_eq!(data_state, SidebarDataState::Error("refresh failed".into()));
    }

    #[test]
    fn viewport_resize_preserves_focused_card_reveal_and_requests_synchronization() {
        let config = WorkspaceConfig::default();
        let mut workspace = Workspace::new(config).unwrap();
        let cards = [CardId::new(), CardId::new(), CardId::new()];
        for (index, id) in cards.into_iter().enumerate() {
            workspace
                .add_card(
                    WorkspaceCard::new(id, format!("card {index}"), CardWidth::Fixed(480.0))
                        .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        workspace.focus(cards[2]).unwrap();

        let (_, synchronize_scroll) =
            update_workspace_viewport(&mut workspace, WorkspaceViewportEvent::Resized(600.0));
        let expected_target = workspace
            .geometry()
            .focus_offset(cards[2], 600.0, config.reveal_margin)
            .unwrap();

        assert!(synchronize_scroll);
        assert_eq!(workspace.focused(), Some(cards[2]));
        assert_eq!(workspace.camera().target(), expected_target);
        assert!(workspace.camera().is_animating());
    }

    fn terminal_card_state() -> CardState {
        let launch = launch_recipe("/tmp");
        CardState {
            id: CardId::new(),
            generation: 0,
            title: "Terminal".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating {
                epoch: INITIAL_HYDRATION_EPOCH,
            },
            kind: CardKind::Terminal(TerminalSession {
                id: TerminalId::new(),
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            }),
        }
    }

    #[test]
    fn camera_only_viewport_update_does_not_require_persistence() {
        let card = terminal_card_state();
        let mut durable = WorkspaceState {
            focused: Some(card.id),
            ..WorkspaceState::default()
        };
        durable.cards.push(card);
        let before = durable.clone();
        let camera_only = ViewportUpdate {
            offset_changed: true,
            transitions: Vec::new(),
        };

        assert!(!apply_durable_viewport_update(
            &mut durable,
            before.focused,
            &camera_only.transitions,
        ));
        assert_eq!(durable, before);
    }

    #[test]
    fn viewport_exposure_and_focus_changes_require_persistence() {
        let mut card = terminal_card_state();
        card.exposure = Exposure::Offscreen;
        let card_id = card.id;
        let mut durable = WorkspaceState {
            focused: Some(card_id),
            ..WorkspaceState::default()
        };
        durable.cards.push(card);
        let exposure_change = VisibilityTransition {
            id: card_id,
            from: VisibilityClass::Offscreen,
            to: VisibilityClass::Visible,
        };

        assert!(apply_durable_viewport_update(
            &mut durable,
            Some(card_id),
            &[exposure_change],
        ));
        assert_eq!(durable.cards[0].exposure, Exposure::Visible);
        assert!(!apply_durable_viewport_update(
            &mut durable,
            Some(card_id),
            &[exposure_change],
        ));
        assert!(apply_durable_viewport_update(&mut durable, None, &[],));
        assert_eq!(durable.focused, None);
    }

    #[test]
    fn restored_offscreen_terminal_waits_for_lifecycle_hydration() {
        let mut state = terminal_card_state();
        state.exposure = Exposure::Offscreen;
        state.residency = Residency::Cold;
        let card = state.id;
        let expected_session = match &state.kind {
            CardKind::Terminal(session) => session.clone(),
            _ => unreachable!(),
        };

        let restored = restored_runtime_residency(&state.kind, state.exposure, &state.residency);
        assert_eq!(restored, Residency::Hibernated);

        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget::default());
        lifecycle.register(card, state.exposure, restored, now);
        assert!(
            lifecycle
                .reconcile(&HashSet::new(), &HashSet::new(), now)
                .iter()
                .all(|action| !matches!(action, LifecycleAction::Hydrate { .. }))
        );

        let actions = lifecycle.reconcile(&HashSet::from([card]), &HashSet::new(), now);
        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card: id, .. } if *id == card)
        ));
        assert_eq!(
            deferred_terminal_session(Some(&state), true),
            Some(expected_session)
        );
        assert!(deferred_terminal_session(Some(&state), false).is_none());
    }

    #[test]
    fn initial_terminal_hydration_prioritizes_visible_cards_within_budget() {
        let visible = CardId::new();
        let prefetch = CardId::new();
        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        lifecycle.register(visible, Exposure::Visible, Residency::Hibernated, now);
        lifecycle.register(prefetch, Exposure::Prefetch, Residency::Hibernated, now);

        let actions =
            lifecycle.reconcile(&HashSet::from([visible]), &HashSet::from([prefetch]), now);
        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == visible)
        ));
        assert!(actions.iter().all(
            |action| !matches!(action, LifecycleAction::Hydrate { card, .. } if *card == prefetch)
        ));
    }

    #[test]
    fn restored_static_cards_do_not_block_visible_terminal_admission() {
        let static_kind = CardKind::Repository(RepositoryView {
            project: ProjectId::new(),
            repository: PathBuf::from("/repository"),
            worktree: PathBuf::from("/repository/worktree"),
        });
        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        for static_card in (0..8).map(|_| CardId::new()) {
            if let Some(residency) =
                restored_terminal_residency(&static_kind, Exposure::Visible, &Residency::Hot)
            {
                lifecycle.register(static_card, Exposure::Visible, residency, now);
            }
        }

        let terminal = terminal_card_state();
        let residency =
            restored_terminal_residency(&terminal.kind, terminal.exposure, &terminal.residency)
                .unwrap();
        lifecycle.register(terminal.id, terminal.exposure, residency, now);
        let actions = lifecycle.reconcile(&HashSet::from([terminal.id]), &HashSet::new(), now);

        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == terminal.id)
        ));
    }

    #[test]
    fn late_terminal_start_is_rejected_after_card_close() {
        let mut state = terminal_card_state();
        assert!(can_accept_terminal_start(Some(&state), true, true));

        state.existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("test close".into()),
            },
        };
        assert!(!can_accept_terminal_start(Some(&state), true, true));
        assert!(!can_accept_terminal_start(Some(&state), false, true));
    }

    #[test]
    fn completed_terminal_start_uses_current_exposure_for_visibility() {
        let mut state = terminal_card_state();
        assert_eq!(terminal_start_visibility(Some(&state)), Some(true));

        state.exposure = Exposure::Prefetch;
        assert_eq!(terminal_start_visibility(Some(&state)), Some(false));
        state.exposure = Exposure::Offscreen;
        assert_eq!(terminal_start_visibility(Some(&state)), Some(false));
    }

    #[test]
    fn failed_terminal_hydration_frees_budget_for_an_immediate_sweep() {
        let failed = CardId::new();
        let waiting = CardId::new();
        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        lifecycle.register(
            failed,
            Exposure::Visible,
            Residency::Hydrating { epoch: 7 },
            now,
        );
        lifecycle.register(waiting, Exposure::Visible, Residency::Hibernated, now);
        assert!(lifecycle.hydration_finished(
            failed,
            7,
            Err(RuntimeError {
                code: "start_failed".into(),
                message: "could not start".into(),
            }),
        ));

        let actions = lifecycle.reconcile(&HashSet::from([failed, waiting]), &HashSet::new(), now);
        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == waiting)
        ));
    }

    #[test]
    fn restored_nonvisible_browser_waits_for_visible_lifecycle_hydration() {
        let url = Url::parse("about:blank").unwrap();
        let kind = CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: BrowserRestoreFidelity::ExactLive,
        });
        assert_eq!(
            restored_runtime_residency(&kind, Exposure::Offscreen, &Residency::Cold),
            Residency::Hibernated
        );
        assert_eq!(
            restored_runtime_residency(&kind, Exposure::Prefetch, &Residency::Hot),
            Residency::Hibernated
        );
        assert_eq!(
            restored_runtime_residency(&kind, Exposure::Visible, &Residency::Hot),
            Residency::Hibernated
        );

        let card = CardId::new();
        let state = CardState {
            id: card,
            generation: 0,
            title: "Browser".into(),
            width_fraction: 0.9,
            existence: Existence::Open,
            exposure: Exposure::Prefetch,
            residency: Residency::Hibernated,
            kind,
        };
        assert!(can_register_deferred_browser(&state, false, true));

        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget::default());
        lifecycle.register(card, Exposure::Prefetch, Residency::Hibernated, now);
        assert!(
            lifecycle
                .reconcile(&HashSet::new(), &HashSet::new(), now)
                .iter()
                .all(|action| !matches!(action, LifecycleAction::Hydrate { .. }))
        );
        let actions = lifecycle.reconcile(&HashSet::from([card]), &HashSet::new(), now);
        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card: id, .. } if *id == card)
        ));
    }

    #[test]
    fn restored_terminal_and_browser_share_the_startup_hot_budget() {
        let terminal = CardId::new();
        let browser = CardId::new();
        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        lifecycle.register(
            terminal,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            now,
        );
        lifecycle.register(browser, Exposure::Visible, Residency::Hibernated, now);

        let actions =
            lifecycle.reconcile(&HashSet::from([terminal, browser]), &HashSet::new(), now);
        assert!(actions.iter().all(
            |action| !matches!(action, LifecycleAction::Hydrate { card, .. } if *card == browser)
        ));
    }

    #[test]
    fn crashed_browser_frees_budget_for_immediate_reconciliation() {
        let crashed = CardId::new();
        let waiting = CardId::new();
        let now = Instant::now();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        lifecycle.register(crashed, Exposure::Visible, Residency::Hot, now);
        lifecycle.register(waiting, Exposure::Visible, Residency::Hibernated, now);
        lifecycle.register(
            crashed,
            Exposure::Visible,
            Residency::Failed {
                typed_error: RuntimeError {
                    code: "browser_crashed".into(),
                    message: "crashed".into(),
                },
                retryable: true,
            },
            now,
        );

        let actions = lifecycle.reconcile(&HashSet::from([crashed, waiting]), &HashSet::new(), now);
        assert!(actions.iter().any(
            |action| matches!(action, LifecycleAction::Hydrate { card, .. } if *card == waiting)
        ));
    }

    #[test]
    fn failed_eviction_rolls_back_to_retryable_hot_state() {
        let now = Instant::now();
        let card = CardId::new();
        let mut lifecycle = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        lifecycle.register(card, Exposure::Offscreen, Residency::Hot, now);
        let epoch = lifecycle
            .reconcile(&HashSet::new(), &HashSet::new(), now)
            .into_iter()
            .find_map(|action| match action {
                LifecycleAction::Hibernate {
                    card: candidate,
                    epoch,
                } if candidate == card => Some(epoch),
                _ => None,
            })
            .unwrap();

        assert_eq!(
            rollback_failed_hibernation(&mut lifecycle, card, epoch, now),
            Some(Residency::Hot)
        );
        assert!(
            lifecycle
                .reconcile(&HashSet::new(), &HashSet::new(), now)
                .iter()
                .any(|action| matches!(
                    action,
                    LifecycleAction::Hibernate {
                        card: candidate,
                        ..
                    } if *candidate == card
                ))
        );
    }

    #[test]
    fn ctrl_w_closes_the_focused_card_and_is_a_no_op_without_cards() {
        let mut workspace = Workspace::default();
        assert!(close_focused_message(&workspace).is_none());

        let card = CardId::new();
        workspace
            .add_card(
                WorkspaceCard::new(card, "Terminal", CardWidth::Fixed(320.0)).unwrap(),
                0,
                false,
            )
            .unwrap();
        assert!(matches!(
            close_focused_message(&workspace),
            Some(Message::CloseCard(id)) if id == card
        ));
    }

    #[test]
    fn valid_settings_edits_request_persistence_and_unchanged_values_do_not() {
        let settings = AppSettings::default();
        assert_eq!(
            settings_update(
                &settings,
                SettingsField::TerminalFontSize,
                &settings.terminal_font_size.to_string(),
            ),
            SettingsDecision::Unchanged
        );
        let SettingsDecision::Persist(updated) =
            settings_update(&settings, SettingsField::TerminalFontSize, "16")
        else {
            panic!("a valid changed setting must request persistence");
        };
        assert_eq!(updated.terminal_font_size, 16.0);
        assert_eq!(updated.sidebar_width, settings.sidebar_width);
    }

    #[test]
    fn settings_edits_report_field_validation_before_persistence() {
        let settings = AppSettings::default();
        assert!(matches!(
            settings_update(&settings, SettingsField::MaxHotCards, "0"),
            SettingsDecision::Invalid(message) if message.contains("max_hot_cards")
        ));
        let out_of_range = sidebar::EXPANDED_WIDTH_MAX + 1.0;
        assert!(matches!(
            settings_update(
                &settings,
                SettingsField::SidebarWidth,
                &out_of_range.to_string()
            ),
            SettingsDecision::Invalid(message) if message.contains(&sidebar_width_range())
        ));
        assert!(matches!(
            settings_update(&settings, SettingsField::DefaultColumnFraction, "not a number"),
            SettingsDecision::Invalid(message) if message.contains("number")
        ));
    }

    #[test]
    fn dialog_transition_is_symmetric_and_reversible() {
        let started_at = Instant::now();
        let midpoint = started_at + DIALOG_TRANSITION_DURATION / 2;
        let finished = started_at + DIALOG_TRANSITION_DURATION;
        let opening = InspectorTransition {
            started_at,
            from: 0.0,
            to: 1.0,
        };
        let closing = InspectorTransition {
            started_at,
            from: 1.0,
            to: 0.0,
        };

        assert_eq!(opening.progress(started_at), 0.0);
        assert_eq!(opening.progress(midpoint), 0.5);
        assert_eq!(opening.progress(finished), 1.0);
        assert_eq!(closing.progress(started_at), 1.0);
        assert_eq!(closing.progress(midpoint), 0.5);
        assert_eq!(closing.progress(finished), 0.0);
        let reversal_started = started_at + DIALOG_TRANSITION_DURATION / 4;
        let reversal_from = opening.progress(reversal_started);
        let reversed = InspectorTransition {
            started_at: reversal_started,
            from: reversal_from,
            to: 0.0,
        };
        assert_eq!(reversed.progress(reversal_started), reversal_from);
        assert_eq!(
            reversed.progress(reversal_started + reversed.duration()),
            0.0
        );
    }

    #[test]
    fn escape_dismisses_dialogs_only_on_the_initial_press() {
        let key = keyboard::Key::Named(keyboard::key::Named::Escape);
        let event = |repeat| keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key.clone(),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat,
        };

        assert!(is_escape_press(&event(false)));
        assert!(!is_escape_press(&event(true)));
    }

    #[test]
    fn a_shell_shortened_working_directory_is_recognised_as_the_same_path() {
        // A prompt that abbreviates every component but the last would
        // otherwise stack two spellings of one path in the card header.
        assert!(title_abbreviates_path(
            "~/D/r/cagents",
            "~/Documents/repositories/cagents"
        ));
        assert!(title_abbreviates_path(
            "~/Documents/repositories/cagents",
            "~/Documents/repositories/cagents"
        ));
        assert!(title_abbreviates_path("~", "~"));
    }

    #[test]
    fn a_title_naming_anything_but_the_directory_survives() {
        let caption = "~/Documents/repositories/cagents";

        assert!(!title_abbreviates_path("cargo test", caption));
        assert!(!title_abbreviates_path("~/D/r/other", caption));
        // A prefix of the components is a different, shorter path.
        assert!(!title_abbreviates_path("~/D/r", caption));
        assert!(!title_abbreviates_path("~/D/r/cagents/src", caption));
        assert!(!title_abbreviates_path("", caption));
    }

    #[test]
    fn a_path_outside_the_home_directory_is_shown_in_full() {
        // `~` is only an abbreviation when the prefix is actually the home
        // directory, and the home directory itself has no shorter tail.
        assert_eq!(display_path(Path::new("/etc/hosts")), "/etc/hosts");
        let home = directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned());
        if let Some(home) = home {
            assert_eq!(display_path(&home), home.to_string_lossy());
            assert_eq!(display_path(&home.join("src/x")), "~/src/x");
        }
    }
}
