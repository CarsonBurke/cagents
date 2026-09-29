mod account_ui;
mod agent_ui;
mod automation_ui;
mod branch_ui;
#[cfg(feature = "integrated-browser")]
mod browser_import_ui;
mod channel_ui;
mod chat_ui;
mod chrome_ui;
mod daemon_recovery;
mod daemon_stop;
pub(crate) mod dialog_ui;
mod file_ui;
mod git_ui;
mod history_ui;
#[cfg(feature = "latency-harness")]
mod latency_ui;
mod notification_ui;
mod project_ui;
mod pull_request_ui;
mod refresh_ui;
mod review_request_ui;
#[cfg(test)]
mod safety_tests;
mod settings_ui;
mod sidebar_selection_ui;
mod terminal_adoption;
mod terminal_close;
mod update_ui;
mod workspace_model_sync;
mod worktree_dialog;
mod worktree_operations;
mod worktree_ui;
pub use daemon_stop::run_daemon_stop_confirmation;
use worktree_dialog::{
    WorktreeDialog, WorktreeDraft, WorktreeField, WorktreeInputError, worktree_dialog_focus_id,
};

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, LazyLock, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[cfg(feature = "integrated-browser")]
use std::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
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
    account::{self, Account},
    design::{self, DockLayout, InspectorLayout},
    github::{
        CreatePullRequestRequest, GitHubAvailability, GitHubRepoKey, GitHubService,
        ListPullRequestsRequest, PullRequestCreationOutcome, PullRequestStateFilter,
    },
    inspector::{self, InspectorAction, RuntimeStatus as InspectorRuntimeStatus},
    instance,
    lifecycle::{LifecycleAction, LifecycleBudget, LifecycleManager},
    location::SanitizedLocation,
    lsp::{LspCoordinates, LspDocumentId, LspError, LspEvent, LspManager, LspPosition, LspStatus},
    model::{
        CardId, CardKind, CardState, Existence, Exposure, HostSessionProof, LaunchRecipe,
        ProjectId, ProjectKind, ProjectState, RepositoryView, Residency, RuntimeError,
        TerminalContinuity, TerminalExit, TerminalId, TerminalSession, WorkspaceState,
        WorktreeState,
    },
    pull_request_review,
    repository::{
        CancellationToken, CommitOutcome, CommitRequest, CommitSummary, CreateWorktreeRequest,
        DiffBasis, HistoryRequest, HistoryRevision, MutationOutcome, PushOutcome, PushRequest,
        RepoPath, RepositoryId, RepositoryInfo, RepositoryService, RepositorySnapshot,
        StageRequest, StatusSnapshot, UnstageRequest, WorktreeCreationOutcome, WorktreeRecord,
    },
    repository_widget::{
        self, LspIntent, RepositoryAction, RepositoryRuntime, RepositoryRuntimeStatus,
    },
    session::{
        QUARANTINE_GUIDANCE, QuarantinedWrite, ReplacedIdentity, SessionRuntime, SessionSaveHealth,
        SessionSaveHealthEvent, WriteSubject,
    },
    settings::{
        self, AppSettings, AppTheme, KeyboardShortcuts, LinkOpenDestination, LinkOpenGesture,
        PullRequestOpenTarget, TerminalCursorStyle,
    },
    sidebar::{
        self, ActiveWorktreeKey, MutationKind, PanelSection, SidebarAction, SidebarDataState,
        SidebarFocus, SidebarMode, SidebarModel, SidebarProject, SidebarPullRequest, SidebarState,
        SidebarWorktree, SourceControlModel,
    },
    splitter::{Edge as SplitterEdge, Splitter},
    terminal::{
        HostedTerminalHandle, TerminalConfig, TerminalEvent, TerminalSurface, TerminalWidget,
    },
    workspace::{
        CardWidth, ViewportUpdate, VisibilityClass, VisibilityTransition, Workspace, WorkspaceCard,
        WorkspaceConfig, normalize_card_name,
    },
    workspace_widget::{self, FrameClock, WorkspaceViewportEvent},
};

#[cfg(not(feature = "integrated-browser"))]
use crate::capability::{BrowserRestoreAction, browser_restore_action};

#[cfg(feature = "integrated-browser")]
use crate::{
    browser::{
        BrowserEvent, BrowserHandle, BrowserLifecycle, BrowserRestoreState, BrowserWidget,
        DelegateRequest, DelegateRequestId, InputMethodState, PhysicalViewport,
        RawWaylandSubsurfaceFactory, WaylandSubsurfaceFactory,
    },
    model::{BrowserId, BrowserRestoreFidelity, BrowserSession},
};

const APPLICATION_ICON_PNG: &[u8] = include_bytes!("../packaging/dev.cagents.Cagents.png");
const APPLICATION_ICON_SIZE: u32 = 128;

/// The name shown to a person, as opposed to `APP_ID`, which only matches the
/// window to its desktop entry.
const APPLICATION_NAME: &str = instance::APPLICATION_NAME;

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

#[cfg(any(feature = "integrated-browser", test))]
const INITIAL_HYDRATION_EPOCH: u64 = 1;

#[cfg(feature = "integrated-browser")]
#[derive(Debug)]
struct ParentWindowLifetime {
    window_id: window::Id,
    active: AtomicBool,
}

#[cfg(feature = "integrated-browser")]
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

#[cfg(feature = "integrated-browser")]
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

#[derive(Debug, Clone)]
pub struct StartedTerminal {
    handle: TerminalSurface,
    session: TerminalSession,
    spawned: bool,
}

const TOOLBAR_FOCUS_ID: &str = "cagents-workspace-toolbar";
const NOTICE_ROW_HEIGHT: f32 = design::ROW_HEIGHT;
const BROWSER_TOOLBAR_HEIGHT: f32 = design::ROW_HEIGHT;
#[cfg(feature = "integrated-browser")]
const BROWSER_PROGRESS_HEIGHT: f32 = 2.0;
const ICON_CONTROL_SIZE: f32 = design::PRIMARY_CONTROL_SIZE;
const DIALOG_TRANSITION_DURATION: Duration = Duration::from_millis(160);
/// How long an exit waits once its window is gone: a frame, which lets winit
/// finish its pass and flush the window's destruction before iced tears down.
const EXIT_FLUSH_GRACE: Duration = Duration::from_millis(16);
/// How long an exit waits for iced to report its window gone. iced reports it
/// within a frame; the bound only keeps a lost report from leaving an
/// invisible process holding the session lease, so no new window could open.
const EXIT_WINDOW_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CONTENT_MAX_WIDTH: f32 = 560.0;
const STATUS_SURFACE_MAX_WIDTH: f32 = 360.0;

/// Variants are ordered by escalation, so `max` picks the severity a combined
/// surface must present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

    /// The component whose filled `base` a notice surface paints itself with.
    fn component(self, theme: &cosmic::theme::CosmicTheme) -> &cosmic::cosmic_theme::Component {
        match self {
            Self::Success => &theme.success,
            Self::Warning => &theme.warning,
            Self::Error => &theme.destructive,
        }
    }

    /// Subordinate text on that filled surface: the surface's own foreground,
    /// dimmed, so the hierarchy reads without losing legibility.
    fn detail_color(self) -> iced::Color {
        let cosmic = cosmic::theme::active();
        let on: iced::Color = self.component(cosmic.cosmic()).on.into();
        on.scale_alpha(0.75)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransientNotice {
    severity: NoticeSeverity,
    message: String,
}

/// Notices retained for review. Bounded because nothing else bounds them: the
/// shell raises a notice from almost a hundred call sites, several of them on
/// per-frame or per-poll paths.
const NOTICE_LOG_CAPACITY: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoggedNotice {
    id: u64,
    notice: TransientNotice,
    /// Repeats of an identical message collapse onto one entry rather than
    /// flooding the log; a failing poll would otherwise evict everything else.
    repeats: u32,
}

/// The shell's notice history: a bounded ring plus the identity counter that
/// lets a dismiss request name exactly one entry.
#[derive(Debug, Default)]
struct NoticeLog {
    entries: VecDeque<LoggedNotice>,
    next_id: u64,
}

impl NoticeLog {
    fn record(&mut self, notice: TransientNotice) {
        // Scanning the whole log rather than only its tail is what makes
        // deduplication survive interleaving: a poll that raises two failures
        // in turn would otherwise alternate them forever and fill the log with
        // copies. The repeat moves to the back because the user cares that it
        // is happening now, and `recent` reads newest first.
        if let Some(index) = self.entries.iter().position(|entry| entry.notice == notice) {
            let mut entry = self.entries.remove(index).expect("index came from the log");
            entry.repeats = entry.repeats.saturating_add(1);
            self.entries.push_back(entry);
            return;
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push_back(LoggedNotice {
            id: self.next_id,
            notice,
            repeats: 1,
        });
        while self.entries.len() > NOTICE_LOG_CAPACITY {
            self.entries.pop_front();
        }
    }

    fn dismiss(&mut self, id: u64) {
        self.entries.retain(|entry| entry.id != id);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Newest first: the log reads downward from the most recent event.
    ///
    /// Nothing retires on a timer. A notice the user never saw is worse than
    /// one that waits: the header carries the severity until it is dismissed,
    /// and capacity is what bounds the log.
    fn recent(&self) -> impl Iterator<Item = &LoggedNotice> {
        self.entries.iter().rev()
    }

    fn severity(&self) -> Option<NoticeSeverity> {
        self.entries.iter().map(|entry| entry.notice.severity).max()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveHealthState {
    database_path: Option<PathBuf>,
    revision: u64,
    health: SessionSaveHealth,
    last_successful_flush_at_unix_ms: Option<i64>,
    quarantined: Vec<QuarantinedWrite>,
    retry_requested: bool,
}

impl SaveHealthState {
    fn new(database_path: PathBuf) -> Self {
        Self {
            database_path: Some(database_path),
            revision: 0,
            health: SessionSaveHealth::Healthy,
            last_successful_flush_at_unix_ms: None,
            quarantined: Vec::new(),
            retry_requested: false,
        }
    }

    fn writer_unavailable(database_path: Option<PathBuf>) -> Self {
        Self {
            database_path,
            revision: 1,
            health: SessionSaveHealth::WriterUnavailable,
            last_successful_flush_at_unix_ms: None,
            quarantined: Vec::new(),
            retry_requested: false,
        }
    }

    fn apply(&mut self, event: SessionSaveHealthEvent) -> bool {
        if event.revision <= self.revision {
            return false;
        }
        self.revision = event.revision;
        if event.last_successful_flush_at_unix_ms.is_some() {
            self.last_successful_flush_at_unix_ms = event.last_successful_flush_at_unix_ms;
        }
        // Recovery is a user-visible fact, not a transient edge. A later
        // healthy flush updates its timestamp but cannot erase it before the
        // person has acknowledged that persistence resumed.
        if !matches!(
            (&self.health, &event.health),
            (
                SessionSaveHealth::Recovered { .. },
                SessionSaveHealth::Healthy
            )
        ) {
            self.health = event.health;
        }
        self.quarantined = event.quarantined;
        self.retry_requested = false;
        true
    }

    /// Whether a retry can do anything the lane will not do on its own: after
    /// its automatic attempts ran out, or for a write storage refused. A
    /// writer that stopped would take the request and never answer it, leaving
    /// the notice waiting for good.
    fn offers_retry(&self) -> bool {
        !self.retry_requested
            && match self.health {
                SessionSaveHealth::RetryExhausted { .. } => true,
                SessionSaveHealth::WriterUnavailable => false,
                _ => !self.quarantined.is_empty(),
            }
    }

    fn request_retry(&mut self, session: &SessionRuntime) -> bool {
        if !self.offers_retry() {
            return false;
        }
        self.retry_requested = session.retry_save();
        self.retry_requested
    }

    fn acknowledge_recovery(&mut self) -> bool {
        if matches!(self.health, SessionSaveHealth::Recovered { .. }) {
            self.health = SessionSaveHealth::Healthy;
            true
        } else {
            false
        }
    }

    fn is_visible(&self) -> bool {
        !matches!(self.health, SessionSaveHealth::Healthy) || !self.quarantined.is_empty()
    }

    /// Whether storage refused part of the window's own state, which a
    /// terminal handoff has to be committed in.
    fn window_state_refused(&self) -> bool {
        self.quarantined
            .iter()
            .any(|write| write.subject.is_window_state())
    }
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

fn unix_time_label(value: i64) -> String {
    let seconds = value.div_euclid(1_000);
    let milliseconds = value.rem_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_unix_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{milliseconds:03} UTC")
}

/// Converts days since 1970-01-01 to the proleptic Gregorian calendar.
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn last_success_label(value: Option<i64>) -> String {
    value.map_or_else(
        || "none in this run".to_owned(),
        |value| unix_time_label(value),
    )
}

fn notice_surface<'a>(
    severity: NoticeSeverity,
    body: Element<'a, Message>,
    action: Option<Element<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let mut children = vec![
        crate::ui_controls::symbolic_icon(severity.icon())
            .icon()
            .size(16)
            .into(),
        body,
    ];
    if let Some(action) = action {
        children.push(action);
    }
    let mut surface = widget::container(
        widget::row::with_children(children)
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
    )
    .class(cosmic::theme::Container::custom(move |theme| {
        let component = severity.component(theme.cosmic());
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
    .padding(if compact {
        Padding::from([0, spacing.space_xs])
    } else {
        Padding::from(spacing.space_xs)
    })
    .width(Length::Fill);
    if compact {
        surface = surface.height(Length::Fixed(NOTICE_ROW_HEIGHT));
    }
    surface.into()
}

/// Notice text the user has to read and often copy.
///
/// Two reasons this is not `design::body`/`design::caption`. Notice bodies
/// carry database paths, URLs and error strings, so they must be selectable —
/// the chat transcript already uses `selectable_text` for exactly that. And
/// `design::caption` attaches its own muted text class, which resolves against
/// the *ambient* container and therefore overrides the `text_color` that
/// `notice_surface` sets for the severity, leaving the title in the severity
/// foreground and the detail lines in the ambient one.
fn notice_body<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_BODY)
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(Length::Fill)
        .into()
}
/// Selectable equivalent of `design::caption`: diagnostic detail the user
/// needs to copy into a bug report, a shell, or a search. For a plain dialog
/// surface; subordinate text on a filled severity surface belongs in
/// `notice_surface_detail`.
fn notice_detail<'a>(text: impl Into<Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_CAPTION)
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(Length::Fill)
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .into()
}

/// Subordinate text inside a `notice_surface`.
///
/// The tone comes from the surface's own foreground rather than
/// `design::muted_text`, which resolves against the window background: on a
/// filled severity surface that produced a near-illegible grey on pink.
fn notice_surface_detail<'a>(
    severity: NoticeSeverity,
    text: impl Into<Cow<'a, str>> + 'a,
) -> Element<'a, Message> {
    widget::selectable_text(text)
        .size(design::TEXT_CAPTION)
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(Length::Fill)
        .class(cosmic::theme::Text::Color(severity.detail_color()))
        .into()
}

/// Button styling for a control inside a `notice_surface`.
///
/// The default button classes resolve their text and hover fills against the
/// window background, so on a filled severity surface they rendered close to
/// invisible — a pink label on a pink error surface. Every colour here comes
/// from the surface's own component instead.
fn notice_surface_button(severity: NoticeSeverity) -> cosmic::theme::Button {
    // Keyboard focus draws the outline at full strength and wider: the
    // shell's accent focus ring would vanish against a severity fill.
    let appearance = move |focused: bool, theme: &cosmic::Theme| {
        let component = severity.component(theme.cosmic());
        let on: iced::Color = component.on.into();
        widget::button::Style {
            background: None,
            text_color: Some(on),
            icon_color: Some(on),
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            border_width: if focused { 2.0 } else { 1.0 },
            border_color: if focused { on } else { on.scale_alpha(0.4) },
            ..widget::button::Style::new()
        }
    };
    // A wash of the surface's own foreground: the only fill guaranteed to read
    // as raised whichever severity colour is underneath it.
    let wash = move |theme: &cosmic::Theme, alpha: f32| {
        let on: iced::Color = severity.component(theme.cosmic()).on.into();
        iced::Background::Color(on.scale_alpha(alpha))
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| appearance(focused, theme)),
        disabled: Box::new(move |theme| widget::button::Style {
            text_color: Some(severity.detail_color()),
            icon_color: Some(severity.detail_color()),
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(wash(theme, 0.12)),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(wash(theme, 0.2)),
            ..appearance(focused, theme)
        }),
    }
}

fn notice_surface_action<'a>(
    severity: NoticeSeverity,
    label: &'a str,
    accessible_name: &'a str,
    id: widget::Id,
    message: Message,
) -> Element<'a, Message> {
    widget::button::text(label)
        .id(id)
        .name(accessible_name)
        .class(notice_surface_button(severity))
        .on_press(message)
        .into()
}

/// The save-health notices: the writer lane's state, and the writes storage
/// refused, which the lane reports beside its state rather than as one.
/// `card_title` names the card a refused terminal identity belongs to.
fn save_health_views<'a>(
    health: &'a SaveHealthState,
    card_title: impl Fn(CardId) -> Option<&'a str>,
) -> Vec<Element<'a, Message>> {
    let path = health.database_path.as_deref().map_or_else(
        || "unavailable (the data directory could not be resolved)".to_owned(),
        |path| path.display().to_string(),
    );
    let refused = save_quarantine_view(health, &path, card_title);
    save_lane_view(health, path)
        .into_iter()
        .chain(refused)
        .collect()
}

/// One refused write as the save-health notice lists it.
fn quarantined_write_line<'a>(
    write: &QuarantinedWrite,
    card_title: &impl Fn(CardId) -> Option<&'a str>,
) -> String {
    let subject = match &write.subject {
        WriteSubject::Workspace { card: None } => "Window layout and cards".to_owned(),
        WriteSubject::Workspace { card: Some(card) } => card_title(*card).map_or_else(
            || "Window layout and cards (one card was refused)".to_owned(),
            |title| format!("Window layout and cards (card “{title}” was refused)"),
        ),
        WriteSubject::CardIdentity {
            card,
            kind: ReplacedIdentity::Terminal,
        } => card_title(*card).map_or_else(
            || "A relaunched terminal's new identity".to_owned(),
            |title| format!("Relaunched terminal “{title}”"),
        ),
        WriteSubject::CardIdentity {
            card,
            kind: ReplacedIdentity::Chat,
        } => card_title(*card).map_or_else(
            || "A chat card's new conversation".to_owned(),
            |title| format!("New conversation in “{title}”"),
        ),
        WriteSubject::Project { name, .. } => format!("Project “{name}”"),
        WriteSubject::Worktree { path, branch, .. } => match branch {
            Some(branch) => format!("Worktree {} ({branch})", path.display()),
            None => format!("Worktree {}", path.display()),
        },
    };
    let mut line = format!(
        "{subject} — {} · since {}",
        write.category.label(),
        unix_time_label(write.refused_at_unix_ms),
    );
    // Changing the card cannot supersede a refusal of the card
    // itself, so name the remedy the generic guidance only implies.
    if matches!(
        write.subject,
        WriteSubject::CardIdentity { .. } | WriteSubject::Workspace { card: Some(_) }
    ) {
        line.push_str(" · closing the card clears it");
    }
    match write.held_back {
        0 => {}
        1 => line.push_str(" · 1 worktree waits on it"),
        count => line.push_str(&format!(" · {count} worktrees wait on it")),
    }
    line
}

fn save_quarantine_view<'a>(
    health: &'a SaveHealthState,
    path: &str,
    card_title: impl Fn(CardId) -> Option<&'a str>,
) -> Option<Element<'a, Message>> {
    if health.quarantined.is_empty() {
        return None;
    }
    let title = match health.quarantined.len() {
        1 => "A change cannot be saved — storage refused it".to_owned(),
        count => format!("{count} changes cannot be saved — storage refused them"),
    };
    let mut detail = health
        .quarantined
        .iter()
        .map(|write| quarantined_write_line(write, &card_title))
        .collect::<Vec<_>>();
    detail.push(format!("Database: {path}"));
    // An exhausted lane offers the one retry on its own notice; a retry
    // re-offers these writes too, so a second button would only repeat it.
    let lane_offers_retry = matches!(health.health, SessionSaveHealth::RetryExhausted { .. });
    let (guidance, action) = if health.retry_requested {
        ("Retry requested; waiting for the session writer.", None)
    } else {
        (
            QUARANTINE_GUIDANCE,
            (health.offers_retry() && !lane_offers_retry).then(|| {
                notice_surface_action(
                    NoticeSeverity::Error,
                    "Retry save",
                    "Offer the refused changes to storage again",
                    widget::Id::new("session-save.retry"),
                    Message::RetrySessionSave,
                )
            }),
        )
    };
    let spacing = cosmic::theme::spacing();
    Some(notice_surface(
        NoticeSeverity::Error,
        widget::column::with_children(vec![
            notice_body(title),
            notice_surface_detail(NoticeSeverity::Error, detail.join("\n")),
            notice_surface_detail(NoticeSeverity::Error, guidance),
        ])
        .spacing(spacing.space_xxxs)
        .width(Length::Fill)
        .into(),
        action,
        false,
    ))
}

fn save_lane_view(health: &SaveHealthState, path: String) -> Option<Element<'_, Message>> {
    let last_success = last_success_label(health.last_successful_flush_at_unix_ms);
    let (severity, title, detail, guidance, action): (
        _,
        String,
        String,
        &'static str,
        Option<Element<'_, Message>>,
    ) = match &health.health {
        SessionSaveHealth::Healthy => return None,
        SessionSaveHealth::Retrying {
            category,
            first_failed_at_unix_ms,
            attempts,
        } => (
            NoticeSeverity::Warning,
            format!("Session changes are not saved — {}", category.label()),
            format!(
                "Database: {path}\nFirst failure: {} · Attempts: {attempts}\nLast successful flush: {last_success}",
                unix_time_label(*first_failed_at_unix_ms),
            ),
            "Automatic retries are still running; cagents remains usable.",
            None,
        ),
        SessionSaveHealth::RetryExhausted {
            category,
            first_failed_at_unix_ms,
            attempts,
        } => {
            let (guidance, action) = if health.retry_requested {
                ("Retry requested; waiting for the session writer.", None)
            } else {
                (
                    category.guidance(),
                    Some(notice_surface_action(
                        NoticeSeverity::Error,
                        "Retry save",
                        "Retry saving the session",
                        widget::Id::new("session-save.retry"),
                        Message::RetrySessionSave,
                    )),
                )
            };
            (
                NoticeSeverity::Error,
                format!("Session changes are not saved — {}", category.label()),
                format!(
                    "Database: {path}\nFirst failure: {} · Attempts: {attempts}\nLast successful flush: {last_success}",
                    unix_time_label(*first_failed_at_unix_ms),
                ),
                guidance,
                action,
            )
        }
        SessionSaveHealth::Recovered {
            recovered_at_unix_ms,
        } => (
            NoticeSeverity::Success,
            "Session saving recovered".to_owned(),
            format!(
                "Database: {path}\nRecovered: {}\nLast successful flush: {last_success}",
                unix_time_label(*recovered_at_unix_ms),
            ),
            "Changes are being saved again.",
            Some(crate::interaction::tooltip(
                widget::button::icon(crate::ui_controls::symbolic_icon("window-close-symbolic"))
                    .id(widget::Id::new("session-save.acknowledge-recovery"))
                    .name("Acknowledge session save recovery")
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .class(notice_surface_button(NoticeSeverity::Success))
                    .on_press(Message::AcknowledgeSessionRecovery),
                "Acknowledge recovery",
            )),
        ),
        SessionSaveHealth::WriterUnavailable => (
            NoticeSeverity::Error,
            "Session changes cannot be saved — writer unavailable".to_owned(),
            format!("Database: {path}\nLast successful flush: {last_success}"),
            "Keep cagents open if you need the running terminals, then restart it to restore session saving.",
            None,
        ),
    };
    let spacing = cosmic::theme::spacing();
    Some(notice_surface(
        severity,
        widget::column::with_children(vec![
            notice_body(title),
            notice_surface_detail(severity, detail),
            notice_surface_detail(severity, guidance),
        ])
        .spacing(spacing.space_xxxs)
        .width(Length::Fill)
        .into(),
        action,
        false,
    ))
}

#[derive(Debug, Clone, Copy)]
struct TerminalWakeup {
    card: CardId,
    generation: u64,
}

#[derive(Clone)]
struct TerminalWakeSource(async_channel::Receiver<TerminalWakeup>);

#[derive(Clone)]
struct UiActivationSource(async_channel::Receiver<instance::UiActivation>);

/// Hosted terminals observed on the daemon, from the inventory observer thread.
#[derive(Clone)]
struct HostedTerminalSource(async_channel::Receiver<Vec<crate::terminal::ipc::TerminalDescriptor>>);

impl PartialEq for HostedTerminalSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for HostedTerminalSource {}

impl std::hash::Hash for HostedTerminalSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-hosted-terminals", state);
    }
}

/// Shared workspace model updates, from the observer and publisher threads.
#[derive(Clone)]
struct WorkspaceModelSource(async_channel::Receiver<workspace_model_sync::ModelUpdate>);

impl PartialEq for WorkspaceModelSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for WorkspaceModelSource {}

impl std::hash::Hash for WorkspaceModelSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-workspace-model", state);
    }
}

fn workspace_model_stream(
    source: &WorkspaceModelSource,
) -> Pin<
    Box<
        dyn cosmic::iced::futures::Stream<Item = workspace_model_sync::ModelUpdate>
            + Send
            + 'static,
    >,
> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|update| (update, receiver)) },
    ))
}

#[derive(Clone)]
struct SessionHealthSource(async_channel::Receiver<SessionSaveHealthEvent>);

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

impl PartialEq for UiActivationSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for UiActivationSource {}

impl std::hash::Hash for UiActivationSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-ui-activation", state);
    }
}

impl PartialEq for SessionHealthSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for SessionHealthSource {}

impl std::hash::Hash for SessionHealthSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-session-save-health", state);
    }
}

/// Keyed by the manager's generation, so replacing the manager - which is how
/// a settings change takes effect, because a shutdown is one-way - rebuilds the
/// subscription onto the new channel instead of leaving it reading a queue
/// nobody writes to.
#[derive(Clone)]
struct LspEventSource(async_channel::Receiver<LspEvent>, u64);

impl PartialEq for LspEventSource {
    fn eq(&self, other: &Self) -> bool {
        self.1 == other.1
    }
}

impl Eq for LspEventSource {}

impl std::hash::Hash for LspEventSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&"cagents-language-server-events", state);
        std::hash::Hash::hash(&self.1, state);
    }
}

fn lsp_event_stream(
    source: &LspEventSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = LspEvent> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|event| (event, receiver)) },
    ))
}

fn terminal_wake_stream(
    source: &TerminalWakeSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = TerminalWakeup> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|wake| (wake, receiver)) },
    ))
}

fn hosted_terminal_stream(
    source: &HostedTerminalSource,
) -> Pin<
    Box<
        dyn cosmic::iced::futures::Stream<Item = Vec<crate::terminal::ipc::TerminalDescriptor>>
            + Send
            + 'static,
    >,
> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move {
            receiver
                .recv()
                .await
                .ok()
                .map(|observed| (observed, receiver))
        },
    ))
}

fn ui_activation_stream(
    source: &UiActivationSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = instance::UiActivation> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move {
            receiver
                .recv()
                .await
                .ok()
                .map(|activation| (activation, receiver))
        },
    ))
}

fn session_health_stream(
    source: &SessionHealthSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = SessionSaveHealthEvent> + Send + 'static>> {
    Box::pin(cosmic::iced::futures::stream::unfold(
        (source.0.clone(), false),
        |(receiver, ended)| async move {
            if ended {
                return None;
            }
            match receiver.recv().await {
                Ok(event) => {
                    let ended = matches!(event.health, SessionSaveHealth::WriterUnavailable);
                    Some((event, (receiver, ended)))
                }
                Err(_) => Some((
                    SessionSaveHealthEvent {
                        revision: u64::MAX,
                        health: SessionSaveHealth::WriterUnavailable,
                        last_successful_flush_at_unix_ms: None,
                        // The writer is gone, and with it every write it held;
                        // the unavailable notice already says nothing saves.
                        quarantined: Vec::new(),
                    },
                    (receiver, true),
                )),
            }
        },
    ))
}

/// What a relaunch of this terminal would do, as its card offers it.
fn terminal_relaunch_advice(session: &TerminalSession) -> &'static str {
    if session.agent_resume_recipe().is_some() {
        "Restart resumes the saved agent session in a new terminal."
    } else {
        "Restart starts a new terminal; no exact agent session is available."
    }
}

fn terminal_completed(session: &TerminalSession) -> bool {
    matches!(&session.continuity,
        TerminalContinuity::RelaunchAvailable { last_exit: Some(exit), .. }
        | TerminalContinuity::Exited { last_exit: exit }
        if exit.code == Some(0) && exit.signal.is_none())
}

/// The terminal a card shows, once the card is not in the middle of starting
/// one.
///
/// A relaunch carries the previous exit forward as metadata, so between
/// pressing Restart and the daemon answering the session still reads as ended.
/// Hydration is what tells the two apart.
fn settled_terminal(state: Option<&CardState>) -> Option<&TerminalSession> {
    let state = state?;
    if matches!(state.residency, Residency::Hydrating { .. }) {
        return None;
    }
    match &state.kind {
        CardKind::Terminal(session) => Some(session),
        _ => None,
    }
}

fn completed_terminal(state: Option<&CardState>) -> Option<&TerminalSession> {
    settled_terminal(state).filter(|session| terminal_completed(session))
}

/// Whether a terminal's process has ended, cleanly or not.
///
/// An unclean exit is still an end, not a runtime failure: the surface holds
/// the only account of what the process did, and that account is wanted most
/// precisely when the process died badly.
fn terminal_ended(session: &TerminalSession) -> bool {
    matches!(
        &session.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(_),
            ..
        } | TerminalContinuity::Exited { .. }
    )
}

fn ended_terminal(state: Option<&CardState>) -> Option<&TerminalSession> {
    settled_terminal(state).filter(|session| terminal_ended(session))
}

/// How an ended terminal finished, as the card's status line states it.
fn terminal_end_summary(session: &TerminalSession) -> String {
    if terminal_completed(session) {
        return "Completed".into();
    }
    let (TerminalContinuity::RelaunchAvailable {
        last_exit: Some(last_exit),
        ..
    }
    | TerminalContinuity::Exited { last_exit }) = &session.continuity
    else {
        return "Ended".into();
    };
    match (last_exit.code, last_exit.signal.as_deref()) {
        // The host reports the signal the way a shell names it -- "Killed",
        // "Terminated", "Hangup" -- which is already the whole sentence.
        (_, Some(signal)) => signal.to_owned(),
        (Some(code), None) => format!("Exited with status {code}"),
        (None, None) => "Ended".into(),
    }
}

fn restored_runtime_residency(
    kind: &CardKind,
    _exposure: Exposure,
    durable_residency: &Residency,
) -> Residency {
    match kind {
        CardKind::Terminal(_) => Residency::Hibernated,
        CardKind::Browser(_) => Residency::Hibernated,
        CardKind::Chat(_) => Residency::Hibernated,
        CardKind::Repository(_) | CardKind::PullRequests(_) => durable_residency.clone(),
        // Utility panes hold no process or page, so they are never hibernated.
        CardKind::Settings | CardKind::Accounts | CardKind::Automations => Residency::Hot,
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

#[cfg(feature = "integrated-browser")]
fn can_register_deferred_browser(state: &CardState, runtime_is_loading: bool) -> bool {
    runtime_is_loading && state.existence.is_open() && matches!(&state.kind, CardKind::Browser(_))
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

fn lifecycle_sweep_timer(delay: Duration, generation: u64) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(delay).await;
        Message::LifecycleSweepElapsed(generation)
    })
}

/// Whether a wakeup at `requested` needs a timer of its own, given the wakeup
/// already pending.
///
/// A pending timer already fires no later than `requested` and re-arms itself
/// for whatever remains, so only an earlier deadline needs a new timer.
fn sweep_needs_rearming(pending: Option<Instant>, requested: Instant) -> bool {
    pending.is_none_or(|deadline| requested < deadline)
}

fn should_show_card_navigator(total: usize) -> bool {
    total > 1
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

/// Normalises every title restored from the state database.
///
/// A title stored by a build that predates [`normalize_card_name`] was never
/// bounded or stripped, and a restart must not reinstate one: the same title
/// reaches the card header, the five accessibility strings and the compositor
/// toplevel title as a freshly reported one does. A title left with nothing
/// legible falls back to its card kind rather than blanking the header.
fn normalize_restored_titles(state: &mut WorkspaceState) {
    for card in &mut state.cards {
        card.title = normalize_card_name(&card.title)
            .unwrap_or_else(|| card_kind_labels(Some(&card.kind)).type_label.to_owned());
    }
}

/// Retires every restored utility pane after the first of its kind.
///
/// A window holds one Settings pane and one Accounts pane: opening one again
/// focuses the existing pane, and their views share fixed widget ids. Only a
/// damaged or hand-edited snapshot can hold two, and restoring both would
/// leave the second one unreachable by that path.
fn retire_duplicate_utility_cards(state: &mut WorkspaceState) {
    let mut seen = Vec::<CardKind>::new();
    let mut retired = false;
    for card in &mut state.cards {
        if !card.existence.is_open() || !card.kind.is_utility() {
            continue;
        }
        if seen.contains(&card.kind) {
            card.existence = project_ui::removed_existence("duplicate utility pane");
            retired = true;
        } else {
            seen.push(card.kind.clone());
        }
    }
    if retired {
        state.normalize_focus();
        state.normalize_channels();
    }
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
        Some(CardKind::Chat(_)) => CardLabels {
            type_label: "Chat",
            icon: "chat-bubble-text-symbolic",
            starting: "Connecting to agent…",
            unavailable: "Agent connection unavailable",
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
            icon: "pull-request-symbolic",
            starting: "Loading pull requests…",
            unavailable: "Pull requests unavailable",
        },
        Some(CardKind::Settings) => CardLabels {
            type_label: "Settings",
            icon: "emblem-system-symbolic",
            starting: "Loading settings…",
            unavailable: "Settings unavailable",
        },
        Some(CardKind::Accounts) => CardLabels {
            type_label: "Accounts",
            icon: "avatar-default-symbolic",
            starting: "Loading accounts…",
            unavailable: "Accounts unavailable",
        },
        Some(CardKind::Automations) => CardLabels {
            type_label: "Automations",
            icon: "alarm-symbolic",
            starting: "Loading automations…",
            unavailable: "Automations unavailable",
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
        CardKind::Chat(session) => display_path(&session.launch.recipe.working_directory),
        CardKind::Browser(session) => {
            Cow::Owned(SanitizedLocation::from(&session.current_url).into_inner())
        }
        CardKind::PullRequests(view) => display_path(&view.repository),
        CardKind::Repository(view) => display_path(&view.worktree),
        // Neither pane belongs to a place: both apply to the whole application.
        CardKind::Settings | CardKind::Accounts | CardKind::Automations => {
            Cow::Borrowed("Application-wide")
        }
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

/// The process's starting directory, already formatted for display.
///
/// Nothing in this application calls `set_current_dir`, so this value cannot
/// change while the process runs. The empty-workspace surface is the default
/// first-run view and `view` re-runs on every redraw, so resolving this there
/// cost a blocking `getcwd` plus three allocations per frame.
static STARTING_DIRECTORY: LazyLock<PathBuf> =
    LazyLock::new(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
static STARTING_DIRECTORY_CAPTION: LazyLock<String> =
    LazyLock::new(|| format!("Starting directory: {}", display_path(&STARTING_DIRECTORY)));

/// A terminal that has never run, under a new identity, waiting to launch `launch`.
pub(super) fn fresh_terminal_session(launch: LaunchRecipe) -> TerminalSession {
    let id = TerminalId::new();
    TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        id,
        transcript_path: transcript_path_for(&launch, id),
        launch: launch.clone(),
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    }
}

fn terminal_transcript_path(id: TerminalId) -> Option<PathBuf> {
    instance::project_dirs().map(|dirs| {
        dirs.data_dir()
            .join("transcripts")
            .join(format!("{id}.log"))
    })
}

fn terminal_exit(exit_code: u32, signal: Option<&str>) -> TerminalExit {
    TerminalExit {
        // A signalled end has no meaningful status of its own: the shell's
        // 128+n convention would otherwise read as a plain exit code.
        code: signal
            .is_none()
            .then(|| i32::try_from(exit_code).ok())
            .flatten(),
        signal: signal.map(str::to_owned),
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
    let resume_recipe = session.agent_resume_recipe();
    let resume_hint = resume_recipe
        .as_ref()
        .and_then(|_| session.resume_hint.clone());
    let recipe = resume_recipe.unwrap_or(recipe);
    let id = TerminalId::new();
    TerminalSession {
        owned_codex_listener: None,
        recovery_origin: Some(session.recovery_origin.unwrap_or(session.id)),
        id,
        launch: recipe.clone(),
        transcript_path: transcript_path_for(&recipe, id),
        resume_hint,
        continuity: TerminalContinuity::RelaunchAvailable { recipe, last_exit },
    }
}

/// Where a new, relaunched or cloned terminal records its output: nowhere
/// for a provider sign-in, whose screen can carry a pasted authorization
/// code.
fn transcript_path_for(recipe: &LaunchRecipe, id: TerminalId) -> Option<PathBuf> {
    if account_ui::is_sign_in(recipe) {
        return None;
    }
    terminal_transcript_path(id)
}

fn repository_refresh_targets(projects: &[SidebarProject]) -> Vec<(PathBuf, ProjectId)> {
    projects
        .iter()
        .map(|project| (project.repository.clone(), project.id))
        .collect()
}

/// Older releases could persist several project IDs for one common Git
/// directory and move stable worktree IDs between them. Reconcile the startup
/// snapshot before building rows or scheduling refreshes; never delete the
/// underlying records or reset their optimistic writer generations.
fn reconcile_restored_projects(
    projects: &mut Vec<ProjectState>,
    worktrees: &mut [WorktreeState],
    order: &[ProjectId],
) {
    projects.sort_by_key(|project| {
        order
            .iter()
            .position(|id| *id == project.id)
            .unwrap_or(usize::MAX)
    });
    let mut repositories = HashMap::new();
    let mut aliases = HashMap::new();
    projects.retain_mut(|project| {
        if !project.existence.is_open() {
            return true;
        }
        if let Ok(canonical) = project.repository.canonicalize() {
            project.repository = canonical;
        }
        match repositories.entry(project.repository.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(project.id);
                true
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                aliases.insert(project.id, *entry.get());
                false
            }
        }
    });
    for worktree in worktrees {
        if let Some(project_id) = aliases.get(&worktree.project_id) {
            worktree.project_id = *project_id;
        }
    }
}

/// Record a failed repository read on the project it belongs to.
///
/// One checkout that cannot be read is one unreadable project. Replacing the
/// whole list with a pane-wide error took every healthy project down with the
/// broken one, so the reason is written onto that project's own row and the
/// pane-wide error is kept for the single case it describes truthfully: no
/// project row is left to carry it.
fn finish_repository_load_failure(model: &mut SidebarModel, project_id: ProjectId, error: String) {
    if let Some(project) = model
        .projects
        .iter_mut()
        .find(|project| project.id == project_id)
    {
        project.unreachable = Some(error);
        project.reloading = false;
        model.data_state = SidebarDataState::Ready;
        return;
    }
    // A failure with no row of its own is the first read of a freshly picked
    // path, which has no durable project yet, or a project closed while its
    // read was in flight. Either way there is nothing to mark, and the status
    // notice already carries the reason.
    model.data_state = if model.projects.is_empty() {
        SidebarDataState::Error(error)
    } else {
        SidebarDataState::Ready
    };
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
        WorkspaceViewportEvent::PointerEntered(card) => (
            workspace
                .focus_without_scrolling(card)
                .unwrap_or_else(|_| ViewportUpdate::unchanged()),
            false,
        ),
        WorkspaceViewportEvent::PointerFocused(card) => {
            let update = workspace
                .focus(card)
                .unwrap_or_else(|_| ViewportUpdate::unchanged());
            // The clicked child already placed its caret/selection. Activating
            // its pane must not move keyboard focus back to the pane title.
            workspace.take_widget_focus_request();
            (update, true)
        }
        // Reordering also updates the durable order, so the application owns it.
        WorkspaceViewportEvent::Reordered { .. } => (ViewportUpdate::unchanged(), false),
        WorkspaceViewportEvent::DragScrolled(offset) => (
            workspace.native_scroll(offset, workspace.viewport_width()),
            true,
        ),
    }
}

fn pull_request_head(
    projects: &[SidebarProject],
    project_id: ProjectId,
    worktree: Option<&Path>,
) -> String {
    projects
        .iter()
        .find(|project| project.id == project_id)
        .and_then(|project| match worktree {
            Some(path) => project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .and_then(|worktree| worktree.branch.clone()),
            // A project-level create has no worktree to bind to. The first
            // row's branch is not a choice the reader made.
            None => None,
        })
        .unwrap_or_default()
}

fn lone_chat_width(available: f32, preferred: CardWidth) -> f32 {
    let available = if available.is_finite() {
        available.max(0.0)
    } else {
        0.0
    };
    let limits = crate::workspace::WidthLimits::default();
    let preferred = preferred
        .resolve(available, limits)
        .clamp(limits.minimum.min(available), available);
    let wide = ((available - limits.minimum * 2.0) / limits.minimum).clamp(0.0, 1.0);
    available + (preferred - available) * wide
}

/// The GitHub base branch a comparison has already proved, or empty.
///
/// A remote-tracking ref is reduced with the remote names this worktree
/// actually configured, so `refs/remotes/origin/trunk` becomes `trunk` and a
/// remote whose name contains a slash is not split at the first one. Nothing
/// here invents `main`.
fn proven_pull_request_base(changes: &sidebar::BranchChanges, remotes: &[String]) -> String {
    let base = match changes {
        sidebar::BranchChanges::Ready(model) => &model.base_ref,
        sidebar::BranchChanges::Loading(base)
        | sidebar::BranchChanges::Error { base_ref: base, .. } => base,
        sidebar::BranchChanges::Unavailable(_) => return String::new(),
    };
    let name = base.as_str();
    if let Some(branch) = name.strip_prefix("refs/heads/") {
        return branch.to_owned();
    }
    let Some(rest) = name.strip_prefix("refs/remotes/") else {
        return String::new();
    };
    remotes
        .iter()
        .filter(|remote| {
            rest.strip_prefix(remote.as_str())
                .is_some_and(|tail| tail.starts_with('/'))
        })
        .max_by_key(|remote| remote.len())
        .map(|remote| rest[remote.len() + 1..].to_owned())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct LoadedRepository {
    project: ProjectState,
    repository: Option<RepositoryInfo>,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    github: GitHubAvailability,
    review_repository: Option<GitHubRepoKey>,
    head_repositories: HashMap<crate::model::WorktreeId, GitHubRepoKey>,
}

type WorktreePrCacheKey = (GitHubRepoKey, GitHubRepoKey, String, String);
static WORKTREE_PR_CACHE: LazyLock<
    Mutex<HashMap<WorktreePrCacheKey, (Instant, Option<crate::github::PullRequest>)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WORKTREE_PR_LOOKUP_LOCK: Mutex<()> = Mutex::new(());
const WORKTREE_PR_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// Resolve branches missing from the recent PR page within their source
/// repository. Historical results must name this head or an ancestor of it;
/// branch names alone are shared by forks and can be reused after closure.
fn attached_worktree_pull_requests(
    github: &GitHubService,
    key: &GitHubRepoKey,
    worktrees: &[(WorktreeRecord, Option<StatusSnapshot>)],
    recent: &[crate::github::PullRequest],
    head_repositories: &HashMap<crate::model::WorktreeId, GitHubRepoKey>,
    cancellation: &CancellationToken,
) -> Vec<crate::github::PullRequest> {
    // One repository lookup wave at a time keeps a global refresh from
    // multiplying the eight gh subprocesses by the number of projects.
    let _lookup_guard = WORKTREE_PR_LOOKUP_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if cancellation.is_cancelled() {
        return Vec::new();
    }
    let mut unique = HashSet::new();
    let missing: Vec<_> = worktrees
        .iter()
        .filter_map(|(record, _)| {
            if record.detached || record.bare {
                return None;
            }
            let branch = record.branch.as_deref()?.strip_prefix("refs/heads/")?;
            let oid = record.head_oid.as_deref()?.to_owned();
            let head_repository = head_repositories.get(&record.id)?;
            if matching_attached_pr(record, head_repository, recent).is_some() {
                return None;
            }
            let branch = branch.to_owned();
            unique
                .insert((head_repository.clone(), branch.clone(), oid.clone()))
                .then_some((branch, oid, record.path.clone(), head_repository.clone()))
        })
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    WORKTREE_PR_CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, (at, pr)| {
            at.elapsed()
                < if pr.is_some() {
                    WORKTREE_PR_CACHE_TTL
                } else {
                    Duration::from_secs(5 * 60)
                }
        });
    let found = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for shard in 0..8 {
            let missing = &missing;
            let found = &found;
            scope.spawn(move || {
                for (branch, oid, path, head_repository) in missing.iter().skip(shard).step_by(8) {
                    if cancellation.is_cancelled() {
                        break;
                    }
                    let cache_key = (
                        key.clone(),
                        head_repository.clone(),
                        branch.clone(),
                        oid.clone(),
                    );
                    let cached = WORKTREE_PR_CACHE
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&cache_key)
                        .filter(|(at, pr)| {
                            at.elapsed()
                                < if pr.is_some() {
                                    WORKTREE_PR_CACHE_TTL
                                } else {
                                    Duration::from_secs(5 * 60)
                                }
                        })
                        .map(|(_, pr)| pr.clone());
                    let resolved = match cached {
                        Some(result) => result,
                        None => match github.resolve_branch_head(
                            key,
                            head_repository,
                            branch,
                            oid,
                            cancellation,
                        ) {
                            Ok(result) => {
                                let result = result.filter(|pr| {
                                    pr.head_ref_oid == *oid
                                        || pr_head_is_ancestor(path, &pr.head_ref_oid, oid)
                                });
                                WORKTREE_PR_CACHE
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .insert(cache_key, (Instant::now(), result.clone()));
                                result
                            }
                            Err(_) => continue,
                        },
                    };
                    if let Some(pr) = resolved {
                        found.lock().unwrap_or_else(|p| p.into_inner()).push(pr);
                    }
                }
            });
        }
    });
    let mut result = found.into_inner().unwrap_or_else(|p| p.into_inner());
    result.sort_by_key(|pr| pr.number);
    result.dedup_by_key(|pr| pr.number);
    result
}

fn matching_attached_pr<'a>(
    record: &WorktreeRecord,
    head_repository: &GitHubRepoKey,
    pull_requests: &'a [crate::github::PullRequest],
) -> Option<&'a crate::github::PullRequest> {
    if record.detached || record.bare {
        return None;
    }
    let branch = record.branch.as_deref()?.strip_prefix("refs/heads/")?;
    let oid = record.head_oid.as_deref()?;
    let mut exact = pull_requests.iter().filter(|pr| {
        pr.head_ref_name == branch && pr.head_ref_oid == oid && pr.belongs_to_head(head_repository)
    });
    let first = exact.next()?;
    exact.next().is_none().then_some(first)
}

fn pr_head_is_ancestor(worktree: &Path, pr_head: &str, local_head: &str) -> bool {
    if !pr_head.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !local_head.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["merge-base", "--is-ancestor", pr_head, local_head])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn apply_attached_worktree_prs(
    project: &mut SidebarProject,
    worktrees: &[(WorktreeRecord, Option<StatusSnapshot>)],
    recent: &[crate::github::PullRequest],
    archived: &[crate::github::PullRequest],
) {
    let records = worktrees
        .iter()
        .map(|(record, _)| (record.id, record))
        .collect::<HashMap<_, _>>();
    for row in &mut project.worktrees {
        let Some(record) = records.get(&row.id).copied() else {
            continue;
        };
        if record.detached || record.bare {
            row.clear_pull_request();
            continue;
        }
        let Some(head_repository) = row.head_repository.as_ref() else {
            row.clear_pull_request();
            continue;
        };
        let mut exact = recent.iter().chain(archived).filter(|pr| {
            record
                .branch
                .as_deref()
                .and_then(|branch| branch.strip_prefix("refs/heads/"))
                == Some(pr.head_ref_name.as_str())
                && record.head_oid.as_deref() == Some(pr.head_ref_oid.as_str())
                && pr.belongs_to_head(head_repository)
        });
        let first = exact.next();
        let ambiguous = first.is_some_and(|first| exact.any(|pr| pr.number != first.number));
        if ambiguous {
            row.clear_pull_request();
        } else if let Some(pr) = first {
            row.set_pull_request(SidebarPullRequest::from_github(
                project.id,
                pr,
                &pr.check_summaries(),
            ));
        } else if let Some(branch) = record
            .branch
            .as_deref()
            .and_then(|branch| branch.strip_prefix("refs/heads/"))
            && let Some(pr) = archived
                .iter()
                .find(|pr| pr.head_ref_name == branch && pr.belongs_to_head(head_repository))
        {
            // This fallback was already checked by resolve_branch_head: it is
            // the sole PR for a branch in this same source repository. Its old head
            // may differ from the checkout after later local commits.
            row.set_pull_request(SidebarPullRequest::from_github(
                project.id,
                pr,
                &pr.check_summaries(),
            ));
        } else if matches!(
            row.pull_request_state,
            Some(
                sidebar::WorktreePullRequestState::Closed
                    | sidebar::WorktreePullRequestState::Merged
            )
        ) {
            // A branch-only historical match from the recent page may belong
            // to another incarnation of this branch. Open associations keep
            // working while a local branch is ahead of its remote head.
            row.clear_pull_request();
        }
    }
}

/// Everything `bootstrap` resolves before iced exists, named rather than
/// positional: this used to be a nine-wide tuple mutated through `bootstrap.7`.
pub(crate) struct Bootstrap {
    workspace: WorkspaceState,
    session: Option<SessionRuntime>,
    projects: Vec<ProjectState>,
    worktrees: Vec<WorktreeState>,
    settings: AppSettings,
    settings_path: Option<PathBuf>,
    settings_write_blocked: bool,
    /// The one failure that is fatal under the launch handshake, so it stays
    /// separate from the advisory warnings below.
    status: Option<String>,
    save_health: Option<SaveHealthState>,
    /// Cards that storage held but could not restore. Advisory, never fatal.
    restore_warnings: Vec<String>,
    /// Settings the file holds that this build could not use. Advisory.
    settings_warnings: Vec<String>,
}

/// Loaded before iced starts: session restoration and file locks never wait on
/// the native event loop. Only successfully restored launches can report ready.
pub struct Startup {
    bootstrap: Bootstrap,
    window_view: Option<WindowView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestDialog {
    repository: GitHubRepoKey,
    project_id: ProjectId,
    /// The worktree this draft was opened against, when it was opened from one.
    worktree: Option<PathBuf>,
    base: String,
    head: String,
    title: String,
    body: String,
    draft: bool,
    /// An uncertain remote outcome requires reconciliation before another create.
    retry_confirmation: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ContextTab {
    #[default]
    Files,
    Changes,
    PullRequest,
    Details,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WindowView {
    sidebar_width: f32,
    inspector_width: f32,
    context_inspector_open: bool,
    context_tab: ContextTab,
    sidebar: sidebar::SidebarView,
    worktree_offsets: HashMap<PathBuf, f32>,
}

/// Captured on the event loop; all storage work runs on the blocking pool.
struct SettingsSave {
    generation: u64,
    path: PathBuf,
    base: AppSettings,
    desired: AppSettings,
    view: WindowView,
    store: Option<Arc<crate::persistence::StateStore>>,
    lane: Arc<std::sync::Mutex<Option<SettingsSaved>>>,
    reset: bool,
}

#[derive(Clone, Debug)]
pub struct SettingsSaved {
    generation: u64,
    path: PathBuf,
    settings: AppSettings,
}

impl SettingsSave {
    fn run(self) -> Result<SettingsSaved, String> {
        let mut lane = self
            .lane
            .lock()
            .map_err(|_| "settings save lane unavailable")?;
        if let Some(saved) = lane.as_ref()
            && saved.generation > self.generation
        {
            return Ok(saved.clone());
        }
        if let Some(store) = self.store {
            let value = serde_json::to_string(&self.view).map_err(|error| error.to_string())?;
            store
                .save_window_view(&value)
                .map_err(|error| error.to_string())?;
        }
        if self.reset {
            settings::save(&self.path, &self.desired)
        } else {
            let base = lane.as_ref().map_or(&self.base, |saved| &saved.settings);
            settings::save_merged(&self.path, base, &self.desired)
        }
        .map_err(|error| error.to_string())?;
        let saved = SettingsSaved {
            generation: self.generation,
            path: self.path,
            settings: self.desired,
        };
        *lane = Some(saved.clone());
        Ok(saved)
    }
}

struct WorkspaceSave {
    settings: SettingsSave,
    flush: std::sync::mpsc::Receiver<Result<(), String>>,
}

async fn flush_workspace(
    save: Result<WorkspaceSave, String>,
) -> (Result<SettingsSaved, String>, Result<(), String>) {
    let outcome = tokio::task::spawn_blocking(move || {
        let save = match save {
            Ok(save) => save,
            Err(error) => return (Err(error.clone()), Err(error)),
        };
        let saved = save.settings.run();
        let result = saved
            .as_ref()
            .map(|_| ())
            .map_err(Clone::clone)
            .and_then(|()| {
                save.flush
                    .recv_timeout(Duration::from_secs(20))
                    .map_err(|error| format!("workspace save did not complete: {error}"))?
            });
        (saved, result)
    })
    .await;
    outcome.unwrap_or_else(|error| {
        let error = error.to_string();
        (Err(error.clone()), Err(error))
    })
}

impl ContextTab {
    const ALL: [Self; 4] = [Self::Files, Self::Changes, Self::PullRequest, Self::Details];

    const fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
            // The panel holds the Changes, Branch, and History views; naming
            // it after the first of them said "Changes" twice.
            Self::Changes => "Source control",
            Self::PullRequest => "Pull request",
            Self::Details => "Details",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    Notifications,
    General,
    Appearance,
    Terminal,
    Editor,
    Links,
    Keyboard,
    Accounts,
}

impl SettingsCategory {
    const ALL: [Self; 8] = [
        Self::General,
        Self::Notifications,
        Self::Appearance,
        Self::Editor,
        Self::Terminal,
        Self::Links,
        Self::Keyboard,
        Self::Accounts,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Notifications => "Notifications",
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Editor => "Editor",
            Self::Links => "Browser & links",
            Self::Keyboard => "Keyboard",
            Self::Accounts => "Accounts",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InspectorTransition {
    started_at: Instant,
    from: f32,
    to: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextOverlayChange {
    Entered,
    Exited,
    Unchanged,
}

const fn context_overlay_change(was_overlay: bool, is_overlay: bool) -> ContextOverlayChange {
    match (was_overlay, is_overlay) {
        (false, true) => ContextOverlayChange::Entered,
        (true, false) => ContextOverlayChange::Exited,
        _ => ContextOverlayChange::Unchanged,
    }
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

fn is_save_press(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(key),
            modifiers,
            repeat: false,
            ..
        } if key.eq_ignore_ascii_case("s")
            && modifiers.control()
            && !modifiers.alt()
            && !modifiers.shift()
            && !modifiers.logo()
    )
}

fn is_tab_press(event: &keyboard::Event) -> Option<bool> {
    match event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modifiers,
            repeat: false,
            ..
        } if !modifiers.control() && !modifiers.alt() && !modifiers.logo() => {
            Some(modifiers.shift())
        }
        _ => None,
    }
}

fn repository_action_allowed_while_modal(action: &RepositoryAction) -> bool {
    matches!(
        action,
        RepositoryAction::FilesLoaded { .. }
            | RepositoryAction::FileLoaded { .. }
            | RepositoryAction::DiffLoaded { .. }
            | RepositoryAction::DiffBasisResolved { .. }
            | RepositoryAction::SaveFinished { .. }
            // A dropped page render leaves the viewer reading "Rendering
            // page" for a page it will never draw, because the fence that
            // accepts the result is the only thing that clears it.
            | RepositoryAction::PdfPageRendered { .. }
            | RepositoryAction::WorkingDiffDue { .. }
            | RepositoryAction::WorkingDiffLoaded { .. }
            | RepositoryAction::AutoSaveDue { .. }
            // Refusing these strands the card's file list and markdown preview
            // behind their own debounce until the next self-heal.
            | RepositoryAction::FileListUnchanged { .. }
            | RepositoryAction::MarkdownDue { .. }
    )
}

/// The discard confirmation's own two answers.
///
/// The dialog is mounted by [`Cagents::dialog`] rather than by
/// `sidebar::view`, so it is the one surface whose commands must survive the
/// modal guard - refusing them would strand the dialog with no way out. It
/// is exactly two variants, and neither can destroy anything the dialog did
/// not already count and show.
fn answers_discard_confirmation(action: &SidebarAction) -> bool {
    matches!(
        action,
        SidebarAction::ConfirmDiscard { .. } | SidebarAction::CancelDiscard
    )
}

/// What one worktree mutation is reported as, or `None` when it did all of
/// what it was asked.
///
/// A partial application is a failure with a known good half, and an unknown
/// outcome is not a success: both are said out loud, because the alternative
/// is a panel that refreshes and silently shows fewer paths staged than the
/// user selected.
fn mutation_report(
    label: &str,
    outcome: Result<MutationOutcome, crate::repository::RepositoryError>,
) -> Option<String> {
    match outcome {
        Ok(MutationOutcome::Applied { .. }) => None,
        Ok(MutationOutcome::PartiallyApplied { paths, failures }) => Some(format!(
            "{label} applied to {} path(s); {}",
            paths.len(),
            join_failures(&failures)
        )),
        Ok(MutationOutcome::NotApplied { failure }) => Some(format!("{label} failed: {failure}")),
        Ok(MutationOutcome::OutcomeUnknown { failure }) => Some(format!(
            "{label} may or may not have run: {failure}. Refresh to see what Git holds."
        )),
        Err(error) => Some(format!("{label} could not run: {error}")),
    }
}

/// The commit report, and whether a commit was actually created. Only a
/// created commit may clear the message input.
fn commit_report(
    outcome: Result<CommitOutcome, crate::repository::RepositoryError>,
) -> (Option<String>, bool) {
    match outcome {
        Ok(CommitOutcome::Created { warnings, .. }) if warnings.is_empty() => (None, true),
        Ok(CommitOutcome::Created { warnings, .. }) => (
            Some(format!("Commit created, but {}", join_failures(&warnings))),
            true,
        ),
        Ok(CommitOutcome::NotCreated { failure }) => {
            (Some(format!("Commit failed: {failure}")), false)
        }
        Ok(CommitOutcome::OutcomeUnknown { failure }) => (
            Some(format!(
                "Commit may or may not have been created: {failure}. Refresh to see what Git holds."
            )),
            false,
        ),
        Err(error) => (Some(format!("Commit could not run: {error}")), false),
    }
}

fn push_report(outcome: Result<PushOutcome, crate::repository::RepositoryError>) -> Option<String> {
    match outcome {
        Ok(PushOutcome::Pushed { .. }) => None,
        Ok(PushOutcome::NotPushed { failure }) => Some(format!("Push failed: {failure}")),
        Ok(PushOutcome::OutcomeUnknown { failure }) => Some(format!(
            "Push may or may not have reached the remote: {failure}. Refresh to see what Git holds."
        )),
        Err(error) => Some(format!("Push could not run: {error}")),
    }
}

fn join_failures(failures: &[crate::repository::MutationFailure]) -> String {
    failures
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn user_workspace_message_blocked_while_modal(message: &Message) -> bool {
    match message {
        Message::NewTerminal
        | Message::OpenRepository
        | Message::FocusCard(_)
        | Message::TerminalDropped { .. }
        | Message::ResizeCard { .. }
        | Message::ResizeCardFinished(_)
        | Message::CyclePaneWidth
        | Message::CloseCard(_)
        | Message::OpenBrowser(_)
        | Message::OpenLink(..)
        | Message::OpenDocumentLink(..)
        | Message::PreviewLocalFile(_)
        | Message::OpenTerminalOutput(_)
        | Message::NewBrowser
        | Message::RetryCard(_)
        | Message::FocusAdjacent(_)
        | Message::FocusProjectSearch
        | Message::ToggleContextInspector
        | Message::SelectContextTab(_)
        | Message::ContextInspector(_)
        // The settings pane sits under a dialog like any other card, so a
        // keystroke aimed at the dialog must not edit a setting behind it.
        | Message::SelectSettingsCategory(_)
        | Message::SettingsSearchChanged(_)
        | Message::SettingsChanged { .. }
        | Message::SettingsDraftChanged { .. }
        | Message::ResetSettings
        | Message::DismissStatus => true,
        Message::Repository(action) => !repository_action_allowed_while_modal(action),
        Message::ChatUi(action) => !action.allowed_while_modal(),
        Message::ChromeUi(action) => !matches!(action, chrome_ui::Action::Close),
        Message::ChannelUi(action) => !action.answers_own_dialog(),
        Message::SidebarSelection(_) => false,
        Message::AccountUi(action) => action.blocked_while_modal(),
        Message::AutomationUi(action) => !action.background(),
        Message::UpdateUi(update_ui::Action::Apply) => true,
        Message::PullRequestUi(action) => !action.allowed_while_modal(),
        Message::PullRequestReview { action, .. } => !action.allowed_while_modal(),
        Message::Sidebar(action) => !answers_discard_confirmation(action),
        #[cfg(feature = "integrated-browser")]
        Message::BrowserBack(_)
        | Message::BrowserForward(_)
        | Message::BrowserReload(_)
        | Message::BrowserAddressChanged { .. }
        | Message::BrowserNavigate(_) => true,
        _ => false,
    }
}

fn user_message_blocked_during_update(message: &Message) -> bool {
    user_workspace_message_blocked_while_modal(message)
        || matches!(
            message,
            Message::KeyEvent { .. }
                | Message::Viewport { .. }
                | Message::NewWindow
                | Message::WindowCloseRequested(_)
                | Message::CommitClose(_)
                | Message::ConfirmTerminalClose
                | Message::CancelTerminalClose
                | Message::TerminalCloseDontAskChanged(_)
                | Message::SaveAndCloseRepository(_)
                | Message::OverwriteAndCloseRepository(_)
                | Message::DiscardAndCloseRepository(_)
                | Message::CancelRepositoryClose
                | Message::WorktreeBranchChanged(_)
                | Message::WorktreeBaseChanged(_)
                | Message::WorktreeDestinationChanged(_)
                | Message::ToggleWorktreeAdvanced
                | Message::SubmitWorktree
                | Message::PullRequestBaseChanged(_)
                | Message::PullRequestHeadChanged(_)
                | Message::PullRequestTitleChanged(_)
                | Message::PullRequestBodyChanged(_)
                | Message::PullRequestDraftChanged(_)
                | Message::PullRequestRetryConfirmed(_)
                | Message::SubmitPullRequest
                | Message::CancelDialog
                | Message::SelectSettingsCategory(_)
                | Message::SettingsSearchChanged(_)
                | Message::SettingsChanged { .. }
                | Message::SettingsDraftChanged { .. }
                | Message::ResetSettings
                | Message::ConfirmResetSettings
                | Message::CancelResetSettings
                | Message::NativeWindowAction(_)
                | Message::ProjectUi(
                    project_ui::Action::Command { .. }
                        | project_ui::Action::ConfirmDelete
                        | project_ui::Action::Dismiss
                )
                | Message::ChannelUi(
                    channel_ui::Action::Delete(_)
                        | channel_ui::Action::ConfirmDelete
                        | channel_ui::Action::Dismiss
                )
                | Message::SidebarSelection(_)
        )
}

fn focused_save_in_flight(
    focused: Option<CardId>,
    mut is_saving: impl FnMut(CardId) -> bool,
) -> bool {
    focused.is_some_and(&mut is_saving)
}

fn resolve_restore_card(
    preferred: Option<CardId>,
    current: Option<CardId>,
    mut exists: impl FnMut(CardId) -> bool,
) -> Option<CardId> {
    preferred.filter(|card| exists(*card)).or(current)
}

fn workspace_navigation_blocked_during_save(message: &Message) -> bool {
    // The channel deletion dialog is the one channel command that navigates
    // nowhere, so a save in flight must not strand it with no way out.
    if let Message::ChannelUi(action) = message {
        return !action.answers_own_dialog();
    }
    matches!(
        message,
        Message::NewTerminal
            | Message::NewBrowser
            | Message::OpenBrowser(_)
            | Message::OpenLink(..)
            | Message::OpenDocumentLink(..)
            | Message::PreviewLocalFile(_)
            | Message::ChatUi(chat_ui::Action::OpenUrl(_))
            | Message::PullRequestUi(
                pull_request_ui::Action::OpenUrl(_) | pull_request_ui::Action::OpenMarkdownLink(_)
            )
            | Message::ChromeUi(chrome_ui::Action::Launch(_))
            | Message::FocusCard(_)
            | Message::FocusAdjacent(_)
            | Message::Viewport {
                event: WorkspaceViewportEvent::PointerFocused(_)
                    | WorkspaceViewportEvent::PointerEntered(_),
                ..
            }
            | Message::ToggleContextInspector
            | Message::Repository(RepositoryAction::NewTerminal { .. })
            | Message::Repository(RepositoryAction::MarkdownLink { .. })
            | Message::Sidebar(
                SidebarAction::OpenRepositoryPicker
                    | SidebarAction::OpenRepository { .. }
                    | SidebarAction::OpenWorktree { .. }
                    | SidebarAction::OpenPullRequest { .. }
                    | SidebarAction::BrowsePullRequests { .. }
                    | SidebarAction::NewWorktree { .. }
                    | SidebarAction::NewChannel { .. }
                    | SidebarAction::OpenChannel { .. }
                    | SidebarAction::CreatePullRequest { .. }
                    | SidebarAction::OpenSettings
                    | SidebarAction::OpenAutomations
            )
    ) || matches!(
        message,
        Message::ContextInspector(action) if context_inspector_action_navigates(action)
    )
}

fn context_inspector_action_navigates(action: &InspectorAction) -> bool {
    matches!(
        action,
        InspectorAction::NewTerminalAt(_)
            | InspectorAction::Retry(_)
            | InspectorAction::ReloadBrowser(_)
            | InspectorAction::OpenBrowser(_)
            | InspectorAction::OpenLspHoverLink(_)
            | InspectorAction::CreatePullRequest { .. }
            | InspectorAction::FocusProjectSearch
    )
}

fn completion_navigates(message: &Message) -> bool {
    matches!(
        message,
        Message::RepositoryLoaded {
            open_cards: true,
            result: Ok(_),
            ..
        } | Message::WorktreeCreated {
            result: Ok(WorktreeCreationOutcome::Created { .. }),
            ..
        } | Message::PullRequestCreated {
            result: Ok(PullRequestCreationOutcome::Created { .. }),
            ..
        }
    )
}

fn next_dirty_close_control(
    current: DirtyCloseControl,
    backwards: bool,
    saving: bool,
    discardable: bool,
) -> DirtyCloseControl {
    if saving {
        return DirtyCloseControl::Cancel;
    }
    if !discardable {
        return match current {
            DirtyCloseControl::Cancel => DirtyCloseControl::Save,
            DirtyCloseControl::Save => DirtyCloseControl::Cancel,
            DirtyCloseControl::Discard if backwards => DirtyCloseControl::Cancel,
            DirtyCloseControl::Discard => DirtyCloseControl::Save,
        };
    }
    match (current, backwards) {
        (DirtyCloseControl::Cancel, false) | (DirtyCloseControl::Save, true) => {
            DirtyCloseControl::Discard
        }
        (DirtyCloseControl::Discard, false) | (DirtyCloseControl::Cancel, true) => {
            DirtyCloseControl::Save
        }
        (DirtyCloseControl::Save, false) | (DirtyCloseControl::Discard, true) => {
            DirtyCloseControl::Cancel
        }
    }
}

fn repository_task(task: iced::Task<RepositoryAction>) -> Task<Message> {
    task.map(Message::Repository).map(cosmic::Action::App)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsField {
    Notifications(crate::notifications::Field),
    AppTheme,
    UiScale,
    TerminalLineHeight,
    TerminalPadding,
    TerminalScrollSize,
    TerminalScrollSpeed,
    TerminalCursorStyle,
    TerminalTheme,
    MaxHotCards,
    HibernationDelayMs,
    TerminalFontSize,
    TerminalFontFamily,
    TerminalScrollbackLines,
    ManagedWorktreeDirectory,
    WorktreeSort,
    DefaultWorktreePane,
    DefaultChannelPane,
    DefaultColumnFraction,
    LinkOpenDestination,
    EditorAutoSave,
    EditorWrapLines,
    ChatEnterSends,
    ShowStatusBar,
    RelaunchAgentsOnUpgrade,
    EditorSyntaxTheme,
    ConfirmTerminalClose,
    FocusFollowsMouse,
    LinkOpenGesture,
    PullRequestOpenTarget,
    ShortcutNewTerminal,
    ShortcutNewChannel,
    ShortcutClonePane,
    ShortcutCloseFocusedColumn,
    ShortcutSearchProjects,
    ShortcutPreviousColumn,
    ShortcutNextColumn,
    ShortcutCyclePaneWidth,
    ShortcutPreviousWorkspace,
    ShortcutNextWorkspace,
    ShortcutReviewToggleViewed,
    ShortcutReviewNextFile,
    ShortcutReviewPreviousFile,
    ShortcutReviewNextUnviewed,
    ShortcutReviewNextChange,
    ShortcutReviewPreviousChange,
}

impl SettingsField {
    /// The shortcut table in `KeyboardShortcuts::actions` order.
    const SHORTCUTS: [Self; 16] = [
        Self::ShortcutNewTerminal,
        Self::ShortcutNewChannel,
        Self::ShortcutClonePane,
        Self::ShortcutCloseFocusedColumn,
        Self::ShortcutSearchProjects,
        Self::ShortcutPreviousColumn,
        Self::ShortcutNextColumn,
        Self::ShortcutCyclePaneWidth,
        Self::ShortcutPreviousWorkspace,
        Self::ShortcutNextWorkspace,
        Self::ShortcutReviewToggleViewed,
        Self::ShortcutReviewNextFile,
        Self::ShortcutReviewPreviousFile,
        Self::ShortcutReviewNextUnviewed,
        Self::ShortcutReviewNextChange,
        Self::ShortcutReviewPreviousChange,
    ];

    /// The label the settings pane shows for this setting, which is also
    /// the name its messages use for it.
    const fn label(self) -> &'static str {
        use crate::notifications::Field;
        match self {
            Self::Notifications(Field::Enabled) => "System notifications",
            Self::Notifications(Field::Success) => "Successful completion",
            Self::Notifications(Field::Failure) => "Failure or cancellation",
            Self::Notifications(Field::Waiting) => "Waiting for you",
            Self::Notifications(Field::ReviewRequests) => "Review requests",
            Self::Notifications(Field::WhileFocused) => "Notify while focused",
            Self::AppTheme => "App theme",
            Self::UiScale => "Interface scale",
            Self::TerminalLineHeight => "Line height",
            Self::TerminalPadding => "Inner padding",
            Self::TerminalScrollSize => "Scroll size",
            Self::TerminalScrollSpeed => "Scroll speed",
            Self::TerminalCursorStyle => "Cursor shape",
            Self::TerminalTheme => "Color theme",
            Self::MaxHotCards => "Maximum hot columns",
            Self::HibernationDelayMs => "Hibernation delay (ms)",
            Self::TerminalFontSize => "Font size",
            Self::TerminalFontFamily => "Font family",
            Self::TerminalScrollbackLines => "Scrollback lines",
            Self::ManagedWorktreeDirectory => "Managed worktree directory",
            Self::WorktreeSort => "Worktree order",
            Self::DefaultWorktreePane => "Default worktree pane",
            Self::DefaultChannelPane => "Default channel pane",
            Self::DefaultColumnFraction => "New column width",
            Self::LinkOpenDestination => "Open links in",
            Self::EditorAutoSave => "Auto-save",
            Self::EditorWrapLines => "Word wrap",
            Self::ChatEnterSends => "Chat send key",
            Self::ShowStatusBar => "Status bar",
            Self::RelaunchAgentsOnUpgrade => "Unresumable agents",
            Self::EditorSyntaxTheme => "Syntax theme",
            Self::ConfirmTerminalClose => "Active processes",
            Self::FocusFollowsMouse => "Pane activation",
            Self::LinkOpenGesture => "System browser gesture",
            Self::PullRequestOpenTarget => "Open pull requests in",
            Self::ShortcutNewTerminal => "New terminal",
            Self::ShortcutNewChannel => "New channel",
            Self::ShortcutClonePane => "Clone focused pane",
            Self::ShortcutCloseFocusedColumn => "Close focused column",
            Self::ShortcutSearchProjects => "Search projects",
            Self::ShortcutPreviousColumn => "Previous column",
            Self::ShortcutNextColumn => "Next column",
            Self::ShortcutCyclePaneWidth => "Cycle pane width",
            Self::ShortcutPreviousWorkspace => "Previous workspace",
            Self::ShortcutNextWorkspace => "Next workspace",
            Self::ShortcutReviewToggleViewed => "Mark reviewed file viewed",
            Self::ShortcutReviewNextFile => "Next reviewed file",
            Self::ShortcutReviewPreviousFile => "Previous reviewed file",
            Self::ShortcutReviewNextUnviewed => "Next unviewed file",
            Self::ShortcutReviewNextChange => "Next changed hunk",
            Self::ShortcutReviewPreviousChange => "Previous changed hunk",
        }
    }

    /// The category whose page holds this setting.
    const fn category(self) -> SettingsCategory {
        match self {
            Self::Notifications(_) => SettingsCategory::Notifications,
            Self::AppTheme | Self::UiScale | Self::ShowStatusBar | Self::DefaultColumnFraction => {
                SettingsCategory::Appearance
            }
            Self::TerminalLineHeight
            | Self::TerminalPadding
            | Self::TerminalScrollSize
            | Self::TerminalScrollSpeed
            | Self::TerminalCursorStyle
            | Self::TerminalTheme
            | Self::TerminalFontSize
            | Self::TerminalFontFamily
            | Self::TerminalScrollbackLines
            | Self::ConfirmTerminalClose
            | Self::RelaunchAgentsOnUpgrade => SettingsCategory::Terminal,
            Self::MaxHotCards
            | Self::HibernationDelayMs
            | Self::ManagedWorktreeDirectory
            | Self::WorktreeSort
            | Self::DefaultWorktreePane
            | Self::DefaultChannelPane
            | Self::ChatEnterSends
            | Self::FocusFollowsMouse => SettingsCategory::General,
            Self::EditorAutoSave | Self::EditorWrapLines | Self::EditorSyntaxTheme => {
                SettingsCategory::Editor
            }
            Self::LinkOpenDestination | Self::LinkOpenGesture | Self::PullRequestOpenTarget => {
                SettingsCategory::Links
            }
            Self::ShortcutNewTerminal
            | Self::ShortcutNewChannel
            | Self::ShortcutClonePane
            | Self::ShortcutCloseFocusedColumn
            | Self::ShortcutSearchProjects
            | Self::ShortcutPreviousColumn
            | Self::ShortcutNextColumn
            | Self::ShortcutCyclePaneWidth
            | Self::ShortcutPreviousWorkspace
            | Self::ShortcutNextWorkspace
            | Self::ShortcutReviewToggleViewed
            | Self::ShortcutReviewNextFile
            | Self::ShortcutReviewPreviousFile
            | Self::ShortcutReviewNextUnviewed
            | Self::ShortcutReviewNextChange
            | Self::ShortcutReviewPreviousChange => SettingsCategory::Keyboard,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SettingsFeedback {
    Saved,
    Pending,
    /// At least one entered value was refused; each is reported under its
    /// own field, from `settings_invalid`.
    Invalid,
    Error(String),
}

/// The shortcut rows exactly as typed.
///
/// A chord list edits as one comma-separated line, and reformatting it from the
/// parsed list on every keystroke would delete a separator the moment the user
/// typed it. The parsed form lives in `AppSettings`; this is only the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShortcutDraft {
    new_terminal: String,
    new_channel: String,
    clone_pane: String,
    close_focused_column: String,
    search_projects: String,
    previous_column: String,
    next_column: String,
    cycle_pane_width: String,
    previous_workspace: String,
    next_workspace: String,
    review_toggle_viewed: String,
    review_next_file: String,
    review_previous_file: String,
    review_next_unviewed: String,
    review_next_change: String,
    review_previous_change: String,
}

impl ShortcutDraft {
    fn from_shortcuts(shortcuts: &KeyboardShortcuts) -> Self {
        Self {
            new_terminal: settings::format_chords(&shortcuts.new_terminal),
            new_channel: settings::format_chords(&shortcuts.new_channel),
            clone_pane: settings::format_chords(&shortcuts.clone_pane),
            close_focused_column: settings::format_chords(&shortcuts.close_focused_column),
            search_projects: settings::format_chords(&shortcuts.search_projects),
            previous_column: settings::format_chords(&shortcuts.previous_column),
            next_column: settings::format_chords(&shortcuts.next_column),
            cycle_pane_width: settings::format_chords(&shortcuts.cycle_pane_width),
            previous_workspace: settings::format_chords(&shortcuts.previous_workspace),
            next_workspace: settings::format_chords(&shortcuts.next_workspace),
            review_toggle_viewed: settings::format_chords(&shortcuts.review_toggle_viewed),
            review_next_file: settings::format_chords(&shortcuts.review_next_file),
            review_previous_file: settings::format_chords(&shortcuts.review_previous_file),
            review_next_unviewed: settings::format_chords(&shortcuts.review_next_unviewed),
            review_next_change: settings::format_chords(&shortcuts.review_next_change),
            review_previous_change: settings::format_chords(&shortcuts.review_previous_change),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsDraft {
    notifications: crate::notifications::Preferences,
    app_theme: String,
    ui_scale: String,
    terminal_line_height: String,
    terminal_padding: String,
    terminal_scroll_size: String,
    terminal_scroll_speed: String,
    terminal_cursor_style: String,
    terminal_theme: String,
    has_custom_terminal_theme: bool,
    max_hot_cards: String,
    hibernation_delay_ms: String,
    terminal_font_size: String,
    terminal_font_family: String,
    terminal_scrollback_lines: String,
    managed_worktree_directory: String,
    worktree_sort: String,
    default_worktree_pane: settings::DefaultPane,
    default_channel_pane: settings::DefaultPane,
    default_column_fraction: String,
    editor_auto_save: bool,
    editor_wrap_lines: bool,
    editor_syntax_theme: settings::EditorSyntaxTheme,
    confirm_terminal_close: bool,
    chat_enter_sends: bool,
    show_status_bar: bool,
    relaunch_agents_on_upgrade: bool,
    focus_follows_mouse: bool,
    link_open_destination: LinkOpenDestination,
    link_open_gesture: LinkOpenGesture,
    pull_request_open_target: PullRequestOpenTarget,
    keyboard_shortcuts: ShortcutDraft,
}

impl SettingsDraft {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            notifications: settings.notifications,
            app_theme: settings.app_theme.key().into(),
            ui_scale: settings.ui_scale.to_string(),
            terminal_line_height: settings.terminal_line_height.to_string(),
            terminal_padding: settings.terminal_padding.to_string(),
            terminal_scroll_size: settings.terminal_scroll_size.to_string(),
            terminal_scroll_speed: settings.terminal_scroll_speed.to_string(),
            terminal_cursor_style: match settings.terminal_cursor_style {
                TerminalCursorStyle::Application => "application",
                TerminalCursorStyle::Block => "block",
                TerminalCursorStyle::Beam => "beam",
                TerminalCursorStyle::Underline => "underline",
            }
            .into(),
            terminal_theme: settings.terminal_theme.id().into(),
            has_custom_terminal_theme: settings.custom_terminal_theme.is_some(),
            max_hot_cards: settings.max_hot_cards.to_string(),
            hibernation_delay_ms: settings.hibernation_delay_ms.to_string(),
            terminal_font_size: settings.terminal_font_size.to_string(),
            terminal_font_family: settings.terminal_font_family.clone(),
            terminal_scrollback_lines: settings.terminal_scrollback_lines.to_string(),
            managed_worktree_directory: settings.managed_worktree_directory.clone(),
            worktree_sort: match settings.worktree_sort {
                settings::WorktreeSort::Newest => "newest",
                settings::WorktreeSort::Oldest => "oldest",
                settings::WorktreeSort::Alphabetical => "alphabetical",
            }
            .into(),
            default_column_fraction: settings.default_column_fraction.to_string(),
            default_worktree_pane: settings.default_worktree_pane,
            default_channel_pane: settings.default_channel_pane,
            editor_auto_save: settings.editor_auto_save,
            editor_wrap_lines: settings.editor_wrap_lines,
            confirm_terminal_close: settings.confirm_terminal_close,
            chat_enter_sends: settings.chat_enter_sends,
            show_status_bar: settings.show_status_bar,
            relaunch_agents_on_upgrade: settings.relaunch_agents_on_upgrade,
            editor_syntax_theme: settings.editor_syntax_theme,
            focus_follows_mouse: settings.focus_follows_mouse,
            link_open_destination: settings.link_open_destination,
            link_open_gesture: settings.link_open_gesture,
            pull_request_open_target: settings.pull_request_open_target,
            keyboard_shortcuts: ShortcutDraft::from_shortcuts(&settings.keyboard_shortcuts),
        }
    }

    fn set(&mut self, field: SettingsField, value: String) {
        match field {
            SettingsField::Notifications(field) => self.notifications.set(field, value == "true"),
            SettingsField::AppTheme => self.app_theme = value,
            SettingsField::UiScale => self.ui_scale = value,
            SettingsField::TerminalLineHeight => self.terminal_line_height = value,
            SettingsField::TerminalPadding => self.terminal_padding = value,
            SettingsField::TerminalScrollSize => self.terminal_scroll_size = value,
            SettingsField::TerminalScrollSpeed => self.terminal_scroll_speed = value,
            SettingsField::TerminalTheme => self.terminal_theme = value,
            SettingsField::TerminalCursorStyle => self.terminal_cursor_style = value,
            SettingsField::MaxHotCards => self.max_hot_cards = value,
            SettingsField::HibernationDelayMs => self.hibernation_delay_ms = value,
            SettingsField::TerminalFontSize => self.terminal_font_size = value,
            SettingsField::TerminalFontFamily => self.terminal_font_family = value,
            SettingsField::TerminalScrollbackLines => self.terminal_scrollback_lines = value,
            SettingsField::ManagedWorktreeDirectory => self.managed_worktree_directory = value,
            SettingsField::WorktreeSort => self.worktree_sort = value,
            SettingsField::DefaultWorktreePane => {
                if let Some(pane) = settings::DefaultPane::from_key(&value) {
                    self.default_worktree_pane = pane;
                }
            }
            SettingsField::DefaultChannelPane => {
                if let Some(pane) = settings::DefaultPane::from_key(&value) {
                    self.default_channel_pane = pane;
                }
            }
            SettingsField::DefaultColumnFraction => self.default_column_fraction = value,
            SettingsField::EditorAutoSave => self.editor_auto_save = value == "true",
            SettingsField::EditorWrapLines => self.editor_wrap_lines = value == "true",
            SettingsField::ConfirmTerminalClose => self.confirm_terminal_close = value == "true",
            SettingsField::ChatEnterSends => self.chat_enter_sends = value == "true",
            SettingsField::ShowStatusBar => self.show_status_bar = value == "true",
            SettingsField::RelaunchAgentsOnUpgrade => {
                self.relaunch_agents_on_upgrade = value == "true"
            }
            SettingsField::EditorSyntaxTheme => {
                if let Some(theme) = settings::EditorSyntaxTheme::from_key(&value) {
                    self.editor_syntax_theme = theme;
                }
            }
            SettingsField::FocusFollowsMouse => self.focus_follows_mouse = value == "true",
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
            SettingsField::ShortcutNewChannel => self.keyboard_shortcuts.new_channel = value,
            SettingsField::ShortcutClonePane => self.keyboard_shortcuts.clone_pane = value,
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
            SettingsField::ShortcutCyclePaneWidth => {
                self.keyboard_shortcuts.cycle_pane_width = value;
            }
            SettingsField::ShortcutPreviousWorkspace => {
                self.keyboard_shortcuts.previous_workspace = value;
            }
            SettingsField::ShortcutNextWorkspace => {
                self.keyboard_shortcuts.next_workspace = value;
            }
            SettingsField::PullRequestOpenTarget => {
                self.pull_request_open_target = if value == PullRequestOpenTarget::Link.as_str() {
                    PullRequestOpenTarget::Link
                } else {
                    PullRequestOpenTarget::Review
                };
            }
            SettingsField::ShortcutReviewToggleViewed => {
                self.keyboard_shortcuts.review_toggle_viewed = value;
            }
            SettingsField::ShortcutReviewNextFile => {
                self.keyboard_shortcuts.review_next_file = value;
            }
            SettingsField::ShortcutReviewPreviousFile => {
                self.keyboard_shortcuts.review_previous_file = value;
            }
            SettingsField::ShortcutReviewNextUnviewed => {
                self.keyboard_shortcuts.review_next_unviewed = value;
            }
            SettingsField::ShortcutReviewNextChange => {
                self.keyboard_shortcuts.review_next_change = value;
            }
            SettingsField::ShortcutReviewPreviousChange => {
                self.keyboard_shortcuts.review_previous_change = value;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SettingsDecision {
    Invalid(String),
    Unchanged,
    Persist(AppSettings),
}

fn settings_update(
    current: &AppSettings,
    field: SettingsField,
    raw_value: &str,
) -> SettingsDecision {
    let value = raw_value.trim();
    let mut next = current.clone();
    let parsed = match field {
        SettingsField::Notifications(field) => value
            .parse::<bool>()
            .map(|enabled| next.notifications.set(field, enabled))
            .map_err(|_| "Choose whether this notification is enabled.".to_owned()),
        SettingsField::FocusFollowsMouse => value
            .parse::<bool>()
            .map(|enabled| next.focus_follows_mouse = enabled)
            .map_err(|_| "Choose whether hovering activates panes.".to_owned()),
        SettingsField::ConfirmTerminalClose => value
            .parse::<bool>()
            .map(|enabled| next.confirm_terminal_close = enabled)
            .map_err(|_| "Choose whether active terminal closes require confirmation.".to_owned()),
        SettingsField::ChatEnterSends => value
            .parse::<bool>()
            .map(|enabled| next.chat_enter_sends = enabled)
            .map_err(|_| "Choose which key sends a chat message.".to_owned()),
        SettingsField::ShowStatusBar => value
            .parse::<bool>()
            .map(|enabled| next.show_status_bar = enabled)
            .map_err(|_| "Choose whether the status bar is shown.".to_owned()),
        SettingsField::RelaunchAgentsOnUpgrade => value
            .parse::<bool>()
            .map(|enabled| next.relaunch_agents_on_upgrade = enabled)
            .map_err(|_| "Choose whether upgrades relaunch unresumable agents.".to_owned()),
        SettingsField::WorktreeSort => match value {
            "newest" => {
                next.worktree_sort = settings::WorktreeSort::Newest;
                Ok(())
            }
            "oldest" => {
                next.worktree_sort = settings::WorktreeSort::Oldest;
                Ok(())
            }
            "alphabetical" => {
                next.worktree_sort = settings::WorktreeSort::Alphabetical;
                Ok(())
            }
            _ => Err("Choose Newest, Oldest, or Alphabetical.".into()),
        },
        SettingsField::DefaultWorktreePane => settings::DefaultPane::from_key(value)
            .map(|pane| next.default_worktree_pane = pane)
            .ok_or_else(|| "Choose Native chat or Terminal.".to_owned()),
        SettingsField::DefaultChannelPane => settings::DefaultPane::from_key(value)
            .map(|pane| next.default_channel_pane = pane)
            .ok_or_else(|| "Choose Native chat or Terminal.".to_owned()),
        SettingsField::EditorAutoSave => value
            .parse::<bool>()
            .map(|enabled| next.editor_auto_save = enabled)
            .map_err(|_| "Choose whether editor auto-save is enabled.".to_owned()),
        SettingsField::EditorSyntaxTheme => settings::EditorSyntaxTheme::from_key(value)
            .map(|theme| next.editor_syntax_theme = theme)
            .ok_or_else(|| "Choose one of the listed syntax themes.".to_owned()),
        SettingsField::EditorWrapLines => value
            .parse::<bool>()
            .map(|enabled| next.editor_wrap_lines = enabled)
            .map_err(|_| "Choose whether long editor lines wrap.".to_owned()),
        SettingsField::AppTheme => AppTheme::from_key(value)
            .map(|theme| next.app_theme = theme)
            .ok_or_else(|| "Choose one of the listed app themes.".to_owned()),
        SettingsField::TerminalCursorStyle => match value {
            "application" => {
                next.terminal_cursor_style = TerminalCursorStyle::Application;
                Ok(())
            }
            "block" => {
                next.terminal_cursor_style = TerminalCursorStyle::Block;
                Ok(())
            }
            "beam" => {
                next.terminal_cursor_style = TerminalCursorStyle::Beam;
                Ok(())
            }
            "underline" => {
                next.terminal_cursor_style = TerminalCursorStyle::Underline;
                Ok(())
            }
            _ => Err("Choose an application, block, beam, or underline cursor.".into()),
        },
        SettingsField::TerminalTheme => {
            let theme = if value == "custom" {
                next.custom_terminal_theme
                    .map(crate::terminal::TerminalTheme::Custom)
            } else {
                crate::terminal::TerminalTheme::PRESETS
                    .iter()
                    .find(|(_, _, id)| *id == value)
                    .map(|(theme, _, _)| *theme)
            };
            match theme {
                Some(theme) => {
                    next.terminal_theme = theme;
                    Ok(())
                }
                None => Err(
                    "Choose a preset or configure custom_terminal_theme in settings.toml.".into(),
                ),
            }
        }
        SettingsField::UiScale => value
            .parse::<f32>()
            .map(|value| next.ui_scale = value)
            .map_err(|_| "Interface scale must be a number.".to_owned()),
        SettingsField::TerminalLineHeight => value
            .parse::<f32>()
            .map(|value| next.terminal_line_height = value)
            .map_err(|_| "Line height must be a number.".to_owned()),
        SettingsField::TerminalPadding => value
            .parse::<f32>()
            .map(|value| next.terminal_padding = value)
            .map_err(|_| "Inner padding must be a number.".to_owned()),
        SettingsField::TerminalScrollSize => value
            .parse::<usize>()
            .map(|value| next.terminal_scroll_size = value)
            .map_err(|_| "Terminal scroll size must be a whole number.".to_owned()),
        SettingsField::TerminalScrollSpeed => value
            .parse::<f32>()
            .map(|value| next.terminal_scroll_speed = value)
            .map_err(|_| "Scroll speed must be a number.".to_owned()),
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
            .map_err(|_| "Font size must be a number.".to_owned()),
        SettingsField::TerminalFontFamily => {
            next.terminal_font_family = value.to_owned();
            Ok(())
        }
        SettingsField::TerminalScrollbackLines => value
            .parse::<usize>()
            .map(|value| next.terminal_scrollback_lines = value)
            .map_err(|_| "Terminal scrollback must be a whole number.".to_owned()),
        SettingsField::ManagedWorktreeDirectory => {
            next.managed_worktree_directory = value.to_owned();
            Ok(())
        }
        SettingsField::DefaultColumnFraction => value
            .parse::<f32>()
            .map(|value| next.default_column_fraction = value)
            .map_err(|_| "New column width must be a number.".to_owned()),
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
            next.keyboard_shortcuts.new_terminal = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutNewChannel => {
            next.keyboard_shortcuts.new_channel = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutClonePane => {
            next.keyboard_shortcuts.clone_pane = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutCloseFocusedColumn => {
            next.keyboard_shortcuts.close_focused_column = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutSearchProjects => {
            next.keyboard_shortcuts.search_projects = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutPreviousColumn => {
            next.keyboard_shortcuts.previous_column = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutNextColumn => {
            next.keyboard_shortcuts.next_column = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutCyclePaneWidth => {
            next.keyboard_shortcuts.cycle_pane_width = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutPreviousWorkspace => {
            next.keyboard_shortcuts.previous_workspace = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutNextWorkspace => {
            next.keyboard_shortcuts.next_workspace = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::PullRequestOpenTarget => match value {
            "review" => {
                next.pull_request_open_target = PullRequestOpenTarget::Review;
                Ok(())
            }
            "link" => {
                next.pull_request_open_target = PullRequestOpenTarget::Link;
                Ok(())
            }
            _ => Err("Choose how pull requests should open.".to_owned()),
        },
        SettingsField::ShortcutReviewToggleViewed => {
            next.keyboard_shortcuts.review_toggle_viewed = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReviewNextFile => {
            next.keyboard_shortcuts.review_next_file = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReviewPreviousFile => {
            next.keyboard_shortcuts.review_previous_file = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReviewNextUnviewed => {
            next.keyboard_shortcuts.review_next_unviewed = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReviewNextChange => {
            next.keyboard_shortcuts.review_next_change = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReviewPreviousChange => {
            next.keyboard_shortcuts.review_previous_change = settings::parse_chords(value);
            Ok(())
        }
    };
    if let Err(message) = parsed {
        return SettingsDecision::Invalid(refusal(field, &next).unwrap_or(message));
    }
    let attempted = next.clone();
    let next = match next.validate() {
        Ok(next) => next,
        Err(error) => {
            return SettingsDecision::Invalid(
                refusal(field, &attempted).unwrap_or_else(|| error.to_string()),
            );
        }
    };
    if next == *current {
        SettingsDecision::Unchanged
    } else {
        SettingsDecision::Persist(next)
    }
}

/// Plain-language copy for a value the settings pane refused.
///
/// It names the setting as the pane labels it and says what it accepts,
/// rather than quoting the settings file's key, and a shortcut clash names
/// the action that already holds the chord. `None` leaves the parser's own
/// message, which for a field without copy here is already worded for the
/// pane.
fn refusal(field: SettingsField, attempted: &AppSettings) -> Option<String> {
    let label = field.label();
    Some(match field {
        SettingsField::TerminalScrollSize => format!(
            "{label} must be 1–{} lines.",
            settings::MAX_TERMINAL_SCROLL_SIZE
        ),
        SettingsField::TerminalScrollbackLines => format!(
            "Scrollback must be 0–{} lines.",
            grouped_digits(settings::MAX_TERMINAL_SCROLLBACK_LINES)
        ),
        SettingsField::MaxHotCards => format!("{label} must be a whole number, at least 1."),
        SettingsField::HibernationDelayMs => {
            "Hibernation delay must be a whole number of milliseconds.".to_owned()
        }
        SettingsField::TerminalFontFamily
            if attempted.terminal_font_family.len() > settings::MAX_TERMINAL_FONT_FAMILY_BYTES =>
        {
            format!(
                "{label} must be at most {} bytes.",
                settings::MAX_TERMINAL_FONT_FAMILY_BYTES
            )
        }
        SettingsField::TerminalFontFamily => {
            format!("{label} must be a font family name, or empty for the system monospace font.")
        }
        SettingsField::ManagedWorktreeDirectory
            if attempted.managed_worktree_directory.trim().is_empty() =>
        {
            "Enter a directory for managed worktrees.".to_owned()
        }
        SettingsField::ManagedWorktreeDirectory => {
            format!("{label} must be a path without control characters.")
        }
        field => {
            let action = SettingsField::SHORTCUTS
                .iter()
                .position(|shortcut| *shortcut == field)?;
            shortcut_refusal(action, &attempted.keyboard_shortcuts)?
        }
    })
}

/// Why the shortcut table cannot take the edit to `action`.
fn shortcut_refusal(action: usize, shortcuts: &KeyboardShortcuts) -> Option<String> {
    let fault = shortcuts.fault()?;
    let Some(owner) = fault.conflicts_with else {
        let settings::SettingsError::InvalidShortcut(reason) = fault.error else {
            return None;
        };
        return Some(sentence(reason));
    };
    // The table reports the later of two clashing actions, which is not
    // necessarily the one just edited.
    let other = match (fault.action == action, owner == action) {
        (true, _) => owner,
        (false, true) => fault.action,
        (false, false) => return None,
    };
    let actions = shortcuts.actions();
    let bindings = |index: usize| {
        actions[index]
            .iter()
            .filter_map(|chord| Some((chord, settings::parse_shortcut(chord).ok()?)))
            .collect::<Vec<_>>()
    };
    let mine = bindings(action);
    let chord = if other == action {
        // The chord that repeats one before it in the same row.
        mine.iter()
            .enumerate()
            .find(|(index, (_, binding))| {
                mine[..*index].iter().any(|(_, earlier)| earlier == binding)
            })
            .map(|(_, (chord, _))| chord.as_str())
    } else {
        let theirs = bindings(other);
        mine.iter()
            .find(|(_, binding)| theirs.iter().any(|(_, held)| held == binding))
            .map(|(chord, _)| chord.as_str())
    };
    let chord = chord.map_or_else(|| "This shortcut".to_owned(), str::to_owned);
    Some(if other == action {
        format!("{chord} is listed twice.")
    } else {
        format!(
            "{chord} is already the shortcut for “{}”.",
            SettingsField::SHORTCUTS[other].label()
        )
    })
}

/// `message` with a capital first letter and a closing full stop.
fn sentence(message: &str) -> String {
    let mut characters = message.chars();
    let mut sentence: String = characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default();
    if !sentence.ends_with('.') {
        sentence.push('.');
    }
    sentence
}

/// `value` with its thousands separated by commas.
fn grouped_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A worktree snapshot for the source-control panel.
    ///
    /// `key` is the binding the request was made against. The panel rebinds
    /// whenever the focused card names a different worktree, so a result whose
    /// key is no longer the bound one is a read of a tree nobody is looking at
    /// and is dropped rather than rendered.
    SourceControlLoaded {
        key: ActiveWorktreeKey,
        request: u64,
        result: Result<Box<RepositorySnapshot>, String>,
    },
    DirectoryFilesLoaded {
        key: ActiveWorktreeKey,
        request: u64,
        query: String,
        result: Result<Vec<repository_widget::RepositoryFile>, String>,
    },
    /// One page of the bound worktree's history.
    SourceControlHistory {
        generation: u64,
        key: ActiveWorktreeKey,
        page: u32,
        head: crate::repository::Oid,
        result: Result<Vec<CommitSummary>, String>,
    },
    /// A finished stage, unstage, discard, commit, or push.
    ///
    /// `report` is `None` when the mutation did everything it was asked to do.
    /// Every outcome triggers a re-snapshot rather than an optimistic edit:
    /// the only authority on what the index holds is Git.
    SourceControlMutated {
        key: ActiveWorktreeKey,
        kind: MutationKind,
        report: Option<String>,
        /// A commit that succeeded clears the message input; one that did not
        /// must keep it, because the text is the user's work.
        committed: bool,
    },
    NewTerminal,
    TerminalStarted {
        card: CardId,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
    },
    /// `None` when the terminal is no longer hosted and would have to be
    /// launched again, which waits until its card is opened.
    RestoredAgentAttached {
        card: CardId,
        terminal: TerminalId,
        result: Result<Option<StartedTerminal>, String>,
    },
    TerminalHandoffsCommitted {
        result: Result<Vec<(TerminalSession, Result<(), String>)>, String>,
    },
    /// Hosted terminals the daemon reports, either as its full inventory on
    /// connect or as one terminal another client just created.
    HostedTerminalsObserved(Vec<crate::terminal::ipc::TerminalDescriptor>),
    TerminalWake {
        card: CardId,
        generation: u64,
    },
    TerminalError {
        card: CardId,
        message: String,
    },
    /// Files or text dropped on a terminal pane.
    TerminalDropped {
        card: CardId,
        payload: crate::file_drop::Payload,
    },
    OpenRepository,
    RepositorySelected(Result<PathBuf, String>),
    RepositoryLoaded {
        open_cards: bool,
        project_id: ProjectId,
        generation: u64,
        result: Result<Box<LoadedRepository>, String>,
    },
    AttachedWorktreePrsResolved {
        project_id: ProjectId,
        generation: u64,
        worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
        recent: Vec<crate::github::PullRequest>,
        archived: Vec<crate::github::PullRequest>,
        expected: HashMap<crate::model::WorktreeId, Option<SidebarPullRequest>>,
    },
    /// One shared sidebar model update, from the daemon observer or publisher.
    WorkspaceModelObserved(workspace_model_sync::ModelUpdate),
    Repository(RepositoryAction),
    ResolvedDiffBasis {
        card: CardId,
        basis: DiffBasis,
    },
    Sidebar(SidebarAction),
    FocusCard(CardId),
    ResizeCard {
        card: CardId,
        width: f32,
    },
    ResizeCardFinished(CardId),
    CyclePaneWidth,
    CloseCard(CardId),
    CloseCardUnasked(CardId),
    WindowCloseRequested(window::Id),
    /// iced has removed a window, and winit has queued its destruction.
    WindowClosed(window::Id),
    NewWindow,
    NewWindowOpened {
        saved: Result<SettingsSaved, String>,
        result: Result<(), String>,
    },
    SettingsSaved {
        generation: u64,
        result: Result<SettingsSaved, String>,
    },
    StartupWindowOpened(window::Id),
    StartupReported(Result<(), String>),
    UiUpdate(instance::UiUpdateRequest),
    UiUpdateWatchdog(u64),
    UiUpdateFlushed {
        generation: u64,
        committing: bool,
        response: std::sync::mpsc::SyncSender<String>,
        saved: Result<SettingsSaved, String>,
        result: Result<(), String>,
    },
    CommitClose(CardId),
    ConfirmTerminalClose,
    CancelTerminalClose,
    TerminalCloseDontAskChanged(bool),
    TerminalCloseFinished {
        card: CardId,
        terminal: TerminalId,
        proof: HostSessionProof,
        generation: u64,
        outcome: terminal_close::CloseOutcome,
    },
    CompletedTerminalClosed {
        card: CardId,
        session: TerminalSession,
        result: Result<terminal_close::CompletedCloseOutcome, String>,
    },
    SaveAndCloseRepository(CardId),
    OverwriteAndCloseRepository(CardId),
    DiscardAndCloseRepository(CardId),
    CancelRepositoryClose,
    Viewport {
        generation: u64,
        event: WorkspaceViewportEvent,
    },
    #[cfg(feature = "latency-harness")]
    LatencyRedraw {
        generation: crate::performance::Generation,
        at: Instant,
    },
    #[cfg(feature = "latency-harness")]
    LatencyFixtureStep,
    Frame(Instant),
    LifecycleSweep,
    /// A sweep timer fired, carrying the generation it was armed for; a
    /// generation that has since been superseded is discarded.
    LifecycleSweepElapsed(u64),
    #[cfg(feature = "integrated-browser")]
    InitializeBrowserFactory,
    #[cfg(feature = "integrated-browser")]
    BrowserFactoryReady(Result<BrowserFactoryBinding, String>),
    WindowScaleFactorChanged(f32),
    WindowResized(f32, f32),
    ActivateUi {
        token: Option<String>,
    },
    OpenBrowser(Url),
    OpenLink(CardId, Url, LinkOpenGesture),
    OpenDocumentLink(CardId, String),
    PreviewLocalFile(CardId),
    OpenTerminalOutput(CardId),
    ExternalLinkOpened(Result<(), String>),
    NewBrowser,
    #[cfg(not(feature = "integrated-browser"))]
    ExternalUrlOpened(Result<(), String>),
    #[cfg(feature = "integrated-browser")]
    BrowserBack(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserForward(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserReload(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserImport(browser_import_ui::Action),
    RetryCard(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserAddressChanged {
        card: CardId,
        value: String,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserNavigate(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserEvent {
        card: CardId,
        event: BrowserEvent,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserError {
        card: CardId,
        message: String,
    },
    #[cfg(feature = "integrated-browser")]
    ClipboardRead {
        card: CardId,
        request: DelegateRequestId,
        value: Option<String>,
    },
    #[cfg(feature = "integrated-browser")]
    FilePickerResult {
        card: CardId,
        request: DelegateRequestId,
        result: Result<Option<Vec<PathBuf>>, String>,
    },
    WorktreeBranchChanged(String),
    WorktreeBaseChanged(String),
    WorktreeDestinationChanged(String),
    ToggleWorktreeAdvanced,
    WorktreeInputResolved {
        project_id: ProjectId,
        operation_id: crate::model::OperationId,
        result: Result<CreateWorktreeRequest, WorktreeInputError>,
    },
    SubmitWorktree,
    WorktreeProgress(crate::repository::WorktreeProgressEvent),
    WorktreeElapsed,
    WorktreeOperationFinished {
        operation: crate::model::OperationId,
        generation: u64,
        result: Result<crate::repository::WorktreeOperationReport, String>,
    },
    WorktreeOperationsRehydrated(Result<Vec<crate::repository::WorktreeOperationReport>, String>),
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
    PullRequestRetryConfirmed(bool),
    SubmitPullRequest,
    PullRequestCreated {
        dialog: PullRequestDialog,
        generation: u64,
        result: Result<PullRequestCreationOutcome, String>,
    },
    CancelDialog,
    KeyEvent {
        event: keyboard::Event,
        /// Whether a widget already consumed this keystroke. The shell's
        /// fallback chains must stand down when it did, or Escape typed to
        /// clear a terminal selection also closes the inspector behind it.
        captured: bool,
    },
    FocusAdjacent(crate::workspace::FocusDirection),
    FocusProjectSearch,
    ToggleContextInspector,
    SelectContextTab(ContextTab),
    ContextInspector(InspectorAction),
    SelectSettingsCategory(SettingsCategory),
    SettingsSearchChanged(String),
    SettingsChanged {
        field: SettingsField,
        value: String,
    },
    /// Retains the typed draft until its per-field debounce expires.
    SettingsDraftChanged {
        field: SettingsField,
        value: String,
    },
    SettingsAutosave {
        field: SettingsField,
        generation: u64,
    },
    ProjectUi(project_ui::Action),
    PullRequestUi(pull_request_ui::Action),
    PullRequestReview {
        card: CardId,
        action: pull_request_review::Action,
    },
    NativeWindowAction(cosmic::app::Action),
    ChatUi(chat_ui::Action),
    UpdateUi(update_ui::Action),
    ChromeUi(chrome_ui::Action),
    ChannelUi(channel_ui::Action),
    SidebarSelection(sidebar_selection_ui::Action),
    BranchUi(branch_ui::Action),
    CommitUi(history_ui::Action),
    RefreshUi(refresh_ui::Action),
    ReviewRequests(review_request_ui::Action),
    GitUi(git_ui::Action),
    NotificationUi(notification_ui::Action),
    AccountUi(account_ui::Action),
    AutomationUi(crate::automation_ui::Action),
    /// The shell's light or dark mode changed, so a rendered document page no
    /// longer matches the surface it sits on.
    DocumentToneStale,
    /// Opens the confirmation; `ConfirmResetSettings` performs the reset.
    ResetSettings,
    ConfirmResetSettings,
    CancelResetSettings,
    SessionSaveHealthChanged(SessionSaveHealthEvent),
    Lsp(LspEvent),
    LanguageServersReaped,
    RetrySessionSave,
    AcknowledgeSessionRecovery,
    DismissStatus,
    /// Dismisses exactly one logged notice; a stale id is a no-op.
    DismissNotice(u64),
    DismissAllNotices,
}

/// An application exit in progress.
///
/// The window closes as soon as the exit is committed. The process ends only
/// once both halves below are done: before the window is gone, iced's
/// teardown would run while it is still on screen, and before the servers
/// are reaped it would take away the runtime that stops them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ApplicationExit {
    /// The window the exit closed, until iced reports it gone.
    closing: Option<window::Id>,
    servers_reaped: bool,
}

impl ApplicationExit {
    fn is_complete(self) -> bool {
        self.closing.is_none() && self.servers_reaped
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtyCloseIntent {
    Card(CardId),
    Application(window::Id),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirtyCloseControl {
    Cancel,
    Discard,
    Save,
}

/// A source-control row's navigation, held until the repository card it is
/// aimed at has a file list to resolve the path against.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPanelOpen {
    card: CardId,
    /// Worktree-relative, exactly as the card's own file list names paths.
    path: PathBuf,
}

struct DirtyRepositoryClose {
    card: CardId,
    remaining: VecDeque<CardId>,
    intent: DirtyCloseIntent,
    focused: DirtyCloseControl,
    restore_card: Option<CardId>,
}

impl DirtyRepositoryClose {
    fn next_unresolved(&mut self, mut unresolved: impl FnMut(CardId) -> bool) -> Option<CardId> {
        while let Some(card) = self.remaining.pop_front() {
            if unresolved(card) {
                self.card = card;
                return Some(card);
            }
        }
        None
    }
}

#[cfg(feature = "integrated-browser")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserAddress {
    Location {
        authoritative: Url,
        presented: SanitizedLocation,
    },
    UserInput(String),
    Invalid(SanitizedLocation),
}

#[cfg(feature = "integrated-browser")]
impl BrowserAddress {
    fn location(url: &Url) -> Self {
        Self::Location {
            authoritative: url.clone(),
            presented: SanitizedLocation::from(url),
        }
    }

    fn user_input(value: String) -> Self {
        Self::UserInput(value)
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Location { presented, .. } => presented.as_str(),
            Self::UserInput(value) => value,
            Self::Invalid(presented) => presented.as_str(),
        }
    }

    /// Resolves the value to navigate without deriving authority from its
    /// redacted presentation. A restored or reported location therefore keeps
    /// its complete URL when the user submits the unchanged address, while a
    /// submitted draft is immediately replaced by its safe presentation.
    fn submit(&mut self) -> Result<Url, ()> {
        let result = match self {
            Self::Location { authoritative, .. } => Ok(authoritative.clone()),
            Self::UserInput(value) => {
                let value = value.trim();
                Url::parse(value).or_else(|error| {
                    if has_explicit_url_scheme(value) {
                        Err(error)
                    } else {
                        Url::parse(&format!("https://{value}"))
                    }
                })
            }
            Self::Invalid(_) => return Err(()),
        };

        match result {
            Ok(url) => {
                *self = Self::location(&url);
                Ok(url)
            }
            Err(_) => {
                *self = Self::Invalid(SanitizedLocation::invalid());
                Err(())
            }
        }
    }
}

#[cfg(feature = "integrated-browser")]
fn has_explicit_url_scheme(value: &str) -> bool {
    value.split_once(':').is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && chars.all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
    })
}

#[cfg(feature = "integrated-browser")]
struct BrowserRuntime {
    handle: BrowserHandle,
    ime: Option<InputMethodState>,
    lifecycle: BrowserLifecycle,
    address: BrowserAddress,
    load_state: Option<String>,
}

enum CardRuntime {
    Loading,
    Failed(String),
    Completed,
    Terminal(TerminalSurface),
    Chat(Box<chat_ui::Runtime>),
    #[cfg(feature = "integrated-browser")]
    Browser(BrowserRuntime),
    Repository(Box<RepositoryRuntime>),
    PullRequests(Box<PullRequestPane>),
    /// The settings pane. Its drafts are the window's, not the card's, so
    /// the runtime only marks that the card is ready.
    Settings,
    Accounts,
    Automations,
}

impl CardRuntime {
    /// The runtime for a utility pane kind.
    fn utility(kind: &CardKind) -> Self {
        match kind {
            CardKind::Settings => Self::Settings,
            CardKind::Accounts => Self::Accounts,
            CardKind::Automations => Self::Automations,
            _ => unreachable!("only utility panes have a utility runtime"),
        }
    }
}

/// The pull request card: a review beside a standalone repository runtime
/// that renders whichever file the review selects.
///
/// The runtime is never asked for the worktree's files; it only ever shows
/// committed content from the fetched pull request range, so the working
/// tree is left alone and the card cannot dirty it.
struct PullRequestPane {
    repository: PathBuf,
    review: pull_request_review::State,
    runtime: RepositoryRuntime,
    /// The pull request a restored card was showing, opened once the
    /// project's GitHub state has loaded enough to name the repository.
    pending_number: Option<u64>,
}

impl PullRequestPane {
    fn project(&self, model: &sidebar::SidebarModel) -> Option<ProjectId> {
        model
            .projects
            .iter()
            .find(|project| project.repository == self.repository)
            .map(|project| project.id)
    }
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
    title_id: widget::Id,
    container_id: widget::Id,
    close_id: widget::Id,
    resize_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_back_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_forward_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_reload_id: widget::Id,
    browser_address_id: widget::Id,
    type_label: &'static str,
    title: String,
    focused_name: String,
    caption: String,
    unfocused_name: String,
    resize_name: String,
    close_name: String,
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
            title_id: card_control_id(id, "title"),
            container_id: workspace_widget::card_widget_id(id),
            close_id: card_control_id(id, "close"),
            resize_id: card_control_id(id, "resize"),
            #[cfg(feature = "integrated-browser")]
            browser_back_id: card_control_id(id, "browser.back"),
            #[cfg(feature = "integrated-browser")]
            browser_forward_id: card_control_id(id, "browser.forward"),
            #[cfg(feature = "integrated-browser")]
            browser_reload_id: card_control_id(id, "browser.reload"),
            browser_address_id: card_control_id(id, "browser.address"),
            type_label,
            title: title.to_owned(),
            caption: caption.to_owned(),
            focused_name: card_accessible_name(type_label, title, caption, true),
            unfocused_name: card_accessible_name(type_label, title, caption, false),
            resize_name: format!("Resize {type_label} column {title}"),
            close_name: format!(
                "{}, {}",
                card_close_name(type_label, title),
                workspace_widget::shortcut_chords(
                    workspace_widget::GlobalShortcut::CloseFocusedCard,
                    shortcuts,
                ),
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

/// Which position-addressed request the inspector asked for.
#[derive(Debug, Clone, Copy)]
enum LspRequestKind {
    Hover,
    Definition,
    References,
}

/// A caret jump waiting for its target file to be the card's open buffer.
#[derive(Debug, Clone)]
struct PendingLspJump {
    path: PathBuf,
    at: LspPosition,
    coordinates: LspCoordinates,
}

/// One card's language-server results, with the buffer version each describes.
///
/// A server answers the text it was given. Keeping the version beside the
/// answer is what lets a stale one be refused instead of applied to text that
/// has since moved: a caret jump to a line that no longer exists, or a
/// completion whose edit range describes characters the user has already
/// replaced, are both silent corruptions, and neither is recoverable by
/// looking at the answer alone.
#[derive(Debug, Default)]
struct LspInspectorState {
    context: inspector::LspInspectorContext,
    /// The document, once opened. `None` while no file is open, or when no
    /// configured server claims this file's extension.
    document: Option<LspDocumentId>,
    /// Absolute path of the card's open buffer, recorded even when no server
    /// claims it. A jump target arrives absolute while the card's file list is
    /// worktree-relative, so this is the only side both can be compared on.
    path: Option<PathBuf>,
    /// The version the diagnostics were computed for, as the manager stamped
    /// them. Compared against [`LspManager::document_version`] and never
    /// against a count of this side's own: the manager re-opens a document on
    /// a restart, a crash-respawn, or a second card opening the same file, and
    /// none of those are edits this side made.
    diagnostics_version: Option<i32>,
    /// A failure the user has not dismissed. Outranks `progress`, because an
    /// indexing report arriving a second later must not erase the one message
    /// saying their hover and completions were thrown away.
    failure: Option<String>,
    /// The server's current progress report, if it is still working.
    progress: Option<String>,
}

impl LspInspectorState {
    /// Drops every answer that describes text the buffer no longer holds.
    ///
    /// Diagnostics are kept and re-dated instead of dropped: the server
    /// replaces them wholesale on its own schedule, and blanking them on each
    /// keystroke would make the list flicker empty while the user typed. They
    /// are refused at the point of use by version rather than erased here.
    fn invalidate_answers(&mut self) {
        self.context.hover = None;
        self.context.locations = None;
        self.context.completions.clear();
    }

    /// Recomputes the single notice the surface renders from the two
    /// independent things that can want it.
    fn refresh_notice(&mut self) {
        self.context.notice = self.failure.clone().or_else(|| self.progress.clone());
    }
}

/// Whether a published diagnostics list still describes the buffer on screen.
///
/// `published` is the version the manager stamped the list with; `document` is
/// the version its snapshot holds now. Both must be known and equal: an
/// unknown document has no diagnostics worth jumping into, and an unstamped
/// list is one this side never received.
const fn diagnostics_are_current(published: Option<i32>, document: Option<i32>) -> bool {
    match (published, document) {
        (Some(published), Some(document)) => published == document,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceDestination {
    Folder(ProjectId),
    Worktree(crate::model::WorktreeId),
    Channel(crate::model::ChannelId),
}

impl WorkspaceDestination {
    fn from_action(action: &SidebarAction) -> Option<Self> {
        match action {
            SidebarAction::OpenRepository { project_id, .. } => Some(Self::Folder(*project_id)),
            SidebarAction::OpenWorktree { worktree_id, .. } => Some(Self::Worktree(*worktree_id)),
            SidebarAction::OpenChannel { channel_id } => Some(Self::Channel(*channel_id)),
            _ => None,
        }
    }
}

pub struct Cagents {
    workspace: Workspace,
    durable: Arc<WorkspaceState>,
    cards: HashMap<CardId, CardRuntime>,
    card_chrome: HashMap<CardId, CardChrome>,
    lifecycle: LifecycleManager,
    /// The shared language-server client. One manager serves every card; a
    /// session is per project root, not per card, so two cards in one worktree
    /// share the server rather than starting a second copy of it.
    lsp: LspManager,
    /// Which card owns each open LSP document, so an event that names only a
    /// document reaches the surface that asked for it.
    lsp_documents: HashMap<LspDocumentId, CardId>,
    /// Results the inspector renders. Owned here rather than by the widget
    /// because they arrive on the manager's channel, not from widget state.
    lsp_contexts: HashMap<CardId, LspInspectorState>,
    /// A jump whose target file is not the card's open buffer, held until that
    /// file is open. A location's column is only placeable against the text it
    /// indexes, and for a `Utf16` location the text is not even loaded yet, so
    /// the jump waits for the open rather than landing on the wrong column.
    lsp_pending_jumps: HashMap<CardId, PendingLspJump>,
    next_lsp_document: u64,
    // A sweep is a full lifecycle reconcile plus a view rebuild, and scrolling
    // requests one per visibility transition, so requests are debounced onto a
    // single in-flight timer. `lifecycle_sweep_deadline` is `Some` exactly when
    // a timer of the current generation is pending; timer futures cannot be
    // cancelled, so superseding one means bumping the generation and letting
    // the stale delivery be dropped.
    lifecycle_sweep_deadline: Option<Instant>,
    lifecycle_sweep_generation: u64,
    terminal_wake_tx: async_channel::Sender<TerminalWakeup>,
    terminal_wake_rx: async_channel::Receiver<TerminalWakeup>,
    hosted_terminals_rx: async_channel::Receiver<Vec<crate::terminal::ipc::TerminalDescriptor>>,
    /// Terminal starts this window has dispatched and not yet accounted for.
    ///
    /// Adoption must not claim a terminal a start is about to claim, and a
    /// start can change the identity it ends up with, so the only safe answer
    /// is to know whether any start is outstanding. The counter is atomic for
    /// interior mutability - the start path holds `&self` - and never for
    /// concurrency: every mutation happens on the update thread.
    terminal_starts: std::sync::Mutex<HashMap<CardId, usize>>,
    /// Observations held back while a start was outstanding.
    pending_adoptions: Vec<TerminalSession>,
    /// Restored agent terminals that are not on screen, attached one at a
    /// time after startup so their rows show what the agent is doing.
    restored_agent_attaches: VecDeque<CardId>,
    relaunched_terminals: HashSet<TerminalId>,
    terminal_crash_notices: HashMap<CardId, crate::model::HostSessionProof>,
    /// The shared sidebar model as this window last saw it.
    workspace_model: crate::workspace_model::WorkspaceModel,
    /// Repositories this window has already been asked to open on the shared
    /// model's behalf. Asking twice would mint a second project on one checkout.
    workspace_model_opens: HashSet<PathBuf>,
    workspace_model_rx: async_channel::Receiver<workspace_model_sync::ModelUpdate>,
    workspace_model_tx: async_channel::Sender<crate::workspace_model::ModelEdit>,
    activation_rx: async_channel::Receiver<instance::UiActivation>,
    notification_ui: notification_ui::State,
    session: Option<SessionRuntime>,
    save_health: Option<SaveHealthState>,
    settings: AppSettings,
    settings_base: AppSettings,
    settings_save_generation: u64,
    settings_ack_generation: u64,
    settings_reset_generation: Option<u64>,
    settings_save_lane: Arc<std::sync::Mutex<Option<SettingsSaved>>>,
    update_prepared: bool,
    update_flush_pending: bool,
    update_generation: u64,
    update_committed: bool,
    /// Set once the application has committed to exiting. No later close
    /// request may start the exit again.
    exit: Option<ApplicationExit>,
    /// A shell theme change has landed that the open document pages were not
    /// rendered for. It is recorded rather than acted on, because the notice
    /// arrives on a path whose returned task libcosmic discards, and it is
    /// cleared only where the render is actually dispatched.
    document_tone_stale: bool,
    // These owners are declared before `core`: application Drop empties and
    // joins them explicitly, and field drop order is the final backstop.
    #[cfg(feature = "integrated-browser")]
    browser_teardowns: Vec<JoinHandle<()>>,
    #[cfg(feature = "integrated-browser")]
    browser_import: browser_import_ui::State,
    terminal_teardowns: Vec<JoinHandle<()>>,
    #[cfg(feature = "integrated-browser")]
    browser_factory: Option<Arc<RawWaylandSubsurfaceFactory>>,
    #[cfg(feature = "integrated-browser")]
    browser_parent_lifetime: Option<Arc<ParentWindowLifetime>>,
    /// Compositor scale, before the user's interface scale multiplier.
    window_scale_factor: f32,
    /// Starts at an estimate, because the pane density and the strip's viewport
    /// are both derived from it and the surface does not report a width until
    /// it has been mapped. `Message::WindowResized` replaces it.
    window_width: f32,
    window_height: f32,
    core: Core,
    settings_path: Option<PathBuf>,
    settings_write_blocked: bool,
    /// Bounded notice history, fed from `status` at the end of every `update`.
    notices: NoticeLog,
    sidebar_model: SidebarModel,
    project_ui: project_ui::State,
    channel_ui: channel_ui::State,
    sidebar_selection_ui: sidebar_selection_ui::State,
    chat_ui: chat_ui::State,
    update_ui: update_ui::State,
    branch_ui: branch_ui::State,
    history_ui: history_ui::State,
    refresh_ui: refresh_ui::State,
    account_ui: account_ui::State,
    automation_ui: crate::automation_ui::State,
    chrome_ui: chrome_ui::State,
    pull_request_inspector: pull_request_ui::State,
    sidebar_state: SidebarState,
    active_workspace: Option<PathBuf>,
    worktree_focus: HashMap<PathBuf, CardId>,
    worktree_offsets: HashMap<PathBuf, f32>,
    channel_offsets: HashMap<crate::model::ChannelId, f32>,
    worktree_owners: HashMap<CardId, PathBuf>,
    known_worktree_roots: HashSet<PathBuf>,
    workspace_view_generation: u64,
    workspace_scroll_id: widget::Id,
    github_availability: HashMap<ProjectId, GitHubAvailability>,
    /// Locally derived read identity; never grants GitHub mutation authority.
    review_repositories: HashMap<ProjectId, GitHubRepoKey>,
    /// The generation of the latest load of each project. Every generation is
    /// drawn from `repository_load_sequence`, so generations of different
    /// projects are ordered too: a load that reaches another project's
    /// repository can tell whether that project has been loaded since it began.
    repository_generations: HashMap<ProjectId, u64>,
    repository_pr_cancellations: HashMap<ProjectId, CancellationToken>,
    repository_load_sequence: u64,
    worktree_dialog: Option<WorktreeDialog>,
    /// Resetting settings destroys every customization with no undo, so it
    /// confirms first like every other destructive action in the shell.
    settings_reset_confirm: bool,
    pull_request_dialog: Option<PullRequestDialog>,
    /// Opening or dismissing a newer draft supersedes in-flight draft recovery.
    pull_request_dialog_generation: u64,
    dirty_repository_close: Option<DirtyRepositoryClose>,
    terminal_close: Option<terminal_close::CloseIntent>,
    queued_card_closes: VecDeque<terminal_close::QueuedCardClose>,
    terminal_close_attempts: HashSet<TerminalId>,
    terminal_close_generation: u64,
    closed_terminal_cards: HashSet<CardId>,
    uncertain_terminal_proofs: HashMap<CardId, HostSessionProof>,
    terminal_recovery_attempts: HashSet<TerminalId>,
    pending_terminal_handoffs: HashMap<TerminalId, TerminalSession>,
    terminal_handoff_commit_inflight: bool,
    worktree_operations:
        HashMap<crate::model::OperationId, worktree_operations::ActiveWorktreeOperation>,
    /// A source-control navigation waiting for its repository card to finish
    /// listing files.
    ///
    /// The card opens a path by looking it up in the list it already holds, so
    /// a sidebar row pressed before that list has landed - or against a card
    /// whose list predates the change - would open nothing at all. The
    /// navigation is therefore always paired with a refresh of that card's
    /// list and performed when the list arrives. One slot: the last press is
    /// the one the user is waiting on.
    pending_panel_open: Option<PendingPanelOpen>,
    deferred_navigation: VecDeque<Message>,
    frame_clock: FrameClock,
    latency: crate::performance::InteractionLatency,
    latency_focus: Option<CardId>,
    #[cfg(feature = "latency-harness")]
    latency_fixture: Option<crate::latency_fixture::Fixture>,
    #[cfg(feature = "latency-harness")]
    latency_fixture_started: bool,
    context_inspector_open: bool,
    context_inspector_transition: Option<InspectorTransition>,
    inspector_diagnostics_card: Option<CardId>,
    context_tab: ContextTab,
    /// The overlay state whose focus and native-surface transition has already
    /// been applied. The rendered mount also depends on workspace focus, so it
    /// cannot be derived solely from inspector-open and window-resize events.
    context_inspector_overlay_active: bool,
    context_inspector_restore_focus: Option<widget::Id>,
    settings_category: SettingsCategory,
    settings_search: String,
    tab_history: crate::tab_history::TabHistory<CardId>,
    workspace_history: crate::tab_history::TabHistory<WorkspaceDestination>,
    workspace_cycle_modifiers: keyboard::Modifiers,
    keyboard_modifiers: keyboard::Modifiers,
    settings_draft: SettingsDraft,
    settings_feedback: Option<SettingsFeedback>,
    /// Whether the settings pane held focus after the last update, so the
    /// update that moves focus away can flush the drafts typed into it.
    settings_pane_focused: bool,
    /// A utility pane the user asked for while its only pane was mid-close;
    /// it opens afresh once that close commits.
    reopen_utility: Option<CardKind>,
    settings_pending: HashMap<SettingsField, (u64, String)>,
    settings_generation: u64,
    settings_invalid: HashMap<SettingsField, String>,
    settings_save_error: Option<String>,
    status: Option<TransientNotice>,
}

async fn open_system_url(url: Url) -> Result<(), String> {
    let portal_result = async {
        let request = ashpd::desktop::open_uri::OpenFileRequest::default()
            .send_uri(&url)
            .await
            .map_err(|error| error.to_string())?;
        request.response().map_err(|error| error.to_string())
    }
    .await;
    match portal_result {
        Ok(()) => Ok(()),
        // Portal diagnostics are also intentionally omitted: they are an
        // external error boundary and may include the URI supplied above.
        Err(_) => open_system_url_with_opener(&url, tokio::process::Command::new("xdg-open")).await,
    }
}

async fn open_system_url_with_opener(
    url: &Url,
    mut opener: tokio::process::Command,
) -> Result<(), String> {
    let presented = SanitizedLocation::from(url);
    let result = opener
        .arg(url.as_str())
        .stdin(Stdio::null())
        // An opener can echo its argument in diagnostics. The authoritative
        // URL may contain credentials, query data, or a fragment, so neither
        // child stream may inherit cagents' presentation or logging streams.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "desktop portal could not open {presented}; system opener exited with {status}"
        )),
        Err(error) => Err(format!(
            "desktop portal could not open {presented}; system opener could not be started: {error}"
        )),
    }
}

impl Cagents {
    fn bootstrap(strict: bool) -> Result<Bootstrap, String> {
        let mut settings_warnings = Vec::new();
        let (settings, settings_path, settings_write_blocked, status) =
            match settings::default_path() {
                Ok(path) => match settings::load_detailed(&path) {
                    Ok(loaded) => {
                        settings_warnings.extend(loaded.set_aside());
                        (loaded.settings, Some(path), false, None)
                    }
                    Err(error) => (
                        AppSettings::default(),
                        Some(path),
                        true,
                        Some(error.to_string()),
                    ),
                },
                Err(error) => (AppSettings::default(), None, true, Some(error.to_string())),
            };
        if strict && let Some(error) = &status {
            return Err(error.clone());
        }
        let default_database_path = SessionRuntime::default_database_path().ok();
        let (session, restored, projects, worktrees, save_health, restore_warnings) =
            match SessionRuntime::open_default() {
                Ok((mut session, restored)) => {
                    let (projects, worktrees) = session.take_catalog();
                    let save_health = SaveHealthState::new(session.database_path().to_owned());
                    // A single directory scan plus one read-only query per
                    // other open window, so it belongs on the path that has
                    // just opened the database rather than in a background task
                    // that would race the first terminal into the same
                    // directory. Failure is diagnostic only: transcripts are a
                    // convenience, and refusing to start because stale ones
                    // could not be reclaimed would be absurd.
                    match session.prune_transcripts() {
                        Ok(_) => {}
                        Err(error) => tracing::warn!(
                            %error,
                            "could not reclaim unowned terminal transcripts"
                        ),
                    }
                    let warnings = session.restore_warnings().to_vec();
                    (
                        Some(session),
                        restored,
                        projects,
                        worktrees,
                        Some(save_health),
                        warnings,
                    )
                }
                Err(error) => {
                    if strict {
                        return Err(format!("could not restore workspace: {error}"));
                    }
                    let save_health = SaveHealthState::writer_unavailable(default_database_path);
                    (
                        None,
                        None,
                        Vec::new(),
                        Vec::new(),
                        Some(save_health),
                        Vec::new(),
                    )
                }
            };
        let mut restored = restored.unwrap_or_default();
        normalize_restored_titles(&mut restored);
        retire_duplicate_utility_cards(&mut restored);
        Ok(Bootstrap {
            workspace: restored,
            session,
            projects,
            worktrees,
            settings,
            settings_path,
            settings_write_blocked,
            status,
            save_health,
            restore_warnings,
            settings_warnings,
        })
    }

    pub fn load_startup() -> Result<Startup, String> {
        #[cfg(feature = "latency-harness")]
        if crate::latency_fixture::Fixture::from_env().is_some() {
            return Ok(Startup {
                bootstrap: Self::fixture_bootstrap(),
                window_view: None,
            });
        }
        let strict = instance::ui_launch_handshake_requested();
        let mut bootstrap = Self::bootstrap(strict)?;
        let view = bootstrap
            .session
            .as_ref()
            .map(|session| {
                session
                    .operation_store()
                    .load_window_view()
                    .map_err(|error| error.to_string())?
                    .map(|value| {
                        serde_json::from_str::<WindowView>(&value)
                            .map_err(|error| format!("could not restore window view: {error}"))
                    })
                    .transpose()
            })
            .transpose();
        let window_view = match view {
            Ok(view) => view.flatten(),
            Err(error) if strict => return Err(error),
            Err(error) => {
                bootstrap.status = Some(error);
                None
            }
        };
        Ok(Startup {
            bootstrap,
            window_view,
        })
    }

    fn rebuild_workspace(state: &WorkspaceState) -> Workspace {
        let mut workspace = Workspace::new(WorkspaceConfig::default()).unwrap_or_default();
        for card in state.cards.iter().filter(|card| card.existence.is_open()) {
            let width = if card.width_fraction.is_finite() && card.width_fraction > 0.0 {
                CardWidth::Fraction(card.width_fraction.min(1.0))
            } else {
                CardWidth::Fraction(0.5)
            };
            let index = workspace.cards().len();
            let _ = workspace.add_card(
                WorkspaceCard::new(card.id, card.title.clone(), width).unwrap_or_else(|_| {
                    WorkspaceCard {
                        id: card.id,
                        accessible_name: "Workspace card".into(),
                        width: CardWidth::Fraction(0.5),
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
        &mut self,
        path: PathBuf,
        project_id: Option<ProjectId>,
        open_cards: bool,
    ) -> Task<Message> {
        self.project_load_task(path, project_id, open_cards, None)
    }

    fn project_load_task(
        &mut self,
        path: PathBuf,
        project_id: Option<ProjectId>,
        open_cards: bool,
        kind: Option<ProjectKind>,
    ) -> Task<Message> {
        let project_id = project_id.unwrap_or_default();
        if open_cards && let Ok(repository) = crate::workspace_model::canonical_repository(&path) {
            if self.project_ui.is_reopening(&repository) {
                self.status = Some(TransientNotice::warning(
                    "This project is still opening. Try again when it finishes.",
                ));
                return Task::none();
            }
            if self
                .project_ui
                .closing
                .values()
                .any(|closing| *closing == repository)
            {
                self.status = Some(TransientNotice::warning(
                    "Wait for this project to finish closing before reopening it.",
                ));
                return Task::none();
            }
            self.project_ui.closed.remove(&repository);
            self.project_ui.reopening.insert(project_id, repository);
        }
        self.repository_load_sequence = self.repository_load_sequence.wrapping_add(1);
        let generation = self.repository_load_sequence;
        self.repository_generations.insert(project_id, generation);
        if let Some(previous) = self
            .repository_pr_cancellations
            .insert(project_id, CancellationToken::new())
        {
            previous.cancel();
        }
        // Liveness is not authority. A poll starting says nothing about whether
        // the user is still authorized against the same repository, so the last
        // confirmed identity rides along and the sidebar keeps its pull-request
        // actions until a refresh actually completes and reports otherwise.
        let availability = GitHubAvailability::Loading {
            confirmed: self
                .github_availability
                .get(&project_id)
                .and_then(GitHubAvailability::repository_key)
                .cloned(),
        };
        if let Some(project) = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.github_status = availability.label().into();
            project.reloading = true;
        }
        self.github_availability.insert(project_id, availability);
        self.repository_read_task(path, project_id, generation, open_cards, kind)
    }

    #[cfg(feature = "integrated-browser")]
    fn browser_factory_task(&self) -> Task<Message> {
        if self.browser_factory.is_some() {
            return Task::none();
        }
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

    #[cfg(feature = "integrated-browser")]
    fn register_deferred_browsers(&mut self) {
        // A browser created before the factory arrives is already registered
        // as Hydrating, but has no worker. Reset every Loading browser so the
        // lifecycle can actually start it once the parent window is ready.
        let pending = self
            .durable
            .cards
            .iter()
            .filter(|card| {
                can_register_deferred_browser(
                    card,
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
        let mut repository_tasks = Vec::new();
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
            #[cfg(feature = "integrated-browser")]
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
                    if terminal_ended(&session) {
                        // An ended terminal is restored the way it was left,
                        // however it ended. Its card states the outcome and
                        // offers a relaunch; it is not a runtime failure.
                        self.lifecycle.remove(card.id);
                        self.cards.insert(card.id, CardRuntime::Completed);
                    } else {
                        // Restored terminals remain detached until lifecycle
                        // reconciliation admits them under the terminal startup
                        // budget. An agent's row still has to report what the
                        // agent is doing, which takes an attached connection
                        // but no visible surface.
                        if agent_ui::observes_terminal_agent(&session) {
                            self.restored_agent_attaches.push_back(card.id);
                        }
                        self.cards.insert(card.id, CardRuntime::Loading);
                    }
                }
                CardKind::Chat(session) => {
                    // Whether this is an attach, a daemon-proven upgrade
                    // handoff or a fresh resume is not knowable here: only the
                    // live daemon proof answers it, and that is read on the
                    // connection. The card states what it wants.
                    repository_tasks.push(self.open_chat(
                        card.id,
                        session,
                        crate::chat::Intent::Restore,
                    ));
                }
                #[cfg(feature = "integrated-browser")]
                CardKind::Browser(_) => {
                    if let Some(state) = self.card_state_mut(card.id) {
                        state.residency = runtime_residency;
                    }
                    self.cards.insert(card.id, CardRuntime::Loading);
                }
                #[cfg(not(feature = "integrated-browser"))]
                CardKind::Browser(session) => {
                    debug_assert!(matches!(
                        browser_restore_action(&session),
                        BrowserRestoreAction::OfferExternal(_)
                    ));
                    self.cards.insert(
                        card.id,
                        CardRuntime::Failed(
                            "This cagents build does not include the integrated browser. Open the saved URL in your system browser."
                                .into(),
                        ),
                    );
                }
                CardKind::Repository(view) => {
                    let mut runtime = self.repository_runtime(view);
                    runtime.set_auto_save(self.settings.editor_auto_save);
                    runtime.set_wrap_lines(self.settings.editor_wrap_lines);
                    runtime.set_syntax_theme(self.settings.editor_syntax_theme);
                    runtime.set_external_editors(self.sidebar_model.external_editors.clone());
                    repository_tasks.push(repository_task(runtime.initial_load(card.id)));
                    self.cards
                        .insert(card.id, CardRuntime::Repository(Box::new(runtime)));
                }
                CardKind::PullRequests(view) => {
                    let pane = self.pull_request_pane(&view);
                    self.cards
                        .insert(card.id, CardRuntime::PullRequests(Box::new(pane)));
                    repository_tasks.push(self.open_pending_pull_request(card.id));
                }
                kind @ (CardKind::Settings | CardKind::Accounts | CardKind::Automations) => {
                    if let Some(state) = self.card_state_mut(card.id) {
                        state.residency = Residency::Hot;
                    }
                    self.cards.insert(card.id, CardRuntime::utility(&kind));
                    if matches!(kind, CardKind::Automations) {
                        repository_tasks
                            .push(self.update_automation_ui(crate::automation_ui::Action::Refresh));
                    }
                }
            }
        }

        // Restored integrated browsers wait for the rendering factory. A
        // browserless build leaves their durable descriptors untouched and
        // renders an explicit external-open path instead.
        Task::batch([
            self.reconcile_lifecycle(&initially_visible, &initially_prefetch),
            self.attach_next_restored_agent(),
            Task::batch(repository_tasks),
        ])
    }

    fn start_terminal(
        &self,
        card: CardId,
        session: TerminalSession,
        visible: bool,
        explicit_retry: bool,
    ) -> Task<Message> {
        // Recovery can replace the identity this start ends up with, so
        // adoption has to know a start is outstanding before the task runs.
        self.begin_terminal_start(card);
        let config = self.terminal_config(visible);
        let sender = self.terminal_wake_tx.clone();
        let terminal = session.id;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let (recovered, mode) = recover_terminal(session, explicit_retry)?;
                open_terminal(card, recovered, mode, config, sender)
            })
            .await
            .map_err(|error| format!("terminal worker join failed: {error}"))
            .and_then(|result| result);
            Message::TerminalStarted {
                card,
                terminal,
                result,
            }
        })
    }

    /// Attaches a restored agent terminal that is not on screen, so its row
    /// reports what the agent is doing before anyone opens the card. Only a
    /// terminal the daemon still hosts is attached: launching one again is the
    /// user's decision, made by opening the card.
    fn attach_restored_agent(&self, card: CardId, session: TerminalSession) -> Task<Message> {
        self.begin_terminal_start(card);
        let config = self.terminal_config(false);
        let sender = self.terminal_wake_tx.clone();
        let terminal = session.id;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let (recovered, mode) = recover_terminal(session, false)?;
                if matches!(mode, TerminalStartMode::Spawn(_)) {
                    return Ok(None);
                }
                open_terminal(card, recovered, mode, config, sender).map(Some)
            })
            .await
            .map_err(|error| format!("terminal worker join failed: {error}"))
            .and_then(|result| result);
            Message::RestoredAgentAttached {
                card,
                terminal,
                result,
            }
        })
    }

    /// Starts the next queued restored agent attach that still applies, once
    /// no other terminal start is outstanding. Each attach holds the daemon's
    /// recovery lease, so they run one at a time and never ahead of a card
    /// being opened, which needs that lease too.
    pub(super) fn attach_next_restored_agent(&mut self) -> Task<Message> {
        if self.terminal_starts_in_flight() > 0 {
            return Task::none();
        }
        // A worktree being deleted is about to close its cards, so theirs
        // wait: a deletion that fails leaves them to be attached after all.
        let mut deleting = Vec::new();
        let mut task = Task::none();
        while let Some(card) = self.restored_agent_attaches.pop_front() {
            if !matches!(self.lifecycle.state(card), Some((_, Residency::Hibernated))) {
                continue;
            }
            if self
                .card_state(card)
                .is_some_and(|state| self.project_ui.deleting_card(state))
            {
                deleting.push(card);
                continue;
            }
            if let Some(session) = deferred_terminal_session(
                self.card_state(card),
                matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
            ) {
                task = self.attach_restored_agent(card, session);
                break;
            }
        }
        for card in deleting.into_iter().rev() {
            self.restored_agent_attaches.push_front(card);
        }
        task
    }

    fn terminal_started(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
    ) -> Task<Message> {
        let accept = can_accept_terminal_start(
            self.durable.cards.iter().find(|state| state.id == card),
            self.lifecycle.state(card).is_some(),
            matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
        ) && self.card_state(card).is_some_and(
            |state| matches!(&state.kind, CardKind::Terminal(session) if session.id == terminal),
        );
        if !accept {
            if let Ok(started) = result
                && started.spawned
            {
                self.begin_terminal_teardown(started.handle);
            }
            // Nothing will claim this identity, so a held observation
            // is free to decide about it.
            return self.finish_terminal_start(card);
        }
        // A card still hibernated when its start lands is a restored agent
        // attached only for its row.
        let hibernated = matches!(self.lifecycle.state(card), Some((_, Residency::Hibernated)));

        let mut tasks = Vec::new();
        match result {
            Ok(started) => {
                if let Some(proof) = started.session.continuity.live_proof() {
                    // A replacement daemon cannot own the previous
                    // daemon's in-memory receipts.
                    self.pending_terminal_handoffs.retain(|_, pending| {
                        pending
                            .continuity
                            .live_proof()
                            .is_none_or(|stored| stored == proof)
                    });
                    self.pending_terminal_handoffs
                        .insert(started.session.id, started.session.clone());
                }
                self.terminal_recovery_attempts.remove(&terminal);
                self.terminal_recovery_attempts.remove(&started.session.id);
                if started.session.id != terminal
                    && let Some(writer) = &self.session
                {
                    writer.replace_terminal_identity(card, terminal, started.session.clone());
                }
                if let Some(state) = self.card_state_mut(card) {
                    state.kind = CardKind::Terminal(started.session);
                }
                let handle = started.handle;
                // A worker can publish its first event before this start
                // result reaches the UI. Adopt and drain it before sending
                // commands, which could otherwise mask a permanent failure
                // as a closed visibility channel.
                self.cards.insert(card, CardRuntime::Terminal(handle));
                tasks.push(self.drain_terminal(card, 0));
                if ended_terminal(self.card_state(card)).is_some() {
                    // The daemon can report a session that already
                    // ended without a queued event to drain, and a card
                    // registered as hydrating has nothing else left to
                    // resolve it.
                    self.lifecycle.remove(card);
                    if let Some(state) = self.card_state_mut(card) {
                        state.residency = Residency::Hibernated;
                    }
                    self.persist();
                    tasks.push(self.commit_terminal_handoffs());
                    tasks.push(self.finish_terminal_start(card));
                    return Task::batch(tasks);
                }
                let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                    tasks.push(cosmic::task::message(Message::LifecycleSweep));
                    self.persist();
                    tasks.push(self.commit_terminal_handoffs());
                    tasks.push(self.finish_terminal_start(card));
                    return Task::batch(tasks);
                };
                if let Err(error) = handle.set_theme(self.settings.terminal_theme) {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not apply terminal theme: {error}"
                    )));
                }
                let visible = terminal_start_visibility(
                    self.durable.cards.iter().find(|state| state.id == card),
                )
                .expect("accepted terminal start has an open terminal card")
                    && self.workspace_surfaces_visible();
                // A restored agent attached while its card stays
                // hibernated keeps only the connection its row needs.
                let visibility = if hibernated {
                    handle.hibernate()
                } else if visible {
                    handle.reveal()
                } else {
                    handle.hide()
                };
                match visibility {
                    Ok(()) => {
                        if self.finish_initial_hydration(card, Ok(())) {
                            let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
                            tasks.push(self.request_lifecycle_sweep(delay));
                        }
                    }
                    Err(error) => {
                        let error =
                            format!("could not apply restored terminal visibility: {error}");
                        self.fail_runtime(card, error.clone());
                        self.status = Some(TransientNotice::error(error));
                        tasks.push(cosmic::task::message(Message::LifecycleSweep));
                    }
                }
                // The initial focus request ran while this card still held
                // its loading surface, so it could only reach the title
                // button. Once the terminal is mounted, move keyboard focus
                // to its input surface; otherwise that button consumes the
                // first Enter even though typed characters reach the PTY.
                if self.workspace.focused() == Some(card) && !self.workspace_inputs_blocked() {
                    tasks.push(
                        iced::widget::operation::focus(self.card_keyboard_focus(card))
                            .map(cosmic::Action::App),
                    );
                }
            }
            Err(error) => {
                self.fail_runtime(card, error.clone());
                self.status = Some(TransientNotice::error(error));
                tasks.push(cosmic::task::message(Message::LifecycleSweep));
            }
        }
        // A hibernated card holds no hot slot for the sweep to reclaim.
        if !hibernated
            && self
                .card_state(card)
                .is_some_and(|state| state.exposure == Exposure::Offscreen)
        {
            tasks.push(self.reconcile_scoped_lifecycle());
        }
        self.persist();
        tasks.push(self.commit_terminal_handoffs());
        // Released last: until the card holds the identity this start
        // settled on, adoption would see an unclaimed live terminal.
        tasks.push(self.finish_terminal_start(card));
        Task::batch(tasks)
    }

    fn terminal_config(&self, visible: bool) -> TerminalConfig {
        TerminalConfig {
            initially_visible: visible,
            max_scrollback: self.settings.terminal_scrollback_lines,
            theme: self.settings.terminal_theme,
            ..TerminalConfig::default()
        }
    }

    fn add_terminal(&mut self, working_directory: PathBuf) -> Task<Message> {
        let mut launch = match LaunchRecipe::shell(working_directory.clone()) {
            Ok(launch) => launch,
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "could not create terminal: {error}"
                )));
                return Task::none();
            }
        };
        // A shell can start either provider's CLI. Give it the same default
        // homes as native conversations, and persist those choices in its
        // launch recipe so a restored terminal never silently changes login.
        for provider in Account::PROVIDERS {
            let account = self.default_account_for_directory(provider, &working_directory);
            launch.unset_environment.extend(
                account
                    .credential_overrides()
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
            if let (Some(variable), Some(home)) =
                (account::home_variable(provider), account.resolved_home())
            {
                launch
                    .environment
                    .insert(variable.to_owned(), home.display().to_string());
            }
        }
        self.add_terminal_recipe(launch)
    }

    fn add_terminal_recipe(&mut self, launch: LaunchRecipe) -> Task<Message> {
        if self.refuse_in_deleting_worktree(&launch.working_directory) {
            return Task::none();
        }
        let scope = if self.active_workspace.as_deref() != Some(launch.working_directory.as_path())
        {
            self.select_worktree(launch.working_directory.clone(), false)
        } else {
            Task::none()
        };
        Task::batch([
            scope,
            self.insert_terminal_card(fresh_terminal_session(launch), None),
        ])
    }

    /// Opens the new terminal `session` in the current scope: at `slot`, as
    /// strip and saved-order indices, or else beside the focused card.
    pub(super) fn insert_terminal_card(
        &mut self,
        session: TerminalSession,
        slot: Option<(usize, usize)>,
    ) -> Task<Message> {
        if self.refuse_in_deleting_worktree(&session.launch.working_directory) {
            return Task::none();
        }
        let working_directory = session.launch.working_directory.clone();
        let card_id = CardId::new();
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            // A directory name is chosen outside the application, so it needs
            // the same normalisation a reported title gets: it reaches the card
            // header, the accessibility strings and the window title.
            title: working_directory
                .file_name()
                .and_then(|name| normalize_card_name(&name.to_string_lossy()))
                .unwrap_or_else(|| "Terminal".into()),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating { epoch: 1 },
            kind: CardKind::Terminal(session.clone()),
        };
        let (index, durable_index) = slot.unwrap_or_else(|| self.terminal_insertion_indices());
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.insert(durable_index, card.clone());
        durable.focused = Some(card_id);
        self.register_channel_card(card_id);
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
            self.start_terminal(card_id, session, true, false),
            self.apply_viewport(update, true, false),
        ])
    }

    fn clone_focused_pane(&mut self) -> Task<Message> {
        if self.workspace_inputs_blocked() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let Some(source) = self.workspace.focused().and_then(|id| self.card_state(id)) else {
            return Task::none();
        };
        if self.project_ui.deleting_card(source) {
            return Task::none();
        }
        // A window holds one of each utility pane; a copy would show the
        // same application state twice.
        if !source.existence.is_open() || source.kind.is_utility() {
            return Task::none();
        }
        #[cfg(not(feature = "integrated-browser"))]
        if let CardKind::Browser(session) = &source.kind {
            return Self::open_external_url(session.current_url.clone());
        }
        let mut card = source.clone();
        card.id = CardId::new();
        card.generation = self.durable.generation;
        card.exposure = Exposure::Visible;
        card.residency = Residency::Hydrating { epoch: 1 };
        match &mut card.kind {
            CardKind::Terminal(session) => {
                if crate::agent::Provider::from_program(&session.launch.program).is_some() {
                    let Some(recipe) = crate::model::agent_recovery::fresh_recipe(&session.launch)
                    else {
                        self.status = Some(TransientNotice::error(
                            "Cannot clone this agent's launch options safely.",
                        ));
                        return Task::none();
                    };
                    session.launch = recipe;
                }
                session.id = TerminalId::new();
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: None,
                };
                session.resume_hint = None;
                session.transcript_path = transcript_path_for(&session.launch, session.id);
            }
            CardKind::Chat(session) => {
                let Some(recipe) =
                    crate::model::agent_recovery::fresh_recipe(&session.launch.recipe)
                else {
                    self.status = Some(TransientNotice::error(
                        "Cannot clone this conversation's launch options safely.",
                    ));
                    return Task::none();
                };
                session.id = TerminalId::new();
                session.launch.recipe = recipe;
                session.launch.session_id = None;
                session.proof = None;
                session.terminal = None;
                card.title = crate::app::chat_ui::chat_card_title(session.launch.provider).into();
            }
            CardKind::Browser(session) => {
                session.id = crate::model::BrowserId::new();
            }
            CardKind::Repository(_) | CardKind::PullRequests(_) => {
                card.residency = Residency::Hot;
            }
            CardKind::Settings | CardKind::Accounts | CardKind::Automations => {
                unreachable!("utility panes are refused before cloning")
            }
        }
        let (index, durable_index) = self.terminal_insertion_indices();
        let id = card.id;
        let update = match self.workspace.add_card(
            WorkspaceCard::new(
                id,
                card.title.clone(),
                CardWidth::Fraction(card.width_fraction),
            )
            .expect("cloned pane has valid width"),
            index,
            true,
        ) {
            Ok(update) => update,
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                return Task::none();
            }
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.insert(durable_index, card.clone());
        durable.focused = Some(id);
        self.register_channel_card(id);
        self.lifecycle
            .register(id, card.exposure, card.residency, Instant::now());
        let start = match card.kind {
            CardKind::Terminal(session) => {
                self.cards.insert(id, CardRuntime::Loading);
                self.start_terminal(id, session, true, false)
            }
            CardKind::Chat(session) => self.open_chat(id, session, crate::chat::Intent::Start),
            #[cfg(feature = "integrated-browser")]
            CardKind::Browser(_) => self.retry_card(id),
            #[cfg(not(feature = "integrated-browser"))]
            CardKind::Browser(_) => unreachable!("browserless clones open externally"),
            CardKind::Repository(view) => {
                let mut runtime = self.repository_runtime(view);
                runtime.set_auto_save(self.settings.editor_auto_save);
                runtime.set_wrap_lines(self.settings.editor_wrap_lines);
                runtime.set_syntax_theme(self.settings.editor_syntax_theme);
                runtime.set_external_editors(self.sidebar_model.external_editors.clone());
                let load = repository_task(runtime.initial_load(id));
                self.cards
                    .insert(id, CardRuntime::Repository(Box::new(runtime)));
                load
            }
            CardKind::PullRequests(view) => {
                let pane = self.pull_request_pane(&view);
                self.cards
                    .insert(id, CardRuntime::PullRequests(Box::new(pane)));
                self.open_pending_pull_request(id)
            }
            CardKind::Settings | CardKind::Accounts | CardKind::Automations => {
                unreachable!("utility panes are refused before cloning")
            }
        };
        self.persist();
        Task::batch([start, self.apply_viewport(update, true, false)])
    }

    fn pull_request_pane(&self, view: &crate::model::PullRequestView) -> PullRequestPane {
        let project = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == view.repository)
            .map_or_else(ProjectId::new, |project| project.id);
        let mut runtime = RepositoryRuntime::new(RepositoryView {
            selected_file: None,
            project,
            repository: view.repository.clone(),
            worktree: view.repository.clone(),
        });
        runtime.set_standalone(true);
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        runtime.set_syntax_theme(self.settings.editor_syntax_theme);
        PullRequestPane {
            repository: view.repository.clone(),
            review: pull_request_review::State::new(
                Arc::new(GitHubService::system("gh")),
                RepositoryService::system("git"),
            )
            .with_cache(self.session.as_ref().map(|session| {
                session.operation_store() as Arc<dyn pull_request_review::cache::Store>
            })),
            runtime,
            pending_number: view.selected_number,
        }
    }

    /// The target for a pull request the project's GitHub state lists, or
    /// `None` until that state has loaded or when the number is not open.
    fn pull_request_target(
        &self,
        project: ProjectId,
        number: u64,
    ) -> Option<pull_request_review::ReviewTarget> {
        let project = self
            .sidebar_model
            .projects
            .iter()
            .find(|candidate| candidate.id == project)?;
        let GitHubAvailability::Ready {
            repository_key,
            pull_requests,
            ..
        } = self.github_availability.get(&project.id)?
        else {
            return None;
        };
        let (pull_request, _) = pull_requests
            .iter()
            .find(|(pull_request, _)| pull_request.number == number)?;
        Some(pull_request_review::ReviewTarget {
            repository: repository_key.clone(),
            // Fetched refs land in the repository's shared object store, so
            // the main worktree serves every review of the project.
            worktree: project.repository.clone(),
            pull_request: Arc::new(pull_request.clone()),
        })
    }

    /// Opens the pull request a card was restored with once it can be named.
    /// Called on creation and again whenever a project's GitHub state lands.
    fn open_pending_pull_request(&mut self, card: CardId) -> Task<Message> {
        let source = self.ensure_pull_request_source(card);
        let open = self.open_pending_pull_request_target(card);
        Task::batch([source, open])
    }

    fn open_pending_pull_request_target(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card) else {
            return Task::none();
        };
        let Some(number) = pane.pending_number else {
            return Task::none();
        };
        let Some(project) = pane.project(&self.sidebar_model) else {
            return Task::none();
        };
        let Some(target) = self.pull_request_target(project, number) else {
            // Sidebar refreshes are bounded. A saved PR outside that first
            // batch still deserves an exact lookup, not a silent dismissal.
            if pane.review.source().is_some()
                && let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card)
            {
                pane.pending_number = None;
                return self.apply_pull_request_review(
                    card,
                    pull_request_review::Action::OpenNumber(number),
                );
            }
            return Task::none();
        };
        if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
            pane.pending_number = None;
        }
        self.apply_pull_request_review(card, pull_request_review::Action::Open(target))
    }

    /// Every restored pull request card still waiting on this project.
    fn open_pending_pull_requests(&mut self, project: ProjectId) -> Task<Message> {
        let waiting: Vec<CardId> = self
            .cards
            .iter()
            .filter_map(|(card, runtime)| match runtime {
                CardRuntime::PullRequests(pane)
                    if pane.project(&self.sidebar_model) == Some(project) =>
                {
                    Some(*card)
                }
                _ => None,
            })
            .collect();
        Task::batch(
            waiting
                .into_iter()
                .map(|card| self.open_pending_pull_request(card)),
        )
    }

    /// Shows a pull request in the review pane for its project: the open
    /// pull request card for that repository in this workspace, or a new one.
    fn review_pull_request(&mut self, project: ProjectId, number: u64) -> Task<Message> {
        let target = self.pull_request_target(project, number);
        let Some(repository) = self
            .sidebar_model
            .project(project)
            .map(|project| project.repository.clone())
        else {
            return Task::none();
        };
        let existing = self.durable.cards.iter().find(|card| {
            card.existence.is_open()
                && self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card.id))
                    .map(|channel| channel.id)
                    == self.durable.active_channel
                && matches!(&card.kind, CardKind::PullRequests(view) if view.repository == repository)
        });
        let (card, created) = match existing {
            Some(card) => (card.id, Task::none()),
            None => self.add_pull_requests_card(repository),
        };
        let focus = cosmic::task::message(Message::FocusCard(card));
        let source = self.ensure_pull_request_source(card);
        let action = target.map_or(
            pull_request_review::Action::OpenNumber(number),
            pull_request_review::Action::Open,
        );
        let open = self.apply_pull_request_review(card, action);
        Task::batch([created, focus, source, open])
    }

    fn browse_pull_requests(&mut self, project: ProjectId) -> Task<Message> {
        let Some(project) = self.sidebar_model.project(project) else {
            return Task::none();
        };
        let repository = project.repository.clone();
        let existing = self.durable.cards.iter().find(|card| {
            card.existence.is_open()
                && self.durable.channels.iter().any(|channel| {
                    Some(channel.id) == self.durable.active_channel && channel.cards.contains(&card.id)
                })
                && matches!(&card.kind, CardKind::PullRequests(view) if view.repository == repository)
        });
        let (card, created) = match existing {
            Some(card) => (card.id, Task::none()),
            None => self.add_pull_requests_card(repository),
        };
        let source = self.ensure_pull_request_source(card);
        let close = self.apply_pull_request_review(card, pull_request_review::Action::Close);
        Task::batch([
            created,
            source,
            close,
            cosmic::task::message(Message::FocusCard(card)),
        ])
    }

    fn ensure_pull_request_source(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card) else {
            return Task::none();
        };
        let source = pane
            .project(&self.sidebar_model)
            .and_then(|id| {
                self.github_availability
                    .get(&id)
                    .and_then(GitHubAvailability::repository_key)
                    .or_else(|| self.review_repositories.get(&id))
            })
            .map(|repository| pull_request_review::ReviewSource {
                repository: repository.clone(),
                worktree: pane.repository.clone(),
            });
        self.apply_pull_request_review(card, pull_request_review::Action::SetSource(source))
    }

    /// Drops fetched ranges of pull requests the project's GitHub state no
    /// longer lists as open, keeping whatever a pane is still reviewing.
    /// The refs live in the repository's shared store, which two projects
    /// rooted in worktrees of one clone both reach, so every pane's target
    /// is kept, not only those of panes on this project's root.
    fn prune_pull_request_refs(&self, project: ProjectId) -> Task<Message> {
        let Some(project) = self
            .sidebar_model
            .projects
            .iter()
            .find(|candidate| candidate.id == project)
        else {
            return Task::none();
        };
        let Some(GitHubAvailability::Ready { pull_requests, .. }) =
            self.github_availability.get(&project.id)
        else {
            return Task::none();
        };
        let mut retain: Vec<u64> = pull_requests
            .iter()
            .map(|(pull_request, _)| pull_request.number)
            .collect();
        retain.extend(self.cards.values().filter_map(|runtime| {
            match runtime {
                CardRuntime::PullRequests(pane) => pane
                    .review
                    .target()
                    .map(|target| target.pull_request.number),
                _ => None,
            }
        }));
        pull_request_review::prune_fetched_refs(
            RepositoryService::system("git"),
            project.repository.clone(),
            retain,
            CancellationToken::new(),
        )
        .discard()
    }

    fn add_pull_requests_card(&mut self, repository: PathBuf) -> (CardId, Task<Message>) {
        let card_id = CardId::new();
        let view = crate::model::PullRequestView {
            repository: repository.clone(),
            selected_number: None,
        };
        let title = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.repository == repository)
            .map(|project| format!("{} PRs", project.name))
            .unwrap_or_else(|| {
                repository
                    .file_name()
                    .and_then(|name| normalize_card_name(&name.to_string_lossy()))
                    .map_or_else(|| "Pull requests".to_owned(), |name| format!("{name} PRs"))
            });
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: title.clone(),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::PullRequests(view.clone()),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card);
        durable.focused = Some(card_id);
        self.register_channel_card(card_id);
        let pane = self.pull_request_pane(&view);
        self.cards
            .insert(card_id, CardRuntime::PullRequests(Box::new(pane)));
        let index = self.workspace.cards().len();
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    title,
                    CardWidth::Fraction(self.settings.default_column_fraction),
                )
                .expect("pull request card is valid"),
                index,
                true,
            )
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                ViewportUpdate::unchanged()
            });
        self.persist();
        let source = self.ensure_pull_request_source(card_id);
        (
            card_id,
            Task::batch([self.apply_viewport(update, true, false), source]),
        )
    }

    /// Routes one review action to a pull request card and carries out what
    /// the review asks of the shell: which file its runtime shows, which link
    /// to open, and which pull request the card remembers.
    fn handle_pull_request_review(
        &mut self,
        card: CardId,
        action: pull_request_review::Action,
    ) -> Task<Message> {
        if self.modal_is_open() && !action.allowed_while_modal() {
            return Task::none();
        }
        self.apply_pull_request_review(card, action)
    }

    /// The kind of the focused pane, which decides which shortcut scopes
    /// apply. A terminal counts as focused exactly when it is the one its own
    /// widget forwards keys from, so a chord its scope leaves to the terminal
    /// always reaches one.
    ///
    /// A terminal that has ended, or is closing, is read-only and forwards
    /// nothing, so it counts as any other pane.
    fn focused_pane(&self) -> workspace_widget::FocusedPane {
        let Some(card) = self.workspace.focused() else {
            return workspace_widget::FocusedPane::Other;
        };
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(_))
                if ended_terminal(self.card_state(card)).is_none()
                    && !self.terminal_close_pending(card) =>
            {
                workspace_widget::FocusedPane::Terminal
            }
            Some(CardRuntime::PullRequests(_)) => workspace_widget::FocusedPane::Review,
            _ => workspace_widget::FocusedPane::Other,
        }
    }

    /// [`Self::handle_pull_request_review`] without the modal gate, for
    /// actions the shell itself originates: restoring a card's pull request
    /// and opening one from the sidebar are not keystrokes aimed at a dialog.
    fn apply_pull_request_review(
        &mut self,
        card: CardId,
        action: pull_request_review::Action,
    ) -> Task<Message> {
        let action = match action {
            pull_request_review::Action::Select(number) => {
                let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card) else {
                    return Task::none();
                };
                let Some(target) = pane.review.listed_target(number).or_else(|| {
                    pane.project(&self.sidebar_model)
                        .and_then(|project| self.pull_request_target(project, number))
                }) else {
                    return Task::none();
                };
                pull_request_review::Action::Open(target)
            }
            action => action,
        };
        let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        let mut tasks = vec![
            pane.review
                .update(action)
                .map(move |action| Message::PullRequestReview { card, action })
                .map(cosmic::Action::App),
        ];
        let mut selection: Option<Option<u64>> = None;
        let mut links = Vec::new();
        let mut checkout = None;
        for effect in pane.review.take_effects() {
            match effect {
                pull_request_review::Effect::OpenFile {
                    path,
                    old_path,
                    basis,
                } => tasks.push(repository_task(pane.runtime.update(
                    RepositoryAction::OpenCommittedFile {
                        card,
                        path,
                        old_path,
                        basis,
                    },
                ))),
                pull_request_review::Effect::NextChange => tasks.push(repository_task(
                    pane.runtime.update(RepositoryAction::NextChange { card }),
                )),
                pull_request_review::Effect::PreviousChange => tasks.push(repository_task(
                    pane.runtime
                        .update(RepositoryAction::PreviousChange { card }),
                )),
                pull_request_review::Effect::CloseFile => tasks.push(repository_task(
                    pane.runtime.update(RepositoryAction::BackToFiles { card }),
                )),
                pull_request_review::Effect::RevealFileDiscussion => tasks.push(repository_task(
                    pane.runtime
                        .update(RepositoryAction::RevealDiffTop { card }),
                )),
                pull_request_review::Effect::OpenUrl(url) => links.push(url),
                pull_request_review::Effect::Checkout(target) => {
                    checkout = pane
                        .project(&self.sidebar_model)
                        .map(|project| (project, target));
                }
                pull_request_review::Effect::SelectionChanged(number) => {
                    selection = Some(number);
                }
            }
        }
        if let Some(number) = selection
            && let Some(state) = self.card_state_mut(card)
            && let CardKind::PullRequests(view) = &mut state.kind
            && view.selected_number != number
        {
            view.selected_number = number;
            self.persist();
        }
        for url in links {
            tasks.push(self.open_clicked_link(url));
        }
        if let Some((project_id, target)) = checkout {
            tasks.push(self.open_worktree_dialog(
                project_id,
                target.repository.clone(),
                WorktreeDraft {
                    base: target.head.as_str().to_owned(),
                    ..WorktreeDraft::branch(target.suggested_branch())
                },
                None,
            ));
        }
        Task::batch(tasks)
    }

    /// The pull request card: the project's open pull requests, or the
    /// review of the chosen one, beside the file the review selected.
    fn pull_request_card_view<'a>(
        &'a self,
        id: CardId,
        pane: &'a PullRequestPane,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let modal_open = self.modal_is_open();
        let review = &pane.review;
        let mut side: Vec<Element<'a, pull_request_review::Action>> = Vec::new();
        if review.is_open() {
            side.push(review.header(modal_open));
            side.push(design::horizontal_rule());
            side.push(review.summary(modal_open));
            side.push(review.notice(modal_open));
            side.push(review.file_list(modal_open));
        } else {
            let heading = pane
                .project(&self.sidebar_model)
                .and_then(|id| self.sidebar_model.project(id))
                .map(|project| project.name.clone())
                .unwrap_or_else(|| "Pull requests".to_owned());
            side.push(
                widget::container(
                    widget::row::with_children(vec![
                        crate::ui_controls::symbolic_icon("pull-request-symbolic")
                            .icon()
                            .size(design::ROW_ICON_SIZE)
                            .into(),
                        design::heading(heading)
                            .wrapping(iced::widget::text::Wrapping::None)
                            .ellipsize(iced::widget::text::Ellipsize::End(
                                EllipsizeHeightLimit::Lines(1),
                            ))
                            .width(Length::Fill)
                            .into(),
                    ])
                    .spacing(spacing.space_xxs)
                    .align_y(Alignment::Center),
                )
                .height(Length::Fixed(design::HEADER_CONTENT_HEIGHT))
                .padding([0, spacing.space_xs])
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into(),
            );
            side.push(design::horizontal_rule());
            side.push(review.browser(modal_open));
        }
        let side: Element<'a, pull_request_review::Action> = widget::container(
            widget::column::with_children(side)
                .spacing(0)
                .height(Length::Fill),
        )
        .width(Length::Fixed(design::NAVIGATION_DEFAULT_WIDTH))
        .height(Length::Fill)
        .into();
        let side = side.map(move |action| Message::PullRequestReview { card: id, action });

        let mut main: Vec<Element<'a, Message>> = Vec::new();
        let file_comment = review
            .file_comment_control(modal_open, pane.runtime.can_show_file_discussion())
            .map(|control| {
                crate::ui_controls::toolbar_actions(vec![control])
                    .map(move |action| RepositoryAction::Review { card: id, action })
            });
        if let Some(toolbar) =
            repository_widget::selected_file_toolbar_with(id, &pane.runtime, file_comment)
        {
            main.push(toolbar.map(Message::Repository));
            main.push(design::horizontal_rule());
        }
        if review.selected().is_some() {
            main.push(
                repository_widget::view_with_review(id, &pane.runtime, Some(review))
                    .map(Message::Repository),
            );
        } else {
            let (title, detail) = if review.is_open() {
                (
                    "Choose a file",
                    "Pick a changed file to read its diff. The shortcuts move between files and mark them viewed.",
                )
            } else {
                (
                    "Choose a pull request",
                    "Pick an open pull request to fetch its changes and review them here.",
                )
            };
            main.push(
                widget::container(
                    widget::column::with_children(vec![
                        crate::ui_controls::symbolic_icon("pull-request-symbolic")
                            .icon()
                            .size(32)
                            .into(),
                        widget::text::title3(title).into(),
                        design::body(detail)
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .align_x(Alignment::Center)
                            .into(),
                    ])
                    .align_x(Alignment::Center)
                    .spacing(spacing.space_xs)
                    .max_width(420),
                )
                .center(Length::Fill)
                .padding(spacing.space_m)
                .into(),
            );
        }
        widget::row::with_children(vec![
            side,
            design::vertical_rule(),
            widget::column::with_children(main)
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        ])
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn repository_runtime(&self, view: RepositoryView) -> RepositoryRuntime {
        if self
            .sidebar_model
            .project(view.project)
            .is_some_and(|project| project.kind == ProjectKind::Folder)
        {
            RepositoryRuntime::new_directory(view)
        } else {
            RepositoryRuntime::new(view)
        }
    }

    fn folder_scope(&self, key: &ActiveWorktreeKey) -> bool {
        self.sidebar_model
            .project(key.project_id)
            .is_some_and(|project| project.folder_key().as_ref() == Some(key))
    }

    fn active_folder_scope(&self) -> bool {
        self.sidebar_state
            .active_worktree()
            .is_some_and(|key| self.folder_scope(key))
    }

    fn add_repository_card(
        &mut self,
        project: ProjectId,
        repository: PathBuf,
        worktree: PathBuf,
    ) -> Task<Message> {
        let card_id = CardId::new();
        let view = RepositoryView {
            selected_file: None,
            project,
            repository,
            worktree: worktree.clone(),
        };
        let title = worktree
            .file_name()
            .and_then(|name| normalize_card_name(&name.to_string_lossy()))
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
        self.register_channel_card(card_id);
        let mut runtime = self.repository_runtime(view);
        runtime.set_auto_save(self.settings.editor_auto_save);
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        runtime.set_syntax_theme(self.settings.editor_syntax_theme);
        runtime.set_external_editors(self.sidebar_model.external_editors.clone());
        let load = repository_task(runtime.initial_load(card_id));
        self.cards
            .insert(card_id, CardRuntime::Repository(Box::new(runtime)));
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
        Task::batch([self.apply_viewport(update, true, false), load])
    }
    fn open_link(&mut self, url: Url) -> Task<Message> {
        self.open_link_in(url, self.settings.link_open_destination)
    }

    fn open_link_in(&mut self, url: Url, destination: LinkOpenDestination) -> Task<Message> {
        if url.scheme() == "file" {
            return self.open_document_link(self.workspace.focused(), url.as_str());
        }
        match destination {
            LinkOpenDestination::IntegratedBrowser => self.add_browser(url),
            LinkOpenDestination::SystemDefault => cosmic::task::future(async move {
                Message::ExternalLinkOpened(open_system_url(url).await)
            }),
        }
    }

    fn open_clicked_link(&mut self, url: Url) -> Task<Message> {
        let gesture =
            self.keyboard_modifiers
                .control()
                .then_some(if self.keyboard_modifiers.shift() {
                    LinkOpenGesture::ControlShiftClick
                } else {
                    LinkOpenGesture::ControlClick
                });
        self.open_link_in(url, self.settings.link_destination(gesture))
    }

    #[cfg(feature = "integrated-browser")]
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
            match spawn_browser(
                &session,
                true,
                effective_surface_scale(self.window_scale_factor, self.settings.ui_scale),
                Some(factory),
            ) {
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
            title: url
                .host_str()
                .and_then(normalize_card_name)
                .unwrap_or_else(|| "Browser".into()),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating {
                epoch: INITIAL_HYDRATION_EPOCH,
            },
            kind: CardKind::Browser(session),
        });
        durable.focused = Some(card_id);
        self.register_channel_card(card_id);
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

    #[cfg(not(feature = "integrated-browser"))]
    fn add_browser(&mut self, url: Url) -> Task<Message> {
        Self::open_external_url(url)
    }

    #[cfg(not(feature = "integrated-browser"))]
    fn open_external_url(url: Url) -> Task<Message> {
        cosmic::task::future(async move { Message::ExternalUrlOpened(open_system_url(url).await) })
    }

    fn persist(&mut self) {
        self.snapshot_workspace_scope();
        let saved = self.persistent_workspace();
        if let Some(session) = &self.session {
            session.snapshot(saved);
        }
    }
    fn commit_chat_session(&mut self, card: CardId, session: crate::chat::Session) {
        match self.install_chat_session(card, session) {
            Some(true) => {
                let saved = self.persistent_workspace();
                if let Some(writer) = &self.session {
                    writer.snapshot_conversion(saved);
                }
            }
            Some(false) => self.persist(),
            None => {}
        }
    }

    /// Puts `session` in card `card`'s durable state without saving it.
    /// `None` when the card already holds exactly that session; otherwise
    /// whether the card was converted from another kind.
    fn install_chat_session(
        &mut self,
        card: CardId,
        session: crate::chat::Session,
    ) -> Option<bool> {
        let unchanged = self.card_state(card).is_some_and(
            |state| matches!(&state.kind, CardKind::Chat(current) if current == &session),
        );
        // A successful attachment completes hydration even when the provider
        // has not enriched the already-saved descriptor yet.
        self.lifecycle.remove(card);
        if let Some(state) = self.card_state_mut(card) {
            state.residency = Residency::Hot;
        }
        if unchanged {
            return None;
        }
        let conversion = self
            .card_state(card)
            .is_some_and(|state| !matches!(state.kind, CardKind::Chat(_)));
        if let Some(state) = self.card_state_mut(card) {
            // A tab still carrying the default title names the provider it was
            // opened with; a provider switch on a fresh chat renames it so the
            // tab does not keep naming an agent it no longer talks to.
            if let CardKind::Chat(current) = &state.kind {
                let old = crate::app::chat_ui::chat_card_title(current.launch.provider);
                if current.launch.provider != session.launch.provider && state.title == old {
                    state.title =
                        crate::app::chat_ui::chat_card_title(session.launch.provider).into();
                }
            }
            state.kind = CardKind::Chat(session);
            state.residency = Residency::Hot;
        }
        self.card_chrome.remove(&card);
        Some(conversion)
    }

    /// Puts a new conversation in chat card `card` in place of the one it
    /// holds: another provider or account for a thread that has not begun, or
    /// a fresh start after a failed resume. Callers pass only a conversation
    /// that has not begun, which is all storage accepts this way.
    ///
    /// Storage refuses a snapshot that re-points a chat card at another
    /// conversation, so the replacement is recorded explicitly, together with
    /// the snapshot that shows it, the way a terminal relaunch records its new
    /// identity.
    fn replace_chat_conversation(&mut self, card: CardId, session: crate::chat::Session) {
        let expected = match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(current)) if current != &session => current.id,
            _ => return self.commit_chat_session(card, session),
        };
        if self.install_chat_session(card, session.clone()).is_none() {
            return;
        }
        self.snapshot_workspace_scope();
        let saved = self.persistent_workspace();
        if let Some(writer) = &self.session {
            writer.replace_chat_identity(card, expected, session, saved);
        }
    }

    fn finish_chat_terminal_handoff(
        &mut self,
        card: CardId,
        session: TerminalSession,
    ) -> Task<Message> {
        let Some(state) = self
            .card_state(card)
            .filter(|state| state.existence.is_open())
            .cloned()
        else {
            return Task::none();
        };
        let residency = Residency::Hydrating {
            epoch: unix_millis().max(1) as u64,
        };
        self.lifecycle
            .register(card, state.exposure, residency.clone(), Instant::now());
        if let Some(state) = self.card_state_mut(card) {
            state.kind = CardKind::Terminal(session.clone());
            state.residency = residency;
        }
        self.closed_terminal_cards.remove(&card);
        self.cards.insert(card, CardRuntime::Loading);
        self.card_chrome.remove(&card);
        let saved = self.persistent_workspace();
        if let Some(writer) = &self.session {
            writer.snapshot_conversion(saved);
        }
        self.pending_terminal_handoffs
            .insert(session.id, session.clone());
        let commit = self.commit_terminal_handoffs();
        let attach = self.start_terminal(
            card,
            session,
            state.exposure == Exposure::Visible && self.workspace_surfaces_visible(),
            false,
        );
        Task::batch([commit, attach])
    }

    /// The flush is ordered after conversion snapshots. Never release daemon
    /// replay evidence based only on receiving or attaching its replacement.
    fn commit_terminal_handoffs(&mut self) -> Task<Message> {
        if self.terminal_handoff_commit_inflight
            || self.pending_terminal_handoffs.is_empty()
            || self.save_health.as_ref().is_some_and(|health| {
                !matches!(
                    health.health,
                    SessionSaveHealth::Healthy | SessionSaveHealth::Recovered { .. }
                ) || health.window_state_refused()
            })
        {
            return Task::none();
        }
        let Some(writer) = &self.session else {
            return Task::none();
        };
        let Ok(flush) = writer.request_flush() else {
            return Task::none();
        };
        let terminals = self
            .pending_terminal_handoffs
            .values()
            .cloned()
            .collect::<Vec<_>>();
        self.terminal_handoff_commit_inflight = true;
        cosmic::task::future(async move {
            let joined = tokio::task::spawn_blocking(move || {
                flush
                    .recv_timeout(Duration::from_secs(30))
                    .map_err(|error| format!("session flush: {error}"))??;
                let results = crate::chat::client::acknowledge_terminal_handoffs(&terminals);
                Ok(terminals.into_iter().zip(results).collect())
            })
            .await;
            let result = joined
                .unwrap_or_else(|error| Err(format!("handoff acknowledgement worker: {error}")));
            Message::TerminalHandoffsCommitted { result }
        })
    }

    fn agent_worktree(&self, card: CardId) -> Option<ActiveWorktreeKey> {
        let state = self
            .card_state(card)
            .filter(|state| state.existence.is_open())?;
        if !matches!(state.kind, CardKind::Terminal(_) | CardKind::Chat(_)) {
            return None;
        }
        self.sidebar_model.worktree_at(self.card_worktree(state)?)
    }

    /// Whether `project` holds the scope the workspace shows: the active
    /// channel's project, or the one with a worktree at or above `active`,
    /// which is either a worktree root or a directory no known root owned.
    fn project_owns_active_scope(&self, project: ProjectId, active: &Path) -> bool {
        if let Some(channel) = self.durable.active_channel {
            return self
                .durable
                .channels
                .iter()
                .any(|known| known.id == channel && known.project_id == project);
        }
        self.sidebar_model.project(project).is_some_and(|project| {
            active.starts_with(&project.repository)
                || project
                    .worktrees
                    .iter()
                    .any(|worktree| active.starts_with(&worktree.path))
        })
    }

    fn pane_is_active(&self, card: CardId) -> bool {
        self.workspace.focused() == Some(card)
            && self.workspace_surfaces_visible()
            && !self.modal_is_open()
    }

    pub(super) fn observe_agent_update_for_card(&mut self, card: CardId) {
        if self.pane_is_active(card)
            || !self
                .card_state(card)
                .is_some_and(|state| state.existence.is_open())
        {
            return;
        }
        if let Some(key) = self.agent_worktree(card) {
            self.sidebar_state
                .observe_agent_update(card, key.project_id, key.worktree_id);
        } else {
            self.sidebar_state.observe_card_update(card);
        }
    }

    pub(super) fn observe_agent_activity_for_card(
        &mut self,
        card: CardId,
        activity: Option<crate::agent::Activity>,
    ) {
        let previous = self
            .sidebar_state
            .card_activity(card)
            .map(|activity| activity.state);
        let next = activity.map(|activity| activity.state);
        if (next == Some(crate::agent::ActivityState::Waiting) && previous != next)
            || (previous == Some(crate::agent::ActivityState::Working)
                && next == Some(crate::agent::ActivityState::Idle))
        {
            self.observe_agent_update_for_card(card);
        }
        // A turn that ends has often pushed a branch or opened a pull request;
        // one that stops to ask for permission has not finished anything.
        let finished = previous == Some(crate::agent::ActivityState::Working)
            && !matches!(
                next,
                Some(crate::agent::ActivityState::Working | crate::agent::ActivityState::Waiting)
            );
        // Every terminal wake lands here, so a plain shell resolves nothing.
        let worktree = (finished || activity.is_some())
            .then(|| self.agent_worktree(card))
            .flatten();
        if finished
            && let Some(project) = worktree.as_ref().map(|key| key.project_id).or_else(|| {
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card))
                    .map(|channel| channel.project_id)
            })
        {
            self.refresh_ui.mark_stale(project);
        }
        if let Some((key, activity)) = worktree.zip(activity) {
            self.sidebar_state
                .set_agent_activity(card, key.project_id, key.worktree_id, activity);
        } else if let Some(activity) = activity {
            self.sidebar_state.set_card_activity(card, activity);
        } else {
            self.sidebar_state.clear_agent_activity(card);
        }
    }

    fn acknowledge_active_pane(&mut self) -> Task<Message> {
        let Some(card) = self
            .workspace
            .focused()
            .filter(|&card| self.pane_is_active(card))
        else {
            return Task::none();
        };
        if !self.sidebar_state.has_card_update(card) {
            return Task::none();
        }
        self.sidebar_state.acknowledge_agent_update(card);
        if let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
            && let Err(error) = handle.acknowledge_updates()
        {
            self.status = Some(TransientNotice::error(format!(
                "Could not acknowledge terminal output: {error}"
            )));
        }
        self.acknowledge_chat_updates(card)
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
    /// empty string, and a title of nothing but formatting normalises to one.
    /// Storing it would blank the card header, the card's accessible name and
    /// the window title all at once, so the card keeps the name it already had.
    fn apply_reported_title(&mut self, id: CardId, title: String) -> bool {
        let Some(title) = normalize_card_name(&title) else {
            return false;
        };
        let _ = self.workspace.rename_card(id, title.clone());
        if self
            .card_state(id)
            .is_some_and(|state| state.title != title)
            && let Some(state) = self.card_state_mut(id)
        {
            state.title = title;
            return true;
        }
        false
    }

    /// Builds chrome for cards that appeared or were renamed, drops entries for
    /// cards the workspace no longer holds, and renames the window.
    fn sync_chrome(&mut self) -> Task<Message> {
        for card in &self.durable.cards {
            let type_label = card_kind_labels(Some(&card.kind)).type_label;
            let (title, file_caption) = self.repository_chrome(card);
            let caption = match &card.kind {
                CardKind::Terminal(session) if self.relaunched_terminals.contains(&session.id) => {
                    Cow::Owned(format!(
                        "Relaunched after upgrade · {}",
                        card_kind_caption(&card.kind)
                    ))
                }
                _ => file_caption.unwrap_or_else(|| card_kind_caption(&card.kind)),
            };
            if self
                .card_chrome
                .get(&card.id)
                .is_none_or(|chrome| !chrome.describes(type_label, &title, &caption))
            {
                let chrome = CardChrome::new(
                    card.id,
                    type_label,
                    &title,
                    &caption,
                    &self.settings.keyboard_shortcuts,
                );
                self.card_chrome.insert(card.id, chrome);
            }
        }
        if self.card_chrome.len() != self.durable.cards.len() {
            self.card_chrome
                .retain(|id, _| self.durable.cards.iter().any(|card| card.id == *id));
        }
        self.sync_window_title()
    }

    /// Present the current editor file without overwriting a saved pane name.
    /// The full path in the tooltip distinguishes files sharing a basename.
    fn repository_chrome<'a>(
        &'a self,
        card: &'a CardState,
    ) -> (Cow<'a, str>, Option<Cow<'a, str>>) {
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card.id)
            && let Some(path) = runtime.selected_path()
            && let Some(name) = path.file_name()
        {
            return (
                name.to_string_lossy(),
                Some(Cow::Owned(
                    display_path(&runtime.descriptor.worktree.join(path)).into_owned(),
                )),
            );
        }
        (Cow::Borrowed(&card.title), None)
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
        self.sidebar_state.clear_agent_activity(card);
        self.retire_lsp_card(card);
        match self.cards.remove(&card) {
            Some(CardRuntime::Terminal(_handle)) => {
                // A failed or unreachable surface is not authority to terminate its PTY.
            }
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) => {
                let _ = browser.handle.set_visible(false);
                self.begin_browser_teardown(browser.handle);
            }
            Some(
                CardRuntime::Loading
                | CardRuntime::Failed(_)
                | CardRuntime::Completed
                | CardRuntime::Repository(_)
                | CardRuntime::Chat(_)
                | CardRuntime::PullRequests(_)
                | CardRuntime::Settings
                | CardRuntime::Accounts
                | CardRuntime::Automations,
            )
            | None => {}
        }
        self.cards
            .insert(card, CardRuntime::Failed(message.clone()));
        self.mark_runtime_failed(card, message);
    }

    /// Retires a terminal whose process ended, keeping its surface.
    ///
    /// Every end is retired the same way, clean or not. A process that died is
    /// not a runtime that failed: replacing the card with a failure surface
    /// would throw away the scrollback that says why it died, which is exactly
    /// what the reader wants after an unclean exit.
    fn retire_ended_terminal(
        &mut self,
        card: CardId,
        exit_code: u32,
        signal: Option<&str>,
        intentional: bool,
    ) {
        if let Some(state) = self.card_state_mut(card)
            && let CardKind::Terminal(session) = &mut state.kind
        {
            transition_terminal_to_relaunchable(session, terminal_exit(exit_code, signal));
        }
        self.lifecycle.remove(card);
        self.sidebar_state.clear_agent_activity(card);
        if let Some(state) = self.card_state_mut(card) {
            state.residency = Residency::Hibernated;
        }
        if !intentional && (exit_code != 0 || signal.is_some()) {
            self.status = Some(TransientNotice::error(match signal {
                Some(signal) => format!("terminal exited with {signal}"),
                None => format!("terminal exited with status {exit_code}"),
            }));
        }
    }

    fn completed_terminal_has_native_owner(&self, card: CardId) -> bool {
        // Only a clean finish hands the card to a native conversation. A
        // crashed agent has nothing to open; its card offers a restart.
        completed_terminal(self.card_state(card)).is_some_and(|session| {
            self.chat_has_parked_runtime(card)
                || chat_ui::conversion(session).is_ok_and(|chat| chat.owns_codex_listener())
        })
    }

    fn retry_card(&mut self, card: CardId) -> Task<Message> {
        // A card whose close is already accepted never starts anything again.
        if self
            .card_state(card)
            .is_none_or(|state| !state.existence.is_open())
            || self.chat_start_pending(card)
            || self.refuse_for_deleting_card(card)
        {
            return Task::none();
        }
        if self.completed_terminal_has_native_owner(card) {
            return self.handle_chat_ui(chat_ui::Action::Switch(card));
        }
        if matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        {
            return Task::none();
        }
        let Some(mut state) = self
            .durable
            .cards
            .iter()
            .find(|state| state.id == card && state.existence.is_open())
            .cloned()
        else {
            return Task::none();
        };
        #[cfg(not(feature = "integrated-browser"))]
        if let CardKind::Browser(session) = &state.kind {
            return Self::open_external_url(session.current_url.clone());
        }
        // A utility pane starts nothing, so a retry only reinstalls its view.
        if state.kind.is_utility() {
            self.cards.insert(card, CardRuntime::utility(&state.kind));
            return Task::none();
        }
        let terminal_replacement = if let CardKind::Terminal(session) = &mut state.kind
            && session.continuity.live_proof().is_none()
        {
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

        let surfaces_visible = self.workspace_surfaces_visible();
        match state.kind {
            CardKind::Terminal(session) => {
                self.terminal_recovery_attempts.remove(&session.id);
                self.cards.insert(card, CardRuntime::Loading);
                self.start_terminal(
                    card,
                    session,
                    state.exposure == Exposure::Visible && surfaces_visible,
                    true,
                )
            }
            #[cfg(feature = "integrated-browser")]
            CardKind::Browser(session) => {
                self.cards.insert(card, CardRuntime::Loading);
                let Some(factory) = self.browser_factory.clone() else {
                    return cosmic::task::message(Message::InitializeBrowserFactory);
                };
                match spawn_browser(
                    &session,
                    state.exposure == Exposure::Visible && surfaces_visible,
                    effective_surface_scale(self.window_scale_factor, self.settings.ui_scale),
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
            #[cfg(not(feature = "integrated-browser"))]
            CardKind::Browser(_) => {
                unreachable!("browserless retries return before mutating durable state")
            }
            CardKind::Repository(view) => {
                let mut runtime = self.repository_runtime(view);
                runtime.set_auto_save(self.settings.editor_auto_save);
                runtime.set_wrap_lines(self.settings.editor_wrap_lines);
                runtime.set_syntax_theme(self.settings.editor_syntax_theme);
                runtime.set_external_editors(self.sidebar_model.external_editors.clone());
                let task = repository_task(runtime.initial_load(card));
                self.cards
                    .insert(card, CardRuntime::Repository(Box::new(runtime)));
                task
            }
            CardKind::PullRequests(view) => {
                let pane = self.pull_request_pane(&view);
                self.cards
                    .insert(card, CardRuntime::PullRequests(Box::new(pane)));
                self.open_pending_pull_request(card)
            }
            CardKind::Chat(session) => self.open_chat(card, session, crate::chat::Intent::Restore),
            CardKind::Settings | CardKind::Accounts | CardKind::Automations => {
                unreachable!("utility panes return before a retry registers them")
            }
        }
    }

    fn begin_terminal_teardown(&mut self, handle: TerminalSurface) {
        self.retire_terminal_surface(handle, true);
    }

    /// Drop a terminal surface off the UI thread.
    ///
    /// `terminate` sends `ShutdownTerminal`, which kills the PTY child. A live
    /// Codex switch must not: native chat is attached to that same child, and
    /// the CLI view is the process still sitting on the PTY. Releasing the
    /// surface only detaches this client.
    fn retire_terminal_surface(&mut self, handle: TerminalSurface, terminate: bool) {
        let mut index = 0;
        while index < self.terminal_teardowns.len() {
            if self.terminal_teardowns[index].is_finished() {
                let finished = self.terminal_teardowns.swap_remove(index);
                let _ = finished.join();
            } else {
                index += 1;
            }
        }

        if terminate {
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
            return;
        }

        // Both clones go to the reaper. Dropping one on the UI thread can win
        // the race and join the client worker there, which stalls the frame.
        let companion = handle.clone();
        match std::thread::Builder::new()
            .name("cagents-terminal-reaper".into())
            .spawn(move || {
                handle.release();
                drop(companion);
            }) {
            Ok(join) => self.terminal_teardowns.push(join),
            Err(error) => {
                // Spawn failure drops the closure, and with it both clones.
                tracing::error!(
                    %error,
                    "terminal detach reaper failed; the client detached on the UI thread"
                );
            }
        }
    }

    /// The id of the column title button.
    ///
    /// A terminal surface owns `card_focus_id` for as long as it is mounted.
    /// Chat keeps the title id rather than taking that focus id on the frame
    /// the surface unmounts. The runtime restores named widget state by id
    /// before it looks at the widget type, so the title button would draw the
    /// terminal's state and abort (`Downcast widget state`).
    fn card_header_widget_id(&self, card: CardId) -> widget::Id {
        let title = matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Terminal(_) | CardKind::Chat(_))
        );
        if let Some(chrome) = self.card_chrome.get(&card) {
            if title {
                chrome.title_id.clone()
            } else {
                chrome.focus_id.clone()
            }
        } else if title {
            card_control_id(card, "title")
        } else {
            workspace_widget::card_focus_id(card)
        }
    }

    /// Where keyboard focus for `card` lands.
    fn card_keyboard_focus(&self, card: CardId) -> widget::Id {
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(_)) => self
                .card_chrome
                .get(&card)
                .map(|chrome| chrome.focus_id.clone())
                .unwrap_or_else(|| workspace_widget::card_focus_id(card)),
            Some(CardRuntime::Chat(runtime)) => card_control_id(card, runtime.focus_control()),
            // Arriving at settings means looking for one, so the keyboard
            // lands in its search field.
            Some(CardRuntime::Settings) => crate::settings_widgets::search_id(),
            _ => self.card_header_widget_id(card),
        }
    }

    #[cfg(feature = "integrated-browser")]
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
        workspace_widget::scroll_to(self.workspace_scroll_id.clone(), offset)
            .map(cosmic::Action::App)
    }

    fn reorder_pane(&mut self, card: CardId, before: Option<CardId>) -> Task<Message> {
        if self.workspace_inputs_blocked() {
            return Task::none();
        }
        let order: Vec<_> = self.workspace.order().collect();
        let Some(destination) = crate::reorder::destination(&order, card, before) else {
            return Task::none();
        };
        if order.get(destination) == Some(&card) {
            return Task::none();
        }
        let Ok(update) = self.workspace.reorder(card, destination) else {
            return Task::none();
        };
        let order: Vec<_> = self.workspace.order().collect();
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(source) = durable.cards.iter().position(|state| state.id == card) {
            let moved = durable.cards.remove(source);
            let insertion = before
                .and_then(|id| durable.cards.iter().position(|state| state.id == id))
                .or_else(|| {
                    order.iter().rev().find(|id| **id != card).and_then(|id| {
                        durable
                            .cards
                            .iter()
                            .position(|state| state.id == *id)
                            .map(|index| index + 1)
                    })
                })
                .unwrap_or(durable.cards.len());
            durable.cards.insert(insertion, moved);
        }
        if let Some(channel) = durable
            .channels
            .iter_mut()
            .find(|channel| Some(channel.id) == durable.active_channel)
        {
            channel.cards.sort_by_key(|id| {
                order
                    .iter()
                    .position(|candidate| candidate == id)
                    .unwrap_or(usize::MAX)
            });
        }
        // Runtime objects and focus never move out of their ID-indexed maps.
        self.apply_viewport(update, true, true)
    }

    fn apply_viewport(
        &mut self,
        update: ViewportUpdate,
        synchronize_scroll: bool,
        additional_durable_change: bool,
    ) -> Task<Message> {
        let context_overlay_task = self.sync_context_overlay_state();
        let focused = self.workspace.focused();
        let focus_changed = self.durable.focused != focused;
        let viewport_durable_changed =
            durable_viewport_update_pending(&self.durable, focused, &update.transitions)
                && apply_durable_viewport_update(
                    Arc::make_mut(&mut self.durable),
                    focused,
                    &update.transitions,
                );
        let durable_changed = additional_durable_change || viewport_durable_changed;
        let mut tasks = Vec::new();
        tasks.push(self.acknowledge_active_pane());
        if focus_changed {
            tasks.push(self.reveal_active_tab());
        }
        // Every focus move lands here, including those whose handlers return
        // early and so skip the check at the end of `update`.
        tasks.push(self.flush_settings_on_blur());
        let mut schedule_delayed_sweep = false;
        let mut reconcile_now = false;
        let surfaces_visible = self.workspace_surfaces_visible();
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
                Some(CardRuntime::Chat(runtime)) => {
                    let _ = runtime
                        .set_visible(transition.to == VisibilityClass::Visible && surfaces_visible);
                }
                #[cfg(feature = "integrated-browser")]
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
        if let Some(card) = self.workspace.take_widget_focus_request()
            && !self.context_inspector_overlay_active
        {
            tasks.push(
                iced::widget::operation::focus(self.card_keyboard_focus(card))
                    .map(cosmic::Action::App),
            );
        }
        tasks.push(context_overlay_task);
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

    /// Arms a timer for `deadline`, superseding any timer already pending.
    fn arm_lifecycle_sweep(&mut self, deadline: Instant) -> Task<Message> {
        self.lifecycle_sweep_deadline = Some(deadline);
        self.lifecycle_sweep_generation = self.lifecycle_sweep_generation.wrapping_add(1);
        lifecycle_sweep_timer(
            deadline.saturating_duration_since(Instant::now()),
            self.lifecycle_sweep_generation,
        )
    }

    /// Brings the debounced sweep forward to `now + delay`, if that is sooner
    /// than the wakeup already pending.
    ///
    /// Callers report when one card's grace expires, so the sweep has to run at
    /// the earliest such time: a deadline that only ever moved later would let
    /// a steady stream of newly offscreen cards postpone the oldest card's
    /// eviction indefinitely. Sweeping early costs nothing, because whatever a
    /// sweep leaves un-evicted re-arms itself from the manager.
    fn request_lifecycle_sweep(&mut self, delay: Duration) -> Task<Message> {
        let Some(requested) = Instant::now().checked_add(delay) else {
            // A configured delay this large can never elapse anyway.
            return Task::none();
        };
        if sweep_needs_rearming(self.lifecycle_sweep_deadline, requested) {
            self.arm_lifecycle_sweep(requested)
        } else {
            Task::none()
        }
    }

    fn lifecycle_sweep_elapsed(&mut self, generation: u64) -> Task<Message> {
        if generation != self.lifecycle_sweep_generation {
            // Superseded by a later arming, whose own delivery is still to come.
            return Task::none();
        }
        // A matching generation carries the deadline it was armed with, because
        // `arm_lifecycle_sweep` writes both together, so neither branch below
        // is reachable through the normal path. They are guards, not flow: a
        // sweep that ran without a deadline has nothing to do, and a timer that
        // woke marginally early must wait out the grace rather than cut it.
        let Some(deadline) = self.lifecycle_sweep_deadline else {
            return Task::none();
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            return self.arm_lifecycle_sweep(deadline);
        }
        self.lifecycle_sweep_deadline = None;
        let sweep = self.lifecycle_sweep();
        // That reconcile made the manager's exposure current, so the next
        // wakeup can be placed exactly when the earliest card still holding
        // its grace exhausts it.
        Task::batch([sweep, self.reschedule_offscreen_lifecycle_sweep()])
    }

    fn lifecycle_sweep(&mut self) -> Task<Message> {
        let mut visible = HashSet::new();
        let mut prefetch = HashSet::new();
        for (id, visibility) in self.workspace.mounted_visibility() {
            // A card in a worktree being deleted is not brought up: its
            // session is being closed, and one started now would outlive the
            // directory it runs in.
            if self
                .card_state(id)
                .is_some_and(|card| self.project_ui.deleting_card(card))
            {
                continue;
            }
            if visibility == VisibilityClass::Visible {
                visible.insert(id);
            } else if visibility == VisibilityClass::Overscan {
                let browser = match self.cards.get(&id) {
                    Some(CardRuntime::Terminal(_))
                    | Some(CardRuntime::Repository(_))
                    | Some(CardRuntime::PullRequests(_))
                    | Some(CardRuntime::Settings)
                    | Some(CardRuntime::Accounts)
                    | Some(CardRuntime::Automations) => false,
                    #[cfg(feature = "integrated-browser")]
                    Some(CardRuntime::Browser(_)) => true,
                    _ => self
                        .card_state(id)
                        .is_some_and(|card| matches!(card.kind, CardKind::Browser(_))),
                };
                if !browser {
                    prefetch.insert(id);
                }
            }
        }
        self.reconcile_lifecycle(&visible, &prefetch)
    }

    fn reconcile_lifecycle(
        &mut self,
        visible: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
    ) -> Task<Message> {
        let now = Instant::now();
        let surfaces_visible = self.workspace_surfaces_visible();
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
                            false,
                        ));
                        continue;
                    }
                    #[cfg(feature = "integrated-browser")]
                    let mut deferred_browser_started = false;
                    #[cfg(not(feature = "integrated-browser"))]
                    let deferred_browser_started = false;
                    #[cfg(feature = "integrated-browser")]
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
                    #[cfg(feature = "integrated-browser")]
                    if let Some(session) = deferred_browser {
                        let Some(factory) = self.browser_factory.clone() else {
                            tasks.push(cosmic::task::message(Message::InitializeBrowserFactory));
                            continue;
                        };
                        match spawn_browser(
                            &session,
                            visible.contains(&card) && surfaces_visible,
                            effective_surface_scale(
                                self.window_scale_factor,
                                self.settings.ui_scale,
                            ),
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
                            #[cfg(feature = "integrated-browser")]
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
                        #[cfg(feature = "integrated-browser")]
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

    fn drain_terminal(&mut self, card: CardId, _generation: u64) -> Task<Message> {
        if self.chat_start_pending(card) {
            return Task::none();
        }
        let (proof, collected) = {
            let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                return Task::none();
            };
            let events = handle.events();
            (handle.host_proof(), events.try_iter().collect::<Vec<_>>())
        };
        let mut durable_changed = false;
        let mut recovery = None;
        if let Some(proof) = proof
            && ended_terminal(self.card_state(card)).is_none()
        {
            self.uncertain_terminal_proofs.remove(&card);
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
                TerminalEvent::AgentActivityChanged(activity) => {
                    self.observe_agent_activity_for_card(card, activity);
                }
                TerminalEvent::TitleChanged(title) => {
                    durable_changed |= self.apply_reported_title(card, title);
                }
                TerminalEvent::ConversationTitleChanged(title) => {
                    self.name_channel_from_prompt(card, &title);
                }
                TerminalEvent::ResumeHintChanged(hint) => {
                    if let Some(state) = self.card_state_mut(card)
                        && let CardKind::Terminal(session) = &mut state.kind
                        && session.resume_hint != hint
                    {
                        session.resume_hint = hint;
                        durable_changed = true;
                    }
                }
                TerminalEvent::Bell => {}
                // Root exit is not PTY completion: a descendant may still
                // accept input and produce output. Keep its live surface.
                TerminalEvent::Exited {
                    exit_code: 0,
                    signal: None,
                    ..
                } => {}
                TerminalEvent::Exited {
                    exit_code,
                    signal,
                    intentional,
                }
                | TerminalEvent::Completed {
                    exit_code,
                    signal,
                    intentional,
                } => {
                    self.retire_ended_terminal(card, exit_code, signal.as_deref(), intentional);
                    durable_changed = true;
                }
                TerminalEvent::Error { operation, message } => {
                    // Commands already in flight can lose the race with the
                    // process ending. They cannot turn an end into a failure.
                    if ended_terminal(self.card_state(card)).is_some() {
                        continue;
                    }
                    let message = format!("terminal {operation}: {message}");
                    if operation == "connect to cagents daemon"
                        && let Some(proof) = terminal_continuity(self.card_state(card))
                            .and_then(TerminalContinuity::live_proof)
                    {
                        self.uncertain_terminal_proofs.insert(card, proof);
                    }
                    if operation == "connect to cagents daemon"
                        && let Some(state) = self.card_state(card)
                        && let CardKind::Terminal(session) = &state.kind
                        && session.continuity.live_proof().is_some()
                    {
                        let session = session.clone();
                        let visible = state.exposure == Exposure::Visible
                            && self.workspace_surfaces_visible();
                        // Failing the runtime below forgets that it was
                        // hibernated, which decides how it recovers.
                        let hibernated = self
                            .lifecycle
                            .state(card)
                            .filter(|(_, residency)| **residency == Residency::Hibernated)
                            .map(|(exposure, _)| *exposure);
                        if self.terminal_recovery_attempts.insert(session.id) {
                            recovery = Some((session, visible, hibernated));
                        }
                    }
                    self.fail_runtime(card, message.clone());
                    durable_changed = true;
                    if recovery.is_none() {
                        self.status = Some(TransientNotice::error(message));
                    }
                }
            }
        }
        let activity = self.cards.get(&card).and_then(|runtime| match runtime {
            CardRuntime::Terminal(handle) => handle.agent_activity(),
            _ => None,
        });
        self.observe_agent_activity_for_card(card, activity);
        // Attention follows semantic agent transitions, not ordinary terminal output.
        if durable_changed {
            self.persist();
        }
        if let Some((session, visible, hibernated)) = recovery {
            // A hibernated agent kept attached for its row goes back to
            // waiting for its card: it may be attached again, but launching it
            // again is the user's decision, made by opening the card.
            if let Some(exposure) =
                hibernated.filter(|_| !visible && agent_ui::observes_terminal_agent(&session))
            {
                self.lifecycle
                    .register(card, exposure, Residency::Hibernated, Instant::now());
                if let Some(state) = self.card_state_mut(card) {
                    state.residency = Residency::Hibernated;
                }
                self.cards.insert(card, CardRuntime::Loading);
                self.restored_agent_attaches.push_back(card);
                return self.attach_next_restored_agent();
            }
            let residency = Residency::Hydrating {
                epoch: unix_millis().max(1) as u64,
            };
            if let Some(state) = self.card_state_mut(card) {
                state.residency = residency.clone();
            }
            self.lifecycle.register(
                card,
                if visible {
                    Exposure::Visible
                } else {
                    Exposure::Offscreen
                },
                residency,
                Instant::now(),
            );
            self.cards.insert(card, CardRuntime::Loading);
            return self.start_terminal(card, session, visible, false);
        }
        Task::none()
    }

    #[cfg(feature = "integrated-browser")]
    fn handle_browser_event(&mut self, id: CardId, event: BrowserEvent) -> Task<Message> {
        if !self.browser_import.offered
            && !self.modal_is_open()
            && matches!(
                event,
                BrowserEvent::LifecycleChanged(crate::browser::BrowserLifecycle::Visible)
            )
        {
            let _ = self.update_browser_import(browser_import_ui::Action::Open);
        }
        let mut tasks = Vec::new();
        let mut durable_changed = false;
        match event {
            BrowserEvent::UrlChanged(url) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&id) {
                    browser.address = BrowserAddress::location(&url);
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

    /// The workspace always fills its viewport. Only a lone native chat
    /// keeps a readable, centered surface within that full-width pane.
    fn workspace_card_view(
        &self,
        id: CardId,
        focused: bool,
        splitter: bool,
    ) -> Element<'_, Message> {
        if !self
            .card_state(id)
            .is_some_and(|card| matches!(card.kind, CardKind::Chat(_)))
        {
            return self.card_view(id, focused, splitter);
        }
        let preferred = self
            .workspace
            .card(id)
            .map_or(CardWidth::Fraction(0.5), |card| card.width);
        // Keep the same widget root when a neighbor opens or closes, so
        // composer selection, focus, and transcript state survive re-layout.
        let lone = self.workspace.cards().len() == 1;
        iced::widget::responsive(move |size| {
            let width = if lone {
                lone_chat_width(size.width, preferred)
            } else {
                size.width
            };
            widget::container(
                widget::container(self.card_view(id, focused, splitter))
                    .width(Length::Fixed(width))
                    .height(Length::Fill),
            )
            .center_x(Length::Fill)
            .height(Length::Fill)
            .into()
        })
        .into()
    }

    fn card_view<'a>(
        &'a self,
        id: CardId,
        focused: bool,
        show_resize_splitter: bool,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let card = self.card_state(id);
        let attention = self.sidebar_state.has_card_update(id) && !focused;
        let labels = card_kind_labels(card.map(|card| &card.kind));
        // `sync_chrome` keeps an entry for every durable card; a miss can
        // only be a card that is not durable, which renders degenerate chrome.
        let chrome = self.card_chrome.get(&id);
        #[cfg(feature = "integrated-browser")]
        let browser_runtime = match self.cards.get(&id) {
            Some(CardRuntime::Browser(browser)) => Some(browser),
            _ => None,
        };
        let browser_session = card.and_then(|card| match &card.kind {
            CardKind::Browser(session) => Some(session),
            _ => None,
        });

        #[cfg(feature = "integrated-browser")]
        let browser_toolbar: Option<Element<'a, Message>> = browser_session.map(|session| {
            let address = browser_runtime
                .map(|browser| browser.address.as_str())
                .or_else(|| chrome.map(|chrome| chrome.caption.as_str()))
                .unwrap_or(SanitizedLocation::INVALID);
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
                    crate::interaction::tooltip(
                        widget::button::icon(crate::ui_controls::symbolic_icon(
                            "go-previous-symbolic",
                        ))
                        .id(chrome.map_or_else(
                            || card_control_id(id, "browser.back"),
                            |chrome| chrome.browser_back_id.clone(),
                        ))
                        .name("Back")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press_maybe(can_go_back.then_some(Message::BrowserBack(id))),
                        "Back",
                    ),
                    crate::interaction::tooltip(
                        widget::button::icon(crate::ui_controls::symbolic_icon("go-next-symbolic"))
                            .id(chrome.map_or_else(
                                || card_control_id(id, "browser.forward"),
                                |chrome| chrome.browser_forward_id.clone(),
                            ))
                            .name("Forward")
                            .width(ICON_CONTROL_SIZE)
                            .height(ICON_CONTROL_SIZE)
                            .on_press_maybe(can_go_forward.then_some(Message::BrowserForward(id))),
                        "Forward",
                    ),
                    crate::interaction::tooltip(
                        widget::button::icon(crate::ui_controls::symbolic_icon(
                            "view-refresh-symbolic",
                        ))
                        .id(chrome.map_or_else(
                            || card_control_id(id, "browser.reload"),
                            |chrome| chrome.browser_reload_id.clone(),
                        ))
                        .name("Reload")
                        .width(ICON_CONTROL_SIZE)
                        .height(ICON_CONTROL_SIZE)
                        .on_press_maybe(active.then_some(Message::BrowserReload(id))),
                        "Reload",
                    ),
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
                vec![toolbar.into(), crate::design::horizontal_rule()];
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

        // A card that kept model focus under an open dialog would take the
        // typing aimed at the dialog and write it into a shell, which runs
        // what it is given. An overlay inspector additionally hides the workspace, so
        // that shell would not even be on screen.
        let card_focused = focused && !self.workspace_inputs_blocked();
        #[cfg(not(feature = "integrated-browser"))]
        let browser_toolbar: Option<Element<'a, Message>> = browser_session.map(|_| {
            let address = chrome
                .map(|chrome| chrome.caption.as_str())
                .unwrap_or(SanitizedLocation::INVALID);
            let address = widget::text_input("Saved URL", address)
                .id(chrome.map_or_else(
                    || card_control_id(id, "browser.address"),
                    |chrome| chrome.browser_address_id.clone(),
                ))
                .width(Length::Fill);
            let toolbar = widget::container(
                widget::row::with_children(vec![
                    address.into(),
                    widget::button::suggested("Open in system browser")
                        .on_press(Message::RetryCard(id))
                        .into(),
                ])
                .align_y(Alignment::Center)
                .spacing(spacing.space_xxxs)
                .width(Length::Fill),
            )
            .height(Length::Fixed(BROWSER_TOOLBAR_HEIGHT))
            .padding([0, spacing.space_xxs])
            .class(cosmic::theme::Container::Background);
            widget::column::with_children(vec![toolbar.into(), crate::design::horizontal_rule()])
                .spacing(0)
                .into()
        });

        let retry_label = if cfg!(not(feature = "integrated-browser")) && browser_session.is_some()
        {
            "Open in system browser"
        } else {
            "Retry"
        };
        let content: Element<'a, Message> = match self.cards.get(&id) {
            Some(CardRuntime::Chat(_)) => self.chat_view(id).map(move |message| match message {
                Message::ChatUi(chat_ui::Action::OpenUrl(uri)) => {
                    Message::OpenDocumentLink(id, uri)
                }
                message => message,
            }),
            Some(CardRuntime::Terminal(handle)) => crate::file_drop::target(
                TerminalWidget::new(handle.clone())
                    .read_only(ended_terminal(card).is_some() || self.terminal_close_pending(id))
                    .id(chrome.map_or_else(
                        || workspace_widget::card_focus_id(id),
                        |chrome| chrome.focus_id.clone(),
                    ))
                    .font_size(self.settings.terminal_font_size)
                    .font_family(&self.settings.terminal_font_family)
                    .line_height(self.settings.terminal_line_height)
                    .padding(self.settings.terminal_padding)
                    .scroll_size(self.settings.terminal_scroll_size)
                    .scroll_speed(self.settings.terminal_scroll_speed)
                    .cursor_style(self.settings.terminal_cursor_style)
                    .scale_factor(effective_surface_scale(
                        self.window_scale_factor,
                        self.settings.ui_scale,
                    ))
                    .model_focused(card_focused)
                    .on_focus(Message::FocusCard(id))
                    .on_open_link(move |url, gesture| Message::OpenLink(id, url, gesture))
                    .on_command_error(move |error| Message::TerminalError {
                        card: id,
                        message: error.to_string(),
                    }),
                ("terminal", id),
                // A dialog over the workspace owns the pointer as well as the
                // keyboard, so a drop aimed through it is refused rather than
                // typed into a shell the reader cannot see.
                ended_terminal(card).is_none()
                    && !self.terminal_close_pending(id)
                    && !self.workspace_inputs_blocked(),
                move |payload| Message::TerminalDropped { card: id, payload },
            ),
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) => BrowserWidget::new(&browser.handle)
                .model_focused(card_focused)
                .on_focus(Message::FocusCard(id))
                .scale_factor(effective_surface_scale(
                    self.window_scale_factor,
                    self.settings.ui_scale,
                ))
                .ui_scale(self.settings.ui_scale)
                .virtual_offset(self.workspace.camera().offset())
                .input_method(browser.ime.clone())
                .on_error(move |error| Message::BrowserError {
                    card: id,
                    message: error.to_string(),
                })
                .into(),
            Some(CardRuntime::Repository(runtime)) => {
                repository_widget::view(id, runtime).map(Message::Repository)
            }
            Some(CardRuntime::PullRequests(pane)) => self.pull_request_card_view(id, pane),
            Some(CardRuntime::Settings) => settings_ui::settings_pane(
                self.settings_category,
                &self.settings_draft,
                &self.settings_search,
                self.settings_feedback.as_ref(),
                &self.settings_invalid,
                self.default_account_choices(),
                self.notification_ui.testing,
            ),
            Some(CardRuntime::Accounts) => self.accounts_pane(),
            Some(CardRuntime::Automations) => self.automation_ui.view().map(Message::AutomationUi),
            Some(CardRuntime::Completed) => {
                widget::container(design::body(ended_terminal(card).map_or_else(
                    || "Process finished successfully.".to_string(),
                    |session| {
                        format!(
                            "{}. {}",
                            terminal_end_summary(session),
                            terminal_relaunch_advice(session)
                        )
                    },
                )))
                .center(Length::Fill)
                .into()
            }
            Some(CardRuntime::Failed(message)) => widget::container(
                widget::container(
                    widget::column::with_children(vec![
                        widget::row::with_children(vec![
                            crate::ui_controls::symbolic_icon("dialog-warning-symbolic")
                                .icon()
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
                        self.account_recovery_controls(id)
                            .unwrap_or_else(|| iced::widget::Space::new().into()),
                        widget::row::with_children({
                            let mut controls = vec![
                                widget::button::suggested(retry_label)
                                    .id(card_control_id(id, "retry"))
                                    .name(format!("{retry_label} {}", labels.type_label))
                                    .on_press(Message::RetryCard(id))
                                    .into(),
                            ];
                            // A conversation no retry can reach still has one
                            // thing left to do with it, and the reader has to
                            // be the one who asks for it.
                            if self.chat_offers_fresh_conversation(id) {
                                controls.push(
                                    widget::button::standard("Start a new conversation here")
                                        .id(card_control_id(id, "chat.fresh"))
                                        .name("Start a new conversation in the same directory")
                                        .on_press(Message::ChatUi(chat_ui::Action::StartFresh(id)))
                                        .into(),
                                );
                            }
                            controls.push(
                                widget::button::standard("Close")
                                    .id(card_control_id(id, "failed.close"))
                                    .name(format!("Close {}", labels.type_label))
                                    .on_press(Message::CloseCard(id))
                                    .into(),
                            );
                            controls
                        })
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
                widget::container(
                    widget::column::with_children(vec![
                        crate::ui_controls::activity_spinner_tone(24.0, design::accent_color),
                        widget::text::title3(labels.starting).into(),
                        design::caption("Preparing this workspace column…")
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                            .into(),
                    ])
                    .align_x(Alignment::Center)
                    .spacing(spacing.space_xs),
                )
                .class(cosmic::theme::Container::Card)
                .max_width(STATUS_SURFACE_MAX_WIDTH)
                .padding(spacing.space_m),
            )
            .center(Length::Fill)
            .padding(spacing.space_m)
            .into(),
        };

        let mut rows: Vec<Element<'a, Message>> = Vec::with_capacity(3);
        // The PTY may report its final exit before the close receipt comes
        // back. Keep the closing card visually stable until the owner either
        // confirms removal or reports that it must stay open.
        let closing_terminal =
            self.closed_terminal_cards.contains(&id) || self.terminal_close_pending(id);
        if let Some(session) = ended_terminal(card).filter(|_| !closing_terminal) {
            let resume = session.agent_resume_recipe().is_some();
            let native = self.completed_terminal_has_native_owner(id);
            let mut controls: Vec<Element<'a, Message>> = vec![
                design::body(terminal_end_summary(session))
                    .width(Length::Fill)
                    .into(),
                widget::button::standard(if native {
                    "Open conversation"
                } else if resume {
                    "Resume"
                } else {
                    "Restart"
                })
                .on_press(Message::RetryCard(id))
                .into(),
            ];
            if session.transcript_path.is_some() {
                controls.push(
                    widget::button::standard("Open saved output")
                        .on_press(Message::OpenTerminalOutput(id))
                        .into(),
                );
            }
            rows.push(
                widget::container(
                    widget::row::with_children(controls)
                        .spacing(8)
                        .align_y(Alignment::Center),
                )
                .padding([4, 8])
                .width(Length::Fill)
                .into(),
            );
        }
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&id)
            && let Some(toolbar) = repository_widget::selected_file_toolbar(id, runtime)
        {
            rows.push(toolbar.map(Message::Repository));
            if runtime
                .selected_path()
                .and_then(Path::extension)
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
                })
            {
                rows.push(
                    widget::button::text("Preview HTML")
                        .on_press(Message::PreviewLocalFile(id))
                        .into(),
                );
            }
            rows.push(design::horizontal_rule());
        }
        if let Some(toolbar) = browser_toolbar {
            rows.push(toolbar);
        }
        rows.push(content);
        let surface: Element<'a, Message> = widget::container(
            widget::column::with_children(rows)
                .spacing(0)
                .height(Length::Fill),
        )
        .class(cosmic::theme::Container::custom(move |theme| {
            let mut accent = iced::Color::from(*theme.cosmic().accent_color());
            accent.a = 0.35;
            iced::widget::container::Style {
                background: Some(iced::Background::Color(theme.cosmic().bg_color().into())),
                border: iced::Border {
                    color: accent,
                    width: if attention { design::BORDER_WIDTH } else { 0.0 },
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        let edge: Element<'a, Message> = if card.is_some() {
            if show_resize_splitter {
                let limits = self.workspace.card_resize_limits(id);
                let width = self
                    .workspace
                    .geometry()
                    .card(id)
                    .map_or(limits.minimum, |card| card.width());
                let maximum = limits.maximum;
                let minimum = limits.minimum;
                Splitter::new(
                    chrome.map_or_else(
                        || card_control_id(id, "resize"),
                        |chrome| chrome.resize_id.clone(),
                    ),
                    chrome.map_or_else(
                        || card.map_or("Resize terminal column", |state| state.title.as_str()),
                        |chrome| chrome.resize_name.as_str(),
                    ),
                    width,
                    minimum,
                    maximum,
                    SplitterEdge::Trailing,
                    self.modal_is_open(),
                    move |width| Message::ResizeCard { card: id, width },
                    move || Message::ResizeCardFinished(id),
                )
                .into()
            } else {
                iced::widget::Space::new().into()
            }
        } else if show_resize_splitter {
            widget::container(crate::design::vertical_rule())
                .center_x(Length::Fixed(crate::splitter::WIDTH))
                .height(Length::Fill)
                .into()
        } else {
            iced::widget::Space::new().into()
        };
        crate::interaction::layer(
            surface,
            widget::container(edge)
                .align_x(Alignment::End)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        )
    }

    fn synchronize_sidebar_selection(&mut self) {
        if self.durable.active_channel.is_some() {
            return;
        }
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
            CardKind::Chat(session) => self.sidebar_model.projects.iter().find_map(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == session.launch.recipe.working_directory)
                    .map(|worktree| SidebarAction::OpenWorktree {
                        project_id: project.id,
                        worktree_id: worktree.id,
                        path: worktree.path.clone(),
                    })
            }),
            CardKind::Browser(session) => self.sidebar_model.projects.iter().find_map(|project| {
                project
                    .display_pull_requests()
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
                            .display_pull_requests()
                            .iter()
                            .find(|pull_request| pull_request.number == number)
                            .map(|pull_request| SidebarAction::OpenPullRequest {
                                project_id: project.id,
                                number,
                                url: pull_request.url.clone(),
                            })
                    })
            }),
            CardKind::Settings | CardKind::Accounts | CardKind::Automations => None,
        };
        if let Some(action) = action {
            self.sidebar_state.apply(&action);
        }
    }

    fn active_workspace_destination(&self) -> Option<WorkspaceDestination> {
        if let Some(id) = self.durable.active_channel {
            return Some(WorkspaceDestination::Channel(id));
        }
        let path = self.active_workspace.as_deref()?;
        self.sidebar_model.projects.iter().find_map(|project| {
            if project.kind == ProjectKind::Folder && project.repository == path {
                Some(WorkspaceDestination::Folder(project.id))
            } else {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == path)
                    .map(|worktree| WorkspaceDestination::Worktree(worktree.id))
            }
        })
    }

    /// Preview workspaces in a frozen MRU order, committing on modifier release.
    fn cycle_workspace(
        &mut self,
        direction: crate::workspace::FocusDirection,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        if self.workspace_inputs_blocked() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let previous_history = self.workspace_history.clone();
        let Some(action) = self.next_workspace(direction) else {
            return Task::none();
        };
        if self
            .sidebar_entered_directory(&action)
            .is_some_and(|directory| self.refuse_in_deleting_worktree(&directory))
        {
            self.workspace_history = previous_history;
            return Task::none();
        }
        let destination = WorkspaceDestination::from_action(&action);
        let previous_pane = self.workspace.focused();
        self.sidebar_state.apply(&action);
        // Select synchronously: the next key event must see this preview,
        // rather than queue another selection against an unchanged workspace.
        let task = match action {
            SidebarAction::OpenRepository { repository, .. } => {
                self.select_worktree(repository, true)
            }
            SidebarAction::OpenWorktree { path, .. } => self.select_worktree(path, true),
            SidebarAction::OpenChannel { channel_id } => {
                self.handle_channel_ui(channel_ui::Action::Open(channel_id))
            }
            _ => {
                unreachable!("workspace destinations only contain folders, worktrees and channels")
            }
        };
        if self.active_workspace_destination() != destination {
            self.workspace_history = previous_history;
            return task;
        }
        self.tab_history.finish(previous_pane);
        self.workspace_cycle_modifiers = modifiers
            & (keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT | keyboard::Modifiers::LOGO);
        if self.workspace_cycle_modifiers.is_empty() {
            self.workspace_history
                .finish(self.active_workspace_destination());
        }
        task
    }

    fn next_workspace(
        &mut self,
        direction: crate::workspace::FocusDirection,
    ) -> Option<SidebarAction> {
        let mut destinations = sidebar::workspace_cycle(&self.sidebar_model, &self.sidebar_state);
        let available: Vec<_> = destinations
            .iter()
            .filter_map(WorkspaceDestination::from_action)
            .collect();
        self.workspace_history.retain(|id| available.contains(&id));
        let selected = self.workspace_history.step(
            &available,
            self.active_workspace_destination(),
            direction == crate::workspace::FocusDirection::Left,
        )?;
        let index = destinations
            .iter()
            .position(|action| WorkspaceDestination::from_action(action) == Some(selected))?;
        Some(destinations.swap_remove(index))
    }

    /// The focused card, reduced to what binding the source-control panel
    /// needs. A card that names no worktree directory - a browser, a
    /// pull-request list - is [`SidebarFocus::None`], which leaves the panel
    /// where it is rather than tearing it down.
    fn sidebar_focus(&self) -> SidebarFocus {
        if let Some(worktree) = &self.active_workspace {
            return SidebarFocus::Repository {
                worktree: worktree.clone(),
            };
        }
        let Some(card) = self.workspace.focused().and_then(|id| self.card_state(id)) else {
            return SidebarFocus::None;
        };
        match &card.kind {
            CardKind::Repository(view) => SidebarFocus::Repository {
                worktree: view.worktree.clone(),
            },
            CardKind::Terminal(session) => SidebarFocus::Terminal {
                working_directory: session.launch.working_directory.clone(),
            },
            CardKind::Chat(session) => SidebarFocus::Terminal {
                working_directory: session.launch.recipe.working_directory.clone(),
            },
            CardKind::Browser(_)
            | CardKind::PullRequests(_)
            | CardKind::Settings
            | CardKind::Accounts
            | CardKind::Automations => SidebarFocus::None,
        }
    }

    /// Rebinds the source-control panel to whatever worktree is now focused,
    /// and loads a snapshot when that is a different worktree.
    ///
    /// Runs at the tail of every update, beside the selection sync, because
    /// focus changes for reasons that are not sidebar actions: a card closing,
    /// a session restoring, a project list arriving. Rebinding drops the
    /// previous worktree's model outright, so a stale row can never be pressed.
    fn rebind_source_control(&mut self) -> Task<Message> {
        let focus = self.sidebar_focus();
        if !self
            .sidebar_state
            .sync_active_worktree(&self.sidebar_model, &focus)
        {
            return self.load_pending_history();
        }
        self.cancel_source_read();
        self.cancel_branch_changes();
        self.history_ui.cancel();
        self.history_ui.next_page_request();
        let Some(key) = self.sidebar_state.active_worktree().cloned() else {
            self.sidebar_model.source_control = None;
            return Task::none();
        };
        if self.folder_scope(&key)
            && matches!(
                self.context_tab,
                ContextTab::Changes | ContextTab::PullRequest
            )
        {
            self.context_tab = ContextTab::Files;
        }
        self.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        self.source_control_snapshot_task(key)
    }

    /// Asks for the first page of history once, and only once the `History`
    /// tab is showing: a page is a `git log` walk, and a view nobody opened is
    /// not worth one.
    fn load_pending_history(&mut self) -> Task<Message> {
        if self.active_folder_scope() {
            return Task::none();
        }
        if self.sidebar_state.changes_tab() != sidebar::ChangesTab::History {
            return Task::none();
        }
        let Some(source) = self.sidebar_model.source_control.as_mut() else {
            return Task::none();
        };
        if !source.wants_history() || source.history_requested() {
            return Task::none();
        }
        source.mark_history_requested();
        let generation = self.history_ui.next_page_request();
        Self::source_control_history_task(
            generation,
            source.key.clone(),
            source
                .head_oid
                .as_deref()
                .and_then(|oid| crate::repository::Oid::parse_object(oid).ok()),
            0,
        )
    }

    /// The open repository card showing this worktree, if one is open.
    fn repository_card_for(&self, worktree: &Path) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
                    && self
                        .durable
                        .channels
                        .iter()
                        .find(|channel| channel.cards.contains(&card.id))
                        .map(|channel| channel.id)
                        == self.durable.active_channel
                    && matches!(&card.kind, CardKind::Repository(view) if view.selected_file.is_none() && view.worktree == worktree)
            })
            .map(|card| card.id)
    }

    /// Routes one panel row to the worktree's repository card.
    ///
    /// The card is opened if the worktree has none. Either way the navigation
    /// is parked in [`Self::pending_panel_open`] and performed when that card's
    /// file list lands, because the card resolves a path against the list it
    /// holds and the sidebar's rows come from a snapshot the list may predate.
    fn route_panel_open(&mut self, key: &ActiveWorktreeKey, path: &RepoPath) -> Task<Message> {
        if self.sidebar_state.active_worktree() != Some(key) {
            return Task::none();
        }
        // A narrow inspector overlays the strip. Selecting a file dismisses
        // only that overlay so the editor is visible; docked browsing stays put.
        let overlay = if self.context_inspector_is_overlay() {
            self.set_context_inspector_open(false)
        } else {
            Task::none()
        };
        let path = path.as_path().to_path_buf();
        let task = match self.repository_card_for(&key.canonical_path) {
            Some(card) => {
                self.pending_panel_open = Some(PendingPanelOpen { card, path });
                let focus = self
                    .workspace
                    .focus(card)
                    .map(|update| self.apply_viewport(update, true, false))
                    .unwrap_or_else(|_| Task::none());
                Task::batch([
                    focus,
                    cosmic::task::message(Message::Repository(RepositoryAction::RefreshFiles {
                        card,
                    })),
                ])
            }
            None => {
                // `add_repository_card` runs the card's own initial load, so
                // the parked navigation is answered by that list rather than by
                // a second one requested here.
                let task = self.add_repository_card(
                    key.project_id,
                    key.canonical_path.clone(),
                    key.canonical_path.clone(),
                );
                if let Some(card) = self.repository_card_for(&key.canonical_path) {
                    self.pending_panel_open = Some(PendingPanelOpen { card, path });
                }
                task
            }
        };
        overlay.chain(task)
    }

    /// Hands the card's buffer lifecycle to the language-server manager.
    ///
    /// The widget records what happened to the buffer; nothing in it can talk
    /// to a process. Draining here is what turns those records into protocol
    /// traffic, and it must happen after *every* repository action, because an
    /// `Open` that is never drained leaves a document the server has not been
    /// told about while the application shows results for it.
    fn drain_lsp_intents(&mut self, card: CardId) -> Task<Message> {
        let (intents, edits, auto_save) = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => {
                let intents = runtime.take_lsp_intents();
                // Strictly after `take_lsp_intents`, which is what materialises
                // the `Change` these edits describe.
                let edits = runtime.take_lsp_edits();
                (
                    intents,
                    edits,
                    Task::batch([
                        repository_task(runtime.auto_save_task(card)),
                        repository_task(runtime.working_diff_task(card)),
                    ]),
                )
            }
            _ => return Task::none(),
        };
        if intents.is_empty() {
            return auto_save;
        }
        let mut tasks = vec![auto_save];
        for intent in intents {
            match intent {
                LspIntent::Open {
                    absolute_path,
                    worktree,
                    version,
                    text,
                } => tasks.push(self.open_lsp_document(
                    card,
                    &absolute_path,
                    &worktree,
                    version,
                    &text,
                )),
                LspIntent::Change { version, text } => {
                    let Some(doc) = self.lsp_document(card) else {
                        continue;
                    };
                    // An empty slice means the widget could not describe the
                    // change as ranges, and the manager then sends the whole
                    // document from `text`. Either way every answer computed
                    // against the old text is dropped in the same breath: a
                    // completion range or a hover span that predates a
                    // keystroke describes characters the buffer no longer has.
                    self.lsp.change(doc, version, &edits, &text);
                    if let Some(state) = self.lsp_contexts.get_mut(&card) {
                        state.invalidate_answers();
                    }
                }
                LspIntent::Save { text } => {
                    if let Some(doc) = self.lsp_document(card) {
                        self.lsp.save(doc, &text);
                    }
                }
                LspIntent::Close => self.close_lsp_document(card),
            }
        }
        Task::batch(tasks)
    }

    /// Retires everything a closed card held at the language-server boundary.
    ///
    /// Dropping the runtime alone leaves the server holding a document for a
    /// file nobody can see - still analysing it, still publishing diagnostics
    /// for it - and leaves this side's three maps growing for the life of the
    /// session. A card is closed far more often than a file is, so this is the
    /// path that decides whether a long session accumulates dead documents.
    fn retire_lsp_card(&mut self, card: CardId) {
        self.close_lsp_document(card);
        self.lsp_contexts.remove(&card);
        self.lsp_pending_jumps.remove(&card);
    }

    fn lsp_document(&self, card: CardId) -> Option<LspDocumentId> {
        self.lsp_contexts.get(&card)?.document
    }

    /// Opens `path` at the LSP boundary and answers any jump parked on it.
    fn open_lsp_document(
        &mut self,
        card: CardId,
        path: &Path,
        worktree: &Path,
        version: i32,
        text: &str,
    ) -> Task<Message> {
        // A previous buffer's document is closed first even without a `Close`
        // intent, because the two are one transition for the user: opening a
        // second file over the first leaves the server holding a document for
        // text nobody is editing, and its diagnostics would keep arriving for
        // a card that has moved on.
        self.close_lsp_document(card);
        let state = self.lsp_contexts.entry(card).or_default();
        state.path = Some(path.to_path_buf());
        let Some(language) = self.lsp.language_for(path) else {
            // No configured server claims this extension. That is a supported
            // configuration, not a failure, so the surface says the file has
            // no server rather than reporting one that is broken.
            let state = self.lsp_contexts.entry(card).or_default();
            state.context.status = Some(LspStatus::Disabled);
            self.lsp_pending_jumps.remove(&card);
            return Task::none();
        };
        let doc = LspDocumentId(self.next_lsp_document);
        self.next_lsp_document += 1;
        let status = self.lsp.open(doc, path, worktree, language, version, text);
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
            runtime.set_lsp_document(Some(doc));
        }
        self.lsp_documents.insert(doc, card);
        let state = self.lsp_contexts.entry(card).or_default();
        state.document = Some(doc);
        state.context.status = Some(status);
        self.flush_pending_lsp_jump(card, path)
    }

    fn close_lsp_document(&mut self, card: CardId) {
        if let Some(state) = self.lsp_contexts.get_mut(&card)
            && let Some(doc) = state.document.take()
        {
            self.lsp.close(doc);
            self.lsp_documents.remove(&doc);
        }
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
            runtime.set_lsp_document(None);
        }
        if let Some(state) = self.lsp_contexts.get_mut(&card) {
            state.context = inspector::LspInspectorContext::default();
            state.diagnostics_version = None;
            state.path = None;
        }
    }

    /// Applies a parked jump now that `path` is the card's open buffer.
    fn flush_pending_lsp_jump(&mut self, card: CardId, path: &Path) -> Task<Message> {
        // Taken on *any* open, not only a matching one. A jump is answered by
        // the open it asked for or by nothing: leaving it parked would let the
        // caret be yanked minutes later, when the user opens that file
        // themselves, to a position from an answer they have forgotten asking
        // for and against text that has since changed.
        let Some(jump) = self.lsp_pending_jumps.remove(&card) else {
            return Task::none();
        };
        if jump.path != path {
            return Task::none();
        }
        let at = match jump.coordinates {
            LspCoordinates::Scalar => jump.at,
            // The column arrived in protocol code units because the file was
            // not open when the server answered. It is open now, so the
            // manager can place it against the text it actually indexes.
            LspCoordinates::Utf16 => {
                let Some(doc) = self.lsp_document(card) else {
                    return Task::none();
                };
                match self.lsp.scalar_position(doc, jump.at) {
                    Ok(at) => at,
                    Err(error) => {
                        self.status = Some(TransientNotice::warning(format!(
                            "opened the file, but could not place the caret: {error}"
                        )));
                        return Task::none();
                    }
                }
            }
        };
        self.move_lsp_caret(card, at)
    }

    /// Places the caret and gives the editor the keyboard.
    ///
    /// Moving the caret alone is invisible: an unfocused `text_editor` draws
    /// no caret at all, so a jump into a result would answer the user with a
    /// frame that looks exactly like the one they clicked in.
    fn move_lsp_caret(&mut self, card: CardId, at: LspPosition) -> Task<Message> {
        let focus = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime
                .move_cursor_to(at)
                .then(|| runtime.focus_editor(card)),
            _ => None,
        };
        focus.unwrap_or_else(|| {
            self.status = Some(TransientNotice::warning(format!(
                "line {} is not in this file any more",
                at.line.saturating_add(1)
            )));
            Task::none()
        })
    }

    /// Routes one language-server event onto the card whose document it names.
    fn apply_lsp_event(&mut self, event: LspEvent) {
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
        // An event for a document that has since been closed has no surface to
        // land on, and diagnostics arrive on every recheck, so this is a map
        // rather than a scan of the open cards.
        let Some(&card) = self.lsp_documents.get(&doc) else {
            return;
        };
        let Some(state) = self.lsp_contexts.get_mut(&card) else {
            return;
        };
        match event {
            LspEvent::Diagnostics { version, items, .. } => {
                state.context.diagnostics = items;
                state.diagnostics_version = Some(version);
            }
            LspEvent::Completion { items, .. } => state.context.completions = items,
            LspEvent::Hover { markdown, .. } => state.context.hover = markdown,
            LspEvent::Locations { kind, items, .. } => {
                state.context.locations = Some((kind, items));
            }
            LspEvent::Status { status, .. } => state.context.status = Some(status),
            LspEvent::Progress { progress, .. } => {
                // Progress is transient by definition, so it is shown in the
                // notice slot the surface already has and cleared on `done`
                // rather than accumulating a log nobody dismisses.
                state.progress = (!progress.done).then(|| {
                    let detail = progress
                        .title
                        .or(progress.message)
                        .unwrap_or_else(|| progress.token.clone());
                    match progress.percentage {
                        Some(percent) => format!("{detail} ({percent}%)"),
                        None => detail,
                    }
                });
                state.refresh_notice();
            }
            LspEvent::RequestFailed { error, .. } => {
                // A superseded request is the ordinary result of typing while
                // one is in flight, and reporting it would put a warning on
                // screen for every keystroke.
                if !matches!(error, LspError::Cancelled) {
                    state.failure = Some(error.to_string());
                    state.refresh_notice();
                }
            }
            LspEvent::DocumentDesynced { error, .. } => {
                // The buffer is intact and the manager has already resent it;
                // what is gone is every answer computed against the text the
                // two sides disagreed about.
                state.invalidate_answers();
                state.failure = Some(format!(
                    "resynchronised with the language server after {error}"
                ));
                state.refresh_notice();
            }
        }
    }

    /// The LSP results to render beside `card`, if it is showing any.
    fn lsp_context(&self, card: CardId) -> Option<&inspector::LspInspectorContext> {
        self.lsp_contexts.get(&card).map(|state| &state.context)
    }

    /// Handles the inspector's language-server actions.
    fn update_lsp(&mut self, action: InspectorAction) -> Task<Message> {
        match action {
            InspectorAction::JumpToLspDiagnostic { card, at } => {
                let Some(state) = self.lsp_contexts.get(&card) else {
                    return Task::none();
                };
                // Diagnostics are unsolicited and replace each other wholesale,
                // so the list on screen can describe text the buffer has moved
                // past. Jumping into it would land the caret on a line the
                // diagnostic never named.
                let published = state.diagnostics_version;
                let current = state
                    .document
                    .and_then(|doc| self.lsp.document_version(doc));
                if !diagnostics_are_current(published, current) {
                    self.status = Some(TransientNotice::warning(
                        "these diagnostics describe an earlier version of the file",
                    ));
                    return Task::none();
                }
                self.move_lsp_caret(card, at)
            }
            InspectorAction::JumpToLspLocation {
                card,
                path,
                at,
                coordinates,
            } => self.jump_to_lsp_location(card, path, at, coordinates),
            InspectorAction::ApplyLspCompletion { card, index, label } => {
                self.apply_lsp_completion(card, index, &label)
            }
            InspectorAction::RequestLspHover(card) => self.request_lsp(card, LspRequestKind::Hover),
            InspectorAction::GoToLspDefinition(card) => {
                self.request_lsp(card, LspRequestKind::Definition)
            }
            InspectorAction::FindLspReferences(card) => {
                self.request_lsp(card, LspRequestKind::References)
            }
            InspectorAction::RestartLanguageServer(card) => {
                let Some(doc) = self.lsp_document(card) else {
                    return Task::none();
                };
                self.lsp.restart(doc);
                if let Some(state) = self.lsp_contexts.get_mut(&card) {
                    // The replacement process republishes from scratch. Keeping
                    // the dead session's diagnostics on screen would show the
                    // user results from a server that no longer exists.
                    state.invalidate_answers();
                    state.context.diagnostics.clear();
                    state.diagnostics_version = None;
                    state.failure = None;
                    state.progress = None;
                    state.refresh_notice();
                    state.context.status = Some(self.lsp.status(doc));
                }
                Task::none()
            }
            InspectorAction::DismissLspResults(card) => {
                if let Some(state) = self.lsp_contexts.get_mut(&card) {
                    state.invalidate_answers();
                    state.failure = None;
                    state.refresh_notice();
                }
                Task::none()
            }
            InspectorAction::OpenLspHoverLink(uri) => match Url::parse(&uri) {
                Ok(url) => self.open_clicked_link(url),
                Err(_) => {
                    self.status = Some(TransientNotice::error(
                        "could not open hover link: invalid absolute URL",
                    ));
                    Task::none()
                }
            },
            other => unreachable!("{other:?} is not a language-server action"),
        }
    }

    fn jump_to_lsp_location(
        &mut self,
        card: CardId,
        path: PathBuf,
        at: LspPosition,
        coordinates: LspCoordinates,
    ) -> Task<Message> {
        let open_path = self
            .lsp_contexts
            .get(&card)
            .and_then(|state| state.path.clone());
        if open_path.as_deref() == Some(path.as_path()) {
            // Already the open buffer, so the column is placeable now. A
            // `Utf16` location for the open file cannot occur - the manager
            // converts against every document it holds - but it is refused
            // rather than trusted, because placing protocol code units as
            // scalars silently lands the caret in the wrong column.
            return match coordinates {
                LspCoordinates::Scalar => self.move_lsp_caret(card, at),
                LspCoordinates::Utf16 => {
                    let Some(doc) = self.lsp_document(card) else {
                        return Task::none();
                    };
                    match self.lsp.scalar_position(doc, at) {
                        Ok(at) => return self.move_lsp_caret(card, at),
                        Err(error) => {
                            self.status = Some(TransientNotice::warning(format!(
                                "could not place the caret: {error}"
                            )));
                        }
                    }
                    Task::none()
                }
            };
        }
        // The card's file list is worktree-relative and a location is absolute.
        // A target outside the worktree has no row to open, and the inspector
        // already renders those as inert, so this is a refusal rather than a
        // guess at which repository the path belongs to.
        let worktree = match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime.descriptor.worktree.clone(),
            _ => return Task::none(),
        };
        let Ok(relative) = path.strip_prefix(&worktree) else {
            self.status = Some(TransientNotice::warning(
                "that definition is outside this worktree",
            ));
            return Task::none();
        };
        let relative = relative.to_path_buf();
        // `OpenFile` answers a path the card has no row for by doing nothing,
        // so parking on that open would wait for something that never happens
        // and leave the user with no answer at all. A generated or ignored
        // source inside the worktree is the ordinary way to reach this.
        let openable = match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime.can_open_file(&relative),
            _ => false,
        };
        if !openable {
            self.status = Some(TransientNotice::warning(format!(
                "{} is not in this worktree's file list",
                relative.display()
            )));
            return Task::none();
        }
        self.lsp_pending_jumps.insert(
            card,
            PendingLspJump {
                path,
                at,
                coordinates,
            },
        );
        cosmic::task::message(Message::Repository(RepositoryAction::OpenFile {
            card,
            path: relative,
        }))
    }

    fn apply_lsp_completion(&mut self, card: CardId, index: usize, label: &str) -> Task<Message> {
        let Some(state) = self.lsp_contexts.get(&card) else {
            return Task::none();
        };
        let Some(item) = state.context.completions.get(index) else {
            return Task::none();
        };
        // The list is addressed by position, and a fresh completion response
        // replaces it wholesale. The label is the fingerprint that catches a
        // list which changed under the press: applying the wrong candidate
        // writes text the user never chose.
        if item.label != label {
            self.status = Some(TransientNotice::warning(
                "the completion list changed; nothing was applied",
            ));
            return Task::none();
        }
        let item = item.clone();
        let applied = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime.apply_completion(&item),
            _ => false,
        };
        if !applied {
            self.status = Some(TransientNotice::warning(format!(
                "`{}` could not be applied to this buffer",
                item.label
            )));
            return Task::none();
        }
        // Applying one candidate makes the rest describe text that no longer
        // exists, and the edit itself produces a `Change` intent that must
        // reach the server before the next request is answered against it.
        if let Some(state) = self.lsp_contexts.get_mut(&card) {
            state.context.completions.clear();
        }
        self.drain_lsp_intents(card)
    }

    fn request_lsp(&mut self, card: CardId, kind: LspRequestKind) -> Task<Message> {
        let Some(doc) = self.lsp_document(card) else {
            return Task::none();
        };
        let at = match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime.cursor_position(),
            _ => None,
        };
        let Some(at) = at else {
            return Task::none();
        };
        match kind {
            LspRequestKind::Hover => self.lsp.request_hover(doc, at),
            LspRequestKind::Definition => self.lsp.request_definition(doc, at),
            LspRequestKind::References => self.lsp.request_references(doc, at),
        };
        Task::none()
    }

    fn flush_pending_panel_open(&mut self, card: CardId) -> Task<Message> {
        if self.pending_panel_open.as_ref().map(|open| open.card) != Some(card) {
            return Task::none();
        }
        let open = self
            .pending_panel_open
            .take()
            .expect("the pending open was just matched");
        cosmic::task::message(Message::Repository(RepositoryAction::OpenFile {
            card,
            path: open.path,
        }))
    }

    /// True when this result still describes the worktree the panel is bound
    /// to. A snapshot for a worktree the panel left is a read of a tree nobody
    /// is looking at.
    fn source_control_is_bound(&self, key: &ActiveWorktreeKey) -> bool {
        self.sidebar_model
            .source_control
            .as_ref()
            .is_some_and(|source| source.key == *key)
    }

    /// Reloads the bound worktree, keeping whatever mutation is in flight
    /// visible so a refresh cannot make a running command look finished.
    fn reload_source_control(&mut self, key: &ActiveWorktreeKey) -> Task<Message> {
        if !self.source_control_is_bound(key) {
            return Task::none();
        }
        if let Some(source) = self.sidebar_model.source_control.as_mut() {
            source.set_reloading(true);
        }
        self.source_control_snapshot_task(key.clone())
    }

    fn source_control_snapshot_task(&mut self, key: ActiveWorktreeKey) -> Task<Message> {
        self.cancel_source_read();
        let request = self.refresh_ui.source_generation;
        let cancellation = CancellationToken::default();
        self.refresh_ui.source_cancel = Some(cancellation.clone());
        if self.folder_scope(&key) {
            let directory = key.canonical_path.clone();
            let expanded = self
                .sidebar_state
                .expanded_directory_paths()
                .map(|path| path.as_path().to_path_buf())
                .collect::<Vec<_>>();
            let query = self.sidebar_state.files_query().to_owned();
            let search = query.clone();
            return cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    if !search.is_empty() {
                        return repository_widget::search_directory_files_for_quick_open_cancellable(
                            &directory, &search, &cancellation,
                        );
                    }
                    let mut files = repository_widget::load_directory_files(&directory)?;
                    for path in expanded {
                        if cancellation.is_cancelled() {
                            return Err("folder read cancelled".into());
                        }
                        files.extend(repository_widget::load_directory_listing(
                            &directory, &path,
                        )?);
                    }
                    Ok(files)
                })
                .await
                .map_err(|error| format!("folder reader failed: {error}"))
                .and_then(|result| result);
                Message::DirectoryFilesLoaded {
                    key,
                    request,
                    query,
                    result,
                }
            });
        }
        let worktree = key.canonical_path.clone();
        let untracked_changes = self.sidebar_state.section_expanded(PanelSection::Untracked);
        let options = crate::repository::SnapshotOptions {
            untracked_changes,
            untracked_files: untracked_changes || self.context_tab == ContextTab::Files,
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .snapshot_with_options(&worktree, options, &cancellation)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("source-control reader join failed: {error}"))
            .and_then(|result| result)
            .map(Box::new);
            Message::SourceControlLoaded {
                key,
                request,
                result,
            }
        })
    }

    fn source_control_history_task(
        generation: u64,
        key: ActiveWorktreeKey,
        head: Option<crate::repository::Oid>,
        page: u32,
    ) -> Task<Message> {
        let Some(head) = head else {
            return Task::none();
        };
        let request = HistoryRequest {
            worktree: key.canonical_path.clone(),
            revision: HistoryRevision::Object { oid: head.clone() },
            page,
            path: None,
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .history(&request, &CancellationToken::default())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("history reader join failed: {error}"))
            .and_then(|result| result);
            Message::SourceControlHistory {
                generation,
                key,
                page,
                head,
                result,
            }
        })
    }

    /// Starts one mutation against the bound worktree.
    ///
    /// Refused outright when the panel has moved on or a command is already
    /// running: two `git add` runs against one index corrupt each other's
    /// `index.lock`, and the panel's own controls stand down while one is in
    /// flight, so this is the guard for a press that raced the frame.
    fn begin_mutation(&mut self, key: &ActiveWorktreeKey, kind: MutationKind) -> bool {
        if !self.source_control_is_bound(key) || self.folder_scope(key) {
            return false;
        }
        let Some(source) = self.sidebar_model.source_control.as_mut() else {
            return false;
        };
        if source.busy() {
            return false;
        }
        source.set_error(None);
        source.set_mutation(Some(kind));
        true
    }

    /// The task one finished mutation turns into: report what Git did, then
    /// re-read the worktree, because the index is the only authority on what
    /// the mutation left behind.
    fn mutation_task(
        key: ActiveWorktreeKey,
        kind: MutationKind,
        run: impl FnOnce(&RepositoryService, &CancellationToken) -> (Option<String>, bool)
        + Send
        + 'static,
    ) -> Task<Message> {
        cosmic::task::future(async move {
            let joined = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git");
                run(&service, &CancellationToken::default())
            })
            .await;
            let (report, committed) = match joined {
                Ok(outcome) => outcome,
                Err(error) => (Some(format!("Git worker failed: {error}")), false),
            };
            Message::SourceControlMutated {
                key,
                kind,
                report,
                committed,
            }
        })
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
                CardRuntime::Chat(runtime) => runtime.set_visible(visible),
                #[cfg(feature = "integrated-browser")]
                CardRuntime::Browser(browser) => browser
                    .handle
                    .set_visible(visible)
                    .map_err(|error| error.to_string()),
                CardRuntime::Loading
                | CardRuntime::Failed(_)
                | CardRuntime::Completed
                | CardRuntime::Repository(_)
                | CardRuntime::PullRequests(_)
                | CardRuntime::Settings
                | CardRuntime::Accounts
                | CardRuntime::Automations => Ok(()),
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

    /// The focused card when it is a repository card showing an editable
    /// buffer, which is the only context in which the search chord means
    /// "find in this file" rather than "find a project".
    fn focused_editable_repository_card(&self) -> Option<CardId> {
        let card = self.workspace.focused()?;
        match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime))
                if runtime.editor_focus_target(card).is_some() =>
            {
                Some(card)
            }
            _ => None,
        }
    }

    /// The focused card when its find bar is open, so Escape can close the bar
    /// before it reaches anything behind it.
    fn focused_repository_card_with_find(&self) -> Option<CardId> {
        let card = self.workspace.focused()?;
        match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) if runtime.find_is_open() => Some(card),
            _ => None,
        }
    }

    /// Whether a modal dialog is on screen.
    ///
    /// libcosmic renders every one of them inside a `popover` that is modal
    /// for the pointer alone: it captures mouse and touch but forwards
    /// keyboard events to the content beneath. Nothing under a dialog may
    /// therefore act on a keystroke, because the user is aiming it at the
    /// dialog - and a terminal acts by writing to a shell.
    fn modal_is_open(&self) -> bool {
        self.update_ui.confirmation.is_some() || self.other_modal_is_open()
    }

    fn other_modal_is_open(&self) -> bool {
        self.form_dialog_is_open() || self.modal_over_form_dialogs()
    }

    /// The worktree and pull request dialogs: forms, whose focused field
    /// captures the keys the dialog itself answers.
    fn form_dialog_is_open(&self) -> bool {
        self.worktree_dialog.is_some() || self.pull_request_dialog.is_some()
    }

    /// Closes the topmost form dialog, stopping whatever it was waiting on.
    fn cancel_form_dialog(&mut self) -> Task<Message> {
        if let Some(dialog) = self.worktree_dialog.take() {
            dialog.cancel();
        } else if self.pull_request_dialog.take().is_some() {
            self.pull_request_dialog_generation =
                self.pull_request_dialog_generation.wrapping_add(1);
        }
        if let Some(target) = self.nested_modal_focus_target() {
            return iced::widget::operation::focus(target).map(cosmic::Action::App);
        }
        self.focus_context_overlay_if_exposed()
    }

    /// Whether a form dialog is the dialog on screen, not one under another.
    fn form_dialog_on_top(&self) -> bool {
        self.form_dialog_is_open()
            && self.update_ui.confirmation.is_none()
            && !self.modal_over_form_dialogs()
    }

    /// The dialogs `dialog` mounts above the form dialogs, besides the
    /// update confirmation, which `modal_is_open` adds.
    fn modal_over_form_dialogs(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return true;
        }
        self.dirty_repository_close.is_some()
            || self.terminal_close_dialog_visible()
            || self.settings_reset_confirm
            || self.project_ui.modal_open()
            || self.channel_ui.modal_open()
            || self.sidebar_selection_ui.dialog.is_some()
            // Mounted by `dialog` from the sidebar's own state, so nothing
            // beneath it can learn about it any other way. Leaving it out is
            // how the one dialog that is on screen *because* it is about to
            // destroy work becomes the one dialog the pane keeps its commands
            // live behind.
            || self.sidebar_state.pending_discard().is_some()
    }

    fn context_inspector_visible(&self) -> bool {
        self.context_inspector_open || self.context_inspector_transition.is_some()
    }

    /// The instant an animation in flight is measured against.
    ///
    /// The shell re-runs one `RedrawRequested` through update and layout
    /// while any pass publishes a message, rebuilding the view between
    /// passes. Reading the wall clock here gave every pass a slightly
    /// different inspector width, so the strip surface measured a new width
    /// each pass and reported it again: three full passes for one frame, and
    /// the shell's relayout warning. The frame an animation is being drawn
    /// for is the same for every pass over that frame.
    ///
    /// Starting a transition resets the frame clock, so the first redraw of
    /// every transition has no frame yet. That redraw draws the animation's
    /// starting point rather than a wall-clock sample, which is both stable
    /// across its passes and where the animation genuinely begins.
    fn animation_frame(&self, transition: &InspectorTransition) -> Instant {
        self.frame_clock.frame().unwrap_or(transition.started_at)
    }

    fn context_inspector_progress(&self) -> f32 {
        self.context_inspector_transition.map_or(
            if self.context_inspector_open {
                1.0
            } else {
                0.0
            },
            |transition| transition.progress(self.animation_frame(&transition)),
        )
    }

    fn context_inspector_is_overlay(&self) -> bool {
        matches!(
            inspector::mount_for_width(self.window_width, self.context_inspector_visible(), true,),
            Some(InspectorLayout::Overlay)
        )
    }

    fn workspace_inputs_blocked(&self) -> bool {
        self.modal_is_open() || self.context_inspector_is_overlay() || self.chrome_ui.is_open()
    }

    fn workspace_surfaces_visible(&self) -> bool {
        !self.context_inspector_is_overlay()
    }

    fn enter_context_overlay(&mut self) -> Task<Message> {
        // The header toggle belongs to the docked tree we are leaving and is
        // guaranteed to return when the modal closes. Recording it before the
        // focus operation makes every breakpoint entry deterministic, even if
        // a focused docked inspector child disappears during the re-layout.
        self.context_inspector_restore_focus = Some(context_inspector_toggle_id());
        if self.modal_is_open() || self.focused_repository_is_saving() {
            Task::none()
        } else {
            iced::widget::operation::focus(context_inspector_toggle_id()).map(cosmic::Action::App)
        }
    }

    fn sync_context_overlay_state(&mut self) -> Task<Message> {
        let was_overlay = self.context_inspector_overlay_active;
        let is_overlay = self.context_inspector_is_overlay();
        self.context_inspector_overlay_active = is_overlay;
        if was_overlay != is_overlay {
            let visible = self.workspace_surfaces_visible();
            self.set_workspace_surfaces_visible(visible);
        }
        match context_overlay_change(was_overlay, is_overlay) {
            ContextOverlayChange::Entered => self.enter_context_overlay(),
            ContextOverlayChange::Exited
                if !self.context_inspector_open || self.workspace.focused().is_none() =>
            {
                self.context_inspector_restore_focus
                    .take()
                    .map_or_else(Task::none, |id| {
                        iced::widget::operation::focus(id).map(cosmic::Action::App)
                    })
            }
            ContextOverlayChange::Exited => {
                // A width change kept the inspector mounted and focused as a
                // docked pane. A later docked close must not restore a target
                // from the preceding modal interval.
                self.context_inspector_restore_focus = None;
                Task::none()
            }
            ContextOverlayChange::Unchanged => Task::none(),
        }
    }

    fn set_context_inspector_open(&mut self, open: bool) -> Task<Message> {
        if self.context_inspector_open == open {
            return Task::none();
        }
        let from = self.context_inspector_progress();
        self.context_inspector_open = open;
        self.context_inspector_transition = Some(InspectorTransition {
            started_at: Instant::now(),
            from,
            to: if open { 1.0 } else { 0.0 },
        });
        self.frame_clock.reset();
        self.sync_context_overlay_state()
    }

    fn advance_context_inspector_transition(&mut self, now: Instant) -> Task<Message> {
        if self
            .context_inspector_transition
            .is_some_and(|transition| transition.is_complete(now))
        {
            self.context_inspector_transition = None;
            let sync = self.sync_context_overlay_state();
            return Task::batch([sync, self.focus_context_overlay_if_exposed()]);
        }
        Task::none()
    }

    fn inspector_runtime_status(&self, card: CardId) -> InspectorRuntimeStatus {
        match self.cards.get(&card) {
            Some(CardRuntime::Loading) | None => InspectorRuntimeStatus::Starting,
            Some(CardRuntime::Failed(_)) => InspectorRuntimeStatus::Unavailable,
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) => match browser.lifecycle {
                BrowserLifecycle::Restoring => InspectorRuntimeStatus::Restoring,
                BrowserLifecycle::Hibernating => InspectorRuntimeStatus::Suspending,
                BrowserLifecycle::Hibernated => InspectorRuntimeStatus::Suspended,
                BrowserLifecycle::Closed => InspectorRuntimeStatus::Unavailable,
                BrowserLifecycle::Visible | BrowserLifecycle::OffscreenHot
                    if browser.load_state.is_some() =>
                {
                    InspectorRuntimeStatus::Loading
                }
                BrowserLifecycle::Visible | BrowserLifecycle::OffscreenHot => {
                    InspectorRuntimeStatus::Ready
                }
            },
            Some(CardRuntime::Chat(_)) => InspectorRuntimeStatus::Ready,
            Some(CardRuntime::Completed)
            | Some(CardRuntime::Terminal(_))
            | Some(CardRuntime::PullRequests(_))
            | Some(CardRuntime::Settings)
            | Some(CardRuntime::Accounts)
            | Some(CardRuntime::Automations) => InspectorRuntimeStatus::Ready,
            Some(CardRuntime::Repository(runtime)) => match runtime.inspector_runtime_status() {
                RepositoryRuntimeStatus::Loading => InspectorRuntimeStatus::Loading,
                RepositoryRuntimeStatus::Ready => InspectorRuntimeStatus::Ready,
                RepositoryRuntimeStatus::Unavailable => InspectorRuntimeStatus::Attention,
            },
        }
    }

    fn handle_context_inspector_action(&mut self, action: InspectorAction) -> Task<Message> {
        match action {
            InspectorAction::Close => self.set_context_inspector_open(false),
            InspectorAction::ToggleDiagnostics(card) => {
                if self.workspace.focused() == Some(card) {
                    self.inspector_diagnostics_card =
                        (self.inspector_diagnostics_card != Some(card)).then_some(card);
                }
                Task::none()
            }
            InspectorAction::Resize { width } => {
                self.settings.inspector_width =
                    width.clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH);
                Task::none()
            }
            InspectorAction::ResizeFinished => self.save_settings_task(false),
            // A request's answer is rendered by this very panel, so dismissing
            // an overlay inspector on the way to asking for it would show the
            // user nothing at all. These five stay put; the four below move the
            // caret, edit the buffer, or leave the application, and for those
            // an overlay covering the workspace is in the way.
            action @ (InspectorAction::RequestLspHover(_)
            | InspectorAction::GoToLspDefinition(_)
            | InspectorAction::FindLspReferences(_)
            | InspectorAction::RestartLanguageServer(_)
            | InspectorAction::DismissLspResults(_)) => self.update_lsp(action),
            action @ (InspectorAction::JumpToLspDiagnostic { .. }
            | InspectorAction::JumpToLspLocation { .. }
            | InspectorAction::ApplyLspCompletion { .. }
            | InspectorAction::OpenLspHoverLink(_)) => {
                let close_overlay = if self.context_inspector_is_overlay() {
                    self.set_context_inspector_open(false)
                } else {
                    Task::none()
                };
                Task::batch([close_overlay, self.update_lsp(action)])
            }
            command => {
                let close_overlay = if self.context_inspector_is_overlay() {
                    self.set_context_inspector_open(false)
                } else {
                    Task::none()
                };
                let command =
                    match command {
                        InspectorAction::NewTerminalAt(path) => self.add_terminal(path),
                        InspectorAction::Retry(card) => {
                            cosmic::task::message(Message::RetryCard(card))
                        }
                        InspectorAction::ReloadBrowser(card) => {
                            #[cfg(feature = "integrated-browser")]
                            {
                                cosmic::task::message(Message::BrowserReload(card))
                            }
                            #[cfg(not(feature = "integrated-browser"))]
                            {
                                cosmic::task::message(Message::RetryCard(card))
                            }
                        }
                        InspectorAction::OpenBrowser(url) => self.open_clicked_link(url),
                        InspectorAction::RefreshRepository(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::RefreshFiles { card }),
                        ),
                        InspectorAction::BackToRepositoryFiles(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::BackToFiles { card }),
                        ),
                        InspectorAction::SaveRepositoryFile(card) => {
                            cosmic::task::message(Message::Repository(RepositoryAction::Save {
                                card,
                                overwrite: false,
                            }))
                        }
                        InspectorAction::OverwriteRepositoryFile(card) => {
                            cosmic::task::message(Message::Repository(RepositoryAction::Save {
                                card,
                                overwrite: true,
                            }))
                        }
                        InspectorAction::ReloadRepositoryFile(card) => {
                            cosmic::task::message(Message::Repository(RepositoryAction::Reload {
                                card,
                            }))
                        }
                        InspectorAction::DiscardRepositoryEdits(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::DiscardEdits { card }),
                        ),
                        InspectorAction::ShowRepositoryEditor(card) => {
                            cosmic::task::message(Message::Repository(
                                RepositoryAction::ApplySourceSurface { card, diff: false },
                            ))
                        }
                        InspectorAction::ShowRepositoryDiff(card) => {
                            cosmic::task::message(Message::Repository(
                                RepositoryAction::ApplySourceSurface { card, diff: true },
                            ))
                        }
                        InspectorAction::ShowMarkdownEdit(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::ApplyMarkdownMode {
                                card,
                                preview: false,
                            }),
                        ),
                        InspectorAction::ShowMarkdownPreview(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::ApplyMarkdownMode {
                                card,
                                preview: true,
                            }),
                        ),
                        InspectorAction::ShowStagedDiff(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::ApplyDiffBasis {
                                card,
                                basis: DiffBasis::HeadToIndex,
                            }),
                        ),
                        InspectorAction::ShowUnstagedDiff(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::ApplyDiffBasis {
                                card,
                                basis: DiffBasis::IndexToWorktree,
                            }),
                        ),
                        InspectorAction::ShowInlineDiff(card) => {
                            cosmic::task::message(Message::Repository(
                                RepositoryAction::ApplyDiffMode { card, split: false },
                            ))
                        }
                        InspectorAction::ShowSplitDiff(card) => {
                            cosmic::task::message(Message::Repository(
                                RepositoryAction::ApplyDiffMode { card, split: true },
                            ))
                        }
                        InspectorAction::RetryRepositoryDiff(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::RetryDiff { card }),
                        ),
                        InspectorAction::PreviousRepositoryChange(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::PreviousChange { card }),
                        ),
                        InspectorAction::NextRepositoryChange(card) => cosmic::task::message(
                            Message::Repository(RepositoryAction::NextChange { card }),
                        ),
                        InspectorAction::CreatePullRequest {
                            project_id,
                            worktree,
                        } => cosmic::task::message(Message::Sidebar(
                            SidebarAction::CreatePullRequest {
                                project_id,
                                worktree: Some(worktree),
                            },
                        )),
                        InspectorAction::FocusProjectSearch => {
                            cosmic::task::message(Message::FocusProjectSearch)
                        }
                        InspectorAction::CloseCard(card) => {
                            cosmic::task::message(Message::CloseCard(card))
                        }
                        InspectorAction::Close
                        | InspectorAction::ToggleDiagnostics(_)
                        | InspectorAction::Resize { .. }
                        | InspectorAction::ResizeFinished
                        | InspectorAction::JumpToLspDiagnostic { .. }
                        | InspectorAction::JumpToLspLocation { .. }
                        | InspectorAction::ApplyLspCompletion { .. }
                        | InspectorAction::RequestLspHover(_)
                        | InspectorAction::GoToLspDefinition(_)
                        | InspectorAction::FindLspReferences(_)
                        | InspectorAction::RestartLanguageServer(_)
                        | InspectorAction::DismissLspResults(_)
                        | InspectorAction::OpenLspHoverLink(_) => {
                            unreachable!("handled above")
                        }
                    };
                Task::batch([close_overlay, command])
            }
        }
    }

    fn focused_repository_is_saving(&self) -> bool {
        focused_save_in_flight(
            self.workspace.focused(),
            |card| matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving()),
        )
    }

    fn release_deferred_navigation_if_unblocked(&mut self) -> Task<Message> {
        let repository_blocks = self.workspace.focused().is_some_and(|card| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        });
        if self.modal_is_open() || repository_blocks {
            return Task::none();
        }
        Task::batch(
            self.deferred_navigation
                .drain(..)
                .map(cosmic::task::message),
        )
    }

    /// Leaves the application: the window first, the process once the window
    /// is gone and the language servers have actually been reaped.
    ///
    /// The window is the only part of the exit anyone sees, so it goes at
    /// once. What follows - the reap below, the final session flush in
    /// `Drop`, the renderer and runtime teardown after `run` returns - takes
    /// seconds on a busy machine, and winit destroys a closed toplevel only on
    /// an event-loop pass after the close: an exit that reaches iced first
    /// leaves the window on screen, unresponsive, until the process is gone.
    ///
    /// `shutdown` only *asks*: the processes are stopped by session tasks on
    /// the tokio runtime, and `iced::exit()` takes that runtime away. Waiting
    /// off the update thread and exiting on the answer is what keeps a
    /// `rust-analyzer` - and the proc-macro server it owns - from outliving the
    /// application that spawned it. The wait is bounded by the module's own
    /// reap timeout, so a server that refuses to die cannot hold the exit.
    fn exit_after_language_servers(&mut self) -> Task<Message> {
        if self.exit.is_some() {
            return Task::none();
        }
        // The process outlives its window by the reap and the teardown below.
        // An activation acknowledged in that time would tell `cagents ui` a
        // window is being raised, and it would open none. Refused, the
        // launcher waits for this process to release the window lease and
        // starts a fresh window.
        self.activation_rx.close();
        // `Drop` writes the final state; this snapshot is queued now because
        // someone who has watched the window close may power off before then.
        self.persist();
        let reaped = self.lsp.begin_shutdown();
        let reap = cosmic::task::future(async move {
            reaped.await;
            Message::LanguageServersReaped
        });
        let close = self.close_main_window();
        self.exit = Some(ApplicationExit {
            closing: close.as_ref().map(|(window, _)| *window),
            servers_reaped: false,
        });
        let Some((window, close)) = close else {
            return reap;
        };
        let close_timeout = cosmic::task::future(async move {
            tokio::time::sleep(EXIT_WINDOW_CLOSE_TIMEOUT).await;
            Message::WindowClosed(window)
        });
        Task::batch([close, reap, close_timeout])
    }

    /// Records one half of the exit and ends the process once both are done.
    fn advance_exit(&mut self, advance: impl FnOnce(&mut ApplicationExit)) -> Task<Message> {
        let Some(exit) = self.exit.as_mut() else {
            return Task::none();
        };
        advance(exit);
        if !exit.is_complete() {
            return Task::none();
        }
        // The window's closed report and this exit can land in the very
        // event-loop pass that destroyed the window, and iced would then tear
        // down before winit flushed that destruction to the compositor.
        iced::Task::future(async { tokio::time::sleep(EXIT_FLUSH_GRACE).await })
            .discard()
            .chain(iced::exit())
    }

    /// Closes the main window while the application keeps running, or
    /// `None` when there is no window to close.
    ///
    /// libcosmic answers the main window closing with an immediate
    /// `iced::exit()`, which would take the runtime away from the reap that
    /// [`Self::exit_after_language_servers`] is still waiting on and orphan
    /// every server not yet stopped. With no main window the close is only a
    /// close; [`Application::view_window`] draws the window empty until iced
    /// removes it.
    fn close_main_window(&mut self) -> Option<(window::Id, Task<Message>)> {
        let window = self.core.main_window_id()?;
        #[cfg(feature = "integrated-browser")]
        self.release_browser_parent();
        self.core.set_main_window_id(None);
        Some((window, iced::window::close(window)))
    }

    /// Tears down everything that borrows the main window's Wayland surface.
    ///
    /// Exact Wayland parent invariant, which must hold before the parent
    /// window is closed or dropped:
    /// 1. request and join every browser still owned by a card;
    /// 2. join every off-UI-path card-close reaper;
    /// 3. drop the raw-handle factory/Wayland host;
    /// 4. deactivate/drop the parent lifetime guard.
    ///
    /// Idempotent: the exit path runs it before closing the window, and
    /// `Drop` runs it again for an exit this application did not drive.
    #[cfg(feature = "integrated-browser")]
    fn release_browser_parent(&mut self) {
        let cards = self
            .cards
            .iter()
            .filter(|(_, runtime)| matches!(runtime, CardRuntime::Browser(_)))
            .map(|(card, _)| *card)
            .collect::<Vec<_>>();
        let mut browsers = Vec::with_capacity(cards.len());
        for card in cards {
            if let Some(CardRuntime::Browser(browser)) = self.cards.remove(&card) {
                // Every engine is asked before any is awaited, so they tear
                // down together rather than one after another.
                browser.handle.request_shutdown();
                browsers.push(browser);
            }
        }
        for mut browser in browsers {
            browser.handle.shutdown();
        }
        for teardown in self.browser_teardowns.drain(..) {
            let _ = teardown.join();
        }
        self.browser_factory.take();
        if let Some(parent_lifetime) = self.browser_parent_lifetime.take() {
            parent_lifetime.deactivate();
        }
    }

    fn activate_main_window(&self, token: Option<String>) -> Task<Message> {
        iced::Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Activate(
            token.unwrap_or_default(),
        )))
    }

    fn primary_dirty_close_control(&self, card: CardId) -> DirtyCloseControl {
        match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) if runtime.can_save() => DirtyCloseControl::Save,
            Some(CardRuntime::Repository(runtime)) if runtime.can_abandon_unsaved_changes() => {
                DirtyCloseControl::Discard
            }
            _ => DirtyCloseControl::Cancel,
        }
    }

    fn begin_dirty_close(
        &mut self,
        intent: DirtyCloseIntent,
        mut cards: VecDeque<CardId>,
    ) -> Task<Message> {
        let Some(card) = cards.pop_front() else {
            return match intent {
                DirtyCloseIntent::Card(card) => cosmic::task::message(Message::CloseCard(card)),
                DirtyCloseIntent::Application(_) => self.exit_after_language_servers(),
            };
        };
        // Keep a stable workspace fallback even when another modal is
        // currently underneath. That modal may finish its own close animation
        // while dirty-close is topmost; Cancel re-checks the live modal tree
        // and uses this only when none remains.
        let restore_card = self.workspace.focused();
        let focused = self.primary_dirty_close_control(card);
        self.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            remaining: cards,
            intent,
            focused,
            restore_card,
        });
        iced::widget::operation::focus(dirty_close_control_id(card, focused))
            .map(cosmic::Action::App)
    }

    fn advance_dirty_close(&mut self) -> Task<Message> {
        let Some(mut close) = self.dirty_repository_close.take() else {
            return Task::none();
        };
        // Every close resolution is revalidated at this boundary. In
        // particular, discarding an editor buffer cannot clear an unresolved
        // post-persist durability warning, and no stale control message may
        // skip the current repository and reach card teardown or app exit.
        if matches!(self.cards.get(&close.card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        {
            let card = close.card;
            let focused = self.primary_dirty_close_control(card);
            close.focused = focused;
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(card, focused))
                .map(cosmic::Action::App);
        }
        if let Some(card) = close.next_unresolved(|card| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        }) {
            let focused = self.primary_dirty_close_control(card);
            close.focused = focused;
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(
                card,
                focused,
            ))
            .map(cosmic::Action::App);
        }
        match close.intent {
            DirtyCloseIntent::Card(card) => cosmic::task::message(Message::CloseCard(card)),
            DirtyCloseIntent::Application(_) => self.exit_after_language_servers(),
        }
    }

    fn cancel_dirty_close(&mut self) -> Task<Message> {
        if self.dirty_repository_close.as_ref().is_some_and(|close| {
            matches!(self.cards.get(&close.card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
        }) {
            return Task::none();
        }
        let Some(close) = self.dirty_repository_close.take() else {
            return Task::none();
        };
        let restore_focus = self.nested_modal_focus_target().unwrap_or_else(|| {
            resolve_restore_card(close.restore_card, self.workspace.focused(), |card| {
                self.workspace.order().any(|candidate| candidate == card)
            })
            .map(|card| self.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
        });
        iced::widget::operation::focus(restore_focus).map(cosmic::Action::App)
    }

    fn nested_modal_focus_target(&self) -> Option<widget::Id> {
        if let Some(dialog) = &self.sidebar_selection_ui.dialog {
            Some(dialog.focus_target())
        } else if self.sidebar_state.pending_discard().is_some() {
            Some(sidebar::discard_confirm_id())
        } else if self.project_ui.modal_open() {
            Some(self.project_ui.focus_target())
        } else if self.channel_ui.modal_open() {
            Some(channel_ui::State::focus_target())
        } else if self.worktree_dialog.is_some() {
            Some(worktree_dialog_focus_id())
        } else if self.pull_request_dialog.is_some() {
            Some(pull_request_dialog_focus_id())
        } else if self.context_inspector_is_overlay() {
            Some(context_inspector_toggle_id())
        } else {
            None
        }
    }

    fn restore_pull_request_dialog(
        &mut self,
        dialog: PullRequestDialog,
        generation: u64,
    ) -> Task<Message> {
        if generation != self.pull_request_dialog_generation || self.pull_request_dialog.is_some() {
            return Task::none();
        }
        self.pull_request_dialog = Some(dialog);
        if self.dirty_repository_close.is_some()
            || self.sidebar_state.pending_discard().is_some()
            || self.worktree_dialog.is_some()
        {
            return Task::none();
        }
        iced::widget::operation::focus(pull_request_dialog_focus_id()).map(cosmic::Action::App)
    }

    fn focus_context_overlay_if_exposed(&self) -> Task<Message> {
        if self.context_inspector_is_overlay()
            && !self.modal_is_open()
            && !self.focused_repository_is_saving()
        {
            iced::widget::operation::focus(context_inspector_toggle_id()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }

    fn cycle_dirty_close_focus(&mut self, backwards: bool) -> Task<Message> {
        let Some(close) = self.dirty_repository_close.as_mut() else {
            return Task::none();
        };
        let saving = matches!(
            self.cards.get(&close.card),
            Some(CardRuntime::Repository(runtime)) if runtime.is_saving()
        );
        if saving {
            return Task::none();
        }
        let discardable = matches!(
            self.cards.get(&close.card),
            Some(CardRuntime::Repository(runtime)) if runtime.can_abandon_unsaved_changes()
        );
        close.focused = next_dirty_close_control(close.focused, backwards, saving, discardable);
        iced::widget::operation::focus(dirty_close_control_id(close.card, close.focused))
            .map(cosmic::Action::App)
    }

    fn utility_card(&self, kind: &CardKind) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| card.existence.is_open() && card.kind == *kind)
            .map(|card| card.id)
    }

    /// Whether the settings pane is the focused card.
    fn settings_card_focused(&self) -> bool {
        self.workspace
            .focused()
            .is_some_and(|card| matches!(self.cards.get(&card), Some(CardRuntime::Settings)))
    }

    /// Shows the window's one pane of a utility `kind` where the user is.
    ///
    /// A utility pane shows application state, not a place, so like a browser
    /// it is nobody's worktree: in a worktree it joins every worktree's strip,
    /// and in a channel it joins that channel. Asking for it again from
    /// somewhere it is not visible moves it here instead of taking the user
    /// away from their work to wherever it was last opened.
    fn open_utility_card(&mut self, kind: CardKind) -> Task<Message> {
        debug_assert!(kind.is_utility());
        let Some(id) = self.utility_card(&kind) else {
            return self.add_utility_card(kind);
        };
        // The pane is on its way out and cannot take focus; asking for it
        // again means the user wants it after all, so it comes back as soon
        // as the close lands rather than the request being lost.
        if self
            .workspace
            .pending_close()
            .is_some_and(|handoff| handoff.unmounting == id)
        {
            self.reopen_utility = Some(kind);
            return Task::none();
        }
        let rehome = if self.workspace.order().any(|card| card == id) {
            Task::none()
        } else {
            self.rehome_utility_card(id)
        };
        let focus = match self.workspace.focus(id) {
            // Focusing also hands the pane the keyboard, even when it was
            // already the focused card.
            Ok(update) => self.apply_viewport(update, true, false),
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            }
        };
        Task::batch([rehome, focus])
    }

    fn add_utility_card(&mut self, kind: CardKind) -> Task<Message> {
        let id = CardId::new();
        let title = card_kind_labels(Some(&kind)).type_label.to_owned();
        let runtime = CardRuntime::utility(&kind);
        let card = CardState {
            id,
            generation: self.durable.generation,
            title: title.clone(),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind,
        };
        let (index, durable_index) = self.terminal_insertion_indices();
        let update = match self.workspace.add_card(
            WorkspaceCard::new(id, title, CardWidth::Fraction(card.width_fraction))
                .expect("utility card has a valid width"),
            index,
            true,
        ) {
            Ok(update) => update,
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                return Task::none();
            }
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.insert(durable_index, card);
        durable.focused = Some(id);
        self.register_channel_card(id);
        self.cards.insert(id, runtime);
        self.persist();
        self.apply_viewport(update, true, false)
    }

    /// Moves an open utility pane into the strip on screen, beside the
    /// focused card, out of whichever channel held it.
    fn rehome_utility_card(&mut self, id: CardId) -> Task<Message> {
        let (index, durable_index) = self.terminal_insertion_indices();
        let durable = Arc::make_mut(&mut self.durable);
        let Some(from) = durable.cards.iter().position(|card| card.id == id) else {
            return Task::none();
        };
        let card = durable.cards.remove(from);
        let (title, width) = (card.title.clone(), card.width_fraction);
        let durable_index = if from < durable_index {
            durable_index - 1
        } else {
            durable_index
        };
        durable
            .cards
            .insert(durable_index.min(durable.cards.len()), card);
        durable.card_scopes.remove(&id);
        for channel in &mut durable.channels {
            channel.cards.retain(|card| *card != id);
            if channel.focused == Some(id) {
                channel.focused = channel.cards.first().copied();
            }
        }
        self.register_channel_card(id);
        let update = match self.workspace.add_card(
            WorkspaceCard::new(id, title, CardWidth::Fraction(width))
                .expect("utility card has a valid width"),
            index,
            true,
        ) {
            Ok(update) => update,
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                return Task::none();
            }
        };
        self.persist();
        self.apply_viewport(update, true, true)
    }

    fn capture_settings_save(&mut self) -> Result<SettingsSave, String> {
        if self.settings_write_blocked {
            return Err(
                "settings were not loaded successfully; use Reset all settings to replace them"
                    .to_owned(),
            );
        }
        let path = match self.settings_path.clone() {
            Some(path) => path,
            None => settings::default_path().map_err(|error| error.to_string())?,
        };
        let mut view = WindowView {
            sidebar_width: self.settings.sidebar_width,
            inspector_width: self.settings.inspector_width,
            context_inspector_open: self.context_inspector_open,
            context_tab: self.context_tab,
            sidebar: self.sidebar_state.saved_view(),
            worktree_offsets: self.worktree_offsets.clone(),
        };
        if self.durable.active_channel.is_none()
            && let Some(path) = &self.active_workspace
        {
            view.worktree_offsets
                .insert(path.clone(), self.workspace.camera().offset());
        }
        self.settings_save_generation += 1;
        Ok(SettingsSave {
            generation: self.settings_save_generation,
            path,
            base: self.settings_base.clone(),
            desired: self.settings.clone(),
            view,
            store: self.session.as_ref().map(SessionRuntime::operation_store),
            lane: Arc::clone(&self.settings_save_lane),
            reset: self.settings_reset_generation.is_some(),
        })
    }

    fn acknowledge_settings_save(&mut self, saved: SettingsSaved) {
        if saved.generation >= self.settings_ack_generation {
            self.settings_ack_generation = saved.generation;
            self.settings_base = saved.settings;
            self.settings_path = Some(saved.path);
            if self
                .settings_reset_generation
                .is_some_and(|generation| saved.generation >= generation)
            {
                self.settings_reset_generation = None;
            }
        }
    }

    fn save_settings_now(&mut self) -> Result<(), String> {
        let saved = self.capture_settings_save()?.run()?;
        self.acknowledge_settings_save(saved);
        Ok(())
    }

    fn save_settings_task(&mut self, reset: bool) -> Task<Message> {
        if reset {
            self.settings_reset_generation = Some(self.settings_save_generation + 1);
        }
        let save = self.capture_settings_save();
        let generation = self.settings_save_generation;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || save?.run())
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::SettingsSaved { generation, result }
        })
    }

    fn capture_workspace_save(&mut self) -> Result<WorkspaceSave, String> {
        let flush = self
            .session
            .as_ref()
            .ok_or_else(|| "workspace storage unavailable".to_owned())?
            .request_flush()?;
        Ok(WorkspaceSave {
            settings: self.capture_settings_save()?,
            flush,
        })
    }

    fn synchronize_lifecycle_budget(&mut self) {
        self.lifecycle.update_budget(LifecycleBudget {
            max_hot_cards: self.settings.max_hot_cards,
            offscreen_grace: Duration::from_millis(self.settings.hibernation_delay_ms),
        });
    }

    /// Re-derives the pending sweep from the manager's own view of the grace,
    /// so the wakeup lands exactly when the earliest hot offscreen card
    /// exhausts it.
    ///
    /// The pending deadline is kept as a ceiling rather than discarded: a card
    /// the workspace has already moved offscreen is not counted until a
    /// reconcile records its exposure, and dropping that wakeup would leave the
    /// card hot until some unrelated event happened to arm another sweep.
    fn reschedule_offscreen_lifecycle_sweep(&mut self) -> Task<Message> {
        let now = Instant::now();
        let derived = self
            .lifecycle
            .next_offscreen_sweep_delay(now)
            .and_then(|delay| now.checked_add(delay));
        let Some(derived) = derived else {
            return Task::none();
        };
        if sweep_needs_rearming(self.lifecycle_sweep_deadline, derived) {
            self.arm_lifecycle_sweep(derived)
        } else {
            Task::none()
        }
    }

    fn synchronize_terminal_theme(&mut self) {
        for (card, runtime) in &self.cards {
            if ended_terminal(self.card_state(*card)).is_some() {
                continue;
            }
            if let CardRuntime::Terminal(surface) = runtime {
                if let Err(error) = surface.set_theme(self.settings.terminal_theme) {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not apply terminal theme: {error}"
                    )));
                }
            }
        }
    }

    fn synchronize_appearance(&mut self, previous_scale: f32) -> Task<Message> {
        self.synchronize_terminal_theme();
        // COSMIC applies its scale action asynchronously. Use the previous
        // persisted preference, not Core's possibly not-yet-updated value.
        self.window_width *= previous_scale / self.settings.ui_scale;
        self.window_height *= previous_scale / self.settings.ui_scale;
        #[cfg(feature = "integrated-browser")]
        for runtime in self.cards.values() {
            if let CardRuntime::Browser(browser) = runtime {
                let _ = browser.handle.set_scale_factor(effective_surface_scale(
                    self.window_scale_factor,
                    self.settings.ui_scale,
                ));
            }
        }
        let theme = self.settings.app_theme.resolve(self.core.system_theme());
        let overlay = self.sync_context_overlay_state();
        let documents = self.retone_documents(&theme);
        Task::batch([
            cosmic::command::set_theme(theme),
            overlay,
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
        Task::batch(self.cards.iter_mut().filter_map(|(&card, runtime)| {
            if let CardRuntime::Repository(runtime) = runtime {
                Some(repository_task(runtime.retone_document(card, theme)))
            } else {
                None
            }
        }))
    }

    /// Renders the open document pages again for the theme now in force.
    ///
    /// The theme is read from the global here, which is the one case where
    /// that is the right source: the shell's change has already been applied
    /// to it by the time anything asks this, and the notice that it changed
    /// carried no theme this application is allowed to keep.
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
    /// then discards on the path a light or dark flip actually takes. A
    /// message of this application's own reaches `update`, where a returned
    /// task is honoured.
    fn document_tone_subscription(&self) -> Subscription<Message> {
        self.core
            .watch_config::<cosmic::cosmic_theme::ThemeMode>(cosmic::cosmic_theme::THEME_MODE_ID)
            .map(|_| Message::DocumentToneStale)
    }

    fn refresh_settings_feedback(&mut self) {
        self.settings_feedback = if let Some(error) = &self.settings_save_error {
            Some(SettingsFeedback::Error(error.clone()))
        } else if !self.settings_invalid.is_empty() {
            Some(SettingsFeedback::Invalid)
        } else if !self.settings_pending.is_empty()
            || self.settings_ack_generation < self.settings_save_generation
        {
            Some(SettingsFeedback::Pending)
        } else {
            Some(SettingsFeedback::Saved)
        };
    }

    fn queue_setting_input(&mut self, field: SettingsField, value: String) -> Task<Message> {
        self.settings_draft.set(field, value.clone());
        self.settings_generation = self.settings_generation.wrapping_add(1);
        let generation = self.settings_generation;
        self.settings_pending.insert(field, (generation, value));
        self.refresh_settings_feedback();
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            Message::SettingsAutosave { field, generation }
        })
    }

    fn autosave_setting(&mut self, field: SettingsField, generation: u64) -> Task<Message> {
        if !self
            .settings_pending
            .get(&field)
            .is_some_and(|pending| pending.0 == generation)
        {
            return Task::none();
        }
        let (_, value) = self
            .settings_pending
            .remove(&field)
            .expect("matched pending draft");
        self.apply_setting_input(field, value)
    }

    /// Commits the pane's drafts when focus leaves it, so an edit is never
    /// left pending behind a pane the user has moved on from.
    fn flush_settings_on_blur(&mut self) -> Task<Message> {
        let focused = self.settings_card_focused();
        let blurred = std::mem::replace(&mut self.settings_pane_focused, focused) && !focused;
        if blurred {
            self.flush_settings_drafts()
        } else {
            Task::none()
        }
    }

    fn flush_settings_drafts(&mut self) -> Task<Message> {
        let mut pending: Vec<_> = self.settings_pending.drain().collect();
        pending.sort_by_key(|(_, (generation, _))| *generation);
        Task::batch(
            pending
                .into_iter()
                .map(|(field, (_, value))| self.apply_setting_input(field, value))
                .collect::<Vec<_>>(),
        )
    }

    /// Folds drafts still waiting out their debounce into the settings the
    /// window is about to save on its way out, without the live effects -
    /// lifecycle sweeps, theme changes, editor auto-saves - that applying
    /// them normally triggers, since nothing is left to show them.
    fn fold_settings_drafts(&mut self) {
        let mut pending: Vec<_> = self.settings_pending.drain().collect();
        pending.sort_by_key(|(_, (generation, _))| *generation);
        for (field, (_, value)) in pending {
            if let SettingsDecision::Persist(settings) =
                settings_update(&self.settings, field, &value)
            {
                self.settings = settings;
            }
        }
    }

    fn apply_setting_input(&mut self, field: SettingsField, value: String) -> Task<Message> {
        self.settings_pending.remove(&field);
        self.settings_draft.set(field, value.clone());
        match settings_update(&self.settings, field, &value) {
            SettingsDecision::Invalid(message) => {
                self.settings_invalid.insert(field, message);
                self.refresh_settings_feedback();
                Task::none()
            }
            SettingsDecision::Unchanged => {
                self.settings_invalid.remove(&field);
                if self.settings_save_error.is_some() {
                    return self.save_settings_task(false);
                }
                self.refresh_settings_feedback();
                Task::none()
            }
            SettingsDecision::Persist(settings) => {
                let previous = std::mem::replace(&mut self.settings, settings);
                self.settings_invalid.remove(&field);
                let save = self.save_settings_task(false);
                self.refresh_settings_feedback();
                let effect = self.synchronize_settings(&previous);
                Task::batch([save, effect])
            }
        }
    }

    /// Applies every live effect of the settings changing from `previous` to
    /// the ones now in force.
    ///
    /// Effects are derived from what changed rather than from which control
    /// changed it, so a single edit and a wholesale reset reach the same
    /// state: a reset that skipped one leaves the pane showing a value the
    /// rest of the application - or the daemon - is not using.
    fn synchronize_settings(&mut self, previous: &AppSettings) -> Task<Message> {
        self.synchronize_lifecycle_budget();
        let mut tasks = Vec::new();
        if previous.keyboard_shortcuts != self.settings.keyboard_shortcuts {
            self.card_chrome.clear();
        }
        if previous.worktree_sort != self.settings.worktree_sort {
            self.sidebar_state
                .set_worktree_sort(self.settings.worktree_sort);
        }
        if previous.app_theme != self.settings.app_theme
            || previous.ui_scale != self.settings.ui_scale
        {
            // Also re-applies the terminal theme.
            tasks.push(self.synchronize_appearance(previous.ui_scale));
        } else if previous.terminal_theme != self.settings.terminal_theme {
            self.synchronize_terminal_theme();
        }
        let (wrap_lines, syntax_theme) = (
            self.settings.editor_wrap_lines,
            self.settings.editor_syntax_theme,
        );
        if previous.editor_wrap_lines != wrap_lines || previous.editor_syntax_theme != syntax_theme
        {
            for runtime in self.cards.values_mut() {
                match runtime {
                    CardRuntime::Repository(runtime) => {
                        runtime.set_wrap_lines(wrap_lines);
                        runtime.set_syntax_theme(syntax_theme);
                    }
                    CardRuntime::PullRequests(pane) => {
                        pane.runtime.set_wrap_lines(wrap_lines);
                        pane.runtime.set_syntax_theme(syntax_theme);
                    }
                    CardRuntime::Chat(runtime) => runtime.set_syntax_theme(syntax_theme),
                    _ => {}
                }
            }
        }
        if previous.editor_auto_save != self.settings.editor_auto_save {
            let auto_save = self.settings.editor_auto_save;
            tasks.extend(self.cards.iter_mut().filter_map(|(&card, runtime)| {
                let CardRuntime::Repository(runtime) = runtime else {
                    return None;
                };
                runtime.set_auto_save(auto_save);
                Some(repository_task(runtime.auto_save_task(card)))
            }));
        }
        if previous.show_status_bar != self.settings.show_status_bar {
            tasks.push(self.accounts_surface_changed());
        }
        if previous.notifications != self.settings.notifications {
            self.notification_ui
                .preferences(self.settings.notifications);
        }
        if previous.hibernation_delay_ms != self.settings.hibernation_delay_ms {
            // Reconciled here rather than through a queued `LifecycleSweep`,
            // so the manager's exposure is current before the new grace is
            // re-derived from it.
            tasks.push(self.lifecycle_sweep());
            tasks.push(self.reschedule_offscreen_lifecycle_sweep());
        } else if previous.max_hot_cards != self.settings.max_hot_cards {
            tasks.push(cosmic::task::message(Message::LifecycleSweep));
        }
        // A no-op unless the configured list changed; a reset restores the
        // built-in list, a different set of processes whenever the user had
        // configured their own.
        tasks.push(self.synchronize_language_servers());
        Task::batch(tasks)
    }

    /// Rebuilds the language-server manager when the configured list no longer
    /// describes the processes it is running.
    ///
    /// A manager's server set is fixed at construction and its shutdown is
    /// one-way, so a changed list is a new manager. Every open buffer is
    /// re-announced to the replacement: leaving them would show the user files
    /// that look checked and are not.
    fn synchronize_language_servers(&mut self) -> Task<Message> {
        if self.lsp.servers() == self.settings.language_servers.as_slice() {
            return Task::none();
        }
        // `shutdown` alone moves the sessions into the outgoing manager's own
        // retiring list and kills nothing, and dropping that manager drops
        // them unreaped - after which no exit path can reach them, because
        // every exit path addresses the manager that replaced it.
        // `begin_shutdown` spawns the reap on the runtime the sessions run on,
        // so it survives the returned future being dropped here.
        drop(self.lsp.begin_shutdown());
        self.lsp = LspManager::new(self.settings.language_servers.clone());
        self.lsp_contexts.clear();
        self.lsp_documents.clear();
        self.lsp_pending_jumps.clear();
        let cards: Vec<CardId> = self.cards.keys().copied().collect();
        let mut tasks = Vec::with_capacity(cards.len());
        for card in cards {
            if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                runtime.set_lsp_document(None);
                runtime.resync_lsp();
            }
            tasks.push(self.drain_lsp_intents(card));
        }
        Task::batch(tasks)
    }

    fn reset_settings(&mut self) -> Task<Message> {
        let previous = std::mem::take(&mut self.settings);
        self.settings_draft = SettingsDraft::from_settings(&self.settings);
        self.settings_pending.clear();
        self.settings_invalid.clear();
        self.settings_write_blocked = false;
        let save = self.save_settings_task(true);
        self.refresh_settings_feedback();
        let effects = self.synchronize_settings(&previous);
        Task::batch([save, effects])
    }
}

impl Application for Cagents {
    type Executor = cosmic::executor::Default;
    type Flags = (
        Option<async_channel::Receiver<instance::UiActivation>>,
        Startup,
    );
    type Message = Message;

    const APP_ID: &'static str = instance::APPLICATION_ID;

    fn on_app_exit(&mut self) -> Option<Message> {
        self.core
            .main_window_id()
            .map(Message::WindowCloseRequested)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::WindowCloseRequested(id))
    }

    /// The shell's own theme changed under us.
    ///
    /// An open document page has the tone it was rendered with baked into its
    /// pixels, so a shell that went from light to dark leaves the reader with
    /// exactly the glaring page this feature exists to prevent. Only the
    /// staleness is recorded here, and never the render itself: libcosmic
    /// discards whatever this returns when the notice arrives as a theme
    /// change rather than a mode change (`app/cosmic.rs` returns its own
    /// window commands and drops this task), so dispatching from here would
    /// leave a page waiting on a render that was never started.
    ///
    /// The theme applied to the window is only the shell's while the
    /// preference follows the system. An explicit Dark or Light choice keeps
    /// the theme this application already set, and that path re-tones through
    /// `synchronize_appearance`, whose task is honoured.
    fn system_theme_update(
        &mut self,
        _keys: &[&'static str],
        _theme: &cosmic::cosmic_theme::Theme,
    ) -> Task<Message> {
        self.document_tone_stale = self.settings.app_theme == AppTheme::System;
        Task::none()
    }

    fn init(mut core: Core, (activation_rx, startup): Self::Flags) -> (Self, Task<Message>) {
        std::thread::spawn(chat_ui::prune_pasted_images);
        core.window.show_headerbar = false;
        core.window.border_padding = Some(0);
        #[cfg(feature = "latency-harness")]
        let latency_fixture = crate::latency_fixture::Fixture::from_env();
        let Bootstrap {
            workspace: durable,
            session,
            mut projects,
            mut worktrees,
            mut settings,
            settings_path,
            settings_write_blocked,
            status,
            save_health,
            restore_warnings,
            settings_warnings,
        } = startup.bootstrap;
        reconcile_restored_projects(&mut projects, &mut worktrees, &durable.project_order);
        let window_view = startup.window_view;
        if let Some(view) = &window_view {
            settings.sidebar_width = sidebar::clamp_expanded_width(view.sidebar_width);
            settings.inspector_width = if view.inspector_width.is_finite() {
                view.inspector_width
                    .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH)
            } else {
                AppSettings::default().inspector_width
            };
        }
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
                            worktree.project_id == project.id
                                && worktree.existence.is_open()
                                && worktree_ui::worktree_directory_available(&worktree.path)
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
                .with_kind(project.kind)
            })
            .collect();
        let workspace = Self::rebuild_workspace(&durable);
        let chat_drafts = durable.chat_drafts.clone();
        let (terminal_wake_tx, terminal_wake_rx) = async_channel::unbounded();
        // The daemon owns the terminal inventory; this window observes it for
        // the whole of its life, so the observer starts with the window rather
        // than with the first card that happens to need a daemon.
        let (hosted_terminals_tx, hosted_terminals_rx) = async_channel::unbounded();
        terminal_adoption::observe_hosted_inventory(hosted_terminals_tx);
        // The daemon owns the shared sidebar model for the same reason it owns
        // the terminal inventory: it outlives every window. Both threads start
        // with the window, so a channel created elsewhere appears without the
        // window having to do anything first.
        let (workspace_model_updates, workspace_model_rx) = async_channel::unbounded();
        let (workspace_model_tx, workspace_model_requests) = async_channel::unbounded();
        workspace_model_sync::observe_workspace_model(workspace_model_updates.clone());
        workspace_model_sync::publish_workspace_model(
            workspace_model_updates,
            workspace_model_requests,
        );
        let activation_rx = activation_rx.unwrap_or_else(|| async_channel::unbounded().1);
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
            lsp: LspManager::new(settings.language_servers.clone()),
            lsp_documents: HashMap::new(),
            lsp_contexts: HashMap::new(),
            lsp_pending_jumps: HashMap::new(),
            next_lsp_document: 0,
            terminal_wake_tx,
            terminal_wake_rx,
            hosted_terminals_rx,
            terminal_starts: std::sync::Mutex::new(HashMap::new()),
            pending_adoptions: Vec::new(),
            restored_agent_attaches: VecDeque::new(),
            relaunched_terminals: HashSet::new(),
            terminal_crash_notices: HashMap::new(),
            workspace_model: crate::workspace_model::WorkspaceModel::default(),
            workspace_model_opens: HashSet::new(),
            workspace_model_rx,
            workspace_model_tx,
            activation_rx,
            notification_ui: notification_ui::State::default(),
            session,
            save_health,
            settings_base: settings.clone(),
            settings_save_generation: 0,
            settings_ack_generation: 0,
            settings_reset_generation: None,
            settings_save_lane: Arc::new(std::sync::Mutex::new(None)),
            update_prepared: false,
            update_flush_pending: false,
            update_generation: 0,
            update_committed: false,
            exit: None,
            document_tone_stale: false,
            settings,
            #[cfg(feature = "integrated-browser")]
            browser_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_import: browser_import_ui::State::default(),
            terminal_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_factory: None,
            #[cfg(feature = "integrated-browser")]
            browser_parent_lifetime: None,
            window_scale_factor,
            window_width: 1_600.0,
            window_height: 900.0,
            settings_path,
            settings_write_blocked,
            notices: NoticeLog::default(),
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, sidebar_projects),
            sidebar_state: SidebarState::default(),
            project_ui: project_ui::State::default(),
            channel_ui: channel_ui::State::default(),
            sidebar_selection_ui: sidebar_selection_ui::State::default(),
            chat_ui: chat_ui::State::with_drafts(chat_drafts),
            update_ui: update_ui::State::default(),
            branch_ui: branch_ui::State::default(),
            history_ui: history_ui::State::default(),
            refresh_ui: refresh_ui::State::default(),
            account_ui: account_ui::State::installation(),
            automation_ui: crate::automation_ui::State::default(),
            chrome_ui: chrome_ui::State::default(),
            pull_request_inspector: pull_request_ui::State::default(),
            active_workspace: None,
            worktree_focus: HashMap::new(),
            worktree_offsets: HashMap::new(),
            channel_offsets: HashMap::new(),
            worktree_owners: HashMap::new(),
            known_worktree_roots: worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect(),
            workspace_view_generation: 0,
            workspace_scroll_id: widget::Id::new("workspace-strip-0"),
            github_availability: HashMap::new(),
            review_repositories: HashMap::new(),
            repository_generations: HashMap::new(),
            repository_pr_cancellations: HashMap::new(),
            repository_load_sequence: 0,
            worktree_dialog: None,
            settings_reset_confirm: false,
            pull_request_dialog: None,
            pull_request_dialog_generation: 0,
            dirty_repository_close: None,
            terminal_close: None,
            queued_card_closes: VecDeque::new(),
            terminal_close_attempts: HashSet::new(),
            terminal_close_generation: 0,
            closed_terminal_cards: HashSet::new(),
            uncertain_terminal_proofs: HashMap::new(),
            terminal_recovery_attempts: HashSet::new(),
            pending_terminal_handoffs: HashMap::new(),
            terminal_handoff_commit_inflight: false,
            worktree_operations: HashMap::new(),
            pending_panel_open: None,
            deferred_navigation: VecDeque::new(),
            frame_clock: FrameClock::default(),
            latency: crate::performance::InteractionLatency::default(),
            latency_focus: None,
            #[cfg(feature = "latency-harness")]
            latency_fixture,
            #[cfg(feature = "latency-harness")]
            latency_fixture_started: false,
            context_inspector_transition: None,
            inspector_diagnostics_card: None,
            context_inspector_open: window_view
                .as_ref()
                .is_none_or(|view| view.context_inspector_open),
            context_tab: window_view
                .as_ref()
                .map_or(ContextTab::default(), |view| view.context_tab),
            context_inspector_overlay_active: false,
            // The initial estimate is wide, but the first compositor resize
            // can immediately mount the default-open inspector as a modal.
            // Keep a valid post-close target ready for that narrow startup.
            context_inspector_restore_focus: Some(context_inspector_toggle_id()),
            settings_category: SettingsCategory::Appearance,
            settings_search: String::new(),
            tab_history: crate::tab_history::TabHistory::default(),
            workspace_history: crate::tab_history::TabHistory::default(),
            workspace_cycle_modifiers: keyboard::Modifiers::empty(),
            keyboard_modifiers: keyboard::Modifiers::empty(),
            settings_draft,
            settings_feedback: None,
            settings_pane_focused: false,
            reopen_utility: None,
            settings_pending: HashMap::new(),
            settings_generation: 0,
            settings_invalid: HashMap::new(),
            settings_save_error: None,
            status: status.map(TransientNotice::error),
            lifecycle_sweep_deadline: None,
            lifecycle_sweep_generation: 0,
        };
        // One notice per lost card, not a single joined line: the log dedupes
        // by message, so distinct failures stay distinct and a repeated one
        // collapses into a count. The alternative this replaces was losing
        // cards with no diagnostic anywhere in the UI.
        for warning in restore_warnings.into_iter().chain(settings_warnings) {
            app.notices.record(TransientNotice::warning(warning));
        }
        if let Some(view) = window_view {
            app.sidebar_state.restore_view(view.sidebar);
            app.worktree_offsets = view.worktree_offsets;
        }
        app.sidebar_model
            .apply_project_order(&app.durable.project_order);
        app.sidebar_state
            .set_worktree_sort(app.settings.worktree_sort);
        // `view` lays the strip out against the width `Workspace` holds, which
        // starts at zero, and `PanSurface` only publishes its real bounds once
        // it has been mounted and redrawn. A strip projected against zero
        // mounts nothing but its overscan band, so without a seed the frame in
        // which the strip first appears - at startup, and again when the first
        // card arrives - flashes empty and rebuilds every card's widget state.
        // The pane and the strip split the window, so subtracting the pane from
        // the starting width is the closest estimate available before the
        // surface reports its own. The strip then replaces the estimate itself,
        // from its own measured bounds, through `WorkspaceViewportEvent`; the
        // window's own resize only re-derives the densities. Card widths are
        // fractions re-derived per frame, so an estimate costs nothing but is
        // enough to mount them.
        let sidebar_width = sidebar::effective_width(
            SidebarMode::for_width(app.window_width),
            app.settings.sidebar_width,
        );
        let inspector_width = if app.context_inspector_open
            && matches!(
                design::ShellLayout::for_width(app.window_width).inspector,
                InspectorLayout::Docked
            ) {
            app.settings.inspector_width
        } else {
            0.0
        };
        let _ = app
            .workspace
            .resize_viewport((app.window_width - sidebar_width - inspector_width).max(0.0));
        #[cfg(feature = "latency-harness")]
        if app.latency_fixture.is_some() {
            app.cards = app
                .durable
                .cards
                .iter()
                .map(|card| {
                    (
                        card.id,
                        CardRuntime::Failed(
                            "Synthetic latency workload; no terminal process".into(),
                        ),
                    )
                })
                .collect();
            app.lifecycle = LifecycleManager::new(LifecycleBudget::default());
            app.context_inspector_open = false;
            let chrome = app.sync_chrome();
            return (app, chrome);
        }
        let scope = app.restore_worktree_scope();
        // Closed/exited replacements can still owe a lost durable acknowledgement.
        for card in &app.durable.cards {
            let terminal = match &card.kind {
                CardKind::Terminal(terminal) => Some(terminal),
                CardKind::Chat(chat) => chat.terminal.as_ref(),
                _ => None,
            };
            if let Some(terminal) = terminal {
                app.pending_terminal_handoffs
                    .insert(terminal.id, terminal.clone());
            }
        }
        app.refresh_sidebar_channels();
        app.refresh_sidebar_worktree_panes();
        app.start_notifications();
        let mut tasks = vec![
            scope,
            app.commit_terminal_handoffs(),
            Self::editor_discovery_task(),
            app.rehydrate_worktree_operations(),
            app.initialize_runtimes(),
            app.synchronize_appearance(1.0),
            app.update_update_ui(update_ui::Action::Check),
            app.update_update_ui(update_ui::Action::LoadDismissal),
            app.update_update_ui(update_ui::Action::Poll),
            app.load_accounts(),
        ];
        // Unused default chats are never saved. Recreate the active scope's
        // placeholder only after restored runtimes have been initialized.
        tasks.push(app.ensure_default_pane());
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
                    app.repository_load_task(project.repository, Some(project.id), false)
                }),
        );
        let task = Task::batch(tasks);
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
        self.tab_history.observe(self.workspace.focused());
        self.workspace_history
            .observe(self.active_workspace_destination());
        // Freeze user input, not lifecycle completions. Dropping a completed
        // save/Git/spawn result would strand that operation if another window vetoed.
        if self.update_prepared && user_message_blocked_during_update(&message) {
            return Task::none();
        }
        if self.modal_is_open() {
            self.chrome_ui.close();
        } else if self.chrome_ui.is_open()
            && !matches!(&message, Message::ChromeUi(_))
            && user_workspace_message_blocked_while_modal(&message)
        {
            return Task::none();
        }
        if self.modal_is_open() && user_workspace_message_blocked_while_modal(&message) {
            return Task::none();
        }
        if self.focused_repository_is_saving() && workspace_navigation_blocked_during_save(&message)
        {
            return Task::none();
        }
        if (self.focused_repository_is_saving() || self.modal_is_open())
            && completion_navigates(&message)
        {
            self.deferred_navigation.push_back(message);
            return Task::none();
        }
        if !self.workspace.is_animating() && self.context_inspector_transition.is_none() {
            // A new animation starts from its first frame, not from time spent
            // idle while the window-frame subscription was disconnected.
            self.frame_clock.reset();
        }
        // Camera-only events cannot change labels, selected worktrees, or
        // repository bindings. Keep per-frame work proportional to visible
        // columns rather than rebuilding every card's chrome.
        let camera_only = matches!(
            &message,
            Message::Frame(_)
                | Message::Viewport {
                    event: WorkspaceViewportEvent::Scrolled(_) | WorkspaceViewportEvent::Resized(_),
                    ..
                }
        );
        let reconcile_row_selection = matches!(
            &message,
            Message::RepositoryLoaded { .. }
                | Message::AttachedWorktreePrsResolved { .. }
                | Message::GitUi(_)
                | Message::WorkspaceModelObserved(_)
                | Message::ProjectUi(_)
                | Message::ChannelUi(_)
                | Message::Sidebar(_)
                | Message::FocusCard(_)
                | Message::FocusAdjacent(_)
                | Message::WindowResized(..)
        );
        let task = match message {
            Message::NewTerminal => self.add_terminal(
                self.terminal_directory()
                    .unwrap_or_else(|| STARTING_DIRECTORY.clone()),
            ),
            Message::TerminalHandoffsCommitted { result } => {
                self.terminal_handoff_commit_inflight = false;
                let failure = match result {
                    Ok(results) => {
                        let mut failures = Vec::new();
                        for (terminal, result) in results {
                            if self.pending_terminal_handoffs.get(&terminal.id) != Some(&terminal) {
                                continue;
                            }
                            match result {
                                Ok(()) => {
                                    self.pending_terminal_handoffs.remove(&terminal.id);
                                }
                                Err(error) => failures.push((terminal.id, error)),
                            }
                        }
                        failures.first().map(|(id, error)| {
                            format!(
                                "{} terminal(s) remain pending; terminal {id}: {error}",
                                failures.len()
                            )
                        })
                    }
                    Err(error) => Some(error),
                };
                if let Some(error) = failure {
                    self.status = Some(TransientNotice::error(format!(
                        "Terminal handoff acknowledgement is pending; it will retry after saving recovers or the window refreshes: {error}"
                    )));
                    Task::none()
                } else {
                    self.commit_terminal_handoffs()
                }
            }
            Message::HostedTerminalsObserved(terminals) => {
                self.observe_hosted_descriptors(terminals)
            }
            Message::WorkspaceModelObserved(update) => self.apply_workspace_model_update(update),
            Message::TerminalStarted {
                card,
                terminal,
                result,
            } => self.terminal_started(card, terminal, result),
            Message::RestoredAgentAttached {
                card,
                terminal,
                result,
            } => {
                match result {
                    Ok(Some(started)) => self.terminal_started(card, terminal, Ok(started)),
                    Ok(None) => self.finish_terminal_start(card),
                    // Nobody is looking at this card, so its failure is not
                    // reported here. Opening it starts the terminal again and
                    // reports whatever still stands in the way.
                    Err(error) => {
                        tracing::debug!(?card, %error, "restored agent terminal stays detached");
                        self.finish_terminal_start(card)
                    }
                }
            }
            Message::TerminalWake { card, generation } => {
                let started =
                    tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::DEBUG)
                        .then(Instant::now);
                let task = self.drain_terminal(card, generation);
                if let Some(started) = started {
                    let activity_sequence = match self.cards.get(&card) {
                        Some(CardRuntime::Terminal(handle)) => {
                            handle.activity().map_or(0, |fact| fact.sequence)
                        }
                        _ => 0,
                    };
                    tracing::debug!(target: "cagents::terminal_pipeline", stage = "ui_apply", elapsed_ns = started.elapsed().as_nanos() as u64, generation, activity_sequence);
                }
                task
            }
            Message::TerminalError { card: _, message } => {
                self.status = Some(TransientNotice::error(message));
                Task::none()
            }
            Message::TerminalDropped { card, payload } => {
                let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                    return Task::none();
                };
                if ended_terminal(self.card_state(card)).is_some()
                    || self.terminal_close_pending(card)
                {
                    return Task::none();
                }
                let insertion = payload
                    .decode()
                    .insertion(crate::file_drop::Spelling::Shell);
                if !insertion.text.is_empty()
                    && let Err(error) =
                        handle.paste(insertion.text.as_str(), crate::terminal::PasteOrigin::Drop)
                {
                    self.status = Some(TransientNotice::error(error.to_string()));
                } else if let Some(refusal) = insertion.refusal() {
                    self.status = Some(TransientNotice::warning(refusal));
                }
                // A drop is aimed the way a click is, so typing carries on in
                // the pane the drop landed in.
                cosmic::task::message(Message::FocusCard(card))
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserError { card: _, message } => {
                self.status = Some(TransientNotice::error(message));
                Task::none()
            }
            #[cfg(not(feature = "integrated-browser"))]
            Message::ExternalUrlOpened(result) => {
                self.status = Some(match result {
                    Ok(()) => TransientNotice::success("Opened in the system browser"),
                    Err(error) => TransientNotice::error(error),
                });
                Task::none()
            }
            Message::OpenRepository => cosmic::task::future(async {
                let dialog = file_chooser::open::Dialog::new().title("Open project folder");
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
            }),
            Message::RepositorySelected(result) => match result {
                Ok(path) => {
                    if self.sidebar_model.projects.is_empty() {
                        self.sidebar_model.data_state = SidebarDataState::Loading;
                    }
                    // A picker path is not a repository identity: it may be a
                    // checkout, subdirectory, symlink, or linked worktree. Do
                    // not revoke an existing project's data as though this
                    // selection were an explicit refresh.
                    self.repository_load_task(path, None, true)
                }
                Err(error) => {
                    if !error.to_ascii_lowercase().contains("cancel") {
                        self.status = Some(TransientNotice::error(error));
                    }
                    Task::none()
                }
            },
            Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result,
            } => {
                if self.repository_generations.get(&project_id) != Some(&generation) {
                    return Task::none();
                }
                self.refresh_ui.projects.remove(&project_id);
                self.refresh_ui.finished(
                    project_id,
                    result.as_ref().is_ok_and(|loaded| {
                        !matches!(loaded.github, GitHubAvailability::Failed { .. })
                    }),
                );
                let mut task = Task::none();
                match result {
                    Ok(loaded) => {
                        if self.project_ui.closed.contains(&loaded.project.repository) {
                            self.repository_generations.remove(&project_id);
                            return Task::none();
                        }
                        let pr_request = if let GitHubAvailability::Ready {
                            repository_key,
                            pull_requests,
                            ..
                        } = &loaded.github
                        {
                            let key = repository_key.clone();
                            let worktrees = loaded
                                .worktrees
                                .iter()
                                .map(|(record, _)| (record.clone(), None))
                                .collect::<Vec<_>>();
                            let recent = pull_requests
                                .iter()
                                .map(|(pr, _)| pr.clone())
                                .collect::<Vec<_>>();
                            Some((key, worktrees, recent, loaded.head_repositories.clone()))
                        } else {
                            None
                        };
                        let worktree = loaded
                            .repository
                            .as_ref()
                            .and_then(|repository| repository.worktree_root.clone())
                            .or_else(|| {
                                loaded
                                    .worktrees
                                    .first()
                                    .map(|(record, _)| record.path.clone())
                            })
                            .unwrap_or_else(|| loaded.project.repository.clone());
                        // Reconcile before publishing either durable records or
                        // widget IDs. Concurrent adds have independent request
                        // IDs but still describe the same repository.
                        if let Some(existing) =
                            self.sidebar_model.projects.iter_mut().find(|project| {
                                project.id != project_id
                                    && (project.repository == loaded.project.repository
                                        || loaded.repository.as_ref().is_some_and(|repository| {
                                            project.repository_id == repository.id
                                        })
                                        || project.worktrees.iter().any(|known| {
                                            loaded
                                                .worktrees
                                                .iter()
                                                .any(|(record, _)| record.id == known.id)
                                        }))
                            })
                        {
                            // A load of that project which began after this
                            // one lists the repository more recently, whether
                            // it has landed yet or not, and it is also how a
                            // deletion is followed up. This listing may still
                            // name a worktree removed since, by this window or
                            // anyone else, so adding from it could bring one
                            // back, open; and the row's reloading or failed
                            // state is that newer load's to settle.
                            let superseded = self
                                .repository_generations
                                .get(&existing.id)
                                .is_some_and(|&latest| latest > generation);
                            let listed = if superseded {
                                &[][..]
                            } else {
                                // This read reached the same repository, so
                                // whatever an earlier failure recorded about
                                // the row it landed on no longer holds.
                                existing.unreachable = None;
                                existing.reloading = false;
                                &loaded.worktrees[..]
                            };
                            for (record, status) in listed {
                                if existing.worktrees.iter().any(|known| known.id == record.id) {
                                    continue;
                                }
                                // A newly created linked worktree can be added
                                // without replacing fresher status/PR data,
                                // pending operations, or cached project chrome.
                                if let Some(session) = &self.session {
                                    session.save_worktrees([WorktreeState {
                                        id: record.id,
                                        project_id: existing.id,
                                        generation: 0,
                                        path: record.path.clone(),
                                        branch: record.branch.as_deref().map(|branch| {
                                            branch
                                                .strip_prefix("refs/heads/")
                                                .unwrap_or(branch)
                                                .to_owned()
                                        }),
                                        existence: Existence::Open,
                                    }]);
                                }
                                existing.worktrees.push(SidebarWorktree::from_repository(
                                    record,
                                    status.as_ref(),
                                ));
                            }
                            let existing_id = existing.id;
                            self.repository_generations.remove(&project_id);
                            if let Some(previous) =
                                self.repository_pr_cancellations.remove(&project_id)
                            {
                                previous.cancel();
                            }
                            self.github_availability.remove(&project_id);
                            self.review_repositories.remove(&project_id);
                            self.sync_worktree_operation_rows();
                            self.sidebar_model.data_state = SidebarDataState::Ready;
                            let task = if open_cards && !self.refuse_in_deleting_worktree(&worktree)
                            {
                                self.select_worktree(worktree, true)
                            } else {
                                Task::none()
                            };
                            // This return skips the derived-row tail below, and
                            // the new row may already hold the pane just opened.
                            self.refresh_sidebar_channels();
                            self.refresh_sidebar_worktree_panes();
                            if open_cards {
                                // The picker can resolve to a project already in this
                                // window. Finish the explicit reopen against that row;
                                // the provisional id never gets a row of its own.
                                if let Some(repository) =
                                    self.project_ui.reopening.remove(&project_id)
                                {
                                    self.project_ui.reopening.insert(existing_id, repository);
                                    self.publish_project_opened(existing_id);
                                }
                            }
                            return task;
                        }
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
                                let mut row =
                                    SidebarWorktree::from_repository(record, status.as_ref());
                                row.head_repository =
                                    loaded.head_repositories.get(&record.id).cloned();
                                row
                            })
                            .collect();
                        let pull_requests = match &loaded.github {
                            GitHubAvailability::Ready { pull_requests, .. } => pull_requests
                                .iter()
                                .map(|(pull_request, checks)| {
                                    SidebarPullRequest::from_github(
                                        loaded.project.id,
                                        pull_request,
                                        checks,
                                    )
                                })
                                .collect(),
                            _ => self
                                .sidebar_model
                                .projects
                                .iter()
                                .find(|project| project.id == project_id)
                                .map(|project| project.display_pull_requests().to_vec())
                                .unwrap_or_default(),
                        };
                        let mut sidebar_project =
                            SidebarProject::new(
                                loaded.project.id,
                                loaded.repository.as_ref().map_or(
                                    RepositoryId(loaded.project.id.as_uuid()),
                                    |repository| repository.id,
                                ),
                                loaded.project.name.clone(),
                                loaded.project.repository.clone(),
                                worktrees,
                                pull_requests,
                            )
                            .with_kind(loaded.project.kind);
                        if let GitHubAvailability::Ready {
                            pull_requests: recent,
                            ..
                        } = &loaded.github
                        {
                            let recent =
                                recent.iter().map(|(pr, _)| pr.clone()).collect::<Vec<_>>();
                            apply_attached_worktree_prs(
                                &mut sidebar_project,
                                &loaded.worktrees,
                                &recent,
                                &[],
                            );
                        }
                        sidebar_project.github_ready = loaded.github.repository_key().is_some();
                        if !sidebar_project.github_ready {
                            sidebar_project.revoke_github();
                        }
                        sidebar_project.github_status =
                            if loaded.project.kind == ProjectKind::Folder {
                                "Folder project".into()
                            } else {
                                loaded.github.label().into()
                            };
                        if let Some(key) = loaded.review_repository.clone() {
                            self.review_repositories.insert(project_id, key);
                        } else {
                            self.review_repositories.remove(&project_id);
                        }
                        self.github_availability
                            .insert(project_id, loaded.github.clone());
                        if let Some(existing) = self
                            .sidebar_model
                            .projects
                            .iter_mut()
                            .find(|project| project.id == loaded.project.id)
                        {
                            *existing = sidebar_project;
                        } else {
                            self.sidebar_model.projects.push(sidebar_project);
                        }
                        self.sidebar_model
                            .apply_project_order(&self.durable.project_order);
                        self.sync_worktree_operation_rows();
                        self.sidebar_model.data_state = SidebarDataState::Ready;
                        let pr_enrichment =
                            if let Some((key, worktrees, recent, head_repositories)) = pr_request {
                                let expected = self
                                    .sidebar_model
                                    .projects
                                    .iter()
                                    .find(|project| project.id == project_id)
                                    .map(|project| {
                                        project
                                            .worktrees
                                            .iter()
                                            .map(|row| (row.id, row.pull_request().cloned()))
                                            .collect::<HashMap<_, _>>()
                                    })
                                    .unwrap_or_default();
                                let cancellation = self
                                    .repository_pr_cancellations
                                    .get(&project_id)
                                    .cloned()
                                    .unwrap_or_default();
                                let request_worktrees = worktrees.clone();
                                let request_recent = recent.clone();
                                cosmic::task::future(async move {
                                    let archived = tokio::task::spawn_blocking(move || {
                                        attached_worktree_pull_requests(
                                            &GitHubService::system("gh"),
                                            &key,
                                            &request_worktrees,
                                            &request_recent,
                                            &head_repositories,
                                            &cancellation,
                                        )
                                    })
                                    .await
                                    .unwrap_or_default();
                                    Message::AttachedWorktreePrsResolved {
                                        project_id,
                                        generation,
                                        worktrees,
                                        recent,
                                        archived,
                                        expected,
                                    }
                                })
                            } else {
                                Task::none()
                            };
                        // Restored pull request cards could not name their
                        // pull request until this project's GitHub state and
                        // sidebar entry both existed.
                        let pending = Task::batch([
                            self.observe_git_repository(&loaded, generation),
                            self.open_pending_pull_requests(project_id),
                            self.prune_pull_request_refs(project_id),
                            self.update_review_requests(review_request_ui::Action::Tick),
                        ]);
                        if open_cards {
                            if !self.refuse_in_deleting_worktree(&worktree) {
                                task = self.select_worktree(worktree, true);
                            }
                        } else if let Some(active) = self.active_workspace.clone()
                            && self.project_owns_active_scope(project_id, &active)
                        {
                            // Discovery can resolve legacy launch directories
                            // into distinct worktrees after session restoration.
                            // Only the project holding the scope can move it,
                            // and re-selecting resolves every card's owner.
                            task = if let Some(channel) = self.durable.active_channel {
                                self.select_channel(channel)
                            } else {
                                self.select_worktree(active, false)
                            };
                        }
                        // Every window shares which projects exist, and a
                        // channel the shared model named in this repository
                        // could not be adopted until the project existed here.
                        self.publish_project_opened(project_id);
                        task = Task::batch([
                            task,
                            pending,
                            pr_enrichment,
                            self.project_avatar_task(project_id),
                            self.reconcile_workspace_model(),
                        ]);
                    }
                    Err(error) => {
                        self.project_ui.reopening.remove(&project_id);
                        let unavailable = GitHubAvailability::Failed {
                            detail: error.clone(),
                        };
                        if let Some(project) = self
                            .sidebar_model
                            .projects
                            .iter_mut()
                            .find(|project| project.id == project_id)
                        {
                            project.github_status = unavailable.label().into();
                        }
                        self.github_availability.insert(project_id, unavailable);
                        self.review_repositories.remove(&project_id);
                        self.status = Some(TransientNotice::error(error.clone()));
                        finish_repository_load_failure(&mut self.sidebar_model, project_id, error);
                        task = self.apply_closed_project_tombstones();
                    }
                }
                task
            }
            Message::AttachedWorktreePrsResolved {
                project_id,
                generation,
                worktrees,
                recent,
                archived,
                expected,
            } => {
                if self.repository_generations.get(&project_id) == Some(&generation)
                    && let Some(project) = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == project_id)
                {
                    let worktrees = worktrees
                        .into_iter()
                        .filter(|(record, _)| {
                            let Some(original) = expected.get(&record.id) else {
                                return false;
                            };
                            project
                                .worktrees
                                .iter()
                                .find(|row| row.id == record.id)
                                .is_some_and(|row| {
                                    row.path == record.path
                                        && row.branch.as_deref()
                                            == record.branch.as_deref().and_then(|branch| {
                                                branch.strip_prefix("refs/heads/")
                                            })
                                        && row.detached == record.detached
                                        && row.pull_request() == original.as_ref()
                                })
                        })
                        .collect::<Vec<_>>();
                    apply_attached_worktree_prs(project, &worktrees, &recent, &archived);
                    self.refresh_ui
                        .git
                        .remember_archived(project_id, generation, &archived);
                }
                Task::none()
            }
            Message::Repository(RepositoryAction::NewTerminal { card }) => {
                match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) => {
                        self.add_terminal(runtime.descriptor.worktree.clone())
                    }
                    _ => Task::none(),
                }
            }
            Message::Repository(RepositoryAction::MarkdownLink { card, uri }) => {
                self.open_document_link(Some(card), &uri)
            }
            Message::Repository(RepositoryAction::OpenExternal { card, editor_id }) => {
                let path = match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) if runtime.can_open_external() => {
                        runtime
                            .selected_path()
                            .map(|path| runtime.descriptor.worktree.join(path))
                    }
                    _ => None,
                };
                path.map_or_else(Task::none, |path| self.open_file_in_editor(path, editor_id))
            }
            Message::Repository(RepositoryAction::StageSelected { card, stage }) => {
                if self.modal_is_open() {
                    return Task::none();
                }
                let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else {
                    return Task::none();
                };
                let Some(source) = self
                    .sidebar_model
                    .source_control
                    .as_ref()
                    .filter(|source| source.key.canonical_path == runtime.descriptor.worktree)
                else {
                    return Task::none();
                };
                let Some(selection) = runtime.selected_staging_selection(stage) else {
                    return Task::none();
                };
                let key = source.key.clone();
                self.update(Message::Sidebar(if stage {
                    SidebarAction::Stage {
                        key,
                        selection: vec![selection],
                    }
                } else {
                    SidebarAction::Unstage {
                        key,
                        selection: vec![selection],
                    }
                }))
            }
            Message::ResolvedDiffBasis { card, basis } => {
                let task = match self.cards.get_mut(&card) {
                    Some(CardRuntime::Repository(runtime)) => repository_task(
                        runtime.update(RepositoryAction::ApplyDiffBasis { card, basis }),
                    ),
                    _ => Task::none(),
                };
                Task::batch([task, self.drain_lsp_intents(card)])
            }
            Message::Repository(RepositoryAction::Review { card, action }) => {
                self.handle_pull_request_review(card, action)
            }
            Message::Repository(action) => {
                if let RepositoryAction::DiffAnnotations {
                    identity,
                    review_revision,
                    ..
                } = &action
                    && !matches!(self.cards.get(&identity.card), Some(CardRuntime::PullRequests(pane)) if pane.review.annotation_revision() == *review_revision)
                {
                    return Task::none();
                }
                if self.modal_is_open() && !repository_action_allowed_while_modal(&action) {
                    return Task::none();
                }
                let card = action.card();
                let basis_resolved = matches!(action, RepositoryAction::DiffBasisResolved { .. });
                let save_finished = matches!(action, RepositoryAction::SaveFinished { .. });
                let saved = matches!(
                    &action,
                    RepositoryAction::SaveFinished { result: Ok(_), .. }
                );
                // A parked source-control navigation is answered by the very
                // file list it is waiting on, and only after the card has taken
                // that list.
                let files_loaded = matches!(action, RepositoryAction::FilesLoaded { .. });
                let closing_card = self.dirty_repository_close.as_ref().map(|close| close.card);
                let task = match self.cards.get_mut(&card) {
                    Some(CardRuntime::Repository(runtime)) => runtime
                        .update(action)
                        .map(move |action| match action {
                            RepositoryAction::ApplyDiffBasis { card, basis } if basis_resolved => {
                                Message::ResolvedDiffBasis { card, basis }
                            }
                            action => Message::Repository(action),
                        })
                        .map(cosmic::Action::App),
                    // The pane's runtime only ever shows committed content
                    // from a fetched range, so the working-tree bookkeeping
                    // below (source-control reloads, dirty closes) has
                    // nothing to do for it.
                    Some(CardRuntime::PullRequests(pane)) => {
                        repository_task(pane.runtime.update(action))
                    }
                    _ => Task::none(),
                };
                // Every buffer transition the action caused is recorded on the
                // runtime and has so far reached nothing. Draining here rather
                // than at the individual actions is what makes that true of all
                // of them, including the ones a later action adds.
                let mut tasks = vec![task, self.drain_lsp_intents(card)];
                let selected_view = match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime))
                        if runtime.descriptor.selected_file.is_some() =>
                    {
                        Some(runtime.descriptor.clone())
                    }
                    _ => None,
                };
                if let Some(view) = selected_view
                    && let Some(state) = self.card_state_mut(card)
                    && state.kind != CardKind::Repository(view.clone())
                {
                    state.kind = CardKind::Repository(view);
                    self.persist();
                }
                if files_loaded {
                    tasks.push(self.flush_pending_panel_open(card));
                }
                if saved
                    && let Some(key) = self.sidebar_state.active_worktree().cloned()
                    && self.repository_card_for(&key.canonical_path) == Some(card)
                {
                    tasks.push(self.reload_source_control(&key));
                }
                if save_finished && closing_card == Some(card) {
                    tasks.push(self.advance_dirty_close());
                }
                if !self.project_ui.closing.is_empty() {
                    tasks.push(self.complete_closed_projects());
                }
                Task::batch(tasks)
            }
            Message::Sidebar(action) => {
                // A control in the pane can still be activated while a dialog
                // is up: the modal captures the pointer, and the focus
                // operation the dialog issues on open cannot reach a pane
                // widget to unfocus it, so a focused control keeps its focus
                // and answers to Enter, and an assistive technology's own
                // activation is not captured at all. Acting on one would
                // replace the dialog the user is filling in with a fresh one,
                // discarding what they had typed. Nothing is recorded either:
                // the selection an open would set is a claim that a card
                // exists, and refusing the open would make it a false one.
                //
                // The one exception is the discard confirmation's own answer.
                // That dialog is itself a modal, and its Cancel and Discard are
                // `SidebarAction`s that arrive right here, so refusing
                // everything while it is up would strand it with no way out - a
                // worse fault than the one this guard exists for.
                if self.modal_is_open() && !answers_discard_confirmation(&action) {
                    return Task::none();
                }
                // Entering a worktree that is being deleted is refused, and
                // for the same reason before the selection would claim it.
                if self
                    .sidebar_entered_directory(&action)
                    .is_some_and(|directory| self.refuse_in_deleting_worktree(&directory))
                {
                    return Task::none();
                }
                self.sidebar_state.apply(&action);
                // Opening a row, by pointer or keyboard, is navigation: it
                // ends any explicit selection and anchors a later range, but
                // selects nothing.
                let opened = match &action {
                    SidebarAction::OpenRepository { project_id, .. } => {
                        Some(sidebar::SelectionRow::Project(*project_id))
                    }
                    SidebarAction::OpenWorktree {
                        project_id,
                        worktree_id,
                        ..
                    } => Some(sidebar::SelectionRow::Worktree(*project_id, *worktree_id)),
                    SidebarAction::OpenChannel { channel_id } => {
                        Some(sidebar::SelectionRow::Channel(*channel_id))
                    }
                    _ => None,
                };
                if let Some(row) = opened {
                    self.select_sidebar_rows(row, sidebar::SelectionIntent::Navigate);
                }
                self.prune_sidebar_row_selection();
                match action {
                    SidebarAction::SelectRows { row, intent } => {
                        self.select_sidebar_rows(row, intent);
                        if matches!(intent, sidebar::SelectionIntent::Toggle | sidebar::SelectionIntent::Range { .. }) {
                            iced::widget::operation::focus(sidebar::selection_row_focus_id(row)).map(cosmic::Action::App)
                        } else { Task::none() }
                    }
                    SidebarAction::DeleteRow(row) => self.delete_sidebar_row(row),
                    SidebarAction::DeleteSelectedRows => self.begin_sidebar_group_delete(),
                    SidebarAction::ClearRowSelection => {
                        self.sidebar_state.clear_row_selection();
                        Task::none()
                    }
                    SidebarAction::DragWindow => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Drag)),
                    SidebarAction::MaximizeWindow => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Maximize)),
                    SidebarAction::WindowMenu => Task::done(cosmic::Action::Cosmic(cosmic::app::Action::ShowWindowMenu)),
                    SidebarAction::ReorderProject { project, before } => {
                        if self.sidebar_model.reorder_project(project, before) {
                            Arc::make_mut(&mut self.durable).project_order =
                                self.sidebar_model.projects.iter().map(|project| project.id).collect();
                            self.persist();
                        }
                        Task::none()
                    }
                    SidebarAction::CancelWorktreeOperation(id) => self.cancel_worktree_operation(id),
                    SidebarAction::DismissWorktreeOperation(id) => self.dismiss_worktree_operation(id),
                    SidebarAction::EditWorktreeOperation(id) => self.reopen_worktree_operation(id, false),
                    SidebarAction::RetryWorktreeOperation(id) => self.reopen_worktree_operation(id, true),
                    SidebarAction::RetryWorktreeDeletion { worktree_id, .. } => {
                        self.handle_project_action(project_ui::Action::RetryDelete(worktree_id))
                    }
                    SidebarAction::RetryChannelDeletion { channel_id } => {
                        self.handle_channel_ui(channel_ui::Action::RetryFailed(channel_id))
                    }
                    SidebarAction::OpenRepositoryPicker => {
                        cosmic::task::message(Message::OpenRepository)
                    }
                    SidebarAction::ProjectCommand { project_id, worktree_id, command } => {
                        self.handle_project_action(project_ui::Action::Command { project_id, worktree_id, command })
                    }
                    SidebarAction::OpenRepository {
                        project_id,
                        repository,
                    } => {
                        let directory = self.project_entry_directory(project_id, &repository);
                        self.select_worktree(directory, true)
                    }
                    SidebarAction::OpenWorktree { path, .. } => self.select_worktree(path, true),
                    SidebarAction::BrowsePullRequests { project_id } => self.browse_pull_requests(project_id),
                    SidebarAction::OpenPullRequest {
                        project_id,
                        number,
                        url,
                    } => match self.settings.pull_request_open_target {
                        PullRequestOpenTarget::Review
                            if self.pull_request_target(project_id, number).is_some() =>
                        {
                            self.review_pull_request(project_id, number)
                        }
                        // A pull request the project's GitHub state no longer
                        // lists can still be read on GitHub.
                        PullRequestOpenTarget::Review | PullRequestOpenTarget::Link => {
                            self.open_clicked_link(url)
                        }
                    },
                    SidebarAction::NewWorktree {
                        project_id,
                        repository,
                    } => {
                        if self.sidebar_model.project(project_id).is_some_and(|project| project.kind == ProjectKind::Folder) {
                            return Task::none();
                        }
                        self.open_worktree_dialog(
                            project_id,
                            repository,
                            WorktreeDraft::default(),
                            None,
                        )
                    }
                    SidebarAction::Refresh => {
                        let targets = repository_refresh_targets(&self.sidebar_model.projects);
                        if targets.is_empty() {
                            return cosmic::task::message(Message::OpenRepository);
                        }
                        self.sidebar_model.data_state = SidebarDataState::Loading;
                        Task::batch(targets.into_iter().map(|(repository, id)| {
                            self.repository_load_task(repository, Some(id), false)
                        }))
                    }
                    SidebarAction::RefreshProject {
                        project_id,
                        repository,
                    } => self.repository_load_task(repository, Some(project_id), false),
                    SidebarAction::CreatePullRequest {
                        project_id,
                        worktree,
                    } => {
                        let Some(repository) = self.github_availability.get(&project_id).and_then(GitHubAvailability::repository_key).cloned()
                        else {
                            self.status = Some(TransientNotice::warning(
                                "This repository is not associated with an authenticated GitHub remote",
                            ));
                            return Task::none();
                        };
                        let head = pull_request_head(
                            &self.sidebar_model.projects,
                            project_id,
                            worktree.as_deref(),
                        );
                        let base = self
                            .sidebar_model
                            .source_control
                            .as_ref()
                            .filter(|source| {
                                source.key.project_id == project_id
                                    && worktree.as_ref().is_none_or(|path| {
                                        source.key.canonical_path == *path
                                    })
                            })
                            .map(|source| {
                                proven_pull_request_base(
                                    &source.branch_changes,
                                    &source.remotes,
                                )
                            })
                            .unwrap_or_default();
                        self.pull_request_dialog_generation =
                            self.pull_request_dialog_generation.wrapping_add(1);
                        self.pull_request_dialog = Some(PullRequestDialog {
                            project_id,
                            repository,
                            worktree,
                            base,
                            head,
                            title: String::new(),
                            body: String::new(),
                            draft: false,
                            retry_confirmation: None,
                        });
                        iced::widget::operation::focus(pull_request_dialog_focus_id())
                            .map(cosmic::Action::App)
                    }
                    SidebarAction::NewChannel { project_id } => {
                        self.handle_channel_ui(channel_ui::Action::Create(project_id))
                    }
                    SidebarAction::OpenChannel { channel_id } => {
                        self.handle_channel_ui(channel_ui::Action::Open(channel_id))
                    }
                    SidebarAction::DeleteChannel { channel_id } => {
                        self.handle_channel_ui(channel_ui::Action::Delete(channel_id))
                    }
                    SidebarAction::ToggleFilesFilter { key } => {
                        if self.source_control_is_bound(&key) && self.sidebar_state.files_filter_open() {
                            widget::text_input::focus(sidebar::files_filter_focus_id(&key))
                                .map(cosmic::Action::App)
                        } else {
                            Task::none()
                        }
                    }
                    SidebarAction::OpenSettings => self.open_utility_card(CardKind::Settings),
                    SidebarAction::OpenAutomations => self.open_automations(),
                    // The handle clamps to its own rendered bounds, and the
                    // settings file accepts a narrower range, so the width is
                    // clamped again before it can reach a rejected save.
                    SidebarAction::Resize { width } => {
                        self.settings.sidebar_width =
                            width.clamp(sidebar::EXPANDED_WIDTH_MIN, sidebar::EXPANDED_WIDTH_MAX);
                        Task::none()
                    }
                    SidebarAction::ResizeFinished => self.save_settings_task(false),
                    SidebarAction::OpenBranchFile { key, path, old_path, base_ref, head, merge_base } => {
                        self.open_branch_file(key, path, old_path, base_ref, head, merge_base)
                    }
                    // Disclosing `Untracked` is what decides whether the
                    // snapshot walks for untracked paths at all, so the read
                    // is reissued with the group's new state.
                    SidebarAction::ToggleSection { key, section: PanelSection::Untracked } => self.reload_source_control(&key),
                    SidebarAction::OpenCommit { key, oid } => self.open_history_commit(key, oid),
                    SidebarAction::CloseCommit { key } => {
                        if self.source_control_is_bound(&key) {
                            self.history_ui.cancel();
                            if let Some(source) = self.sidebar_model.source_control.as_mut() { source.commit_changes = None; }
                        }
                        Task::none()
                    }
                    SidebarAction::ShowMoreCommitFiles { key, oid } => {
                        if self.source_control_is_bound(&key)
                            && let Some(changes) = self.sidebar_model.source_control.as_mut().and_then(|source| source.commit_changes.as_mut())
                            && changes.commit.oid == oid {
                            changes.visible_files = changes.visible_files.saturating_add(200);
                        }
                        Task::none()
                    }
                    SidebarAction::OpenCommitFile { key, oid, path } => self.open_commit_file(key, oid, path),
                    SidebarAction::ToggleDirectory { key, .. }
                    | SidebarAction::FilesQueryChanged { key, .. } if self.folder_scope(&key) => {
                        self.reload_source_control(&key)
                    }
                    SidebarAction::ToggleSection { .. }
                    | SidebarAction::SetChangesTab { .. }
                    | SidebarAction::ToggleSourceControlPanel { .. }
                    | SidebarAction::FilesQueryChanged { .. }
                    | SidebarAction::SetChangeListMode { .. }
                    | SidebarAction::SetWorktreePanel { .. }
                    | SidebarAction::ToggleDirectory { .. }
                    | SidebarAction::ToggleChangeDirectory { .. }
                    | SidebarAction::CommitMessageChanged { .. }
                    | SidebarAction::PublishRemoteChanged { .. }
                    | SidebarAction::PublishBranchChanged { .. }
                    | SidebarAction::CancelPublish
                    // `apply` already cleared the pending discard, and the
                    // dialog it was driving is gone with it.
                    | SidebarAction::CancelDiscard => Task::none(),
                    SidebarAction::RequestDiscard { .. } => {
                        if self.sidebar_state.pending_discard().is_some() {
                            iced::widget::operation::focus(sidebar::discard_confirm_id())
                                .map(cosmic::Action::App)
                        } else {
                            Task::none()
                        }
                    }
                    SidebarAction::RefreshWorktree { key } => self.reload_source_control(&key),
                    SidebarAction::OpenFile { key, path } => {
                        self.route_panel_open(&key, &path)
                    }
                    SidebarAction::OpenFileInEditor { key, path, editor_id } => {
                        if !self.source_control_is_bound(&key) {
                            return Task::none();
                        }
                        self.open_file_in_editor(key.canonical_path.join(path.as_path()), editor_id)
                    }
                    SidebarAction::OpenChange { key, path, area } => {
                        // The card has two diff bases. A conflict has no staged
                        // side to show, so it is opened against the working
                        // tree like any other unstaged change.
                        self.open_working_change(key, path, area)
                    }
                    SidebarAction::Stage { key, selection } => {
                        if !self.begin_mutation(&key, MutationKind::Stage) {
                            return Task::none();
                        }
                        let request = StageRequest {
                            worktree: key.canonical_path.clone(),
                            selection,
                        };
                        Self::mutation_task(key, MutationKind::Stage, move |service, cancel| {
                            (
                                mutation_report("Stage", service.stage(&request, cancel)),
                                false,
                            )
                        })
                    }
                    SidebarAction::Unstage { key, selection } => {
                        if !self.begin_mutation(&key, MutationKind::Unstage) {
                            return Task::none();
                        }
                        let request = UnstageRequest {
                            worktree: key.canonical_path.clone(),
                            selection,
                        };
                        Self::mutation_task(key, MutationKind::Unstage, move |service, cancel| {
                            (
                                mutation_report("Unstage", service.unstage(&request, cancel)),
                                false,
                            )
                        })
                    }
                    // The only arm that destroys work, and the only one the
                    // dialog can raise: `RequestDiscard` builds the request
                    // with `confirmed: false`, the dialog rebuilds it with
                    // `confirmed: true`, and the service refuses an
                    // unconfirmed one outright.
                    SidebarAction::ConfirmDiscard { key, request } => {
                        if !self.begin_mutation(&key, MutationKind::Discard) {
                            return Task::none();
                        }
                        Self::mutation_task(key, MutationKind::Discard, move |service, cancel| {
                            (
                                mutation_report("Discard", service.discard(&request, cancel)),
                                false,
                            )
                        })
                    }
                    SidebarAction::Commit { key, message } => {
                        if !self.begin_mutation(&key, MutationKind::Commit) {
                            return Task::none();
                        }
                        let request = CommitRequest {
                            worktree: key.canonical_path.clone(),
                            message,
                        };
                        Self::mutation_task(key, MutationKind::Commit, move |service, cancel| {
                            commit_report(service.commit(&request, cancel))
                        })
                    }
                    SidebarAction::Push {
                        key,
                        branch,
                        target,
                    } => {
                        if !self.begin_mutation(&key, MutationKind::Push) {
                            return Task::none();
                        }
                        let request = PushRequest {
                            worktree: key.canonical_path.clone(),
                            branch,
                            target,
                        };
                        Self::mutation_task(key, MutationKind::Push, move |service, cancel| {
                            (push_report(service.push(&request, cancel)), false)
                        })
                    }
                    SidebarAction::OpenPublish { .. } => Task::none(),
                    SidebarAction::LoadMoreHistory { key, page } => {
                        if !self.source_control_is_bound(&key) {
                            return Task::none();
                        }
                        if let Some(source) = self.sidebar_model.source_control.as_mut() {
                            if source.history_loading || source.next_history_page != page { return Task::none(); }
                            source.mark_history_requested();
                        }
                        Self::source_control_history_task(self.history_ui.next_page_request(), key, self.sidebar_model.source_control.as_ref().and_then(|source| source.head_oid.as_deref()).and_then(|oid| crate::repository::Oid::parse_object(oid).ok()), page)
                    }
                    SidebarAction::ToggleControls => {
                        if self.sidebar_state.controls_expanded() {
                            iced::widget::operation::focus(sidebar::search_focus_id())
                                .map(cosmic::Action::App)
                        } else {
                            Task::none()
                        }
                    }
                    // Renaming and duplicating belong to standalone ceditor;
                    // this model never enables those menu items. The two copy
                    // commands are pure clipboard work that a worktree browser
                    // needs regardless of file management.
                    SidebarAction::FileCommand { key, path, command } => match command {
                        sidebar::FileCommand::CopyPath => iced::clipboard::write(
                            key.canonical_path.join(path.as_path()).display().to_string(),
                        )
                        .map(cosmic::Action::App),
                        sidebar::FileCommand::CopyRelativePath => {
                            iced::clipboard::write(path.display().into_owned())
                                .map(cosmic::Action::App)
                        }
                        sidebar::FileCommand::Rename | sidebar::FileCommand::CopyFile => {
                            Task::none()
                        }
                    },
                    SidebarAction::SearchChanged(_)
                    | SidebarAction::SetFilter(_)
                    | SidebarAction::ToggleProject(_) => Task::none(),
                }
            }
            Message::FocusCard(id) => match self.workspace.focus(id) {
                Ok(update) => self.apply_viewport(update, true, false),
                Err(error) => {
                    self.status = Some(TransientNotice::error(error.to_string()));
                    Task::none()
                }
            },
            Message::ResizeCard { card, width } => {
                // The settings pane lays itself out against its own width, so
                // dragging its edge is how the user picks rail or stacked.
                if !self.card_state(card).is_some_and(|state| {
                    matches!(&state.kind, CardKind::Terminal(_)) || state.kind.is_utility()
                }) {
                    return Task::none();
                }
                let viewport = self.workspace.viewport_width();
                if viewport <= f32::EPSILON {
                    return Task::none();
                }
                let limits = self.workspace.begin_card_resize(card);
                let maximum = limits.maximum;
                let minimum = limits.minimum;
                let width = width.clamp(minimum, maximum);
                let fraction = self.workspace.preferred_width_for_resize(card, width) / viewport;
                if self.workspace.focused() != Some(card)
                    && let Err(error) = self.workspace.focus(card)
                {
                    self.status = Some(TransientNotice::error(error.to_string()));
                    return Task::none();
                }
                match self
                    .workspace
                    .set_card_width(card, CardWidth::Fraction(fraction))
                {
                    Ok(update) => {
                        if let Some(state) = self.card_state_mut(card) {
                            state.width_fraction = fraction;
                        }
                        self.apply_viewport(update, true, false)
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::CyclePaneWidth => {
                let Some(card) = self.workspace.focused() else {
                    return Task::none();
                };
                let Some(width) = self
                    .workspace
                    .card(card)
                    .map(|card| card.width.next_preset())
                else {
                    return Task::none();
                };
                match self.workspace.animate_card_width(card, width) {
                    Ok(update) => {
                        if let Some(state) = self.card_state_mut(card)
                            && let CardWidth::Fraction(fraction) = width
                        {
                            state.width_fraction = fraction;
                        }
                        self.apply_viewport(update, true, true)
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::ResizeCardFinished(card) => {
                self.workspace.finish_card_resize(card);
                if self.card_state(card).is_some() {
                    self.persist();
                }
                Task::none()
            }
            Message::CloseCard(_) if self.dirty_repository_close.is_some() => Task::none(),
            Message::CloseCard(id) if matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()) => {
                self.begin_dirty_close(DirtyCloseIntent::Card(id), VecDeque::from([id]))
            }
            Message::UpdateUi(action) => self.update_update_ui(action),
            Message::StartupWindowOpened(window) => {
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                let report = cosmic::task::future(async {
                    let result =
                        tokio::task::spawn_blocking(|| instance::report_ui_startup(&Ok(())))
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result.map_err(|error| error.to_string()));
                    Message::StartupReported(result)
                });
                // window::run silently drops callbacks for windows that have not
                // opened yet. Initialize only after iced acknowledges this one.
                #[cfg(feature = "integrated-browser")]
                return Task::batch([report, self.browser_factory_task()]);
                #[cfg(not(feature = "integrated-browser"))]
                report
            }
            Message::StartupReported(result) => match result {
                Ok(()) => Task::none(),
                Err(error) => {
                    tracing::error!(%error, "could not acknowledge UI startup");
                    iced::exit()
                }
            },
            Message::NewWindow => {
                // The new window reads settings from disk, so it must see
                // the drafts this one has not yet saved.
                let drafts = self.flush_settings_drafts();
                self.persist();
                let save = self.capture_workspace_save();
                let source = instance::window_slot().to_owned();
                let open = cosmic::task::future(async move {
                    let (saved, result) = flush_workspace(save).await;
                    let result = match result {
                        Ok(()) => tokio::task::spawn_blocking(move || {
                            instance::launch_window(true, None, Some(&source))
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result),
                        Err(error) => Err(error),
                    };
                    Message::NewWindowOpened { saved, result }
                });
                Task::batch([drafts, open])
            }
            Message::NewWindowOpened { saved, result } => {
                if let Ok(saved) = saved {
                    self.acknowledge_settings_save(saved);
                }
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not open a new window: {error}"
                    )));
                }
                Task::none()
            }
            Message::SettingsSaved { generation, result } => {
                match result {
                    Ok(saved) => {
                        if saved.generation >= self.settings_ack_generation {
                            self.settings_save_error = None;
                        }
                        self.acknowledge_settings_save(saved);
                    }
                    Err(error) if generation >= self.settings_ack_generation => {
                        self.status = Some(TransientNotice::warning(format!(
                            "Could not save settings: {error}"
                        )));
                        self.settings_save_error = Some(error);
                    }
                    Err(_) => {}
                }
                self.refresh_settings_feedback();
                Task::none()
            }
            Message::UiUpdate(request) => match request.action {
                instance::UiUpdateAction::Prepare => {
                    if self.update_prepared {
                        let _ = request
                            .response
                            .send("error: window is already preparing an update".into());
                        return Task::none();
                    }
                    if let Some(reason) = self.window_update_blocker() {
                        let _ = request.response.send(format!("error: {reason}"));
                        return Task::none();
                    }
                    // The update restarts every window from what is on disk.
                    self.update_ui.confirmation = None;
                    let drafts = self.flush_settings_drafts();
                    self.persist();
                    let save = self.capture_workspace_save();
                    self.update_prepared = true;
                    self.update_flush_pending = true;
                    self.update_committed = false;
                    self.update_generation = self.update_generation.wrapping_add(1);
                    let generation = self.update_generation;
                    self.status = Some(TransientNotice::warning("Saving windows for update…"));
                    Task::batch([
                        drafts,
                        cosmic::task::future(async move {
                            let (saved, result) = flush_workspace(save).await;
                            Message::UiUpdateFlushed {
                                generation,
                                committing: false,
                                response: request.response,
                                saved,
                                result,
                            }
                        }),
                        cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            Message::UiUpdateWatchdog(generation)
                        }),
                    ])
                }
                instance::UiUpdateAction::Cancel => {
                    if self.update_committed {
                        let _ = request
                            .response
                            .try_send("error: window is already closing".into());
                        return Task::none();
                    }
                    self.update_prepared = false;
                    self.update_flush_pending = false;
                    self.update_generation = self.update_generation.wrapping_add(1);
                    self.status = Some(TransientNotice::warning(
                        "Update cancelled; window is ready.",
                    ));
                    let _ = request.response.send("ok".into());
                    Task::none()
                }
                instance::UiUpdateAction::Commit => {
                    if !self.update_prepared || self.update_flush_pending || self.update_committed {
                        let _ = request
                            .response
                            .send("error: window was not prepared".into());
                        return Task::none();
                    }
                    if let Some(reason) = self.window_update_blocker() {
                        let _ = request.response.send(format!("error: {reason}"));
                        return Task::none();
                    }
                    self.persist();
                    let save = self.capture_workspace_save();
                    self.update_flush_pending = true;
                    let generation = self.update_generation;
                    cosmic::task::future(async move {
                        let (saved, result) = flush_workspace(save).await;
                        Message::UiUpdateFlushed {
                            generation,
                            committing: true,
                            response: request.response,
                            saved,
                            result,
                        }
                    })
                }
            },
            Message::UiUpdateFlushed {
                generation,
                committing,
                response,
                saved,
                result,
            } => {
                if let Ok(saved) = saved {
                    self.acknowledge_settings_save(saved);
                }
                if generation != self.update_generation || !self.update_prepared {
                    let _ =
                        response.try_send("error: window update preparation was cancelled".into());
                    return Task::none();
                }
                self.update_flush_pending = false;
                match result {
                    Ok(()) => {
                        if response.try_send("ok".into()).is_err() {
                            self.update_prepared = false;
                            self.status = Some(TransientNotice::warning(
                                "Updater disconnected; window is ready.",
                            ));
                            return Task::none();
                        }
                        if committing {
                            self.update_committed = true;
                            return self.exit_after_language_servers();
                        }
                    }
                    Err(error) => {
                        self.update_prepared = false;
                        self.status = Some(TransientNotice::error(error.clone()));
                        let _ = response.try_send(format!("error: {error}").replace('\n', " "));
                    }
                }
                Task::none()
            }
            Message::UiUpdateWatchdog(generation) => {
                if self.update_prepared
                    && !self.update_committed
                    && generation == self.update_generation
                {
                    if !instance::update_in_progress() {
                        self.update_prepared = false;
                        self.update_flush_pending = false;
                        self.update_generation = self.update_generation.wrapping_add(1);
                        self.status =
                            Some(TransientNotice::warning("Update ended; window is ready."));
                    } else {
                        return cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            Message::UiUpdateWatchdog(generation)
                        });
                    }
                }
                Task::none()
            }
            Message::WindowCloseRequested(window) => {
                // libcosmic reports the closed window through this same
                // request, and a second close must not reopen the exit.
                if self.exit.is_some() || self.dirty_repository_close.is_some() {
                    return Task::none();
                }
                let dirty = self
                    .durable
                    .cards
                    .iter()
                    .filter(|card| card.existence.is_open())
                    .map(|card| card.id)
                    .filter(|card| {
                        matches!(self.cards.get(card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
                    })
                    .collect::<VecDeque<_>>();
                self.begin_dirty_close(DirtyCloseIntent::Application(window), dirty)
            }
            Message::CloseCard(id)
                if self
                    .card_state(id)
                    .is_some_and(|state| matches!(state.kind, CardKind::Chat(_)))
                    || self.chat_has_parked_runtime(id) =>
            {
                self.begin_chat_close(id)
            }
            Message::CloseCard(id) => self.begin_card_close(id),
            Message::CloseCardUnasked(id) => self.close_card_unasked(id),
            Message::ConfirmTerminalClose => self.confirm_terminal_close(),
            Message::CancelTerminalClose => self.cancel_terminal_close(),
            Message::TerminalCloseDontAskChanged(value) => {
                if let Some(intent) = self.terminal_close.as_mut()
                    && !intent.pending
                    && intent.active
                    && intent.detail.is_none()
                {
                    intent.dont_ask_again = value;
                }
                Task::none()
            }
            Message::TerminalCloseFinished {
                card,
                terminal,
                proof,
                generation,
                outcome,
            } => self.terminal_close_finished(card, terminal, proof, generation, outcome),
            Message::CompletedTerminalClosed {
                card,
                session,
                result,
            } => self.completed_terminal_closed(card, session, result),
            Message::SaveAndCloseRepository(id) => match self.cards.get_mut(&id) {
                Some(CardRuntime::Repository(runtime))
                    if self
                        .dirty_repository_close
                        .as_ref()
                        .is_some_and(|close| close.card == id) =>
                {
                    repository_task(runtime.request_save(id, false))
                }
                _ => Task::none(),
            },
            Message::OverwriteAndCloseRepository(id) => match self.cards.get_mut(&id) {
                Some(CardRuntime::Repository(runtime))
                    if self
                        .dirty_repository_close
                        .as_ref()
                        .is_some_and(|close| close.card == id) =>
                {
                    repository_task(runtime.request_save(id, true))
                }
                _ => Task::none(),
            },
            Message::DiscardAndCloseRepository(id) => {
                if self
                    .dirty_repository_close
                    .as_ref()
                    .is_none_or(|close| close.card != id)
                {
                    return Task::none();
                }
                let discarded = match self.cards.get_mut(&id) {
                    Some(CardRuntime::Repository(runtime)) => runtime.abandon_unsaved_changes(),
                    _ => false,
                };
                if !discarded {
                    return Task::none();
                }
                self.advance_dirty_close()
            }
            Message::CancelRepositoryClose => self.cancel_dirty_close(),
            Message::CommitClose(id) => {
                if self
                    .card_state(id)
                    .is_some_and(|state| matches!(state.kind, CardKind::Chat(_)))
                    || self.chat_has_parked_runtime(id)
                {
                    return self.begin_chat_close(id);
                }
                if self
                    .card_state(id)
                    .is_none_or(|card| !card.existence.is_open())
                {
                    return Task::none();
                }
                if self.pending_terminal_spawn(id) {
                    return self.begin_card_close(id);
                }
                if terminal_continuity(self.card_state(id))
                    .and_then(TerminalContinuity::live_proof)
                    .is_some()
                    && !self.closed_terminal_cards.contains(&id)
                {
                    return self.begin_card_close(id);
                }
                self.commit_card_removal(id, None)
                    .unwrap_or_else(Task::none)
            }
            Message::Viewport { generation, event } => {
                // Native callbacks already queued by an unmounted worktree
                // must never mutate the newly selected strip.
                if generation != self.workspace_view_generation {
                    return Task::none();
                }
                if let WorkspaceViewportEvent::Reordered { card, before } = event {
                    return self.reorder_pane(card, before);
                }
                if matches!(event, WorkspaceViewportEvent::DragScrolled(_))
                    && self.workspace_inputs_blocked()
                {
                    return Task::none();
                }
                if matches!(
                    event,
                    WorkspaceViewportEvent::PointerFocused(_)
                        | WorkspaceViewportEvent::PointerEntered(_)
                ) && self.workspace_inputs_blocked()
                {
                    return Task::none();
                }
                if matches!(event, WorkspaceViewportEvent::PointerEntered(_))
                    && !self.settings.focus_follows_mouse
                {
                    return Task::none();
                }
                let (update, synchronize_scroll) =
                    update_workspace_viewport(&mut self.workspace, event);
                let task = self.apply_viewport(update, false, false);
                if synchronize_scroll {
                    Task::batch([task, self.synchronize_workspace_scroll()])
                } else {
                    task
                }
            }
            Message::Frame(frame) => {
                let inspector_task = self.advance_context_inspector_transition(frame);
                let Some(elapsed) = self.frame_clock.elapsed(frame) else {
                    let deferred = self.release_deferred_navigation_if_unblocked();
                    return Task::batch([inspector_task, deferred]);
                };
                if self.focused_repository_is_saving() {
                    return inspector_task;
                }
                let update = self.workspace.tick(elapsed);
                Task::batch([inspector_task, self.apply_viewport(update, true, false)])
            }
            Message::LifecycleSweep => self.lifecycle_sweep(),
            Message::LifecycleSweepElapsed(generation) => self.lifecycle_sweep_elapsed(generation),
            // The exit released the parent before closing it. A factory that
            // captured the window just before must not start browsers on a
            // surface that is gone, nor mark the cards it would have loaded
            // failed in the state the exit is about to save.
            #[cfg(feature = "integrated-browser")]
            Message::InitializeBrowserFactory if self.exit.is_some() => Task::none(),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFactoryReady(result) if self.exit.is_some() => {
                if let Ok(BrowserFactoryBinding {
                    factory,
                    parent_lifetime,
                }) = result
                {
                    drop(factory);
                    parent_lifetime.deactivate();
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::InitializeBrowserFactory => self.browser_factory_task(),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFactoryReady(_) if self.browser_factory.is_some() => Task::none(),
            #[cfg(feature = "integrated-browser")]
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
                    #[cfg(feature = "integrated-browser")]
                    for runtime in self.cards.values() {
                        if let CardRuntime::Browser(browser) = runtime {
                            let _ = browser.handle.set_scale_factor(effective_surface_scale(
                                scale_factor,
                                self.settings.ui_scale,
                            ));
                        }
                    }
                }
                Task::none()
            }
            Message::WindowResized(width, height) => {
                if height.is_finite() && height > 0.0 {
                    self.window_height = height;
                }
                if width.is_finite() && width > 0.0 {
                    self.window_width = width;
                }
                if self.focused_repository_is_saving() && self.context_inspector_is_overlay() {
                    self.context_inspector_open = false;
                    self.context_inspector_transition = None;
                }
                self.sync_context_overlay_state()
            }
            Message::ActivateUi { token } => self.activate_main_window(token),
            Message::OpenBrowser(url) => self.add_browser(url),
            Message::OpenLink(card, url, gesture) => {
                if url.scheme() == "file" {
                    self.open_document_link(Some(card), url.as_str())
                } else {
                    self.open_link_in(url, self.settings.link_destination(Some(gesture)))
                }
            }
            Message::OpenDocumentLink(card, uri) => self.open_document_link(Some(card), &uri),
            Message::PreviewLocalFile(card) => self.preview_local_file(card),
            Message::OpenTerminalOutput(card) => {
                let url = ended_terminal(self.card_state(card))
                    .and_then(|session| session.transcript_path.as_ref())
                    .and_then(|path| Url::from_file_path(path).ok());
                match url {
                    Some(url) => self.open_link_in(url, LinkOpenDestination::SystemDefault),
                    None => Task::none(),
                }
            }
            Message::ExternalLinkOpened(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(format!(
                        "could not open link in the system browser: {error}"
                    )));
                }
                Task::none()
            }
            Message::NewBrowser => self.add_browser(
                Url::parse("https://github.com/").expect("default browser URL is valid"),
            ),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserBack(card) => {
                browser_command(&self.cards, card, |handle| handle.back(), &mut self.status)
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserForward(card) => browser_command(
                &self.cards,
                card,
                |handle| handle.forward(),
                &mut self.status,
            ),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserReload(card) => browser_command(
                &self.cards,
                card,
                |handle| handle.reload(),
                &mut self.status,
            ),
            Message::RetryCard(card) => self.retry_card(card),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserAddressChanged { card, value } => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.address = BrowserAddress::user_input(value);
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserNavigate(card) => {
                let url = match self.cards.get_mut(&card) {
                    Some(CardRuntime::Browser(browser)) => browser.address.submit(),
                    _ => return Task::none(),
                };
                match url {
                    Ok(url) => browser_command(
                        &self.cards,
                        card,
                        |handle| handle.navigate(url),
                        &mut self.status,
                    ),
                    Err(_) => {
                        self.status = Some(TransientNotice::error("invalid browser location"));
                        Task::none()
                    }
                }
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserEvent { card, event } => self.handle_browser_event(card, event),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserImport(action) => self.update_browser_import(action),
            #[cfg(feature = "integrated-browser")]
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
            #[cfg(feature = "integrated-browser")]
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
                    dialog.edit(WorktreeField::Branch, value);
                }
                Task::none()
            }
            Message::WorktreeBaseChanged(value) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.edit(WorktreeField::Base, value);
                }
                Task::none()
            }
            Message::WorktreeDestinationChanged(value) => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.edit(WorktreeField::Destination, value);
                }
                Task::none()
            }
            Message::ToggleWorktreeAdvanced => {
                if let Some(dialog) = &mut self.worktree_dialog {
                    dialog.toggle_options();
                }
                Task::none()
            }
            Message::WorktreeInputResolved {
                project_id,
                operation_id,
                result,
            } => self.finish_worktree_input(project_id, operation_id, result),
            Message::WorktreeProgress(event) => self.worktree_progress(event),
            Message::WorktreeElapsed => {
                self.sync_worktree_operation_rows();
                Task::none()
            }
            Message::WorktreeOperationFinished {
                operation,
                generation,
                result,
            } => self.finish_worktree_operation(operation, generation, result),
            Message::WorktreeOperationsRehydrated(result) => {
                self.restore_worktree_operations(result)
            }
            Message::SubmitWorktree => self.submit_worktree_dialog(),
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
                        if let Some(project) = self
                            .sidebar_model
                            .projects
                            .iter_mut()
                            .find(|project| project.id == project_id)
                        {
                            project
                                .worktrees
                                .push(SidebarWorktree::from_repository(&worktree, None));
                        }
                        let terminal = self.select_worktree(worktree.path, true);
                        let chrome = self.sync_chrome();
                        return Task::batch([
                            terminal,
                            chrome,
                            self.repository_load_task(repository, Some(project_id), false),
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
            Message::PullRequestRetryConfirmed(confirmed) => {
                if let Some(dialog) = &mut self.pull_request_dialog
                    && dialog.retry_confirmation.is_some()
                {
                    dialog.retry_confirmation = Some(confirmed);
                }
                Task::none()
            }
            Message::SubmitPullRequest => {
                if self
                    .pull_request_dialog
                    .as_ref()
                    .is_some_and(|dialog| dialog.retry_confirmation == Some(false))
                {
                    return Task::none();
                }
                let Some(dialog) = self.pull_request_dialog.take() else {
                    return Task::none();
                };
                // The draft is fenced on identity, not on liveness: a refresh
                // that completes while the dialog is open and confirms the same
                // repository has changed nothing the user authored against, so
                // only a changed key or a completed refresh that revoked
                // authority may reject the submission.
                if self
                    .github_availability
                    .get(&dialog.project_id)
                    .and_then(GitHubAvailability::repository_key)
                    != Some(&dialog.repository)
                {
                    self.status = Some(TransientNotice::warning(
                        "GitHub identity is stale. Refresh and reopen Create pull request.",
                    ));
                    self.pull_request_dialog = Some(dialog);
                    return Task::none();
                }
                let request = CreatePullRequestRequest {
                    repository: dialog.repository.clone(),
                    base: dialog.base.clone(),
                    head: dialog.head.clone(),
                    title: dialog.title.clone(),
                    body: dialog.body.clone(),
                    draft: dialog.draft,
                    labels: Vec::new(),
                    reviewers: Vec::new(),
                };
                if let Err(error) = request.validate() {
                    // Kept, not discarded: the description is the longest thing
                    // the user types anywhere in the application, and a rejected
                    // title is no reason to take it away from them.
                    self.status = Some(TransientNotice::error(error.to_string()));
                    self.pull_request_dialog = Some(dialog);
                    return iced::widget::operation::focus(pull_request_dialog_focus_id())
                        .map(cosmic::Action::App);
                }
                let generation = self.pull_request_dialog_generation;
                let submit = cosmic::task::future(async move {
                    let mut dialog = dialog;
                    let result = tokio::task::spawn_blocking(move || {
                        GitHubService::system("gh")
                            .create(&request, &CancellationToken::default())
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| {
                        // A worker panic does not prove that gh made no mutation.
                        dialog.retry_confirmation = Some(false);
                        format!("pull request creator join failed: {error}")
                    })
                    .and_then(|result| result);
                    Message::PullRequestCreated {
                        dialog,
                        generation,
                        result,
                    }
                });
                Task::batch([submit, self.focus_context_overlay_if_exposed()])
            }
            Message::PullRequestCreated {
                mut dialog,
                generation,
                result,
            } => match result {
                Ok(PullRequestCreationOutcome::Created { url, .. }) => {
                    self.status = Some(TransientNotice::success(format!(
                        "created pull request {url}"
                    )));
                    self.open_link(url)
                }
                Ok(PullRequestCreationOutcome::NotCreated { failure }) => {
                    self.status = Some(TransientNotice::error(failure.to_string()));
                    self.restore_pull_request_dialog(dialog, generation)
                }
                Ok(PullRequestCreationOutcome::OutcomeUnknown { failure }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "pull request outcome is unknown; reconcile before retrying: {failure}"
                    )));
                    dialog.retry_confirmation = Some(false);
                    self.restore_pull_request_dialog(dialog, generation)
                }
                Err(error) => {
                    self.status = Some(TransientNotice::error(error));
                    self.restore_pull_request_dialog(dialog, generation)
                }
            },
            Message::CancelDialog => self.cancel_form_dialog(),
            Message::KeyEvent { event, captured } => {
                if crate::tab_history::released(&event) {
                    self.tab_history.finish(self.workspace.focused());
                }
                if crate::tab_history::modifiers_released(&event, self.workspace_cycle_modifiers) {
                    self.workspace_history
                        .finish(self.active_workspace_destination());
                    self.workspace_cycle_modifiers = keyboard::Modifiers::empty();
                }
                if !self.workspace_inputs_blocked()
                    && !self.focused_repository_is_saving()
                    && let Some(backwards) = crate::tab_history::step_direction(&event)
                {
                    let available: Vec<_> = self.workspace.order().collect();
                    self.tab_history.retain(|id| {
                        self.durable
                            .cards
                            .iter()
                            .any(|card| card.id == id && card.existence.is_open())
                    });
                    if let Some(id) =
                        self.tab_history
                            .step(&available, self.workspace.focused(), backwards)
                        && let Ok(update) = self.workspace.focus(id)
                    {
                        return self.apply_viewport(update, true, false);
                    }
                    return Task::none();
                }
                if let keyboard::Event::ModifiersChanged(modifiers) = &event {
                    self.keyboard_modifiers = *modifiers;
                }
                if let Some(task) = self.chrome_key_event(&event) {
                    return task;
                }
                if let Some(task) = self.account_key_event(&event) {
                    return task;
                }
                // A widget that consumed Escape has already answered it - a
                // focused terminal clearing its selection, for one. Running the
                // chain anyway would close the inspector behind it in the same
                // keystroke. Global shortcuts below are deliberately exempt:
                // `WorkspaceSurface` captures those chords itself, before any
                // child sees them, and relies on this message to act on them.
                // A form dialog on top answers Escape before anything under
                // it, such as a find bar or picker, and even when its focused
                // field captured the key to blur itself: otherwise closing it
                // would take a second press.
                if is_escape_press(&event) && self.form_dialog_on_top() {
                    return self.cancel_form_dialog();
                }
                if is_escape_press(&event) && !captured {
                    if self.update_ui.confirmation.is_some() {
                        return self.update_update_ui(update_ui::Action::CancelApply);
                    }
                    if self.terminal_close_dialog_visible() {
                        return self.cancel_terminal_close();
                    }
                    if self.sidebar_state.pending_discard().is_some() {
                        return cosmic::task::message(Message::Sidebar(
                            SidebarAction::CancelDiscard,
                        ));
                    }
                    if self.project_ui.modal_open() {
                        return self.handle_project_action(project_ui::Action::Dismiss);
                    }
                    if self.sidebar_selection_ui.dialog.is_some() {
                        return self
                            .update_sidebar_selection(sidebar_selection_ui::Action::Dismiss);
                    }
                    if self.dirty_repository_close.is_some() {
                        if self.dirty_repository_close.as_ref().is_some_and(|close| {
                            matches!(self.cards.get(&close.card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
                        }) {
                            return Task::none();
                        }
                        return cosmic::task::message(Message::CancelRepositoryClose);
                    }
                    // Ahead of the inspector and the sidebar because the find
                    // bar is the innermost thing the user is looking at, and
                    // behind the confirmations above because those are modal.
                    if self.dismiss_chat_picker() {
                        return Task::none();
                    }
                    if let Some(card) = self.focused_repository_card_with_find() {
                        return cosmic::task::message(Message::Repository(
                            RepositoryAction::CloseFind { card },
                        ));
                    }
                    if self.worktree_dialog.is_some() || self.pull_request_dialog.is_some() {
                        return cosmic::task::message(Message::CancelDialog);
                    }
                    if self.sidebar_state.controls_expanded() {
                        self.sidebar_state.apply(&SidebarAction::ToggleControls);
                        return Task::none();
                    }
                    if self.context_inspector_is_overlay() {
                        return cosmic::task::message(Message::ContextInspector(
                            InspectorAction::Close,
                        ));
                    }
                }
                if let Some(backwards) = is_tab_press(&event)
                    && self.terminal_close_dialog_visible()
                {
                    return self.cycle_terminal_close_focus(backwards);
                }
                if let Some(backwards) = is_tab_press(&event)
                    && self.dirty_repository_close.is_some()
                {
                    return self.cycle_dirty_close_focus(backwards);
                }
                // A chord typed at a dialog is not the workspace's to act on:
                // the close chord must not close the card behind the dialog.
                // The dialog is an overlay, so it has already seen the
                // keystroke by the time this runs, and what it does with one
                // is its own affair.
                if self.workspace_inputs_blocked() {
                    return Task::none();
                }
                if !captured && let Some(task) = self.chat_scroll_key(&event) {
                    return task;
                }
                if is_save_press(&event)
                    && let Some(card) = self.workspace.focused()
                    && matches!(self.cards.get(&card), Some(CardRuntime::Repository(_)))
                {
                    return cosmic::task::message(Message::Repository(RepositoryAction::Save {
                        card,
                        overwrite: false,
                    }));
                }
                match workspace_widget::global_shortcut(
                    &event,
                    &self.settings.keyboard_shortcuts,
                    self.focused_pane(),
                ) {
                    Some(workspace_widget::GlobalShortcut::NewTerminal) => {
                        cosmic::task::message(Message::NewTerminal)
                    }
                    Some(workspace_widget::GlobalShortcut::NewChannel) => {
                        self.new_project_channel()
                    }
                    Some(workspace_widget::GlobalShortcut::ClonePane) => self.clone_focused_pane(),
                    Some(workspace_widget::GlobalShortcut::CloseFocusedCard) => {
                        match close_focused_message(&self.workspace) {
                            Some(message) => cosmic::task::message(message),
                            None => Task::none(),
                        }
                    }
                    // A focused editor claims the search chord for its own
                    // buffer, which is what it means in every editor the user
                    // has, and the settings pane for its own search field.
                    // A focused terminal never gets here: its scope leaves
                    // the chord to the shell. Anywhere else it searches
                    // projects.
                    Some(workspace_widget::GlobalShortcut::FocusProjectSearch) => {
                        if self.settings_card_focused() {
                            return iced::widget::operation::focus(
                                crate::settings_widgets::search_id(),
                            )
                            .map(cosmic::Action::App);
                        }
                        match self.focused_editable_repository_card() {
                            Some(card) => cosmic::task::message(Message::Repository(
                                RepositoryAction::OpenFind { card },
                            )),
                            None => cosmic::task::message(Message::FocusProjectSearch),
                        }
                    }
                    Some(workspace_widget::GlobalShortcut::FocusAdjacent(direction)) => {
                        cosmic::task::message(Message::FocusAdjacent(direction))
                    }
                    Some(workspace_widget::GlobalShortcut::CyclePaneWidth) => {
                        cosmic::task::message(Message::CyclePaneWidth)
                    }
                    Some(workspace_widget::GlobalShortcut::CycleWorkspace(direction)) => {
                        let keyboard::Event::KeyPressed { modifiers, .. } = event else {
                            unreachable!("shortcuts come from key presses")
                        };
                        self.cycle_workspace(direction, modifiers)
                    }
                    Some(
                        shortcut @ (workspace_widget::GlobalShortcut::ReviewToggleViewed
                        | workspace_widget::GlobalShortcut::ReviewNextFile
                        | workspace_widget::GlobalShortcut::ReviewPreviousFile
                        | workspace_widget::GlobalShortcut::ReviewNextUnviewed
                        | workspace_widget::GlobalShortcut::ReviewNextChange
                        | workspace_widget::GlobalShortcut::ReviewPreviousChange),
                    ) => {
                        let Some(card) = self.workspace.focused().filter(|card| {
                            matches!(self.cards.get(card), Some(CardRuntime::PullRequests(_)))
                        }) else {
                            return Task::none();
                        };
                        let action = match shortcut {
                            workspace_widget::GlobalShortcut::ReviewToggleViewed => {
                                pull_request_review::Action::ToggleViewedAndAdvance
                            }
                            workspace_widget::GlobalShortcut::ReviewNextFile => {
                                pull_request_review::Action::NextFile
                            }
                            workspace_widget::GlobalShortcut::ReviewNextUnviewed => {
                                pull_request_review::Action::NextUnviewed
                            }
                            workspace_widget::GlobalShortcut::ReviewNextChange => {
                                pull_request_review::Action::NextChange
                            }
                            workspace_widget::GlobalShortcut::ReviewPreviousChange => {
                                pull_request_review::Action::PreviousChange
                            }
                            _ => pull_request_review::Action::PreviousFile,
                        };
                        self.handle_pull_request_review(card, action)
                    }
                    None => Task::none(),
                }
            }
            #[cfg(feature = "latency-harness")]
            Message::LatencyRedraw { generation, at } => {
                if self.latency_focus == self.workspace.focused()
                    && self.latency.redraw_scheduled(generation, at)
                {
                    return self.latency_fixture_step();
                }
                Task::none()
            }
            #[cfg(feature = "latency-harness")]
            Message::LatencyFixtureStep => self.latency_fixture_step(),
            Message::FocusAdjacent(direction) => {
                let previous = self.workspace.focused();
                match self.workspace.focus_adjacent(direction) {
                    Ok(update) => {
                        let task = self.apply_viewport(update, true, false);
                        if previous != self.workspace.focused() {
                            self.latency.commit(crate::performance::Action::CardFocus);
                            self.latency_focus = self.workspace.focused();
                        }
                        task
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::FocusProjectSearch => {
                // `focus` unfocuses every widget whose id does not match, so
                // aiming it at an id the pane is not rendering would blur
                // whatever the user was typing into and focus nothing. Only
                // the pane knows which densities and models give the search a
                // control.
                if !sidebar::search_is_reachable(
                    &self.sidebar_model,
                    SidebarMode::for_width(self.window_width),
                ) {
                    return Task::none();
                }
                if !self.sidebar_state.controls_expanded() {
                    self.sidebar_state.apply(&SidebarAction::ToggleControls);
                }
                let id = sidebar::search_focus_id();
                iced::widget::operation::focus(id.clone())
                    .chain(iced::widget::operation::select_all(id))
                    .map(cosmic::Action::App)
            }
            Message::ToggleContextInspector if self.modal_is_open() => Task::none(),
            Message::ToggleContextInspector => {
                self.set_context_inspector_open(!self.context_inspector_open)
            }
            Message::SelectContextTab(tab) => {
                if self.active_folder_scope()
                    && matches!(tab, ContextTab::Changes | ContextTab::PullRequest)
                {
                    return Task::none();
                }
                let changed = self.context_tab != tab;
                self.context_tab = tab;
                if changed && let Some(key) = self.sidebar_state.active_worktree().cloned() {
                    self.reload_source_control(&key)
                } else {
                    Task::none()
                }
            }
            Message::ContextInspector(_) if self.modal_is_open() => Task::none(),
            Message::ContextInspector(action) => self.handle_context_inspector_action(action),
            Message::SelectSettingsCategory(category) => {
                self.settings_search.clear();
                self.settings_category = category;
                Task::none()
            }
            Message::SettingsSearchChanged(query) => {
                self.settings_search = query;
                Task::none()
            }
            Message::SettingsChanged { field, value } => self.apply_setting_input(field, value),
            Message::SettingsDraftChanged { field, value } => {
                self.queue_setting_input(field, value)
            }
            Message::SettingsAutosave { field, generation } => {
                self.autosave_setting(field, generation)
            }
            Message::ProjectUi(action) => self.handle_project_action(action),
            Message::PullRequestUi(action) => self.handle_pull_request_ui(action),
            Message::PullRequestReview { card, action } => {
                self.handle_pull_request_review(card, action)
            }
            Message::CommitUi(action) => self.update_commit_ui(action),
            Message::ChromeUi(action) => self.update_chrome(action),
            Message::ChannelUi(action) => self.handle_channel_ui(action),
            Message::SidebarSelection(action) => self.update_sidebar_selection(action),
            Message::BranchUi(action) => self.update_branch_ui(action),
            Message::AccountUi(action) => self.update_account_ui(action),
            Message::AutomationUi(action) => self.update_automation_ui(action),
            Message::GitUi(action) => self.update_git(action),
            Message::NotificationUi(action) => self.update_notifications(action),
            Message::ReviewRequests(action) => self.update_review_requests(action),
            Message::RefreshUi(action) => {
                if matches!(&action, refresh_ui::Action::Focused(id, false) if self.core.main_window_id() == Some(*id))
                {
                    self.tab_history.finish(self.workspace.focused());
                    self.workspace_history
                        .finish(self.active_workspace_destination());
                    self.workspace_cycle_modifiers = keyboard::Modifiers::empty();
                }
                let account_focus = match action {
                    refresh_ui::Action::Focused(id, focused)
                        if self.core.main_window_id() == Some(id) =>
                    {
                        Some((focused, focused && !self.refresh_ui.focused))
                    }
                    _ => None,
                };
                // A colour-only theme change reaches the hook but not the mode
                // watch, so the periodic refresh is where that one is caught.
                let refresh = self.update_refresh(action);
                let accounts = account_focus.map_or_else(Task::none, |(focused, regained)| {
                    self.accounts_focus_changed(focused, regained)
                });
                Task::batch([refresh, self.flush_document_tone(), accounts])
            }
            Message::DocumentToneStale => {
                self.document_tone_stale = self.settings.app_theme == AppTheme::System;
                self.flush_document_tone()
            }
            Message::NativeWindowAction(action) => Task::done(cosmic::Action::Cosmic(action)),
            Message::ChatUi(action) => self.handle_chat_ui(action),
            Message::ResetSettings => {
                self.settings_reset_confirm = true;
                Task::none()
            }
            Message::CancelResetSettings => {
                self.settings_reset_confirm = false;
                Task::none()
            }
            Message::ConfirmResetSettings => {
                self.settings_reset_confirm = false;
                self.reset_settings()
            }
            Message::Lsp(event) => {
                self.apply_lsp_event(event);
                Task::none()
            }
            Message::LanguageServersReaped => self.advance_exit(|exit| exit.servers_reaped = true),
            Message::WindowClosed(window) => self.advance_exit(|exit| {
                if exit.closing == Some(window) {
                    exit.closing = None;
                }
            }),
            Message::SessionSaveHealthChanged(event) => {
                if let Some(health) = &mut self.save_health {
                    health.apply(event);
                }
                Task::none()
            }
            Message::RetrySessionSave => {
                if let (Some(session), Some(health)) = (&self.session, &mut self.save_health) {
                    health.request_retry(session);
                }
                Task::none()
            }
            Message::AcknowledgeSessionRecovery => {
                if let Some(health) = &mut self.save_health {
                    health.acknowledge_recovery();
                }
                Task::none()
            }
            Message::DismissStatus | Message::DismissAllNotices => {
                self.status = None;
                self.notices.clear();
                // An update or save-health notice can keep the popup open
                // after the Dismiss all control that held focus is gone.
                let reconciled = self.reconcile_notice_popup();
                Task::batch([reconciled, self.refocus_notices()])
            }
            Message::DismissNotice(id) => {
                self.notices.dismiss(id);
                let reconciled = self.reconcile_notice_popup();
                Task::batch([reconciled, self.refocus_notices()])
            }
            Message::DirectoryFilesLoaded {
                key,
                request,
                query,
                result,
            } => {
                if request != self.refresh_ui.source_generation
                    || !self.source_control_is_bound(&key)
                    || !self.folder_scope(&key)
                {
                    return Task::none();
                }
                self.refresh_ui.source_cancel = None;
                self.sidebar_model.source_control = Some(match result {
                    Ok(files) => {
                        let mut source = SourceControlModel::directory_listing(key, &files);
                        if !query.is_empty() {
                            source.set_file_search(SourceControlModel::file_search_results(
                                query, &files, 0,
                            ));
                        }
                        source
                    }
                    Err(error) => SourceControlModel::failed(key, error),
                });
                Task::none()
            }
            Message::SourceControlLoaded {
                key,
                request,
                result,
            } => {
                if request != self.refresh_ui.source_generation
                    || !self.source_control_is_bound(&key)
                {
                    return Task::none();
                }
                self.refresh_ui.source_cancel = None;
                if let Ok(snapshot) = &result
                    && let Some(worktree) = self
                        .sidebar_model
                        .projects
                        .iter_mut()
                        .find(|project| project.id == key.project_id)
                        .and_then(|project| {
                            project
                                .worktrees
                                .iter_mut()
                                .find(|worktree| worktree.id == key.worktree_id)
                        })
                {
                    worktree.apply_status(&snapshot.status);
                }
                let mut next = match &result {
                    Ok(snapshot) => SourceControlModel::ready(key, snapshot),
                    Err(error) => SourceControlModel::failed(key, error.clone()),
                };
                // The reload replaces the rows and nothing else: an in-flight
                // mutation, its last failure, and the history already walked
                // belong to the binding, not to the page of Git output.
                if let Some(previous) = self.sidebar_model.source_control.as_ref() {
                    previous.carry_mutation_into(&mut next);
                }
                self.sidebar_model.source_control = Some(next);
                match result {
                    Ok(snapshot) => self.refresh_branch_changes(&snapshot),
                    Err(_) => {
                        self.cancel_branch_changes();
                        Task::none()
                    }
                }
            }
            Message::SourceControlHistory {
                generation,
                key,
                page,
                head,
                result,
            } => {
                if !self.source_control_is_bound(&key) || !self.history_ui.accepts_page(generation)
                {
                    return Task::none();
                }
                match result {
                    Ok(commits) => {
                        if let Some(source) = self.sidebar_model.source_control.as_mut() {
                            // A page that arrives out of order would append the
                            // same commits twice, so only the page the walk is
                            // waiting for is taken.
                            if source.head_oid.as_deref() == Some(head.as_str())
                                && source.next_history_page == page
                            {
                                source.append_history(&commits);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(source) = self.sidebar_model.source_control.as_mut() {
                            if source.head_oid.as_deref() == Some(head.as_str())
                                && source.next_history_page == page
                                && source.history_loading
                            {
                                source.history_failed(format!("Could not read history: {error}"));
                            }
                        }
                    }
                }
                Task::none()
            }
            Message::SourceControlMutated {
                key,
                kind,
                report,
                committed,
            } => {
                let mut refreshes = Vec::new();
                for (&card, runtime) in &mut self.cards {
                    if let CardRuntime::Repository(runtime) = runtime
                        && runtime.descriptor.worktree == key.canonical_path
                    {
                        refreshes.push(repository_task(runtime.refresh_after_mutation(card)));
                    }
                }
                if !self.source_control_is_bound(&key) {
                    return Task::batch(refreshes);
                }
                if committed {
                    self.sidebar_state.clear_commit_message();
                }
                if let Some(source) = self.sidebar_model.source_control.as_mut() {
                    source.set_mutation(None);
                    source.set_error(report.clone());
                }
                if let Some(report) = report {
                    self.status = Some(TransientNotice::error(report));
                }
                let _ = kind;
                // Whatever Git did, the index is the authority on the result,
                // so the panel re-reads rather than guessing.
                refreshes.push(self.reload_source_control(&key));
                Task::batch(refreshes)
            }
        };
        // `status` is the shell's write channel: ninety-odd call sites raise a
        // notice by assigning it. Draining it here is what makes those notices
        // survive the next assignment instead of silently replacing each other,
        // and it is the one place that can bound the history.
        // Before the notices drain, so one raised by a committed draft is
        // recorded with this update rather than the next.
        let task = Task::batch([task, self.flush_settings_on_blur()]);
        self.sync_notification_presence();
        self.sync_popup_keyboard_nav();
        if let Some(notice) = self.status.take() {
            match notice.severity {
                NoticeSeverity::Error => {
                    tracing::error!(message = %notice.message, "application notice")
                }
                NoticeSeverity::Warning => {
                    tracing::warn!(message = %notice.message, "application notice")
                }
                NoticeSeverity::Success => {}
            }
            self.notices.record(notice);
        }
        if self.latency_focus != self.workspace.focused() {
            self.latency.abandon_pending();
        }
        if camera_only {
            return Task::batch([task, self.release_deferred_navigation_if_unblocked()]);
        }
        let notices = self.reconcile_notice_popup();
        self.synchronize_sidebar_selection();
        self.refresh_sidebar_channels();
        if reconcile_row_selection {
            self.prune_sidebar_row_selection();
        }
        self.refresh_sidebar_worktree_panes();
        let binding = self.rebind_source_control();
        if let Some(source) = self.sidebar_model.source_control.as_ref() {
            let selected = self
                .workspace
                .focused()
                .and_then(|card| match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime))
                        if runtime.descriptor.worktree == source.key.canonical_path =>
                    {
                        runtime.selected_path()
                    }
                    _ => None,
                });
            self.sidebar_state.select_file(&source.key, selected);
        }
        let pull_request = self.sync_pull_request_inspector();
        // `view` cannot rebuild the chrome cache itself, so refresh it here.
        let chrome = self.sync_chrome();
        let deferred = self.release_deferred_navigation_if_unblocked();
        Task::batch([task, binding, chrome, deferred, pull_request, notices])
    }

    fn subscription(&self) -> Subscription<Message> {
        let operation_subscriptions = self.worktree_progress_subscriptions();
        let mut subscriptions = vec![
            self.chat_subscription(),
            self.notification_subscription(),
            self.refresh_subscription(),
            self.review_request_subscription(),
            self.git_subscription(),
            self.account_subscription(),
            self.automation_subscription(),
            self.document_tone_subscription(),
            self.update_subscription(),
            Subscription::run_with(
                TerminalWakeSource(self.terminal_wake_rx.clone()),
                terminal_wake_stream,
            )
            .map(|wake| Message::TerminalWake {
                card: wake.card,
                generation: wake.generation,
            }),
            Subscription::run_with(
                HostedTerminalSource(self.hosted_terminals_rx.clone()),
                hosted_terminal_stream,
            )
            .map(Message::HostedTerminalsObserved),
            Subscription::run_with(
                WorkspaceModelSource(self.workspace_model_rx.clone()),
                workspace_model_stream,
            )
            .map(Message::WorkspaceModelObserved),
            Subscription::run_with(
                UiActivationSource(self.activation_rx.clone()),
                ui_activation_stream,
            )
            .map(|activation| match activation.update {
                Some(request) => Message::UiUpdate(request),
                None => Message::ActivateUi {
                    token: activation.token,
                },
            }),
            event::listen_with(|event, status, event_window| match event {
                iced::Event::Keyboard(event) => Some(Message::KeyEvent {
                    event,
                    captured: matches!(status, event::Status::Captured),
                }),
                iced::Event::Window(window::Event::Rescaled(scale_factor)) => {
                    Some(Message::WindowScaleFactorChanged(scale_factor))
                }
                iced::Event::Window(window::Event::Resized(size)) => {
                    Some(Message::WindowResized(size.width, size.height))
                }
                iced::Event::Window(window::Event::CloseRequested) => {
                    Some(Message::WindowCloseRequested(event_window))
                }
                iced::Event::Window(window::Event::Closed) => {
                    Some(Message::WindowClosed(event_window))
                }
                iced::Event::Window(window::Event::Focused) => Some(Message::RefreshUi(
                    refresh_ui::Action::Focused(event_window, true),
                )),
                iced::Event::Window(window::Event::Unfocused) => Some(Message::RefreshUi(
                    refresh_ui::Action::Focused(event_window, false),
                )),
                _ => None,
            }),
        ];
        if let Some(session) = &self.session {
            subscriptions.push(
                Subscription::run_with(
                    SessionHealthSource(session.health_receiver()),
                    session_health_stream,
                )
                .map(Message::SessionSaveHealthChanged),
            );
        }
        subscriptions.push(
            Subscription::run_with(
                LspEventSource(self.lsp.events(), self.lsp.generation()),
                lsp_event_stream,
            )
            .map(Message::Lsp),
        );
        subscriptions.extend(operation_subscriptions);
        if let Some(window) = self.core.main_window_id()
            && (self.workspace.is_animating() || self.context_inspector_transition.is_some())
        {
            subscriptions.push(workspace_widget::animation_frames(window, Message::Frame));
        }
        #[cfg(feature = "integrated-browser")]
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
        #[cfg(feature = "integrated-browser")]
        if let Some(dialog) = self.browser_import_dialog() {
            return Some(dialog);
        }
        if let Some(dialog) = self.update_dialog() {
            return Some(dialog);
        }
        if let Some(intent) = &self.terminal_close
            && self.terminal_close_dialog_visible()
        {
            return self.terminal_close_dialog().map(|surface| {
                dialog_ui::overlay(
                    surface,
                    (!intent.pending).then_some(Message::CancelTerminalClose),
                    1.0,
                )
            });
        }
        let spacing = cosmic::theme::spacing();
        // Above the settings surface it is raised from, because it is about to
        // discard every shortcut, font, theme and language-server entry the
        // user configured, with no undo.
        if self.settings_reset_confirm {
            return Some(dialog_ui::overlay(
                dialog_ui::surface(
                    widget::column::with_children(vec![
                        widget::text::title3("Reset all settings?").into(),
                        notice_body(
                            "Every customization returns to its default: keyboard shortcuts, \
                             terminal font and theme, editor syntax theme, interface scale, \
                             pane widths, and the configured language servers.",
                        ),
                        notice_detail("This cannot be undone."),
                        widget::row::with_children(vec![
                            iced::widget::Space::new().width(Length::Fill).into(),
                            widget::button::standard("Cancel")
                                .id(widget::Id::new("dialog.reset-settings.cancel"))
                                .on_press(Message::CancelResetSettings)
                                .into(),
                            widget::button::destructive("Reset settings")
                                .id(widget::Id::new("dialog.reset-settings.confirm"))
                                .on_press(Message::ConfirmResetSettings)
                                .into(),
                        ])
                        .spacing(spacing.space_xxs)
                        .into(),
                    ])
                    .spacing(3.0 * design::GRID_UNIT),
                    480.0,
                ),
                Some(Message::CancelResetSettings),
                1.0,
            ));
        }
        // First, because a discard is the one dialog on screen precisely
        // because it is about to destroy work that exists nowhere else.
        if let Some(pending) = self.sidebar_state.pending_discard() {
            return Some(dialog_ui::overlay(
                sidebar::discard_confirmation_dialog(pending).map(Message::Sidebar),
                Some(Message::Sidebar(SidebarAction::CancelDiscard)),
                1.0,
            ));
        }
        if let Some(close) = self.dirty_repository_close.as_ref() {
            let card = close.card;
            let runtime = match self.cards.get(&card) {
                Some(CardRuntime::Repository(runtime)) => runtime,
                _ => return None,
            };
            let title = self
                .card_state(card)
                .map_or("Repository".into(), |state| state.title.clone());
            let mut rows: Vec<Element<'_, Message>> = vec![
                widget::text::title3(if runtime.has_durability_warning() {
                    format!("Confirm the save to {title} before closing")
                } else {
                    format!("Save changes to {title}?")
                })
                .into(),
                widget::text::body(if runtime.has_durability_warning() {
                    "The file was replaced on disk, but its durability is still unconfirmed. Retry the save, or cancel and reload from disk before closing; discarding editor changes cannot resolve this warning."
                } else {
                    "This editor has unsaved changes. Save them before closing, discard them, or keep the repository open."
                })
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            ];
            if let Some(error) = runtime.save_error() {
                rows.push(
                    widget::text::body(error.to_owned())
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                );
            }
            let save_message = runtime
                .can_save()
                .then_some(if runtime.has_save_conflict() {
                    Message::OverwriteAndCloseRepository(card)
                } else {
                    Message::SaveAndCloseRepository(card)
                });
            let mut controls: Vec<Element<'_, Message>> = vec![
                widget::button::standard("Cancel")
                    .id(dirty_close_control_id(card, DirtyCloseControl::Cancel))
                    .on_press_maybe(
                        (!runtime.is_saving()).then_some(Message::CancelRepositoryClose),
                    )
                    .into(),
            ];
            if runtime.can_abandon_unsaved_changes() {
                controls.push(
                    widget::button::destructive("Discard")
                        .id(dirty_close_control_id(card, DirtyCloseControl::Discard))
                        .on_press(Message::DiscardAndCloseRepository(card))
                        .into(),
                );
            }
            controls.push(
                widget::button::suggested(if runtime.has_save_conflict() {
                    "Overwrite and close"
                } else if runtime.has_durability_warning() {
                    "Retry save and close"
                } else {
                    "Save and close"
                })
                .id(dirty_close_control_id(card, DirtyCloseControl::Save))
                .on_press_maybe(save_message)
                .into(),
            );
            rows.push(
                widget::row::with_children(controls)
                    .spacing(spacing.space_xxs)
                    .into(),
            );
            return Some(dialog_ui::overlay(
                dialog_ui::surface(
                    widget::container(
                        widget::column::with_children(rows).spacing(3.0 * design::GRID_UNIT),
                    )
                    .id(widget::Id::new(format!("dialog.dirty-close.{card}"))),
                    560.0,
                ),
                (!runtime.is_saving()).then_some(Message::CancelRepositoryClose),
                1.0,
            ));
        }
        if let Some(surface) = self.sidebar_selection_dialog() {
            return Some(dialog_ui::overlay(
                surface,
                Some(Message::SidebarSelection(
                    sidebar_selection_ui::Action::Dismiss,
                )),
                1.0,
            ));
        }
        if let Some(surface) = self.project_dialog_view() {
            return Some(dialog_ui::overlay(
                surface,
                Some(Message::ProjectUi(project_ui::Action::Dismiss)),
                1.0,
            ));
        }
        if let Some(surface) = self.channel_dialog_view() {
            return Some(dialog_ui::overlay(
                surface,
                Some(Message::ChannelUi(channel_ui::Action::Dismiss)),
                1.0,
            ));
        }
        if let Some((surface, dismiss)) = self.worktree_dialog_view() {
            return Some(dialog_ui::overlay(surface, dismiss, 1.0));
        }

        let dialog = self.pull_request_dialog.as_ref()?;
        let head = dialog.head.trim();
        let base = dialog.base.trim();
        let draft = if dialog.draft { ", draft" } else { "" };
        let route = if head.is_empty() || base.is_empty() {
            "Choose a head and a base".to_owned()
        } else {
            format!("{head} → {base}{draft}")
        };
        let context = match &dialog.worktree {
            Some(path) => format!("{} · {route}", path.display()),
            None => route,
        };
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::text::title3("Create pull request").into(),
            design::caption(format!("{} · {context}", dialog.repository.gh_selector())).into(),
            widget::column::with_children(vec![
                design::caption("Title").into(),
                widget::text_input("Title", &dialog.title)
                    .id(pull_request_dialog_focus_id())
                    .on_input(Message::PullRequestTitleChanged)
                    .into(),
            ])
            .spacing(spacing.space_xxxs)
            .into(),
            widget::column::with_children(vec![
                design::caption("Head branch").into(),
                widget::text_input("Head branch", &dialog.head)
                    .id(widget::Id::new("dialog.pull-request.head"))
                    .on_input(Message::PullRequestHeadChanged)
                    .into(),
            ])
            .spacing(spacing.space_xxxs)
            .into(),
            widget::column::with_children(vec![
                design::caption("Base branch").into(),
                widget::text_input("Base branch", &dialog.base)
                    .id(widget::Id::new("dialog.pull-request.base"))
                    .on_input(Message::PullRequestBaseChanged)
                    .into(),
            ])
            .spacing(spacing.space_xxxs)
            .into(),
            widget::column::with_children(vec![
                design::caption("Description").into(),
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
        ];
        if let Some(confirmed) = dialog.retry_confirmation {
            rows.push(
                widget::text::body(
                    "GitHub may have created this pull request. Check GitHub before retrying to avoid creating a duplicate.",
                )
                .into(),
            );
            rows.push(
                widget::checkbox(confirmed)
                    .label("I checked GitHub; no pull request was created")
                    .on_toggle(Message::PullRequestRetryConfirmed)
                    .into(),
            );
        }
        rows.push(
            widget::row::with_children(vec![
                widget::button::standard("Cancel")
                    .on_press(Message::CancelDialog)
                    .into(),
                widget::button::suggested("Create")
                    .on_press_maybe(
                        (!head.is_empty()
                            && !base.is_empty()
                            && !dialog.title.trim().is_empty()
                            && dialog.retry_confirmation != Some(false)
                            && self
                                .github_availability
                                .get(&dialog.project_id)
                                .and_then(GitHubAvailability::repository_key)
                                == Some(&dialog.repository))
                        .then_some(Message::SubmitPullRequest),
                    )
                    .into(),
            ])
            .spacing(spacing.space_xxs)
            .into(),
        );
        Some(dialog_ui::overlay(
            dialog_ui::surface(
                widget::column::with_children(rows).spacing(3.0 * design::GRID_UNIT),
                620.0,
            ),
            Some(Message::CancelDialog),
            1.0,
        ))
    }

    /// The application opens no window but its main one, which stops being
    /// the main window only when the exit closes it. libcosmic's default
    /// panics, and iced may still draw the window before it removes it.
    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        widget::Space::new().into()
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
            self.modal_is_open(),
        )
        .map(Message::Sidebar);

        // The focused card's own name and path are the card header's job, and
        // the window's header bar already carries the name; see
        // `sync_window_title`.
        let docked_context_inspector = self.context_inspector_visible()
            && matches!(
                inspector::mount_for_width(window_width, true, true),
                Some(InspectorLayout::Docked)
            );
        // A docked inspector's resize handle draws the viewport's trailing
        // edge; a card scrolled against it would double that rule with its
        // own splitter and compete for the same drag. Every other card keeps
        // the splitter that separates it from its neighbour or the empty
        // strip beyond.
        let card_beside_inspector = docked_context_inspector
            .then(|| self.workspace.card_at_viewport_edge())
            .flatten();
        let has_multiple_panes = self.workspace.cards().len() > 1;

        let workspace: Element<'_, Message> = if self.workspace.cards().is_empty() {
            let action_buttons = || {
                vec![
                    widget::button::suggested("New terminal")
                        .id(widget::Id::new("empty.new-terminal"))
                        .name("New terminal")
                        .leading_icon(crate::ui_controls::symbolic_icon(
                            "utilities-terminal-symbolic",
                        ))
                        .on_press(Message::NewTerminal)
                        .into(),
                    widget::button::standard("Open folder")
                        .id(widget::Id::new("empty.open-repository"))
                        .name("Open folder")
                        .leading_icon(crate::ui_controls::symbolic_icon("folder-open-symbolic"))
                        .on_press(Message::OpenRepository)
                        .into(),
                    widget::button::standard("New browser")
                        .id(widget::Id::new("empty.new-browser"))
                        .name("New browser")
                        .leading_icon(crate::ui_controls::symbolic_icon(
                            "internet-web-browser-symbolic",
                        ))
                        .on_press(Message::NewBrowser)
                        .into(),
                ]
            };
            let actions: Element<'_, Message> = if matches!(
                design::ShellLayout::for_width(window_width).dock,
                DockLayout::Overlay
            ) {
                widget::column::with_children(action_buttons())
                    .spacing(spacing.space_xxs)
                    .width(Length::Fill)
                    .into()
            } else {
                widget::row::with_children(action_buttons())
                    .spacing(spacing.space_xxs)
                    .into()
            };
            let surface = widget::container(
                widget::column::with_children(vec![
                    crate::ui_controls::symbolic_icon("utilities-terminal-symbolic")
                        .icon()
                        .size(32)
                        .into(),
                    widget::text::title2("Start a workspace").into(),
                    widget::text::body(
                        "Open a folder for project context, or start a standalone terminal.",
                    )
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into(),
                    design::caption(STARTING_DIRECTORY_CAPTION.as_str())
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                    actions,
                ])
                .spacing(spacing.space_s)
                .max_width(CONTENT_MAX_WIDTH),
            )
            .class(cosmic::theme::Container::Card)
            .padding(spacing.space_m)
            .width(Length::Fill)
            .max_width(CONTENT_MAX_WIDTH);
            widget::container(surface)
                .center(Length::Fill)
                .padding(spacing.space_m)
                .into()
        } else {
            workspace_widget::view(
                &self.workspace,
                &self.settings.keyboard_shortcuts,
                self.focused_pane(),
                self.settings.focus_follows_mouse,
                self.workspace.viewport_width(),
                self.workspace_scroll_id.clone(),
                move |context| {
                    self.workspace_card_view(
                        context.id,
                        context.focused,
                        has_multiple_panes && card_beside_inspector != Some(context.id),
                    )
                },
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
                    viewport: {
                        let generation = self.workspace_view_generation;
                        move |event| Message::Viewport { generation, event }
                    },
                },
            )
        };

        let main_content = self.workspace_chrome(workspace);

        let context_model = (self.context_inspector_visible()
            && self.context_tab == ContextTab::Details)
            .then(|| self.workspace.focused())
            .flatten()
            .and_then(|card| {
                self.card_state(card).and_then(|state| {
                    let repository = match self.cards.get(&card) {
                        Some(CardRuntime::Repository(runtime)) => Some(runtime.inspector_context()),
                        _ => None,
                    };
                    let model = inspector::model_for_card(
                        state,
                        self.inspector_runtime_status(card),
                        &self.sidebar_model.projects,
                        repository.as_ref(),
                    );
                    Some(match self.lsp_context(card) {
                        Some(lsp) => inspector::with_lsp(model, card, lsp),
                        None => model,
                    })
                })
            });
        let surface: Element<'_, Message> = if self.context_inspector_visible() {
            let width = self
                .settings
                .inspector_width
                .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH)
                .min(window_width.max(0.0));
            let progress = self.context_inspector_progress();
            let visible_width = width * progress;
            let splitter: Element<'_, Message> = Splitter::new(
                widget::Id::new("context-inspector.resize-handle"),
                inspector::RESIZE_HANDLE_NAME,
                width,
                design::INSPECTOR_MIN_WIDTH,
                design::INSPECTOR_MAX_WIDTH,
                SplitterEdge::Leading,
                self.modal_is_open(),
                |width| Message::ContextInspector(InspectorAction::Resize { width }),
                || Message::ContextInspector(InspectorAction::ResizeFinished),
            )
            .into();
            let pane = crate::interaction::reveal_width(
                self.context_panel(context_model, width),
                width,
                progress,
            );
            match inspector::mount_for_width(window_width, true, true)
                .expect("an open contextual inspector with a model has a mount")
            {
                InspectorLayout::Docked => {
                    let content: Element<'_, Message> =
                        widget::row::with_children(vec![sidebar, main_content, pane])
                            .spacing(0)
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into();
                    let edge = widget::container(splitter)
                        .padding(Padding {
                            right: visible_width,
                            ..Padding::ZERO
                        })
                        .align_x(Alignment::End)
                        .width(Length::Fill)
                        .height(Length::Fill);
                    crate::interaction::layer(content, edge.into())
                }
                InspectorLayout::Overlay => {
                    let inspector_surface = crate::interaction::layer(
                        pane,
                        widget::container(splitter)
                            .align_x(Alignment::Start)
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into(),
                    );
                    let background = widget::row::with_children(vec![sidebar, main_content])
                        .spacing(0)
                        .width(Length::Fill)
                        .height(Length::Fill);
                    let dismiss = crate::interaction::opaque(
                        widget::mouse_area(
                            widget::container(iced::widget::Space::new())
                                .width(Length::Fill)
                                .height(Length::Fill)
                                .class(cosmic::theme::Container::custom(|_| {
                                    iced::widget::container::Style {
                                        background: Some(iced::Background::Color(
                                            iced::Color::from_rgba(0.0, 0.0, 0.0, 0.4),
                                        )),
                                        ..Default::default()
                                    }
                                })),
                        )
                        .on_press(Message::ContextInspector(InspectorAction::Close)),
                    );
                    let aligned = widget::container(inspector_surface)
                        .align_x(Alignment::End)
                        .width(Length::Fill)
                        .height(Length::Fill);
                    let foreground = crate::interaction::layer(dismiss, aligned.into());
                    inspector::ModalOverlay::new(background, foreground).into()
                }
            }
        } else {
            widget::row::with_children(vec![sidebar, main_content])
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        };
        // The bar comes and goes, but the tree around the workspace must not:
        // iced matches widget state by position, so wrapping the surface only
        // while the bar shows would reset every scroll offset and focus in it.
        let bar = self
            .status_bar()
            .unwrap_or_else(|| iced::widget::Space::new().height(Length::Shrink).into());
        let update = widget::container(
            self.update_notice(false)
                .unwrap_or_else(|| widget::Space::new().height(Length::Shrink).into()),
        );
        let surface: Element<'_, Message> =
            widget::column::with_children(vec![update.into(), surface, bar])
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        let surface = crate::interaction::drive_animation(
            surface,
            self.workspace.is_animating() || self.context_inspector_transition.is_some(),
        );
        let surface = crate::file_drop::surface(surface, self.core.scale_factor());
        #[cfg(feature = "latency-harness")]
        return latency_ui::instrument(
            surface,
            (self.latency_focus == self.workspace.focused())
                .then(|| self.latency.pending_generation())
                .flatten(),
            self.latency_fixture.is_some() && !self.latency_fixture_started,
        );
        #[cfg(not(feature = "latency-harness"))]
        surface
    }
}

impl Drop for Cagents {
    fn drop(&mut self) {
        self.latency.abandon_pending();
        self.persist();
        self.fold_settings_drafts();
        if let Err(error) = self.save_settings_now() {
            tracing::error!(%error, "failed to persist settings");
        }
        if let Some(session) = &mut self.session {
            session.shutdown();
        }
        // Reached without the exit path having run - a panic, or a close this
        // application did not drive - so nothing has awaited the reap and the
        // runtime is going away regardless. `shutdown` alone would leave every
        // server merely asked to stop, with nobody left to insist.
        self.lsp.shutdown();
        self.lsp.kill_retired_now();

        // The exit path already released the parent before closing the
        // window. Otherwise the window is still open, and this must complete
        // before Rust drops `core` and, with it, the parent window.
        #[cfg(feature = "integrated-browser")]
        self.release_browser_parent();
        for teardown in self.terminal_teardowns.drain(..) {
            let _ = teardown.join();
        }
    }
}

/// The stable header control that invokes and regains focus from the inspector.
fn context_inspector_toggle_id() -> widget::Id {
    widget::Id::new("command.toggle-context-inspector")
}

fn dirty_close_control_id(card: CardId, control: DirtyCloseControl) -> widget::Id {
    let control = match control {
        DirtyCloseControl::Cancel => "cancel",
        DirtyCloseControl::Discard => "discard",
        DirtyCloseControl::Save => "save",
    };
    widget::Id::new(format!("dialog.dirty-close.{card}.{control}"))
}

/// The pull-request dialog's first field, focused when it opens.
fn pull_request_dialog_focus_id() -> widget::Id {
    widget::Id::new("dialog.pull-request.title")
}

#[cfg(feature = "integrated-browser")]
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
            address: BrowserAddress::location(&session.current_url),
            load_state: None,
        })
        .map_err(|error| error.to_string())
}

#[cfg(feature = "integrated-browser")]
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

fn load_project(
    path: &Path,
    project_id: Option<ProjectId>,
    cancellation: CancellationToken,
    kind: Option<ProjectKind>,
) -> Result<LoadedRepository, String> {
    let directory = path
        .canonicalize()
        .map_err(|error| format!("could not resolve project {}: {error}", path.display()))?;
    let folder = match kind {
        Some(ProjectKind::Folder) => true,
        Some(ProjectKind::Git) => false,
        None => {
            let location = crate::project_location::discover(&directory)?;
            location.kind == ProjectKind::Folder
        }
    };
    if !folder {
        return load_repository(&directory, project_id, cancellation);
    }
    std::fs::read_dir(&directory)
        .map_err(|error| format!("could not read folder {}: {error}", directory.display()))?;
    Ok(LoadedRepository {
        project: ProjectState {
            id: project_id.unwrap_or_default(),
            generation: 0,
            name: crate::workspace_model::repository_display_name(&directory),
            repository: directory,
            kind: ProjectKind::Folder,
            existence: Existence::Open,
        },
        repository: None,
        worktrees: Vec::new(),
        github: GitHubAvailability::NotAssociated,
        review_repository: None,
        head_repositories: HashMap::new(),
    })
}

fn load_repository(
    path: &Path,
    project_id: Option<ProjectId>,
    cancellation: CancellationToken,
) -> Result<LoadedRepository, String> {
    let service = RepositoryService::system("git");
    // Git discovery and stable worktree identities must see the real checkout,
    // not a spelling introduced by a picker symlink or a relative launch path.
    let path = path
        .canonicalize()
        .map_err(|error| format!("could not resolve repository {}: {error}", path.display()))?;
    let repository = service
        .discover(&path, &cancellation)
        .map_err(|error| error.to_string())?;
    let worktrees = service
        .list_worktrees(&repository.common_dir, &cancellation)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|worktree| worktree_ui::worktree_directory_available(&worktree.path))
        .map(|worktree| {
            let status = (!worktree.bare)
                .then(|| {
                    service
                        .status_with_untracked(&worktree.path, false, &cancellation)
                        .ok()
                })
                .flatten();
            (worktree, status)
        })
        .collect::<Vec<_>>();
    let head_repositories = worktrees
        .iter()
        .filter_map(|(record, _)| {
            let branch = record.branch.as_deref()?.strip_prefix("refs/heads/")?;
            let key = crate::github::worktree_head_repository(&record.path, branch, &cancellation)?;
            Some((record.id, key))
        })
        .collect();
    let review_repository = service
        .remotes(&repository.selected_path, &cancellation)
        .ok()
        .and_then(|remotes| {
            pull_request_review::ReviewSource::from_remotes(&repository.selected_path, &remotes)
        })
        .map(|source| source.repository);
    let github_availability = load_repository_github(&repository, &cancellation);
    // The shared model's own derivation, not a second copy of it: a project
    // this window opened and one the CLI opened are one row in the registry,
    // and two rules for its name would make the sidebar show whichever of them
    // wrote last.
    let name = crate::workspace_model::repository_display_name(&repository.common_dir);
    Ok(LoadedRepository {
        project: ProjectState {
            kind: Default::default(),
            id: project_id.unwrap_or_default(),
            generation: 0,
            name,
            repository: repository.common_dir.clone(),
            existence: Existence::Open,
        },
        repository: Some(repository),
        worktrees,
        github: github_availability,
        review_repository,
        head_repositories,
    })
}

fn load_repository_github(
    repository: &RepositoryInfo,
    cancellation: &CancellationToken,
) -> GitHubAvailability {
    let service = RepositoryService::system("git");
    let github = GitHubService::system("gh");
    (|| {
        let remotes = service
            .remotes(&repository.selected_path, &cancellation)
            .map_err(|error| GitHubAvailability::Failed {
                detail: error.to_string(),
            })?;
        if !github
            .has_associated_remote(&remotes, &cancellation)
            .map_err(GitHubAvailability::from_error)?
        {
            return Err(GitHubAvailability::NotAssociated);
        }
        let key = github
            .resolve_repository(&repository.selected_path, &cancellation)
            .map_err(GitHubAvailability::from_error)?;
        let auth = github
            .auth_status(&key, &cancellation)
            .map_err(GitHubAvailability::from_error)?;
        if auth.state != crate::github::GhAuthState::Authenticated {
            return Err(GitHubAvailability::Unauthenticated);
        }
        let mut pull_requests = github
            .list(
                &ListPullRequestsRequest {
                    repository: key.clone(),
                    state: PullRequestStateFilter::Open,
                    limit: sidebar::PULL_REQUEST_LIST_LIMIT,
                },
                &cancellation,
            )
            .map_err(GitHubAvailability::from_error)?;
        // Closed history must not consume the open-PR page. Its failure also
        // must not discard successfully refreshed open conversation authority.
        if let Ok(history) = github.list(
            &ListPullRequestsRequest {
                repository: key.clone(),
                state: PullRequestStateFilter::All,
                limit: sidebar::PULL_REQUEST_LIST_LIMIT,
            },
            &cancellation,
        ) {
            for entry in history {
                if let Some(current) = pull_requests
                    .iter_mut()
                    .find(|current| current.number == entry.number)
                {
                    *current = entry;
                } else {
                    pull_requests.push(entry);
                }
            }
        }
        Ok(GitHubAvailability::Ready {
            repository_key: key,
            pull_requests: pull_requests
                .into_iter()
                .map(|pull_request| {
                    let checks = pull_request.check_summaries();
                    (pull_request, checks)
                })
                .collect(),
            refreshed_at: unix_millis(),
        })
    })()
    .unwrap_or_else(|unavailable| unavailable)
}

/// Finds the terminal a start should attach to, or the launch it should make.
fn recover_terminal(
    session: TerminalSession,
    explicit_retry: bool,
) -> Result<(daemon_recovery::RecoveredTerminal, TerminalStartMode), String> {
    let recovered = daemon_recovery::reconcile(session, explicit_retry)?;
    let mode = if recovered.attach_completed {
        TerminalStartMode::Attach(
            recovered
                .expected_host
                .clone()
                .ok_or("completed holder has no owning daemon")?,
        )
    } else {
        terminal_start_mode(&recovered.session)?
    };
    Ok((recovered, mode))
}

fn open_terminal(
    card: CardId,
    recovered: daemon_recovery::RecoveredTerminal,
    mode: TerminalStartMode,
    config: TerminalConfig,
    sender: async_channel::Sender<TerminalWakeup>,
) -> Result<StartedTerminal, String> {
    let recovery_lease = recovered.recovery_lease;
    let session = recovered.session;
    let spawned = matches!(&mode, TerminalStartMode::Spawn(_));
    let wake = move |wake: crate::terminal::TerminalWake| {
        // Each terminal source coalesces visual and semantic edges until the
        // app acknowledges them. The unbounded app queue is therefore O(live
        // cards), not O(terminal event volume), and cannot strand a source
        // edge because a fixed queue filled.
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
            let mut session = session.clone();
            session.launch = recipe;
            HostedTerminalHandle::attach_or_spawn(session, config, recovered.expected_host, wake)
        }
    }
    .map_err(|error| error.to_string())?;
    // The daemon may wait up to its holder startup timeout before it can send
    // Spawned. Keep the handle alive through that reply and a reconnect grace.
    handle.wait_until_attached(crate::terminal::SPAWN_START_TIMEOUT + Duration::from_secs(10))?;
    drop(recovery_lease);
    let surface = TerminalSurface::from(handle);
    Ok(StartedTerminal {
        handle: surface,
        session,
        spawned,
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
        TerminalContinuity::RelaunchAvailable { recipe, .. } => Ok(TerminalStartMode::Spawn(
            session
                .agent_resume_recipe()
                .unwrap_or_else(|| recipe.clone()),
        )),
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

fn close_focused_message(workspace: &Workspace) -> Option<Message> {
    workspace.focused().map(Message::CloseCard)
}

const fn effective_surface_scale(display_scale: f32, ui_scale: f32) -> f32 {
    display_scale * ui_scale
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
    use crate::model::WorktreeId;
    use cosmic::iced::futures::StreamExt;

    #[test]
    fn empty_workspace_new_terminal_uses_the_presented_starting_directory() {
        let mut app = context_inspector_test_app(false);
        assert!(app.terminal_directory().is_none());
        let _ = app.update(Message::NewTerminal);
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        let CardKind::Terminal(terminal) = &card.kind else {
            panic!("New terminal must create a terminal without a project");
        };
        assert_eq!(terminal.launch.working_directory, *STARTING_DIRECTORY);
        assert_eq!(
            *STARTING_DIRECTORY_CAPTION,
            format!(
                "Starting directory: {}",
                display_path(&terminal.launch.working_directory)
            ),
        );
        assert!(app.sidebar_model.projects.is_empty());
    }

    #[test]
    fn update_prepare_vetoes_dirty_editor_without_closing_or_freezing() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        let _ = app.update(Message::UiUpdate(instance::UiUpdateRequest {
            action: instance::UiUpdateAction::Prepare,
            response,
        }));
        assert!(receiver.try_recv().unwrap().starts_with("error:"));
        assert!(!app.update_prepared);
        assert!(app.cards.contains_key(&card));
        assert!(app.dirty_repository_close.is_none());
    }

    #[test]
    fn cancelled_update_unfreezes_the_existing_window() {
        let mut app = context_inspector_test_app(true);
        app.update_prepared = true;
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        let _ = app.update(Message::UiUpdate(instance::UiUpdateRequest {
            action: instance::UiUpdateAction::Cancel,
            response,
        }));
        assert_eq!(receiver.try_recv().unwrap(), "ok");
        assert!(!app.update_prepared);
        assert!(app.workspace.focused().is_some());
    }

    #[test]
    fn update_freeze_drains_diff_basis_resolution_without_accepting_new_choices() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "notes.txt", "");
        let choice = repository_widget::CommittedChoice::LastCommit;
        drop(runtime.update(RepositoryAction::SelectCommittedBasis { card, choice }));
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.update_prepared = true;
        let basis = DiffBasis::CommitToParent {
            oid: crate::repository::Oid::parse_object("ab".repeat(20)).unwrap(),
            parent: None,
        };
        let task = app.update(Message::Repository(RepositoryAction::DiffBasisResolved {
            identity,
            choice,
            result: Ok(repository_widget::ResolvedBasis::Ready(basis)),
        }));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                    while let Some(action) = stream.next().await {
                        if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action
                        {
                            drop(app.update(message));
                        }
                    }
                }
            });
        let context = match &app.cards[&card] {
            CardRuntime::Repository(runtime) => runtime.inspector_context(),
            _ => unreachable!(),
        };
        assert_eq!(context.diff_basis, Some("Commit"));
        drop(
            app.update(Message::Repository(RepositoryAction::ApplyDiffBasis {
                card,
                basis: DiffBasis::IndexToWorktree,
            })),
        );
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            unreachable!()
        };
        assert_eq!(runtime.inspector_context().diff_basis, Some("Commit"));
    }

    fn browser_kind(raw: &str) -> CardKind {
        let url = Url::parse(raw).unwrap();
        CardKind::Browser(crate::model::BrowserSession {
            id: crate::model::BrowserId::new(),
            current_url: url.clone(),
            history: vec![url],
            history_index: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
        })
    }

    #[test]
    fn archived_worktree_pr_requires_exact_branch_and_head() {
        let head = GitHubRepoKey::new("github.com", None, "acme", "repo").unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new(),
            path: PathBuf::from("/repo/worktrees/old"),
            created_at_unix_ms: None,
            head_oid: Some("abc".into()),
            branch: Some("refs/heads/old-feature".into()),
            detached: false,
            bare: false,
            lock: Default::default(),
            prunable_reason: None,
        };
        let pr: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number": 7, "title": "Old feature", "state": "MERGED",
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
            "headRefName": "old-feature", "headRefOid": "abc", "baseRefName": "main",
            "updatedAt": "2026-01-01T00:00:00Z", "url": "https://github.com/acme/repo/pull/7"
        }))
        .unwrap();
        assert_eq!(
            matching_attached_pr(&record, &head, &[pr.clone()]).map(|pr| pr.number),
            Some(7)
        );
        let mut duplicate = pr.clone();
        duplicate.number = 8;
        assert!(matching_attached_pr(&record, &head, &[pr.clone(), duplicate]).is_none());
        assert!(
            matching_attached_pr(
                &WorktreeRecord {
                    head_oid: Some("def".into()),
                    ..record.clone()
                },
                &head,
                &[pr.clone()]
            )
            .is_none()
        );
        assert!(
            matching_attached_pr(
                &WorktreeRecord {
                    branch: Some("refs/heads/other".into()),
                    ..record.clone()
                },
                &head,
                &[pr.clone()]
            )
            .is_none()
        );
        assert!(
            matching_attached_pr(
                &WorktreeRecord {
                    detached: true,
                    ..record
                },
                &head,
                &[pr]
            )
            .is_none()
        );
    }

    #[test]
    fn attached_pr_overrides_reused_branch_without_hiding_unpushed_open_work() {
        let project_id = ProjectId::new();
        let record = WorktreeRecord {
            id: WorktreeId::new(),
            path: "/repo/feature".into(),
            created_at_unix_ms: None,
            head_oid: Some("old".into()),
            branch: Some("refs/heads/feature".into()),
            detached: false,
            bare: false,
            lock: Default::default(),
            prunable_reason: None,
        };
        let open: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number": 9, "title": "New", "state": "OPEN", "headRefName": "feature", "headRefOid": "new",
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
            "baseRefName": "main", "updatedAt": "2026-09-01", "url": "https://github.com/acme/repo/pull/9"
        })).unwrap();
        let merged: crate::github::PullRequest = serde_json::from_value(serde_json::json!({
            "number": 7, "title": "Old", "state": "MERGED", "headRefName": "feature", "headRefOid": "old",
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"acme"},
            "baseRefName": "main", "updatedAt": "2026-01-01", "url": "https://github.com/acme/repo/pull/7"
        })).unwrap();
        let make_project = |record: &WorktreeRecord| {
            let mut row = SidebarWorktree::from_repository(record, None);
            row.head_repository = open.head_repository_key();
            SidebarProject::new(
                project_id,
                RepositoryId::new(),
                "repo".into(),
                "/repo".into(),
                vec![row],
                vec![SidebarPullRequest::from_github(project_id, &open, &[])],
            )
        };
        let mut project = make_project(&record);
        apply_attached_worktree_prs(
            &mut project,
            &[(record.clone(), None)],
            &[open.clone()],
            &[merged],
        );
        assert_eq!(
            project.worktrees[0].pull_request_state,
            Some(sidebar::WorktreePullRequestState::Merged)
        );
        let unpushed = WorktreeRecord {
            head_oid: Some("local-only".into()),
            ..record
        };
        let mut project = make_project(&unpushed);
        apply_attached_worktree_prs(&mut project, &[(unpushed, None)], &[open], &[]);
        assert_eq!(
            project.worktrees[0].pull_request_state,
            Some(sidebar::WorktreePullRequestState::Open)
        );
    }

    #[test]
    fn archived_pr_with_moved_tip_requires_ancestry() {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(directory.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "first",
        ]);
        let pr_head = git(&["rev-parse", "HEAD"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "next",
        ]);
        let advanced = git(&["rev-parse", "HEAD"]);
        assert!(pr_head_is_ancestor(directory.path(), &pr_head, &advanced));
        git(&["checkout", "--orphan", "new-history"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "unrelated",
        ]);
        let unrelated = git(&["rev-parse", "HEAD"]);
        assert!(!pr_head_is_ancestor(directory.path(), &pr_head, &unrelated));
    }

    #[test]
    fn pull_request_head_prefers_the_context_worktree() {
        let project_id = ProjectId::new();
        let selected = PathBuf::from("/repo/selected");
        let project = SidebarProject::new(
            project_id,
            RepositoryId::new(),
            "repo".into(),
            PathBuf::from("/repo"),
            vec![
                SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    PathBuf::from("/repo/first"),
                    Some("first".into()),
                    false,
                    false,
                    None,
                    None,
                    None,
                ),
                SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    selected.clone(),
                    Some("selected".into()),
                    false,
                    false,
                    None,
                    None,
                    None,
                ),
            ],
            Vec::new(),
        );

        assert_eq!(
            pull_request_head(&[project.clone()], project_id, None),
            "",
            "a project-level create must not adopt the first worktree's branch"
        );
        assert_eq!(
            pull_request_head(&[project.clone()], project_id, Some(&selected)),
            "selected"
        );
        assert_eq!(
            pull_request_head(&[project], project_id, Some(Path::new("/repo/missing"))),
            ""
        );
    }

    #[test]
    fn a_proven_remote_default_is_the_branch_github_would_merge_into() {
        let base = crate::repository::ValidatedRefName::parse("refs/remotes/te/am/trunk").unwrap();
        assert_eq!(
            proven_pull_request_base(
                &sidebar::BranchChanges::Loading(base),
                &["te".into(), "te/am".into()],
            ),
            "trunk"
        );
        assert_eq!(
            proven_pull_request_base(&sidebar::BranchChanges::Unavailable("no base"), &[]),
            ""
        );
    }

    #[test]
    fn opening_a_pull_request_does_not_guess_main_or_the_first_branch() {
        let mut app = context_inspector_test_app(false);
        let project_id = ProjectId::new();
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        app.github_availability.insert(
            project_id,
            GitHubAvailability::Ready {
                repository_key: repository,
                pull_requests: Vec::new(),
                refreshed_at: 1,
            },
        );
        let selected = PathBuf::from("/repo/selected");
        app.sidebar_model.projects = vec![SidebarProject::new(
            project_id,
            crate::repository::RepositoryId::new(),
            "repo".into(),
            PathBuf::from("/repo"),
            vec![
                SidebarWorktree::new(
                    WorktreeId::new(),
                    PathBuf::from("/repo/first"),
                    Some("first".into()),
                    false,
                    false,
                    None,
                    None,
                    None,
                ),
                SidebarWorktree::new(
                    WorktreeId::new(),
                    selected.clone(),
                    Some("selected".into()),
                    false,
                    false,
                    None,
                    None,
                    None,
                ),
            ],
            Vec::new(),
        )];

        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id,
            worktree: None,
        }));
        let dialog = app.pull_request_dialog.as_ref().unwrap();
        assert_eq!(dialog.head, "");
        assert_eq!(dialog.base, "");
        assert!(dialog.worktree.is_none());

        let _ = app.update(Message::CancelDialog);
        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id,
            worktree: Some(selected.clone()),
        }));
        let dialog = app.pull_request_dialog.as_ref().unwrap();
        assert_eq!(dialog.head, "selected");
        assert_eq!(dialog.base, "");
        assert_eq!(dialog.worktree.as_deref(), Some(selected.as_path()));
    }

    fn health_event(
        revision: u64,
        health: SessionSaveHealth,
        last_successful_flush_at_unix_ms: Option<i64>,
    ) -> SessionSaveHealthEvent {
        SessionSaveHealthEvent {
            revision,
            health,
            last_successful_flush_at_unix_ms,
            quarantined: Vec::new(),
        }
    }

    #[test]
    fn save_health_timestamps_are_exact_utc_values() {
        assert_eq!(unix_time_label(0), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(
            unix_time_label(946_684_800_123),
            "2000-01-01 00:00:00.123 UTC"
        );
    }

    #[test]
    fn closed_health_channel_reports_unavailable_once_then_ends() {
        use cosmic::iced::futures::{StreamExt, executor::block_on};

        let (sender, receiver) = async_channel::unbounded();
        drop(sender);
        let source = SessionHealthSource(receiver);
        let mut stream = session_health_stream(&source);

        block_on(async {
            assert_eq!(
                stream.next().await,
                Some(SessionSaveHealthEvent {
                    revision: u64::MAX,
                    health: SessionSaveHealth::WriterUnavailable,
                    last_successful_flush_at_unix_ms: None,
                    quarantined: Vec::new(),
                })
            );
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn acknowledging_the_last_recovery_notice_releases_workspace_input() {
        for other_notice in [false, true] {
            let mut app = context_inspector_test_app(false);
            let mut health = SaveHealthState::new("/session.sqlite3".into());
            health.apply(health_event(
                1,
                SessionSaveHealth::Recovered {
                    recovered_at_unix_ms: 1,
                },
                Some(1),
            ));
            app.save_health = Some(health);
            app.status = other_notice.then(|| TransientNotice::error("another failure"));
            let _ = app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices));
            assert!(app.chrome_ui.is_open());
            let _ = app.update(Message::AcknowledgeSessionRecovery);
            assert_eq!(app.chrome_ui.is_open(), other_notice);
            if !other_notice {
                assert!(!app.workspace_inputs_blocked());
            }
        }
    }

    #[test]
    fn save_health_reducer_preserves_recovery_and_rejects_stale_completion() {
        let mut state = SaveHealthState::new(PathBuf::from("/exact/session.sqlite3"));
        assert!(!state.is_visible());
        assert!(state.apply(health_event(1, SessionSaveHealth::Healthy, Some(10))));
        assert_eq!(state.last_successful_flush_at_unix_ms, Some(10));

        assert!(state.apply(health_event(
            2,
            SessionSaveHealth::Retrying {
                category: crate::session::SaveFailureCategory::Busy,
                first_failed_at_unix_ms: 20,
                attempts: 1,
            },
            Some(10),
        )));
        assert!(state.is_visible());
        assert!(state.apply(health_event(
            3,
            SessionSaveHealth::RetryExhausted {
                category: crate::session::SaveFailureCategory::Busy,
                first_failed_at_unix_ms: 20,
                attempts: 4,
            },
            Some(10),
        )));

        assert!(!state.apply(health_event(
            2,
            SessionSaveHealth::Recovered {
                recovered_at_unix_ms: 25,
            },
            Some(25),
        )));
        assert!(matches!(
            state.health,
            SessionSaveHealth::RetryExhausted { .. }
        ));

        assert!(state.apply(health_event(
            4,
            SessionSaveHealth::Recovered {
                recovered_at_unix_ms: 30,
            },
            Some(30),
        )));
        assert!(state.apply(health_event(5, SessionSaveHealth::Healthy, Some(40))));
        assert!(matches!(state.health, SessionSaveHealth::Recovered { .. }));
        assert_eq!(state.last_successful_flush_at_unix_ms, Some(40));
        assert!(state.acknowledge_recovery());
        assert!(!state.is_visible());

        assert!(!state.apply(health_event(
            4,
            SessionSaveHealth::Retrying {
                category: crate::session::SaveFailureCategory::Database,
                first_failed_at_unix_ms: 30,
                attempts: 9,
            },
            None,
        )));
        assert!(state.apply(health_event(6, SessionSaveHealth::WriterUnavailable, None,)));
        assert_eq!(state.last_successful_flush_at_unix_ms, Some(40));
        assert!(state.is_visible());
    }

    #[test]
    fn terminal_handoff_completion_retires_only_exact_successes_and_reports_only_failures() {
        let mut app = context_inspector_test_app(false);
        let terminal = || {
            let CardKind::Terminal(terminal) = terminal_card_state().kind else {
                unreachable!()
            };
            terminal
        };
        let accepted = terminal();
        let refused = terminal();
        let superseded_success = terminal();
        let superseded_failure = terminal();
        for terminal in [
            &accepted,
            &refused,
            &superseded_success,
            &superseded_failure,
        ] {
            app.pending_terminal_handoffs
                .insert(terminal.id, terminal.clone());
        }
        for terminal in [&superseded_success, &superseded_failure] {
            app.pending_terminal_handoffs
                .get_mut(&terminal.id)
                .unwrap()
                .resume_hint = Some("new descriptor".into());
        }
        app.terminal_handoff_commit_inflight = true;
        let _ = app.update(Message::TerminalHandoffsCommitted {
            result: Ok(vec![
                (accepted.clone(), Ok(())),
                (refused.clone(), Err("receipt refused".into())),
                (superseded_success.clone(), Ok(())),
                (superseded_failure.clone(), Err("obsolete failure".into())),
            ]),
        });
        assert!(!app.terminal_handoff_commit_inflight);
        assert!(!app.pending_terminal_handoffs.contains_key(&accepted.id));
        assert_eq!(app.pending_terminal_handoffs.len(), 3);
        let notices = app
            .notices
            .entries
            .iter()
            .map(|entry| entry.notice.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("1 terminal(s) remain pending")
                    && notice.contains(&refused.id.to_string()))
        );
        assert!(
            notices
                .iter()
                .all(|notice| !notice.contains("obsolete failure"))
        );

        // A successful retry removes only the previously failed descriptor;
        // newer receipts that arrived during the batch still need a save.
        let _ = app.update(Message::TerminalHandoffsCommitted {
            result: Ok(vec![(refused.clone(), Ok(()))]),
        });
        assert!(!app.pending_terminal_handoffs.contains_key(&refused.id));
        assert_eq!(app.pending_terminal_handoffs.len(), 2);
    }

    #[test]
    fn terminal_handoff_flush_failure_keeps_all_receipts_pending() {
        let mut app = context_inspector_test_app(false);
        let CardKind::Terminal(terminal) = terminal_card_state().kind else {
            unreachable!()
        };
        app.pending_terminal_handoffs
            .insert(terminal.id, terminal.clone());
        app.terminal_handoff_commit_inflight = true;
        let _ = app.update(Message::TerminalHandoffsCommitted {
            result: Err("session flush: unavailable".into()),
        });
        assert!(!app.terminal_handoff_commit_inflight);
        assert_eq!(
            app.pending_terminal_handoffs.get(&terminal.id),
            Some(&terminal)
        );
    }

    #[test]
    fn refused_writes_keep_save_health_visible_beside_a_healthy_lane() {
        let refused = |subject| QuarantinedWrite {
            subject,
            category: crate::session::SaveFailureCategory::InvalidData,
            refused_at_unix_ms: 20,
            held_back: 0,
        };
        let mut state = SaveHealthState::new(PathBuf::from("/exact/session.sqlite3"));
        let mut event = health_event(1, SessionSaveHealth::Healthy, Some(10));
        event.quarantined = vec![refused(WriteSubject::Project {
            id: ProjectId::new(),
            name: "poisoned".into(),
        })];
        assert!(state.apply(event));
        assert!(
            state.is_visible(),
            "a healthy lane still reports what it refused"
        );
        assert!(state.offers_retry());
        assert!(
            !state.window_state_refused(),
            "a refused catalog row leaves terminal handoffs free to commit"
        );
        assert_eq!(save_health_views(&state, |_| None).len(), 1);

        let mut event = health_event(
            2,
            SessionSaveHealth::RetryExhausted {
                category: crate::session::SaveFailureCategory::Busy,
                first_failed_at_unix_ms: 30,
                attempts: 4,
            },
            Some(10),
        );
        event.quarantined = vec![refused(WriteSubject::Workspace { card: None })];
        assert!(state.apply(event));
        assert!(state.window_state_refused());
        assert_eq!(
            save_health_views(&state, |_| None).len(),
            2,
            "the lane's failure and the refusal are separate notices"
        );

        state.retry_requested = true;
        assert!(!state.offers_retry(), "one retry at a time");
        assert!(state.apply(health_event(3, SessionSaveHealth::Healthy, Some(40))));
        assert!(
            !state.is_visible(),
            "a superseded refusal leaves nothing to show"
        );
        assert!(!state.offers_retry());

        let mut event = health_event(4, SessionSaveHealth::WriterUnavailable, Some(40));
        event.quarantined = vec![refused(WriteSubject::Workspace { card: None })];
        assert!(state.apply(event));
        assert!(
            !state.offers_retry(),
            "a stopped writer cannot answer a retry"
        );
    }

    #[test]
    fn a_refused_card_is_named_with_its_remedy() {
        let card = CardId::new();
        let write = |subject| QuarantinedWrite {
            subject,
            category: crate::session::SaveFailureCategory::InvalidData,
            refused_at_unix_ms: 20,
            held_back: 0,
        };
        let titled = |id| (id == card).then_some("build");
        let untitled = |_| None;

        let snapshot = write(WriteSubject::Workspace { card: Some(card) });
        let line = quarantined_write_line(&snapshot, &titled);
        assert!(
            line.starts_with("Window layout and cards (card “build” was refused)"),
            "{line}"
        );
        assert!(line.contains("closing the card clears it"), "{line}");
        let line = quarantined_write_line(&snapshot, &untitled);
        assert!(line.contains("(one card was refused)"), "{line}");

        let relaunch = write(WriteSubject::CardIdentity {
            card,
            kind: ReplacedIdentity::Terminal,
        });
        let line = quarantined_write_line(&relaunch, &titled);
        assert!(line.starts_with("Relaunched terminal “build”"), "{line}");
        assert!(line.contains("closing the card clears it"), "{line}");

        let line = quarantined_write_line(&write(WriteSubject::Workspace { card: None }), &titled);
        assert!(line.starts_with("Window layout and cards —"), "{line}");
        assert!(!line.contains("closing the card"), "{line}");
    }

    fn launch_recipe(working_directory: impl Into<PathBuf>) -> LaunchRecipe {
        LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: working_directory.into(),
            environment: Default::default(),
        }
    }

    #[test]
    fn browser_chrome_uses_one_sanitized_location_for_visible_and_accessible_text() {
        let kind = browser_kind(
            "https://alice:password@EXAMPLE.test:443/private%20path?oauth=secret#token",
        );
        let caption = card_kind_caption(&kind);
        assert_eq!(caption, "https://example.test/private%20path");

        let chrome = CardChrome::new(
            CardId::new(),
            "Browser",
            "Example",
            &caption,
            &KeyboardShortcuts::default(),
        );
        assert_eq!(chrome.caption, caption);
        assert_eq!(chrome.resize_name, "Resize Browser column Example");
        assert_eq!(
            chrome.accessible_name(true),
            "Browser column Example, https://example.test/private%20path, focused"
        );
        for presented in [chrome.caption.as_str(), chrome.accessible_name(true)] {
            assert!(!presented.contains("alice"));
            assert!(!presented.contains("password"));
            assert!(!presented.contains("oauth"));
            assert!(!presented.contains("secret"));
            assert!(!presented.contains("token"));
        }
    }

    #[test]
    fn system_opener_failure_redaction_contract() {
        let raw =
            "https://alice:password@example.test:8443/private%20path?oauth=secret#access-token";
        let url = Url::parse(raw).unwrap();
        // Execute a stable interpreter, not a newly written executable whose
        // write descriptor a parallel test's fork may still hold (ETXTBSY).
        // The URL stays a separate argument; the failing opener receives its
        // authoritative contents even though presentation must redact them.
        let mut opener = tokio::process::Command::new("/bin/sh");
        opener
            .args([
                "-c",
                "[ \"$1\" = \"$EXPECTED_URI\" ] || exit 43; printf '%s\\n' \"$1\" >&2; exit 42",
                "test-opener",
            ])
            .env("EXPECTED_URI", raw);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(open_system_url_with_opener(&url, opener))
            .unwrap_err();

        assert!(
            error.contains("https://example.test:8443/private%20path"),
            "{error}"
        );
        assert!(error.contains("system opener exited"), "{error}");
        assert!(error.contains("42"), "{error}");
        for sensitive in [
            "alice",
            "password",
            "oauth",
            "secret",
            "access-token",
            "?",
            "#",
        ] {
            assert!(!error.contains(sensitive), "leaked {sensitive:?}: {error}");
        }
    }

    #[cfg(feature = "integrated-browser")]
    #[test]
    fn unchanged_sanitized_address_submits_the_authoritative_location() {
        let url =
            Url::parse("https://alice:password@example.test:8443/path?api_key=hunter2#fragment")
                .unwrap();
        let mut address = BrowserAddress::location(&url);
        assert_eq!(address.as_str(), "https://example.test:8443/path");
        assert_eq!(address.submit(), Ok(url.clone()));
        assert_eq!(address.as_str(), "https://example.test:8443/path");
    }

    #[cfg(feature = "integrated-browser")]
    #[test]
    fn submitted_user_input_keeps_navigation_authority_but_redacts_presentation() {
        let url =
            Url::parse("https://alice:password@example.test:8443/path?api_key=hunter2#fragment")
                .unwrap();
        let mut typed = BrowserAddress::user_input(url.as_str().to_owned());
        assert_eq!(typed.as_str(), url.as_str());
        assert_eq!(typed.submit(), Ok(url));
        assert_eq!(typed.as_str(), "https://example.test:8443/path");
        for sensitive in ["alice", "password", "api_key", "hunter2", "fragment"] {
            assert!(!typed.as_str().contains(sensitive));
        }

        let mut malformed = BrowserAddress::user_input(
            "https://alice:password@example .test/path?token=secret".into(),
        );
        assert_eq!(malformed.submit(), Err(()));
        assert_eq!(malformed.as_str(), SanitizedLocation::INVALID);
    }
    #[test]
    fn application_close_sequence_visits_every_unresolved_repository_card() {
        let origin = CardId::new();
        let first = CardId::new();
        let second = CardId::new();
        let third = CardId::new();
        let window = window::Id::unique();
        let mut close = DirtyRepositoryClose {
            card: first,
            remaining: VecDeque::from([second, third]),
            intent: DirtyCloseIntent::Application(window),
            focused: DirtyCloseControl::Save,
            restore_card: Some(origin),
        };
        assert_eq!(close.next_unresolved(|card| card == second), Some(second));
        assert_eq!(close.restore_card, Some(origin));
        assert_eq!(close.next_unresolved(|card| card == third), Some(third));
        assert_eq!(close.restore_card, Some(origin));
        assert_eq!(close.next_unresolved(|_| true), None);
        assert_eq!(close.intent, DirtyCloseIntent::Application(window));
        let replacement = CardId::new();
        assert_eq!(
            resolve_restore_card(Some(origin), Some(replacement), |card| card == origin),
            Some(origin)
        );
        assert_eq!(
            resolve_restore_card(Some(origin), Some(replacement), |_| false),
            Some(replacement)
        );
    }

    /// The window is the only part of an exit anyone sees, so it closes as
    /// soon as the exit is committed; the language-server reap, the final
    /// session flush and the process teardown all happen after it has gone.
    #[test]
    fn committed_exit_closes_the_window_before_the_process_exits() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));

        let _ = app.update(Message::WindowCloseRequested(window));

        let closing = Some(ApplicationExit {
            closing: Some(window),
            servers_reaped: false,
        });
        assert_eq!(app.exit, closing);
        // Released, so libcosmic's report of the closed window cannot end the
        // application under the reap the exit is still waiting on, and the
        // frames iced may draw before it removes the window must not panic.
        assert_eq!(app.core.main_window_id(), None);
        let _ = app.view_window(window);

        // That report arrives as another close request. It must not start the
        // exit again or raise a dialog in a window that no longer exists.
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        let _ = app.update(Message::WindowCloseRequested(window));
        assert!(app.dirty_repository_close.is_none());
        assert_eq!(app.exit, closing);
    }

    /// Once the window is gone, an activation must be refused rather than
    /// acknowledged, so `cagents ui` opens a new window instead of reporting
    /// one that is already running.
    #[test]
    fn committed_exit_stops_accepting_activations() {
        let mut app = context_inspector_test_app(false);
        let (activations, receiver) = async_channel::unbounded();
        app.activation_rx = receiver;
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        assert!(!activations.is_closed());

        let _ = app.update(Message::WindowCloseRequested(window));

        assert!(activations.is_closed());
        assert!(
            activations
                .send_blocking(instance::UiActivation {
                    token: None,
                    update: None,
                })
                .is_err()
        );
    }

    /// Neither half of the exit may end the process alone: servers reaped
    /// before the window is destroyed would tear iced down with the window
    /// still on screen, and a destroyed window before the reap would take the
    /// runtime away from servers still stopping.
    #[test]
    fn exit_waits_for_both_the_window_and_the_language_servers() {
        for window_first in [true, false] {
            let mut app = context_inspector_test_app(false);
            let window = window::Id::unique();
            app.core.set_main_window_id(Some(window));
            let _ = app.update(Message::WindowCloseRequested(window));

            let (first, second) = if window_first {
                (
                    Message::WindowClosed(window),
                    Message::LanguageServersReaped,
                )
            } else {
                (
                    Message::LanguageServersReaped,
                    Message::WindowClosed(window),
                )
            };
            let _ = app.update(first);
            assert!(!app.exit.unwrap().is_complete());
            let _ = app.update(second);
            assert!(app.exit.unwrap().is_complete());
        }
    }

    /// Only the window the exit closed can finish it.
    #[test]
    fn another_window_closing_does_not_finish_the_exit() {
        let mut app = context_inspector_test_app(false);
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        let _ = app.update(Message::WindowCloseRequested(window));

        let _ = app.update(Message::LanguageServersReaped);
        let _ = app.update(Message::WindowClosed(window::Id::unique()));

        assert!(!app.exit.unwrap().is_complete());
    }

    /// iced reports every window it removes; outside an exit that is not half
    /// of one.
    #[test]
    fn window_closed_outside_an_exit_is_ignored() {
        let mut app = context_inspector_test_app(false);
        let _ = app.update(Message::WindowClosed(window::Id::unique()));
        let _ = app.update(Message::LanguageServersReaped);
        assert_eq!(app.exit, None);
    }

    #[test]
    fn unresolved_repository_keeps_the_window_open_on_close() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        app.core.set_main_window_id(Some(window));
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );

        let _ = app.update(Message::WindowCloseRequested(window));

        assert!(app.dirty_repository_close.is_some());
        assert_eq!(app.exit, None);
        assert_eq!(app.core.main_window_id(), Some(window));
    }

    #[test]
    fn window_close_rejects_discard_for_post_persist_edit_back_fence() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        let runtime = RepositoryRuntime::post_persist_warning_fixture(card);
        assert!(runtime.is_dirty(), "the buffer has edited back from B to A");
        assert!(runtime.has_durability_warning());
        assert!(!runtime.can_abandon_unsaved_changes());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));

        let _ = app.update(Message::WindowCloseRequested(window));
        let _ = app.update(Message::DiscardAndCloseRepository(card));

        assert!(app.dirty_repository_close.as_ref().is_some_and(
            |close| close.card == card && close.intent == DirtyCloseIntent::Application(window)
        ));
        assert!(app.workspace.order().any(|candidate| candidate == card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Repository(runtime))
                if runtime.blocks_navigation()
                    && runtime.has_durability_warning()
                    && !runtime.can_abandon_unsaved_changes()
        ));
    }

    #[test]
    fn card_close_rejects_discard_for_post_persist_edit_back_fence() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let runtime = RepositoryRuntime::post_persist_warning_fixture(card);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));

        let _ = app.update(Message::CloseCard(card));
        let _ = app.update(Message::DiscardAndCloseRepository(card));

        assert!(app.dirty_repository_close.as_ref().is_some_and(
            |close| close.card == card && close.intent == DirtyCloseIntent::Card(card)
        ));
        assert!(app.workspace.order().any(|candidate| candidate == card));
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Repository(runtime))
                if runtime.blocks_navigation() && runtime.has_durability_warning()
        ));
    }

    /// A headless `cosmic::Renderer`. With no graphics device it falls back
    /// to tiny-skia, which needs neither a device nor a surface.
    pub(super) fn headless_renderer() -> cosmic::Renderer {
        use cosmic::iced::advanced::renderer::Headless;
        use cosmic::iced::{Font, Pixels};

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("executor")
            .block_on(cosmic::Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .expect("a headless renderer")
    }

    #[test]
    fn real_tab_header_drag_survives_repaint_and_cannot_activate_destination_close() {
        use cosmic::iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation::Focusable, tree::NAMED},
        };
        use cosmic::iced::{Event, Point, Rectangle, Size, mouse};
        struct Bounds(HashMap<String, Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn Focusable,
            ) {
                if let Some(id) = id {
                    self.0.insert(id.to_string(), bounds);
                }
            }
        }
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let first = app.workspace.focused().unwrap();
        let mut second = app.card_state(first).unwrap().clone();
        second.id = CardId::new();
        second.title = "Second".into();
        let second_id = second.id;
        Arc::make_mut(&mut app.durable).cards.push(second);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1600.0);
        app.cards.insert(second_id, CardRuntime::Loading);
        assert!(!app.workspace_inputs_blocked());
        let renderer = headless_renderer();
        let mut tree = Tree::empty();
        let mut origin = Point::ORIGIN;
        let mut destination = Point::ORIGIN;
        let mut dropped = false;
        for phase in 0..3 {
            let mut element = frame(&app);
            NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
            );
            NAMED.with(|named| named.borrow_mut().clear());
            if phase == 0 {
                let mut bounds = Bounds(HashMap::new());
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut bounds,
                );
                origin = bounds.0[&app.card_header_widget_id(first).to_string()].center();
                destination = bounds.0[&card_control_id(second_id, "close").to_string()].center();
            }
            let point = if phase == 0 { origin } else { destination };
            let event = Event::Mouse(match phase {
                0 => mouse::Event::ButtonPressed(mouse::Button::Left),
                1 => mouse::Event::CursorMoved { position: point },
                _ => mouse::Event::ButtonReleased(mouse::Button::Left),
            });
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(1600.0, 1200.0)),
            );
            drop(element);
            for message in messages {
                let cosmic::Action::App(message) = message else {
                    continue;
                };
                assert!(
                    !matches!(message, Message::CloseCard(_)),
                    "a title drag must not reach the destination close button"
                );
                dropped |= matches!(message, Message::Viewport { event: WorkspaceViewportEvent::Reordered { card, before: None }, .. } if card == first);
                drop(app.update(message));
            }
        }
        assert!(
            dropped,
            "actual title/tooltip/header composition must intercept the drag"
        );
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![second_id, first]
        );
    }

    /// The custom `widget::Id`s that name more than one node in `element`.
    ///
    /// A custom id may name at most one node per frame: the pinned runtime
    /// lifts every named node's state into one thread-local map keyed by name
    /// before it diffs, gives it to the first widget that asks for that name,
    /// and leaves any later claimant holding the `State::None` the lift left
    /// behind. Nothing downstream can recover from that - the tag still
    /// matches, so the node is never rebuilt, and the first draw downcasts a
    /// stateless state and aborts the process.
    ///
    /// Shared rather than written per surface, because every surface that
    /// mounts ids from a list is exposed to the same invariant and a per-test
    /// copy of the walk would drift from the runtime it is modelling.
    fn widget_id_counts(element: &mut Element<'_, Message>) -> HashMap<String, usize> {
        use cosmic::iced::advanced::widget::Tree;
        use iced_accessibility::id::Internal;

        fn tally(tree: &Tree, out: &mut HashMap<String, usize>) {
            if let Some(widget::Id(Internal::Custom(_, name))) = &tree.id {
                *out.entry(name.to_string()).or_default() += 1;
            }
            for child in &tree.children {
                tally(child, out);
            }
        }

        let mut counts = HashMap::new();
        let mut tree = Tree::new(element.as_widget_mut());
        // Responsive constructs its descendants during layout. Counting only
        // Tree::new silently misses those controls and duplicate named states.
        let renderer = headless_renderer();
        let _layout = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &cosmic::iced::advanced::layout::Limits::new(
                cosmic::iced::Size::ZERO,
                cosmic::iced::Size::new(1600.0, 1200.0),
            ),
        );
        tally(&tree, &mut counts);
        counts
    }

    fn duplicate_widget_ids(element: &mut Element<'_, Message>) -> Vec<String> {
        let mut repeated = widget_id_counts(element)
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        repeated.sort();
        repeated
    }

    /// Lays out and draws `frames` frames of `app` the way the pinned runtime
    /// does - lifting every named node's state, diffing, laying out, drawing -
    /// asserting before each that no custom `widget::Id` names two nodes, and
    /// applying `between(app, frame)` once the frame is drawn so the next one
    /// sees a transition. The abort a duplicate id causes lives in `draw`, so
    /// inspecting the view alone proves nothing; this is the one place every
    /// surface that can mount two subtrees at once is driven through it.
    fn draw_frames(app: &mut Cagents, frames: usize, mut between: impl FnMut(&mut Cagents, usize)) {
        use cosmic::iced::advanced::widget::{Tree, tree};
        use cosmic::iced::advanced::{Layout, layout, mouse, renderer};
        use cosmic::iced::{Rectangle, Size};

        let renderer = headless_renderer();
        let bounds = Size::new(1600.0, 1000.0);
        let mut state = Tree::empty();
        for frame in 0..frames {
            {
                let repeated = duplicate_widget_ids(&mut app.view());
                assert!(
                    repeated.is_empty(),
                    "frame {frame} mounts {repeated:?} more than once",
                );
            }

            let mut element = app.view();
            tree::NAMED.with(|named| {
                *named.borrow_mut() = state.take_all_named();
            });
            state.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                &mut state,
                &renderer,
                &layout::Limits::new(Size::ZERO, bounds),
            );
            tree::NAMED.with(|named| named.borrow_mut().clear());
            element.as_widget().draw(
                &state,
                &mut headless_renderer(),
                &cosmic::theme::active(),
                &renderer::Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &Rectangle::with_size(bounds),
            );
            drop(node);
            drop(element);

            between(app, frame);
        }
    }

    /// Collapsing the contextual inspector must not mount two nodes under one
    /// custom `widget::Id`, and must survive the frames that do it.
    ///
    /// The panel's own header and the workspace chrome's header offer the same
    /// notice control and the same inspector toggle, and each carries the id
    /// the application focuses it by. The pinned runtime lifts every named
    /// node's state into one thread-local map keyed by name before it diffs,
    /// gives it to the first widget that asks for that name, and leaves any
    /// later claimant holding the `State::None` the lift left behind. Nothing
    /// downstream can recover from that: the tag still matches, so the node is
    /// never rebuilt, and the first draw downcasts a stateless state and
    /// aborts the process. Asserting the absence of that abort is the whole
    /// contract here, so this test lays out and draws every frame of the
    /// collapse rather than only inspecting the view.
    #[test]
    fn collapsing_the_inspector_leaves_every_widget_id_naming_one_node() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        assert!(
            app.context_inspector_open,
            "the inspector starts open, which is what makes the collapse reachable"
        );
        // Open with a file selected, collapse, then let the collapse settle:
        // the middle frame is the one where the panel is still mounted while
        // the chrome header already believes the inspector is closed.
        draw_frames(&mut app, 4, |app, frame| match frame {
            0 => {
                let _ = app.update(Message::ContextInspector(
                    InspectorAction::ShowRepositoryEditor(card),
                ));
            }
            1 => {
                let _ = app.update(Message::ToggleContextInspector);
                assert!(
                    !app.context_inspector_open && app.context_inspector_visible(),
                    "the collapse leaves the panel mounted while the chrome reopens its slots"
                );
            }
            _ => app.context_inspector_transition = None,
        });
    }

    /// The inspector's Files and Changes tabs mount the same file and source
    /// control panels the rail mounts for the worktree it is bound to, with
    /// the same `sidebar.*` ids on their controls. The rail and the inspector
    /// are on screen together the whole time, so every tab switch is a frame
    /// in which two subtrees could claim one id. Drawn rather than reasoned
    /// about, because the abort is in `draw` and points at the wrong widget.
    #[test]
    fn inspector_tabs_leave_every_widget_id_naming_one_node() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        assert!(app.context_inspector_open);
        assert!(
            sidebar::bound_source_control(&app.sidebar_model, &app.sidebar_state).is_some(),
            "the fixture binds a worktree, or the panels draw their empty chrome and prove nothing"
        );
        let tabs = [
            ContextTab::Files,
            ContextTab::Changes,
            ContextTab::PullRequest,
            ContextTab::Changes,
            ContextTab::Files,
        ];
        draw_frames(&mut app, tabs.len(), |app, frame| {
            let _ = app.update(Message::SelectContextTab(tabs[frame]));
        });
    }

    /// Channel rows carry a custom `widget::Id` keyed on the channel, and they
    /// now appear, disappear and change rank from a daemon edge rather than
    /// from a local click. Every such frame must still leave each id naming one
    /// node, or the pinned runtime aborts the process on the next draw.
    ///
    /// Two ways this could go wrong are worth drawing rather than reasoning
    /// about: a row arriving while another leaves in the same frame, and a
    /// reorder that moves a row past one already mounted. Both are laid out and
    /// drawn here, because the failure is a panic inside `draw` that inspecting
    /// the view cannot see.
    #[test]
    fn shared_channel_edges_leave_every_channel_id_naming_one_node() {
        let mut app = context_inspector_test_app(true);
        let project = crate::model::ProjectId::new();
        let repository = std::path::PathBuf::from("/repo/.git");
        app.sidebar_model
            .projects
            .push(sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId(uuid::Uuid::new_v4()),
                "repo".into(),
                repository.clone(),
                Vec::new(),
                Vec::new(),
            ));
        app.sidebar_state.reveal_project(project);
        let older = crate::model::ChannelId::new();
        let newer = crate::model::ChannelId::new();
        let identity = uuid::Uuid::new_v4();
        let shared = |id, title: &str, ordinal| crate::workspace_model::ModelChannel {
            id,
            repository: repository.clone(),
            title: title.to_owned(),
            ordinal,
        };

        draw_frames(&mut app, 4, |app, frame| {
            let model = |channels| crate::workspace_model::WorkspaceModel {
                identity,
                sequence: frame as u64 + 1,
                projects: Vec::new(),
                channels,
                closed_projects: Vec::new(),
                agents: Vec::new(),
            };
            match frame {
                // One adopted row.
                0 => {
                    let _ = app.apply_workspace_model_update(
                        workspace_model_sync::ModelUpdate::Connected(model(vec![shared(
                            older, "older", 1,
                        )])),
                    );
                }
                // A second row arrives above the first, so the mounted row is
                // pushed down past a newly mounted one.
                1 => {
                    let _ =
                        app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Read(
                            model(vec![shared(older, "older", 1), shared(newer, "newer", 2)]),
                        ));
                }
                // One leaves while the other is reranked past where it was, in
                // a single update.
                _ => {
                    let _ =
                        app.apply_workspace_model_update(workspace_model_sync::ModelUpdate::Read(
                            model(vec![shared(newer, "newer", 9)]),
                        ));
                }
            }
            app.refresh_sidebar_channels();
        });

        // The rows really were mounted, or the walk above proved nothing.
        assert_eq!(
            app.sidebar_model.projects[0]
                .channels
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![newer],
            "the departed channel is gone and the survivor kept its identity"
        );
    }

    pub(super) fn settle_settings_task(app: &mut Cagents, task: Task<Message>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                    while let Some(action) = stream.next().await {
                        if let iced::runtime::Action::Output(cosmic::Action::App(
                            message @ Message::SettingsSaved { .. },
                        )) = action
                        {
                            let _ = app.update(message);
                        }
                    }
                }
            });
    }

    #[test]
    fn out_of_order_settings_saves_and_acknowledgments_preserve_latest_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        app.settings.terminal_font_size = 18.0;
        let older = app.capture_settings_save().unwrap();
        app.settings.terminal_font_size = 20.0;
        let newer = app.capture_settings_save().unwrap();
        let newer_ack = newer.run().unwrap();
        let older_ack = older.run().unwrap();
        app.acknowledge_settings_save(newer_ack);
        app.settings.terminal_font_size = 22.0;
        app.acknowledge_settings_save(older_ack);
        assert_eq!(settings::load(&path).unwrap().terminal_font_size, 20.0);
        assert_eq!(app.settings.terminal_font_size, 22.0);
        app.save_settings_now().unwrap();
        assert_eq!(settings::load(&path).unwrap().terminal_font_size, 22.0);
    }

    #[test]
    fn reverting_an_edit_before_save_acknowledgment_persists_the_revert() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        let original = app.settings.terminal_font_size;
        app.settings.terminal_font_size = 18.0;
        let first = app.capture_settings_save().unwrap();
        app.settings.terminal_font_size = original;
        let second = app.capture_settings_save().unwrap();
        let first_ack = first.run().unwrap();
        let second_ack = second.run().unwrap();
        app.acknowledge_settings_save(second_ack);
        app.acknowledge_settings_save(first_ack);
        assert_eq!(settings::load(&path).unwrap().terminal_font_size, original);
    }

    #[test]
    fn settings_autosave_rejects_stale_debounce_and_flushes_each_valid_field() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        let field = SettingsField::TerminalFontFamily;
        let _ = app.queue_setting_input(field, "Fira".into());
        let stale = app.settings_generation;
        let _ = app.queue_setting_input(field, "Fira Code".into());
        let latest = app.settings_generation;
        let _ = app.autosave_setting(field, stale);
        assert!(!path.exists());
        assert!(app.settings.terminal_font_family.is_empty());
        let task = app.autosave_setting(field, latest);
        settle_settings_task(&mut app, task);
        assert_eq!(
            settings::load(&path).unwrap().terminal_font_family,
            "Fira Code"
        );
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let pane = app.utility_card(&CardKind::Settings).unwrap();
        let _ =
            app.queue_setting_input(SettingsField::ManagedWorktreeDirectory, "../trees/".into());
        // Closing the pane commits the draft still waiting out its debounce.
        let _ = app.update(Message::CloseCard(pane));
        let task = app.update(Message::CommitClose(pane));
        assert!(app.settings_pending.is_empty());
        settle_settings_task(&mut app, task);
        assert_eq!(
            settings::load(&path).unwrap().managed_worktree_directory,
            "../trees/"
        );
    }

    #[test]
    fn invalid_autosaved_draft_survives_dismissal_and_other_valid_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        app.save_settings_now().unwrap();
        let before = std::fs::read(&path).unwrap();
        let field = SettingsField::ShortcutNewTerminal;
        let _ = app.queue_setting_input(field, "Ctrl+".into());
        let _ = app.autosave_setting(field, app.settings_generation);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let task = app.apply_setting_input(SettingsField::TerminalFontSize, "18".into());
        settle_settings_task(&mut app, task);
        assert!(matches!(
            app.settings_feedback,
            Some(SettingsFeedback::Invalid)
        ));
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let pane = app.utility_card(&CardKind::Settings).unwrap();
        let _ = app.update(Message::CloseCard(pane));
        let _ = app.update(Message::CommitClose(pane));
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_ne!(app.utility_card(&CardKind::Settings), Some(pane));
        assert_eq!(app.settings_draft.keyboard_shortcuts.new_terminal, "Ctrl+");
        assert_eq!(
            settings::load(&path).unwrap().keyboard_shortcuts,
            KeyboardShortcuts::default()
        );
        assert_eq!(settings::load(&path).unwrap().terminal_font_size, 18.0);
    }

    fn open_settings_panes(app: &Cagents) -> Vec<CardId> {
        app.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && card.kind == CardKind::Settings)
            .map(|card| card.id)
            .collect()
    }

    #[test]
    fn settings_open_as_one_live_pane_that_reopening_focuses() {
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let terminal = app.durable.cards[0].id;
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let [pane] = open_settings_panes(&app)[..] else {
            panic!("opening settings adds exactly one pane");
        };
        assert_eq!(app.workspace.focused(), Some(pane));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), [terminal, pane]);
        assert!(matches!(app.cards.get(&pane), Some(CardRuntime::Settings)));
        let state = app.card_state(pane).unwrap();
        assert_eq!(state.title, "Settings");
        assert_eq!(state.residency, Residency::Hot);
        assert!(
            app.lifecycle.state(pane).is_none(),
            "a settings pane never enters the hibernation budget"
        );
        // A pane, not a dialog: the terminal beside it keeps its keystrokes.
        assert!(!app.modal_is_open());
        assert!(!app.workspace_inputs_blocked());
        assert_eq!(
            app.card_keyboard_focus(pane).to_string(),
            crate::settings_widgets::search_id().to_string()
        );

        let _ = app.update(Message::FocusCard(terminal));
        assert_eq!(app.workspace.focused(), Some(terminal));
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_eq!(open_settings_panes(&app), [pane]);
        assert_eq!(app.workspace.focused(), Some(pane));

        // Neither a clone nor a second request makes another copy.
        let _ = app.clone_focused_pane();
        assert_eq!(open_settings_panes(&app), [pane]);
        assert_eq!(app.workspace.order().count(), 2);
    }

    #[test]
    fn leaving_the_settings_pane_commits_its_pending_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        let terminal = app.durable.cards[0].id;
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let _ = app.update(Message::SettingsDraftChanged {
            field: SettingsField::TerminalFontFamily,
            value: "Fira Code".into(),
        });
        assert!(!app.settings_pending.is_empty());
        // Unrelated work while the pane keeps focus leaves the draft pending.
        let _ = app.update(Message::SelectSettingsCategory(SettingsCategory::Terminal));
        assert!(!app.settings_pending.is_empty());

        let task = app.update(Message::FocusCard(terminal));
        assert!(app.settings_pending.is_empty());
        assert_eq!(app.settings.terminal_font_family, "Fira Code");
        settle_settings_task(&mut app, task);
        assert_eq!(
            settings::load(&path).unwrap().terminal_font_family,
            "Fira Code"
        );
    }

    #[test]
    fn dropping_the_window_saves_settings_drafts_still_debouncing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;
        let _ = app.queue_setting_input(SettingsField::TerminalFontFamily, "Iosevka".into());
        drop(app);
        assert_eq!(
            settings::load(&path).unwrap().terminal_font_family,
            "Iosevka"
        );
    }

    #[test]
    fn a_dialog_over_the_settings_pane_withholds_edits_behind_it() {
        let mut app = context_inspector_test_app(false);
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        app.settings_reset_confirm = true;
        let before = app.settings_draft.clone();
        let _ = app.update(Message::SettingsDraftChanged {
            field: SettingsField::TerminalFontFamily,
            value: "typed at the dialog".into(),
        });
        let _ = app.update(Message::SettingsSearchChanged("typed".into()));
        assert_eq!(app.settings_draft, before);
        assert!(app.settings_search.is_empty());
        assert!(app.settings_pending.is_empty());
    }

    #[test]
    fn asking_for_settings_while_its_pane_closes_brings_it_back() {
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        let closing = app.utility_card(&CardKind::Settings).unwrap();
        let _ = app.request_card_removal(closing);
        let _ = app.update(Message::Sidebar(SidebarAction::OpenSettings));
        assert_eq!(open_settings_panes(&app), [closing], "still mid-close");
        let _ = app.update(Message::CommitClose(closing));
        let [reopened] = open_settings_panes(&app)[..] else {
            panic!("the request survives the close");
        };
        assert_ne!(reopened, closing);
        assert_eq!(app.workspace.focused(), Some(reopened));
        assert!(app.reopen_utility.is_none());
    }

    #[test]
    fn a_restored_snapshot_keeps_one_settings_pane() {
        let panes: Vec<_> = (0..2)
            .map(|_| {
                let mut pane = terminal_card_state();
                pane.kind = CardKind::Settings;
                pane
            })
            .collect();
        let (first, second) = (panes[0].id, panes[1].id);
        let mut state = WorkspaceState {
            focused: Some(second),
            cards: panes,
            ..WorkspaceState::default()
        };
        retire_duplicate_utility_cards(&mut state);
        assert!(state.cards[0].existence.is_open());
        assert!(!state.cards[1].existence.is_open());
        assert_eq!(state.focused, Some(first));
    }

    #[test]
    fn restored_settings_pane_is_live_without_a_lifecycle_slot() {
        let mut app = context_inspector_test_app(false);
        let mut pane = terminal_card_state();
        pane.kind = CardKind::Settings;
        pane.title = "Settings".into();
        pane.residency = Residency::Hibernated;
        let id = pane.id;
        app.durable = Arc::new(WorkspaceState {
            focused: Some(id),
            cards: vec![pane],
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        let _ = app.initialize_runtimes();
        assert!(matches!(app.cards.get(&id), Some(CardRuntime::Settings)));
        assert_eq!(app.card_state(id).unwrap().residency, Residency::Hot);
        assert!(app.lifecycle.state(id).is_none());
        // Retrying a pane that cannot fail only reinstalls it.
        app.cards.insert(id, CardRuntime::Failed("fixture".into()));
        let _ = app.update(Message::RetryCard(id));
        assert!(matches!(app.cards.get(&id), Some(CardRuntime::Settings)));
        assert!(app.lifecycle.state(id).is_none());
    }

    #[test]
    fn corrupt_settings_protection_retains_save_error_and_reset_cancels_pending_drafts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        std::fs::write(&path, "not valid toml").unwrap();
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(path.clone());
        let task = app.apply_setting_input(SettingsField::TerminalFontSize, "18".into());
        settle_settings_task(&mut app, task);
        assert!(matches!(
            app.settings_feedback,
            Some(SettingsFeedback::Error(_))
        ));
        let _ = app.queue_setting_input(SettingsField::TerminalFontFamily, "Unfinished".into());
        let stale = app.settings_generation;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not valid toml");
        assert!(matches!(
            app.settings_feedback,
            Some(SettingsFeedback::Error(_))
        ));
        let task = app.reset_settings();
        settle_settings_task(&mut app, task);
        let _ = app.autosave_setting(SettingsField::TerminalFontFamily, stale);
        assert_eq!(settings::load(&path).unwrap(), AppSettings::default());
    }

    #[test]
    fn resetting_settings_sends_restored_notification_preferences_to_the_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(directory.path().join("settings.toml"));
        app.settings_write_blocked = false;
        app.settings.notifications.enabled = false;
        app.settings.notifications.while_focused = true;
        assert_eq!(app.notification_ui.pending_preferences(), None);

        let task = app.reset_settings();
        settle_settings_task(&mut app, task);

        // The daemon, not the pane, decides which alerts arrive; a reset that
        // only redraws the toggles leaves it applying the old policy.
        assert_eq!(
            app.notification_ui.pending_preferences(),
            Some(crate::notifications::Preferences::default())
        );
    }

    #[test]
    fn resetting_unchanged_notification_preferences_sends_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = context_inspector_test_app(false);
        app.settings_path = Some(directory.path().join("settings.toml"));
        app.settings_write_blocked = false;
        app.settings.terminal_font_size = 20.0;

        let task = app.reset_settings();
        settle_settings_task(&mut app, task);

        assert_eq!(app.notification_ui.pending_preferences(), None);
        assert_eq!(app.settings, AppSettings::default());
    }

    #[test]
    fn resetting_settings_restores_autosave_for_an_already_open_editor() {
        async fn settle_repository_tasks(app: &mut Cagents, task: Task<Message>) {
            let mut pending = VecDeque::from([task]);
            while let Some(task) = pending.pop_front() {
                let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                    continue;
                };
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ (Message::Repository(_) | Message::SettingsSaved { .. }),
                    )) = action
                    {
                        pending.push_back(app.update(message));
                    }
                }
            }
        }

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("notes.txt");
                std::fs::write(&path, "").unwrap();
                let mut app = context_inspector_test_app(false);
                app.settings_path = Some(directory.path().join("settings.toml"));
                app.settings_write_blocked = false;
                let card = CardId::new();
                app.cards.insert(
                    card,
                    CardRuntime::Repository(Box::new(RepositoryRuntime::file_list_fixture(
                        RepositoryView {
                            selected_file: None,
                            project: ProjectId::new(),
                            repository: directory.path().to_path_buf(),
                            worktree: directory.path().to_path_buf(),
                        },
                        &["notes.txt"],
                    ))),
                );
                let task = app.update(Message::Repository(RepositoryAction::OpenFile {
                    card,
                    path: PathBuf::from("notes.txt"),
                }));
                settle_repository_tasks(&mut app, task).await;

                let task = app.apply_setting_input(SettingsField::EditorAutoSave, "false".into());
                settle_repository_tasks(&mut app, task).await;
                let task = app.update(Message::Repository(RepositoryAction::EditorAction {
                    card,
                    action: cosmic::widget::text_editor::Action::Edit(
                        cosmic::widget::text_editor::Edit::Insert('a'),
                    ),
                }));
                settle_repository_tasks(&mut app, task).await;
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "");

                // Reset schedules the dirty buffer without requiring another edit.
                let task = app.reset_settings();
                settle_repository_tasks(&mut app, task).await;
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
                assert!(
                    settings::load(app.settings_path.as_ref().unwrap())
                        .unwrap()
                        .editor_auto_save
                );

                // The same open editor must keep saving subsequent changes.
                let task = app.update(Message::Repository(RepositoryAction::EditorAction {
                    card,
                    action: cosmic::widget::text_editor::Action::Edit(
                        cosmic::widget::text_editor::Edit::Insert('b'),
                    ),
                }));
                settle_repository_tasks(&mut app, task).await;
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "ab");
            });
    }

    #[test]
    fn modal_backdrop_dismisses_without_leaking_clicks_or_dismissing_surface_padding() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};
        let renderer = headless_renderer();
        for (point, enabled, dismisses) in [
            (Point::new(4.0, 4.0), true, true),
            (Point::new(400.0, 300.0), true, false),
            (Point::new(4.0, 4.0), false, false),
        ] {
            let underlying: Element<'_, Message> = widget::mouse_area(
                widget::container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .on_press(Message::NewTerminal)
            .into();
            let modal = dialog_ui::overlay(
                dialog_ui::surface(iced::widget::Space::new().height(120), 320.0),
                enabled.then_some(Message::CancelDialog),
                1.0,
            );
            let mut element: Element<'_, Message> = crate::interaction::layer(underlying, modal);
            let mut tree = Tree::new(element.as_widget());
            let viewport = Rectangle::with_size(Size::new(800.0, 600.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            drop(shell);
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| matches!(message, Message::CancelDialog))
                    .count(),
                usize::from(dismisses)
            );
            assert!(
                !messages
                    .iter()
                    .any(|message| matches!(message, Message::NewTerminal))
            );
        }
    }

    /// The log is what keeps a report from being overwritten by the next one,
    /// and capacity is the only thing bounding it: the shell raises notices
    /// from a per-poll path, so a repeating failure must not evict the history.
    #[test]
    fn the_notice_log_collapses_repeats_and_bounds_itself() {
        let mut log = NoticeLog::default();
        for message in ["first failure", "second failure"] {
            log.record(TransientNotice::error(message.to_owned()));
        }
        // An identical message collapses onto the entry it repeats rather than
        // adding a second one, and it does so wherever that entry already sits:
        // a poll raising two failures in turn alternates them, which is exactly
        // the case a tail-only comparison got wrong.
        for _ in 0..5 {
            log.record(TransientNotice::error("first failure"));
            log.record(TransientNotice::error("second failure"));
        }
        assert_eq!(log.len(), 2);
        let mut recent = log.recent();
        // The collapse moves the repeat to the front, so the newest entry is
        // whichever message was raised last.
        let newest = recent.next().unwrap();
        assert_eq!(newest.notice.message, "second failure");
        assert_eq!(newest.repeats, 6);
        assert_eq!(recent.next().unwrap().repeats, 6);
        drop(recent);
        assert_eq!(log.severity(), Some(NoticeSeverity::Error));

        // A dismissed entry names exactly one; a stale id is a no-op.
        let newest = log.recent().next().unwrap().id;
        log.dismiss(newest);
        log.dismiss(newest);
        assert_eq!(log.len(), 1);

        // Distinct messages past capacity drop the oldest, never grow.
        for index in 0..NOTICE_LOG_CAPACITY * 2 {
            log.record(TransientNotice::warning(format!("failure {index}")));
        }
        assert_eq!(log.len(), NOTICE_LOG_CAPACITY);
        // The surviving error was evicted, so the header must stop claiming one.
        assert_eq!(log.severity(), Some(NoticeSeverity::Warning));

        log.clear();
        assert!(log.is_empty());
        assert_eq!(log.severity(), None);
    }

    #[test]
    fn modal_controls_remain_available_to_assistive_technology() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let renderer = headless_renderer();
        let mut element = dialog_ui::overlay(
            dialog_ui::surface(
                widget::button::text("Confirm").on_press(Message::CancelDialog),
                320.0,
            ),
            Some(Message::CancelDialog),
            1.0,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(800.0, 600.0)),
        );
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some("Confirm")
                })
        );
    }

    /// The frame the runtime actually builds: libcosmic composes the header,
    /// the application's own view and any dialog into a single element, and
    /// reconciles all of it against one tree.
    ///
    /// Driving `Application::view` alone would miss the dialog entirely, and
    /// driving `Application::dialog` alone would miss that a dialog *joins and
    /// leaves* the popover that holds it - a membership change in the widest
    /// list there is. libcosmic itself flags the hazard where it composes them
    /// ("we have to use a popover even without a dialog to keep the tree from
    /// changing"), and only stops the popover node from coming and going: the
    /// popup beneath it still does.
    fn frame(app: &Cagents) -> cosmic::Element<'_, cosmic::Action<Message>> {
        cosmic::ApplicationExt::view_main(app)
    }

    /// Presents one frame against a carried-over widget tree, exactly the way
    /// `iced_runtime::UserInterface::build` does: the previous frame's named
    /// widget states are lifted into the thread-local map that reconciliation
    /// restores them from, then the tree is reconciled, laid out and drawn.
    ///
    /// Reproduced here rather than driven through `UserInterface` because that
    /// type keeps its reconciled tree private, and the tree is what these
    /// tests are about.
    fn present(
        app: &Cagents,
        renderer: &mut cosmic::Renderer,
        tree: &mut cosmic::iced::advanced::widget::Tree,
    ) {
        use cosmic::iced::advanced::widget::tree::NAMED;
        use cosmic::iced::advanced::{Layout, layout, mouse, renderer::Style};
        use cosmic::iced::{Rectangle, Size};

        let bounds = Size::new(1600.0, 1200.0);
        let mut element = frame(app);
        NAMED.with(|named| {
            *named.borrow_mut() = tree.take_all_named();
        });
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        NAMED.with(|named| named.borrow_mut().clear());
        element.as_widget().draw(
            tree,
            renderer,
            &cosmic::theme::active(),
            &Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &Rectangle::with_size(bounds),
        );
    }

    /// The name a node was given, if it was given one.
    fn node_name(tree: &cosmic::iced::advanced::widget::Tree) -> Option<String> {
        let rendered = format!("{:?}", tree.id.as_ref()?);
        // `Id`'s debug carries a per-process counter that differs between two
        // builds of the same frame; the name does not.
        rendered
            .split_once("Custom(")
            .and_then(|(_, rest)| rest.split_once('"'))
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(name, _)| name.to_owned())
    }

    /// A reconciled tree must describe the frame it was reconciled against.
    ///
    /// Naming a widget - `widget::Id::new(some_string)` - opts it into
    /// libcosmic's named-state mechanism: every build lifts such a node's
    /// state out of the tree into a thread-local map and puts it back only for
    /// the names the new frame still asks for. Surviving children are then
    /// claimed by name *in place* and never repositioned, so a list whose
    /// membership changes leaves the departed nodes stranded at their old
    /// indices with their state gone. Whichever widget now occupies such a
    /// slot is then laid out and drawn against a stranger's state - and a
    /// button drawn against emptied state aborts the process from inside
    /// `draw`, where nothing can catch it.
    ///
    /// Comparing against a tree built fresh for the same frame is the whole
    /// invariant. Both halves of it matter, and each catches a failure the
    /// other cannot see: the names must match, because a stranded node is a
    /// name in the wrong place; and the shape must match, because a list that
    /// lost an *unnamed* widget keeps its state at the wrong index without
    /// renaming anything, and every container zips its children against their
    /// trees - so the tail of that row is silently not laid out, not drawn and
    /// not clickable for the frame.
    fn assert_tree_is_faithful(
        reconciled: &cosmic::iced::advanced::widget::Tree,
        fresh: &cosmic::iced::advanced::widget::Tree,
    ) {
        fn walk(
            reconciled: &cosmic::iced::advanced::widget::Tree,
            fresh: &cosmic::iced::advanced::widget::Tree,
            path: &mut Vec<usize>,
        ) {
            assert_eq!(
                node_name(reconciled),
                node_name(fresh),
                "at {path:?}: the reconciled tree names a different widget than the frame it was reconciled against"
            );
            assert_eq!(
                reconciled.children.len(),
                fresh.children.len(),
                "at {path:?} ({:?}): the reconciled tree holds a different number of children than the frame it was reconciled against, so its parent zips widgets against strangers' state",
                node_name(fresh)
            );
            for (index, (reconciled, fresh)) in reconciled
                .children
                .iter()
                .zip(fresh.children.iter())
                .enumerate()
            {
                path.push(index);
                walk(reconciled, fresh, path);
                path.pop();
            }
        }

        walk(reconciled, fresh, &mut Vec::new());
    }

    fn assert_frame_is_faithful(app: &Cagents, tree: &cosmic::iced::advanced::widget::Tree) {
        // Responsive widgets construct their children during layout. Compare
        // two presented frames so their complete widget trees are available.
        let mut fresh = cosmic::iced::advanced::widget::Tree::empty();
        present(app, &mut headless_renderer(), &mut fresh);
        assert_tree_is_faithful(tree, &fresh);
    }

    /// A repository card whose durable state and runtime both say repository,
    /// with an unsaved buffer and a language server reporting on it - the
    /// state a user is in while editing a file.
    fn editing_repository_app() -> (Cagents, CardId) {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().expect("a focused card");
        {
            let durable = Arc::make_mut(&mut app.durable);
            let state = durable
                .cards
                .iter_mut()
                .find(|state| state.id == card)
                .expect("the focused card is durable");
            state.kind = CardKind::Repository(RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: PathBuf::from("/repo/.git"),
                worktree: PathBuf::from("/repo"),
            });
        }
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
        let state = lsp_state(&mut app, card);
        state.document = Some(LspDocumentId(1));
        state.context.status = Some(crate::lsp::LspStatus::Ready);
        state.context.diagnostics = vec![diagnostic("first"), diagnostic("second")];
        state.diagnostics_version = Some(0);
        app.lsp_documents.insert(LspDocumentId(1), card);
        (app, card)
    }

    #[test]
    fn reordered_panes_keep_runtime_focus_and_restore_the_saved_order() {
        let (mut app, editor) = editing_repository_app();
        let mut order = vec![editor];
        for _ in 0..2 {
            let card = terminal_card_state();
            order.push(card.id);
            app.workspace
                .add_card(
                    WorkspaceCard::new(
                        card.id,
                        &card.title,
                        CardWidth::Fraction(card.width_fraction),
                    )
                    .unwrap(),
                    order.len() - 1,
                    false,
                )
                .unwrap();
            app.cards.insert(card.id, CardRuntime::Loading);
            Arc::make_mut(&mut app.durable).cards.push(card);
        }
        let runtime = match &app.cards[&editor] {
            CardRuntime::Repository(runtime) => &**runtime as *const RepositoryRuntime,
            _ => unreachable!(),
        };
        let event = WorkspaceViewportEvent::Reordered {
            card: editor,
            before: None,
        };
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation + 1,
            event,
        }));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event,
        }));
        let expected = vec![order[1], order[2], editor];
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
        assert_eq!(app.workspace.focused(), Some(editor));
        let CardRuntime::Repository(current) = &app.cards[&editor] else {
            panic!("editor retained")
        };
        assert_eq!(&**current as *const RepositoryRuntime, runtime);
        assert!(current.is_dirty());
        let saved: WorkspaceState =
            serde_json::from_slice(&serde_json::to_vec(&*app.persistent_workspace()).unwrap())
                .unwrap();
        let restored = Cagents::rebuild_workspace(&saved);
        assert_eq!(restored.order().collect::<Vec<_>>(), expected);
        assert_eq!(restored.focused(), Some(editor));
        drop(app.reorder_pane(editor, Some(CardId::new())));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn repository_tab_chrome_tracks_the_selected_file_and_restores_the_pane_name() {
        let (mut app, card) = editing_repository_app();
        app.card_state_mut(card).unwrap().title = "My editor".into();
        let (runtime, _) = RepositoryRuntime::open_fixture(card, "src/main.rs", "fn main() {}\n");
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let _ = app.sync_chrome();

        let chrome = app.card_chrome.get(&card).unwrap();
        assert_eq!(chrome.title, "main.rs");
        assert_eq!(chrome.caption, "/repo/src/main.rs");
        assert!(chrome.accessible_name(true).contains("main.rs"));
        assert!(chrome.close_name.contains("main.rs"));
        assert_eq!(app.card_state(card).unwrap().title, "My editor");

        let _ = app.update(Message::Repository(RepositoryAction::OpenFile {
            card,
            path: PathBuf::from("other.txt"),
        }));
        let chrome = app.card_chrome.get(&card).unwrap();
        assert_eq!(chrome.title, "other.txt");
        assert_eq!(chrome.caption, "/repo/other.txt");
        assert!(chrome.accessible_name(false).contains("other.txt"));
        assert!(chrome.close_name.contains("other.txt"));

        let _ = app.update(Message::Repository(RepositoryAction::BackToFiles { card }));
        let chrome = app.card_chrome.get(&card).unwrap();
        assert_eq!(chrome.title, "My editor");
        assert_eq!(
            chrome.caption,
            card_kind_caption(&app.card_state(card).unwrap().kind)
        );
        assert!(chrome.close_name.contains("My editor"));
        assert_eq!(app.card_state(card).unwrap().title, "My editor");
    }

    /// Discarding retires the Discard and Save commands and offers Back to
    /// files in their place. Before this was fixed that repaint aborted the
    /// process: the contextual inspector's commands were named, so the two
    /// departing buttons stranded their emptied nodes for the surviving
    /// buttons to draw against.
    #[test]
    fn discarding_an_edit_repaints_the_window() {
        use cosmic::iced::advanced::widget::Tree;

        let (mut app, card) = editing_repository_app();
        let mut renderer = headless_renderer();
        let mut tree = Tree::empty();

        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        let _ = app.update(Message::Repository(RepositoryAction::DiscardEdits { card }));
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);
    }

    /// The same reconciliation, over the transitions that move a repository
    /// card between its surfaces. Each one changes which commands the
    /// inspector offers, which is the condition that strands a named node.
    #[test]
    fn moving_between_repository_surfaces_repaints_the_window() {
        use cosmic::iced::advanced::widget::Tree;

        let (mut app, card) = editing_repository_app();
        let mut renderer = headless_renderer();
        let mut tree = Tree::empty();
        present(&app, &mut renderer, &mut tree);

        for action in [
            RepositoryAction::ApplySourceSurface { card, diff: true },
            RepositoryAction::ApplySourceSurface { card, diff: false },
            RepositoryAction::BackToFiles { card },
            RepositoryAction::DiscardEdits { card },
            RepositoryAction::RefreshFiles { card },
        ] {
            let _ = app.update(Message::Repository(action));
            present(&app, &mut renderer, &mut tree);
            assert_frame_is_faithful(&app, &tree);
        }
    }

    /// The dirty-close dialog's controls are named too, and the row they sit
    /// in changes membership: Discard leaves while a save is in flight and
    /// comes back when the save fails, which is exactly the transition a user
    /// walks through by answering "Save and close" to a file the disk then
    /// refuses.
    ///
    /// The dialog itself joins and leaves the popover libcosmic composes the
    /// frame from, so opening one is a membership change in the outermost list
    /// there is - which is why this starts from a frame with no dialog on it.
    #[test]
    fn a_failed_save_repaints_the_dirty_close_dialog() {
        use cosmic::iced::advanced::widget::Tree;

        let (mut app, card) = editing_repository_app();
        let mut renderer = headless_renderer();
        let mut tree = Tree::empty();

        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        let _ = app.update(Message::CloseCard(card));
        assert!(
            app.dirty_repository_close.is_some(),
            "closing a card with an unsaved buffer asks before destroying it"
        );
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        // The save starts: Discard leaves, because there is nothing coherent
        // to abandon while a write is in flight.
        let _ = app.update(Message::SaveAndCloseRepository(card));
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        // The save fails: the buffer is dirty again and Discard comes back,
        // in the middle of the row rather than at its end.
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            panic!("the card is a repository");
        };
        let failed = runtime.failed_active_save_completion();
        let _ = app.update(Message::Repository(failed));
        assert!(
            app.dirty_repository_close.is_some(),
            "a failed save leaves the dialog up"
        );
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);
    }

    #[test]
    fn card_close_still_discards_ordinary_editor_dirtiness() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let runtime = RepositoryRuntime::dirty_fixture(card);
        assert!(runtime.can_abandon_unsaved_changes());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));

        let _ = app.update(Message::CloseCard(card));
        let close = app.update(Message::DiscardAndCloseRepository(card));

        assert!(app.dirty_repository_close.is_none());
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
        ));
        assert!(matches!(
            task_app_message(close),
            Message::CloseCard(candidate) if candidate == card
        ));
    }

    #[test]
    fn application_close_advances_only_after_current_durability_fence_resolves() {
        let mut app = context_inspector_test_app(true);
        let first = app.workspace.focused().unwrap();
        let second_state = terminal_card_state();
        let second = second_state.id;
        Arc::make_mut(&mut app.durable)
            .cards
            .push(second_state.clone());
        app.workspace
            .add_card(
                WorkspaceCard::new(
                    second,
                    second_state.title,
                    CardWidth::Fraction(second_state.width_fraction),
                )
                .unwrap(),
                1,
                false,
            )
            .unwrap();
        app.cards.insert(
            first,
            CardRuntime::Repository(Box::new(RepositoryRuntime::post_persist_warning_fixture(
                first,
            ))),
        );
        app.cards.insert(
            second,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(second))),
        );
        let window = window::Id::unique();

        let _ = app.update(Message::WindowCloseRequested(window));
        let _ = app.update(Message::DiscardAndCloseRepository(first));
        assert!(app.dirty_repository_close.as_ref().is_some_and(
            |close| close.card == first && close.remaining == VecDeque::from([second])
        ));

        let _ = app.update(Message::SaveAndCloseRepository(first));
        let CardRuntime::Repository(runtime) = app.cards.get(&first).unwrap() else {
            panic!("first repository remains installed during close resolution");
        };
        let completion = runtime.durable_active_save_completion();
        let _ = app.update(Message::Repository(completion));

        assert!(
            app.dirty_repository_close
                .as_ref()
                .is_some_and(|close| close.card == second
                    && close.remaining.is_empty()
                    && close.intent == DirtyCloseIntent::Application(window))
        );
        assert!(matches!(
            app.cards.get(&first),
            Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
        ));
        assert!(matches!(
            app.cards.get(&second),
            Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()
        ));
    }

    #[test]
    fn card_close_stays_fenced_after_post_persist_failure_until_retry_succeeds() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let (runtime, failure) = RepositoryRuntime::post_persist_failure_fixture(card);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            remaining: VecDeque::new(),
            intent: DirtyCloseIntent::Card(card),
            focused: DirtyCloseControl::Save,
            restore_card: Some(card),
        });

        let _ = app.update(Message::Repository(failure));

        assert!(app.dirty_repository_close.is_some());
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        assert!(runtime.blocks_navigation());
        assert!(
            runtime
                .save_error()
                .is_some_and(|warning| warning.contains("replaced on disk"))
        );

        let _ = app.update(Message::SaveAndCloseRepository(card));
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let retry_failure = runtime.failed_active_save_completion();
        let _ = app.update(Message::Repository(retry_failure));

        assert!(app.dirty_repository_close.is_some());
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let message = runtime.save_error().expect("close dialog shows the error");
        assert!(message.contains("retry failure before persist"));
        assert!(message.contains("replaced on disk"));
        assert!(runtime.blocks_navigation());
        let context = runtime.inspector_context();
        assert!(context.can_reload);
        assert!(context.can_save);

        let _ = app.update(Message::SaveAndCloseRepository(card));
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let completion = runtime.durable_active_save_completion();
        let _close = app.update(Message::Repository(completion));

        assert!(app.dirty_repository_close.is_none());
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain until close is committed");
        };
        assert!(!runtime.blocks_navigation());
        assert!(runtime.save_error().is_none());
    }

    #[test]
    fn window_close_stays_fenced_after_post_persist_failure_until_retry_succeeds() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let window = window::Id::unique();
        let (runtime, failure) = RepositoryRuntime::post_persist_failure_fixture(card);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            remaining: VecDeque::new(),
            intent: DirtyCloseIntent::Application(window),
            focused: DirtyCloseControl::Save,
            restore_card: Some(card),
        });

        let _ = app.update(Message::Repository(failure));

        assert!(app.dirty_repository_close.is_some());
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()
        ));

        let _ = app.update(Message::SaveAndCloseRepository(card));
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let retry_failure = runtime.failed_active_save_completion();
        let _ = app.update(Message::Repository(retry_failure));

        assert!(app.dirty_repository_close.is_some());
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let message = runtime.save_error().expect("close dialog shows the error");
        assert!(message.contains("retry failure before persist"));
        assert!(message.contains("replaced on disk"));
        assert!(runtime.blocks_navigation());
        let context = runtime.inspector_context();
        assert!(context.can_reload);
        assert!(context.can_save);

        let _ = app.update(Message::SaveAndCloseRepository(card));
        let CardRuntime::Repository(runtime) = app.cards.get(&card).unwrap() else {
            panic!("repository runtime should remain installed");
        };
        let completion = runtime.durable_active_save_completion();
        let _exit = app.update(Message::Repository(completion));

        assert!(app.dirty_repository_close.is_none());
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Repository(runtime)) if !runtime.blocks_navigation()
                && runtime.save_error().is_none()
        ));
    }

    #[test]
    fn active_save_fences_direct_adjacent_pan_and_wheel_navigation() {
        let focused = CardId::new();
        assert!(focused_save_in_flight(Some(focused), |card| card == focused));
        assert!(!focused_save_in_flight(None, |_| true));
        assert!(!focused_save_in_flight(Some(focused), |_| false));

        let messages = [
            Message::NewTerminal,
            Message::NewBrowser,
            Message::OpenLink(
                focused,
                Url::parse("https://example.com").unwrap(),
                crate::settings::LinkOpenGesture::ControlShiftClick,
            ),
            Message::ChatUi(chat_ui::Action::OpenUrl("https://example.com".into())),
            Message::PullRequestUi(pull_request_ui::Action::OpenUrl(
                Url::parse("https://example.com").unwrap(),
            )),
            Message::PullRequestUi(pull_request_ui::Action::OpenMarkdownLink(
                "https://example.com".into(),
            )),
            Message::ContextInspector(InspectorAction::OpenLspHoverLink(
                "https://example.com".into(),
            )),
            Message::FocusCard(CardId::new()),
            Message::FocusAdjacent(crate::workspace::FocusDirection::Right),
            Message::ToggleContextInspector,
            Message::Repository(RepositoryAction::NewTerminal { card: focused }),
        ];
        assert!(
            messages
                .iter()
                .all(workspace_navigation_blocked_during_save)
        );

        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let key = crate::github::PullRequestKey::new(repository, 45).unwrap();
        assert!(completion_navigates(&Message::PullRequestCreated {
            dialog: pull_request_test_dialog(),
            generation: 0,
            result: Ok(PullRequestCreationOutcome::Created {
                key,
                url: Url::parse("https://github.com/owner/repo/pull/45").unwrap(),
                pull_request: None,
                normalization_warning: None,
            }),
        }));
        assert!(!completion_navigates(&Message::ExternalLinkOpened(Ok(()))));
    }

    fn pull_request_test_dialog() -> PullRequestDialog {
        PullRequestDialog {
            project_id: ProjectId::new(),
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            worktree: None,
            base: "release".into(),
            head: "topic/authored-change".into(),
            title: "Keep the authored title".into(),
            body: "A carefully authored description.\n\nWith details for reviewers.".into(),
            draft: true,
            retry_confirmation: None,
        }
    }

    fn authorize_pull_request_test(app: &mut Cagents, dialog: &PullRequestDialog) {
        app.github_availability.insert(
            dialog.project_id,
            GitHubAvailability::Ready {
                repository_key: dialog.repository.clone(),
                pull_requests: Vec::new(),
                refreshed_at: 0,
            },
        );
    }

    fn pull_request_test_failure(exit_code: i32) -> crate::repository::ProcessFailure {
        crate::repository::ProcessFailure {
            kind: crate::repository::ProcessFailureKind::Exit,
            program: "gh".into(),
            exit_code: Some(exit_code),
            signal: None,
            stdout: Vec::new(),
            stderr: b"pull request create failed".to_vec(),
            message: "pull request create failed".into(),
            started: true,
        }
    }

    #[test]
    fn pull_request_local_rejection_preserves_authored_draft() {
        let mut app = context_inspector_test_app(false);
        let mut dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        dialog.title.clear();
        app.pull_request_dialog = Some(dialog.clone());

        let _ = app.update(Message::SubmitPullRequest);

        assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
    }

    #[test]
    fn pull_request_remote_failure_restores_hidden_draft_for_retry() {
        for result in [
            Ok(PullRequestCreationOutcome::NotCreated {
                failure: pull_request_test_failure(4),
            }),
            Err("pull request creator failed".into()),
        ] {
            let mut app = context_inspector_test_app(false);
            let dialog = pull_request_test_dialog();
            authorize_pull_request_test(&mut app, &dialog);
            app.pull_request_dialog = Some(dialog.clone());

            // Dropping the task avoids invoking gh; the update transition itself
            // must remove the draft until its asynchronous completion arrives.
            let _ = app.update(Message::SubmitPullRequest);
            assert!(app.pull_request_dialog.is_none());
            let _ = app.update(Message::PullRequestCreated {
                dialog: dialog.clone(),
                generation: app.pull_request_dialog_generation,
                result,
            });

            assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
            let _ = app.update(Message::SubmitPullRequest);
            assert!(app.pull_request_dialog.is_none(), "retry should submit");
        }
    }

    #[test]
    fn pull_request_unknown_outcome_keeps_edits_but_requires_reconciliation() {
        let mut app = context_inspector_test_app(false);
        let dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        app.pull_request_dialog = Some(dialog.clone());
        let _ = app.update(Message::SubmitPullRequest);

        let _ = app.update(Message::PullRequestCreated {
            dialog: dialog.clone(),
            generation: app.pull_request_dialog_generation,
            result: Ok(PullRequestCreationOutcome::OutcomeUnknown {
                failure: pull_request_test_failure(1),
            }),
        });
        let mut expected = dialog;
        expected.retry_confirmation = Some(false);
        assert_eq!(app.pull_request_dialog.as_ref(), Some(&expected));

        let revised_body = "Revised description after checking the remote".to_owned();
        let _ = app.update(Message::PullRequestBodyChanged(revised_body.clone()));
        let _ = app.update(Message::SubmitPullRequest);
        expected.body = revised_body;
        assert_eq!(app.pull_request_dialog.as_ref(), Some(&expected));

        let _ = app.update(Message::PullRequestRetryConfirmed(true));
        let _ = app.update(Message::SubmitPullRequest);
        assert!(app.pull_request_dialog.is_none());
    }

    #[test]
    fn pull_request_success_does_not_reopen_submitted_draft() {
        let mut app = context_inspector_test_app(false);
        let dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        app.pull_request_dialog = Some(dialog.clone());
        let _ = app.update(Message::SubmitPullRequest);
        let key = crate::github::PullRequestKey::new(dialog.repository.clone(), 45).unwrap();

        let _ = app.update(Message::PullRequestCreated {
            dialog,
            generation: app.pull_request_dialog_generation,
            result: Ok(PullRequestCreationOutcome::Created {
                key,
                url: Url::parse("https://github.com/owner/repo/pull/45").unwrap(),
                pull_request: None,
                normalization_warning: None,
            }),
        });

        assert!(app.pull_request_dialog.is_none());
    }

    #[test]
    fn pull_request_stale_failure_does_not_replace_a_newer_draft() {
        let mut app = context_inspector_test_app(false);
        let dialog = pull_request_test_dialog();
        authorize_pull_request_test(&mut app, &dialog);
        app.pull_request_dialog = Some(dialog.clone());
        let _ = app.update(Message::SubmitPullRequest);
        let generation = app.pull_request_dialog_generation;
        let project_id = ProjectId::new();
        app.repository_generations.insert(project_id, 0);
        app.github_availability.insert(
            project_id,
            GitHubAvailability::Ready {
                repository_key: dialog.repository.clone(),
                pull_requests: Vec::new(),
                refreshed_at: 0,
            },
        );
        let _ = app.update(Message::Sidebar(SidebarAction::CreatePullRequest {
            project_id,
            worktree: None,
        }));
        let _ = app.update(Message::PullRequestTitleChanged("Newer PR".into()));
        let _ = app.update(Message::PullRequestHeadChanged("new-topic".into()));
        let _ = app.update(Message::PullRequestBaseChanged("trunk".into()));
        let _ = app.update(Message::PullRequestBodyChanged("Newer description".into()));
        let newer = app.pull_request_dialog.clone().unwrap();
        let failure = Message::PullRequestCreated {
            dialog,
            generation,
            result: Ok(PullRequestCreationOutcome::NotCreated {
                failure: pull_request_test_failure(4),
            }),
        };

        let _ = app.update(failure.clone());
        assert_eq!(app.pull_request_dialog.as_ref(), Some(&newer));

        let _ = app.update(Message::SubmitPullRequest);
        assert!(app.pull_request_dialog.is_none());
        let _ = app.update(failure.clone());
        assert!(
            app.pull_request_dialog.is_none(),
            "newer request is in flight"
        );

        let _ = app.update(Message::CancelDialog);
        let _ = app.update(failure);
        assert!(
            app.pull_request_dialog.is_none(),
            "dismissed drafts stay closed"
        );
    }

    #[test]
    fn pull_request_recovery_survives_cancelling_an_unrelated_worktree_dialog() {
        for complete_before_cancel in [false, true] {
            let mut app = context_inspector_test_app(false);
            let dialog = pull_request_test_dialog();
            authorize_pull_request_test(&mut app, &dialog);
            app.pull_request_dialog = Some(dialog.clone());
            let _ = app.update(Message::SubmitPullRequest);
            let completion = Message::PullRequestCreated {
                dialog: dialog.clone(),
                generation: app.pull_request_dialog_generation,
                result: Ok(PullRequestCreationOutcome::NotCreated {
                    failure: pull_request_test_failure(4),
                }),
            };
            let _ = app.update(Message::Sidebar(SidebarAction::NewWorktree {
                project_id: ProjectId::new(),
                repository: PathBuf::from("/repo"),
            }));
            assert!(app.worktree_dialog.is_some());
            if complete_before_cancel {
                let _ = app.update(completion.clone());
            }
            let _ = app.update(Message::CancelDialog);
            if !complete_before_cancel {
                let _ = app.update(completion);
            }
            assert!(app.worktree_dialog.is_none());
            assert_eq!(app.pull_request_dialog.as_ref(), Some(&dialog));
            let _ = app.update(Message::CancelDialog);
            assert!(app.pull_request_dialog.is_none());
        }
    }

    #[test]
    fn dirty_dialog_focus_is_stable_trapped_and_save_aware() {
        let mut focus = DirtyCloseControl::Cancel;
        focus = next_dirty_close_control(focus, false, false, true);
        assert_eq!(focus, DirtyCloseControl::Discard);
        focus = next_dirty_close_control(focus, false, false, true);
        assert_eq!(focus, DirtyCloseControl::Save);
        focus = next_dirty_close_control(focus, false, false, true);
        assert_eq!(focus, DirtyCloseControl::Cancel);
        assert_eq!(
            next_dirty_close_control(DirtyCloseControl::Save, true, false, true),
            DirtyCloseControl::Discard
        );
        assert_eq!(
            next_dirty_close_control(DirtyCloseControl::Save, false, true, true),
            DirtyCloseControl::Cancel
        );
        assert_eq!(
            next_dirty_close_control(DirtyCloseControl::Cancel, false, false, false),
            DirtyCloseControl::Save
        );
        assert_eq!(
            next_dirty_close_control(DirtyCloseControl::Cancel, true, false, false),
            DirtyCloseControl::Save
        );
        assert_eq!(
            next_dirty_close_control(DirtyCloseControl::Save, false, false, false),
            DirtyCloseControl::Cancel
        );
    }

    #[test]
    fn modal_rejects_editor_input_but_accepts_async_repository_completions() {
        let card = CardId::new();
        let editor = RepositoryAction::EditorAction {
            card,
            action: cosmic::widget::text_editor::Action::Edit(
                cosmic::widget::text_editor::Edit::Paste(Arc::new("blocked".into())),
            ),
        };
        assert!(!repository_action_allowed_while_modal(&editor));
        let identity = repository_widget::AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("file.txt"),
            generation: 1,
        };
        assert!(repository_action_allowed_while_modal(
            &RepositoryAction::FileLoaded {
                identity,
                result: Err(crate::editor::FileIoError::Io("fixture".into())),
            }
        ));
        // A dropped page render is not a lost frame: the viewer reads
        // "Rendering page" until the result lands, so refusing it while an
        // inspector or a dialog is open would strand the document viewer on a
        // page it never draws.
        assert!(repository_action_allowed_while_modal(
            &RepositoryAction::PdfPageRendered {
                card,
                request: 1,
                result: Err("fixture".into()),
            }
        ));
        for message in [
            Message::NewTerminal,
            Message::NewBrowser,
            Message::FocusCard(card),
            Message::ResizeCard { card, width: 640.0 },
            Message::ResizeCardFinished(card),
            Message::FocusAdjacent(crate::workspace::FocusDirection::Right),
            Message::Repository(RepositoryAction::NewTerminal { card }),
            Message::ToggleContextInspector,
        ] {
            assert!(user_workspace_message_blocked_while_modal(&message));
        }
        assert!(!user_workspace_message_blocked_while_modal(
            &Message::Repository(RepositoryAction::FileLoaded {
                identity: repository_widget::AsyncIdentity {
                    card,
                    worktree: PathBuf::from("/repo"),
                    path: PathBuf::from("file.txt"),
                    generation: 1,
                },
                result: Err(crate::editor::FileIoError::Io("fixture".into())),
            })
        ));
    }

    #[test]
    fn a_new_camera_animation_does_not_consume_the_previous_idle_interval() {
        let mut app = context_inspector_test_app(false);
        let cards = vec![
            terminal_card_state(),
            terminal_card_state(),
            terminal_card_state(),
        ];
        let target = cards[2].id;
        app.durable = Arc::new(WorkspaceState {
            focused: Some(cards[0].id),
            cards,
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(600.0);
        app.workspace.native_scroll(0.0, 600.0);
        let previous_frame = Instant::now();
        let _ = app.frame_clock.elapsed(previous_frame);

        let _ = app.update(Message::FocusCard(target));
        let first_frame = previous_frame + Duration::from_secs(2);
        let _ = app.update(Message::Frame(first_frame));
        assert_eq!(app.workspace.camera().offset(), 0.0);
        let _ = app.update(Message::Frame(first_frame + Duration::from_millis(16)));
        assert!(app.workspace.camera().offset() > 0.0);
        assert!(app.workspace.camera().offset() < app.workspace.camera().target() / 2.0);
    }

    #[test]
    fn switching_inactive_codex_terminal_reveals_and_focuses_its_card() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let mut app = context_inspector_test_app(false);
        app.context_inspector_open = false;
        let mut cards: Vec<_> = (0..4).map(|_| terminal_card_state()).collect();
        let first = cards[0].id;
        let unrelated = cards[1].id;
        let target = cards[3].id;
        let CardKind::Terminal(terminal) = &mut cards[3].kind else {
            unreachable!();
        };
        terminal.launch.program = "codex".into();
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("codex-host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let source = terminal.clone();
        app.durable = Arc::new(WorkspaceState {
            focused: Some(first),
            cards,
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(600.0);
        app.workspace.native_scroll(0.0, 600.0);
        app.observe_agent_update_for_card(target);
        app.observe_agent_update_for_card(unrelated);
        assert_eq!(app.workspace.focused(), Some(first));
        assert_eq!(
            app.workspace.visibility(target),
            Some(VisibilityClass::Offscreen)
        );
        assert!(app.sidebar_state.has_card_update(target));

        let _ = app.update(Message::ChatUi(chat_ui::Action::Switch(target)));

        assert_eq!(app.workspace.focused(), Some(target));
        assert_eq!(app.durable.focused, Some(target));
        assert!(app.workspace.camera().target() > app.workspace.camera().offset());
        // The card is revealed while its managed conversation is resolved.
        assert!(!app.sidebar_state.has_card_update(target));
        assert!(app.sidebar_state.has_card_update(unrelated));
        let update = app.workspace.tick(Duration::from_secs(1));
        let _ = app.apply_viewport(update, true, false);
        assert_eq!(
            app.workspace.visibility(target),
            Some(VisibilityClass::Visible)
        );

        // The source is preserved until the daemon confirms its exact identity.
        assert_eq!(
            app.card_state(target).unwrap().kind,
            CardKind::Terminal(source)
        );
        assert!(app.chat_start_pending(target));
    }

    fn live_agent_app(program: &str) -> (Cagents, CardId, tempfile::TempDir) {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let mut app = context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.window_width = 1600.0;
        let mut card = terminal_card_state();
        let id = card.id;
        let CardKind::Terminal(terminal) = &mut card.kind else {
            unreachable!();
        };
        terminal.launch.program = program.into();
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let terminal_id = terminal.id;
        app.durable = Arc::new(WorkspaceState {
            focused: Some(id),
            cards: vec![card],
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1600.0);
        let directory = tempfile::tempdir().unwrap();
        let handle = HostedTerminalHandle::connect_at(
            directory.path().join("missing.sock"),
            terminal_id,
            None,
            |_| {},
        )
        .expect("terminal client starts before the socket accepts");
        app.cards
            .insert(id, CardRuntime::Terminal(TerminalSurface::from(handle)));
        (app, id, directory)
    }

    fn busy_chat_snapshot() -> crate::chat::Snapshot {
        use crate::chat::{
            Capabilities, Choice, Field, Item, ItemContent, Phase, Request, Role, Snapshot,
            ToolStatus,
        };

        Snapshot {
            generation: 2,
            busy: true,
            waiting: true,
            capabilities: Capabilities {
                steer: true,
                queue: true,
                ..Capabilities::default()
            },
            items: vec![
                Item {
                    id: "user-1".into(),
                    role: Role::User,
                    content: ItemContent::Text {
                        text: "Inspect the switch".into(),
                        thinking: false,
                        phase: Phase::Final,
                    },
                    at: None,
                },
                Item {
                    id: "tool-1".into(),
                    role: Role::Assistant,
                    content: ItemContent::Tool {
                        name: "shell".into(),
                        input: "ls".into(),
                        output: String::new(),
                        status: ToolStatus::Running,
                    },
                    at: None,
                },
                Item {
                    id: "assistant-1".into(),
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: "Still working.".into(),
                        thinking: false,
                        phase: Phase::Commentary,
                    },
                    at: None,
                },
            ],
            requests: vec![Request {
                id: "request-1".into(),
                title: "Allow this command?".into(),
                description: "ls".into(),
                fields: vec![Field {
                    id: "note".into(),
                    label: "Note".into(),
                    description: String::new(),
                    choices: Vec::new(),
                    multiple: false,
                    secret: false,
                    initial: String::new(),
                }],
                actions: vec![
                    Choice {
                        id: "deny".into(),
                        label: "Deny".into(),
                        description: String::new(),
                    },
                    Choice {
                        id: "allow".into(),
                        label: "Allow".into(),
                        description: String::new(),
                    },
                ],
            }],
            ..Snapshot::default()
        }
    }

    #[test]
    fn switching_a_live_terminal_to_native_chat_draws_the_swap() {
        let (mut app, card, _socket) = live_agent_app("codex");
        let focus_id = workspace_widget::card_focus_id(card).to_string();
        let composer_id = card_control_id(card, "chat-composer").to_string();
        assert!(
            widget_id_counts(&mut app.view()).contains_key(&focus_id),
            "the terminal surface has to be mounted before the swap, or the crash frame is not drawn"
        );

        draw_frames(&mut app, 3, |app, frame| match frame {
            0 => {
                let _connect = app.update(Message::ChatUi(chat_ui::Action::Switch(card)));
                let ready = chat_ui::ready_message_for_pending_switch(
                    app,
                    card,
                    crate::chat::Snapshot {
                        generation: 1,
                        title: Some("Live thread".into()),
                        ..crate::chat::Snapshot::default()
                    },
                );
                let _focus = app.update(ready);
                assert!(
                    matches!(
                        app.card_state(card).map(|state| &state.kind),
                        Some(CardKind::Chat(_))
                    ),
                    "a confirmed switch replaces the terminal card with native chat"
                );
                assert!(
                    matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))),
                    "the chat runtime is what the next frame draws"
                );
            }
            1 => {
                assert_eq!(
                    chat_ui::show_chat_snapshot_for_test(app, card, busy_chat_snapshot()),
                    1,
                    "the busy frame has to include the in-progress request"
                );
            }
            _ => {}
        });

        let mounted = widget_id_counts(&mut app.view());
        assert!(
            mounted.contains_key(&composer_id),
            "native chat keeps its composer: {mounted:?}"
        );
        assert!(
            !mounted.contains_key(&focus_id),
            "chat must not adopt the terminal focus id that still holds terminal widget state"
        );
    }

    #[test]
    fn a_refused_switch_keeps_the_terminal_on_screen() {
        let (mut app, card, _socket) = live_agent_app("claude");
        draw_frames(&mut app, 2, |app, frame| {
            if frame == 0 {
                let _connect = app.update(Message::ChatUi(chat_ui::Action::Switch(card)));
                let _ = app.update(chat_ui::refused_switch_message(
                    app,
                    card,
                    "fixture: provider refused the handoff",
                ));
            }
        });
        assert!(
            matches!(
                app.card_state(card).map(|state| &state.kind),
                Some(CardKind::Terminal(_))
            ),
            "a refused handoff leaves the terminal card in place"
        );
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Terminal(_))),
            "the terminal surface is still what the card draws"
        );
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message.contains("refused the handoff")),
            "the refusal is recorded as a notice, not a process abort: {:?}",
            app.notices.entries
        );
    }

    #[test]
    fn dirty_close_restores_the_actual_underlying_modal_focus_target() {
        let mut app = context_inspector_test_app(true);
        let project = ProjectId::new();
        app.worktree_dialog = Some(WorktreeDialog::new(
            project,
            PathBuf::from("/repo"),
            WorktreeDraft {
                base: "main".into(),
                destination: "/worktrees".into(),
                ..WorktreeDraft::default()
            },
        ));
        assert_eq!(
            app.nested_modal_focus_target().unwrap().to_string(),
            worktree_dialog_focus_id().to_string()
        );

        app.worktree_dialog = None;
        app.pull_request_dialog = Some(PullRequestDialog {
            project_id: ProjectId::new(),
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            worktree: None,
            base: "main".into(),
            head: "topic".into(),
            title: String::new(),
            body: String::new(),
            draft: false,
            retry_confirmation: None,
        });
        assert_eq!(
            app.nested_modal_focus_target().unwrap().to_string(),
            pull_request_dialog_focus_id().to_string()
        );

        app.pull_request_dialog = None;
        app.window_width = design::WIDE_BREAKPOINT - 1.0;
        assert!(app.context_inspector_is_overlay());
        assert_eq!(
            app.nested_modal_focus_target().unwrap().to_string(),
            context_inspector_toggle_id().to_string()
        );
    }

    #[test]
    fn column_navigator_appears_only_when_movement_is_possible() {
        assert!(!should_show_card_navigator(0));
        assert!(!should_show_card_navigator(1));
        assert!(should_show_card_navigator(2));
    }

    #[test]
    fn context_overlay_update_captures_traps_and_restores_focus_across_startup_and_escape() {
        let mut app = context_inspector_test_app(true);
        let headerbar_enabled = app.core.window.show_headerbar;
        assert!(!app.context_inspector_is_overlay());
        assert_eq!(
            app.context_inspector_restore_focus
                .as_ref()
                .map(ToString::to_string),
            Some(context_inspector_toggle_id().to_string())
        );

        // The compositor's first narrow size is the real startup transition.
        app.context_inspector_restore_focus = None;
        let _focus_close = app.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0, 900.0));
        assert!(app.context_inspector_is_overlay());
        assert!(app.context_inspector_overlay_active);
        assert!(app.workspace_inputs_blocked());
        assert_eq!(
            app.context_inspector_restore_focus
                .as_ref()
                .map(ToString::to_string),
            Some(context_inspector_toggle_id().to_string())
        );
        assert_eq!(app.core.window.show_headerbar, headerbar_enabled);

        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let close = task_app_message(app.update(Message::KeyEvent {
            event: escape,
            captured: false,
        }));
        assert!(matches!(
            close,
            Message::ContextInspector(InspectorAction::Close)
        ));
        let _restore_focus = app.update(close);
        settle_context_inspector_transition(&mut app);
        assert!(!app.context_inspector_open);
        assert!(!app.context_inspector_overlay_active);
        assert!(!app.workspace_inputs_blocked());
        assert!(app.context_inspector_restore_focus.is_none());
        assert_eq!(app.core.window.show_headerbar, headerbar_enabled);

        // Panel tabs change the tools, not workspace focus or the overlay
        // boundary, and remain available in production builds.
        let focused = app.workspace.focused();
        let _ = app.update(Message::ToggleContextInspector);
        for tab in [ContextTab::Changes, ContextTab::Files, ContextTab::Details] {
            let _ = app.update(Message::SelectContextTab(tab));
            assert_eq!(app.context_tab, tab);
            assert_eq!(app.workspace.focused(), focused);
            assert!(app.context_inspector_overlay_active);
            assert_eq!(app.core.window.show_headerbar, headerbar_enabled);
        }
        app.settings_reset_confirm = true;
        let _ = app.update(Message::SelectContextTab(ContextTab::Files));
        assert_eq!(app.context_tab, ContextTab::Details);
        app.settings_reset_confirm = false;
        let _ = app.update(Message::ContextInspector(InspectorAction::Close));

        // Re-entering from a live docked inspector captures afresh instead of
        // depending on the startup value consumed by the first Escape.
        let _wide = app.update(Message::WindowResized(design::WIDE_BREAKPOINT, 900.0));
        let _open_docked = app.update(Message::ToggleContextInspector);
        assert!(!app.context_inspector_is_overlay());
        app.context_inspector_restore_focus = None;
        let _focus_close = app.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0, 900.0));
        assert!(app.context_inspector_is_overlay());
        assert_eq!(
            app.context_inspector_restore_focus
                .as_ref()
                .map(ToString::to_string),
            Some(context_inspector_toggle_id().to_string())
        );

        // An open inspector remains mounted even before a first card arrives,
        // so its window controls remain reachable on a narrow empty workspace.
        let mut startup = context_inspector_test_app(false);
        let _narrow = startup.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0, 900.0));
        assert!(startup.context_inspector_is_overlay());
        assert!(startup.context_inspector_overlay_active);
        assert_eq!(
            startup
                .context_inspector_restore_focus
                .as_ref()
                .map(ToString::to_string),
            Some(context_inspector_toggle_id().to_string())
        );
        let _focus_close = startup.add_repository_card(
            ProjectId::new(),
            PathBuf::from("/repo"),
            PathBuf::from("/repo/worktree"),
        );
        assert!(startup.workspace.focused().is_some());
        assert!(startup.context_inspector_is_overlay());
        assert!(startup.context_inspector_overlay_active);
        assert!(startup.workspace_inputs_blocked());

        let close = task_app_message(startup.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        }));
        let _restore_focus = startup.update(close);
        settle_context_inspector_transition(&mut startup);
        assert!(!startup.context_inspector_open);
        assert!(!startup.context_inspector_overlay_active);
        assert!(startup.context_inspector_restore_focus.is_none());

        // Closing the final card leaves the open inspector and window controls
        // mounted; only explicitly closing the inspector restores outer focus.
        let _focus_close = startup.update(Message::ToggleContextInspector);
        assert!(startup.context_inspector_overlay_active);
        let final_card = startup.workspace.focused().unwrap();
        startup.workspace.request_close(final_card).unwrap();
        let _restore_focus = startup.update(Message::CommitClose(final_card));
        assert!(startup.workspace.focused().is_none());
        assert!(startup.context_inspector_is_overlay());
        assert!(startup.context_inspector_overlay_active);
        let _restore_focus = startup.update(Message::ContextInspector(InspectorAction::Close));
        settle_context_inspector_transition(&mut startup);
        assert!(!startup.context_inspector_is_overlay());
        assert!(!startup.context_inspector_overlay_active);
        assert!(startup.context_inspector_restore_focus.is_none());
    }

    #[test]
    fn context_inspector_resize_persists_only_when_the_gesture_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let mut app = context_inspector_test_app(true);
        app.settings_path = Some(path.clone());
        app.settings_write_blocked = false;

        let _live_resize = app.update(Message::ContextInspector(InspectorAction::Resize {
            width: 384.0,
        }));
        assert_eq!(app.settings.inspector_width, 384.0);
        assert!(!path.exists(), "live key repeats must not persist settings");

        let finished = app.update(Message::ContextInspector(InspectorAction::ResizeFinished));
        settle_settings_task(&mut app, finished);
        assert_eq!(settings::load(&path).unwrap().inspector_width, 384.0);
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
    fn proof_backed_continuity_is_attach_only() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let launch = launch_recipe("/tmp");
        let mut session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
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
    fn daemon_rejection_preserves_identity_until_safe_recovery() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId},
            terminal::ipc::{
                ClientEnvelope, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
                ServerMessage, read_frame, write_frame,
            },
        };
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let old_proof = HostSessionProof {
            host_session_id: HostSessionId::new("old-session"),
            boot_id: BootId::new("same-boot"),
            host_incarnation: HostIncarnation::new("old-incarnation"),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: HostSessionProof {
                        host_session_id: HostSessionId::new("new-session"),
                        boot_id: BootId::new("same-boot"),
                        host_incarnation: HostIncarnation::new("new-incarnation"),
                    },
                    message: ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::ProofMismatch,
                        "daemon was replaced",
                        false,
                    )),
                },
            )
            .unwrap();
        });

        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let state = app.card_state_mut(card).unwrap();
        let CardKind::Terminal(session) = &mut state.kind else {
            panic!("expected terminal fixture");
        };
        let old_id = session.id;
        session.continuity = TerminalContinuity::DetachedSameBoot {
            host_session_id: old_proof.host_session_id.clone(),
            boot_id: old_proof.boot_id.clone(),
            host_incarnation: old_proof.host_incarnation.clone(),
        };
        let original = session.clone();
        app.lifecycle.register(
            card,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle =
            HostedTerminalHandle::connect_at(socket, old_id, Some(old_proof), move |wake| {
                let _ = wake_tx.send(wake);
            })
            .unwrap();
        let wake = wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
        // A rejected handshake wakes its consumer before it queues the error,
        // so the wake alone does not mean there is anything to drain yet.
        let events = handle.events();
        let deadline = Instant::now() + Duration::from_secs(5);
        while events.is_empty() {
            assert!(Instant::now() < deadline, "the rejection was never queued");
            std::thread::sleep(Duration::from_millis(1));
        }
        // The subscription can deliver this edge before the start task returns.
        let _ = app.update(Message::TerminalWake {
            card,
            generation: wake.generation,
        });
        let _ = app.update(Message::TerminalStarted {
            card,
            terminal: old_id,
            result: Ok(StartedTerminal {
                handle: TerminalSurface::from(handle),
                session: original.clone(),
                spawned: false,
            }),
        });
        assert_eq!(
            &app.card_state(card).unwrap().kind,
            &CardKind::Terminal(original.clone())
        );
        // Discovery has been scheduled, but no unauthenticated replacement
        // identity or replay recipe may enter the durable state.
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        let _ = app.update(Message::TerminalStarted {
            card,
            terminal: old_id,
            result: Err("replacement ownership cannot be established".into()),
        });
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        let _ = app.retry_card(card);
        assert_eq!(
            &app.card_state(card).unwrap().kind,
            &CardKind::Terminal(original)
        );
    }

    #[test]
    fn exited_terminal_retries_with_fresh_spawn_identity_and_exit_metadata() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let launch = launch_recipe("/worktree");
        let mut session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
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

    /// However it ended, a restored terminal is presented the same way it was
    /// left: an ended card that states its outcome and offers a relaunch, not
    /// a failed runtime.
    #[test]
    fn restored_child_exit_stays_retryable_without_starting_the_stale_identity() {
        let launch = launch_recipe("/worktree");
        let session = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
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
            restored_runtime_residency(
                &CardKind::Terminal(session),
                Exposure::Visible,
                &Residency::Hot,
            ),
            Residency::Hibernated
        );
    }

    fn restored_live_terminal(program: &str, exposure: Exposure) -> CardState {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let mut card = terminal_card_state();
        card.exposure = exposure;
        card.residency = Residency::Hibernated;
        let CardKind::Terminal(session) = &mut card.kind else {
            unreachable!("terminal fixture");
        };
        session.launch.program = program.into();
        session.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        card
    }

    /// An agent's row reports what it is doing only through an attached
    /// terminal, so a restored agent off screen is attached without waiting
    /// for its card to be opened, while a plain shell stays detached.
    #[test]
    fn restored_agents_attach_one_at_a_time_after_visible_cards_start() {
        let mut app = context_inspector_test_app(false);
        let visible = restored_live_terminal("/bin/sh", Exposure::Visible);
        let agents = [
            restored_live_terminal("/usr/bin/claude", Exposure::Offscreen),
            restored_live_terminal("/usr/bin/codex", Exposure::Offscreen),
        ];
        let shell = restored_live_terminal("/bin/sh", Exposure::Offscreen);
        app.durable = Arc::new(WorkspaceState {
            focused: Some(visible.id),
            cards: [visible.clone(), shell.clone()]
                .into_iter()
                .chain(agents.clone())
                .collect(),
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        let terminal = |card: &CardState| match &card.kind {
            CardKind::Terminal(session) => session.id,
            _ => unreachable!("terminal fixture"),
        };
        drop(app.initialize_runtimes());
        // The card on screen starts first; agents wait for its recovery lease.
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert_eq!(app.restored_agent_attaches, [agents[0].id, agents[1].id]);
        let _ = app.update(Message::TerminalStarted {
            card: visible.id,
            terminal: terminal(&visible),
            result: Err("fixture".into()),
        });
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert_eq!(app.restored_agent_attaches, [agents[1].id]);
        let notices = app.notices.entries.len();

        // A terminal that would have to be launched again stays detached, and
        // so does one that cannot be attached; neither is a failure anyone
        // has to see.
        let _ = app.update(Message::RestoredAgentAttached {
            card: agents[0].id,
            terminal: terminal(&agents[0]),
            result: Ok(None),
        });
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert!(app.restored_agent_attaches.is_empty());
        let _ = app.update(Message::RestoredAgentAttached {
            card: agents[1].id,
            terminal: terminal(&agents[1]),
            result: Err("the daemon is unavailable".into()),
        });
        assert_eq!(app.terminal_starts_in_flight(), 0);
        for card in agents.iter().chain([&shell]) {
            assert!(matches!(
                app.cards.get(&card.id),
                Some(CardRuntime::Loading)
            ));
            assert_eq!(
                app.lifecycle.state(card.id).map(|(_, residency)| residency),
                Some(&Residency::Hibernated)
            );
        }
        assert_eq!(app.notices.entries.len(), notices);
    }

    #[test]
    fn a_queued_agent_attach_yields_to_the_cards_own_start() {
        let mut app = context_inspector_test_app(false);
        let agent = restored_live_terminal("/usr/bin/claude", Exposure::Offscreen);
        app.durable = Arc::new(WorkspaceState {
            cards: vec![agent.clone()],
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards.insert(agent.id, CardRuntime::Loading);
        app.lifecycle.register(
            agent.id,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        app.restored_agent_attaches.push_back(agent.id);
        drop(app.attach_next_restored_agent());
        assert!(app.restored_agent_attaches.is_empty());
        assert_eq!(app.terminal_starts_in_flight(), 0);
    }

    /// A restored agent off screen, registered as the queue leaves it.
    fn hibernated_agent_app(
        continuity: Option<TerminalContinuity>,
    ) -> (Cagents, CardId, TerminalSession) {
        let mut app = context_inspector_test_app(false);
        let mut agent = restored_live_terminal("/usr/bin/claude", Exposure::Offscreen);
        let CardKind::Terminal(session) = &mut agent.kind else {
            unreachable!("terminal fixture");
        };
        if let Some(continuity) = continuity {
            session.continuity = continuity;
        }
        let session = session.clone();
        app.durable = Arc::new(WorkspaceState {
            cards: vec![agent.clone()],
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards.insert(agent.id, CardRuntime::Loading);
        app.lifecycle.register(
            agent.id,
            Exposure::Offscreen,
            Residency::Hibernated,
            Instant::now(),
        );
        (app, agent.id, session)
    }

    fn unconnected_surface(directory: &Path, terminal: TerminalId) -> TerminalSurface {
        TerminalSurface::from(
            HostedTerminalHandle::connect_at(
                directory.join("missing.sock"),
                terminal,
                None,
                |_| {},
            )
            .expect("terminal client starts before the socket accepts"),
        )
    }

    fn surface_visibility(app: &Cagents, card: CardId) -> crate::terminal_ipc::TerminalVisibility {
        match app.cards.get(&card) {
            Some(CardRuntime::Terminal(surface)) => surface.visibility(),
            _ => panic!("expected an attached terminal"),
        }
    }

    /// An agent in a worktree being deleted waits in the queue rather than
    /// leaving it, so a deletion that fails still has its row attached.
    #[test]
    fn an_agent_waits_out_its_worktrees_deletion_for_its_row_attach() {
        let (mut app, card, session) = hibernated_agent_app(None);
        let target = app
            .project_ui
            .begin_deleting(session.launch.working_directory.clone());
        app.restored_agent_attaches.push_back(card);
        drop(app.attach_next_restored_agent());
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert_eq!(app.restored_agent_attaches, [card]);

        let _ = app.update(Message::ProjectUi(project_ui::Action::Deleted {
            target,
            result: Err("the worktree has changes".into()),
        }));
        // The card is started, by the row attach or by the sweep hydrating it
        // outright, and no hibernated agent is left waiting on the deletion.
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert!(app.restored_agent_attaches.iter().all(|queued| !matches!(
            app.lifecycle.state(*queued),
            Some((_, Residency::Hibernated))
        )));
    }

    /// An attach for the row keeps the card hibernated, so it streams no
    /// screen, and opening the card later reuses that connection.
    #[test]
    fn an_attached_agent_stays_hibernated_until_its_card_is_opened() {
        use crate::terminal_ipc::TerminalVisibility;

        let directory = tempfile::tempdir().unwrap();
        let (mut app, card, session) = hibernated_agent_app(None);
        app.restored_agent_attaches.push_back(card);
        drop(app.attach_next_restored_agent());
        assert_eq!(app.terminal_starts_in_flight(), 1);
        let _ = app.update(Message::RestoredAgentAttached {
            card,
            terminal: session.id,
            result: Ok(Some(StartedTerminal {
                handle: unconnected_surface(directory.path(), session.id),
                session: session.clone(),
                spawned: false,
            })),
        });
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert_eq!(
            surface_visibility(&app, card),
            TerminalVisibility::Hibernated
        );
        assert_eq!(
            app.lifecycle.state(card).map(|(_, residency)| residency),
            Some(&Residency::Hibernated)
        );

        drop(app.reconcile_lifecycle(&HashSet::from([card]), &HashSet::new()));
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert_ne!(
            surface_visibility(&app, card),
            TerminalVisibility::Hibernated
        );
        assert!(!matches!(
            app.lifecycle.state(card).map(|(_, residency)| residency),
            Some(Residency::Hibernated)
        ));
    }

    /// Opening the card while its row's attach is still under way starts the
    /// card's own attach too; whichever lands first is kept, and the other is
    /// let go without failing the card.
    #[test]
    fn an_agent_opened_during_its_row_attach_keeps_one_connection() {
        use crate::terminal_ipc::TerminalVisibility;

        let directory = tempfile::tempdir().unwrap();
        let (mut app, card, session) = hibernated_agent_app(None);
        app.restored_agent_attaches.push_back(card);
        drop(app.attach_next_restored_agent());
        drop(app.reconcile_lifecycle(&HashSet::from([card]), &HashSet::new()));
        assert_eq!(app.terminal_starts_in_flight(), 2);
        let started = || StartedTerminal {
            handle: unconnected_surface(directory.path(), session.id),
            session: session.clone(),
            spawned: false,
        };
        let _ = app.update(Message::RestoredAgentAttached {
            card,
            terminal: session.id,
            result: Ok(Some(started())),
        });
        assert_eq!(app.terminal_starts_in_flight(), 1);
        // The card is being opened, so the row's attach is not kept hibernated.
        assert_ne!(
            surface_visibility(&app, card),
            TerminalVisibility::Hibernated
        );
        let _ = app.update(Message::TerminalStarted {
            card,
            terminal: session.id,
            result: Ok(started()),
        });
        assert_eq!(app.terminal_starts_in_flight(), 0);
        assert!(matches!(
            app.cards.get(&card),
            Some(CardRuntime::Terminal(_))
        ));
        assert!(app.notices.entries.is_empty(), "{:?}", app.notices.entries);
    }

    /// Losing the daemon connection of an agent attached only for its row
    /// queues another attach, and never launches the agent again: that is the
    /// user's decision, made by opening the card.
    #[test]
    fn a_hibernated_agent_that_loses_its_daemon_is_only_attached_again() {
        use crate::{
            model::{BootId, HostIncarnation, HostSessionId},
            terminal::ipc::{
                ClientEnvelope, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
                ServerMessage, read_frame, write_frame,
            },
        };
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
            write_frame(
                &mut stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof: HostSessionProof {
                        host_session_id: HostSessionId::new("replacement"),
                        boot_id: BootId::new("boot"),
                        host_incarnation: HostIncarnation::new("replacement"),
                    },
                    message: ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::ProofMismatch,
                        "daemon was replaced",
                        false,
                    )),
                },
            )
            .unwrap();
        });
        let (mut app, card, session) = hibernated_agent_app(None);
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle =
            HostedTerminalHandle::connect_at(socket, session.id, Some(proof), move |wake| {
                let _ = wake_tx.send(wake);
            })
            .unwrap();
        app.cards
            .insert(card, CardRuntime::Terminal(TerminalSurface::from(handle)));
        let wake = wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
        let notices = app.notices.entries.len();

        drop(app.update(Message::TerminalWake {
            card,
            generation: wake.generation,
        }));
        // The attach-only path took it up again; a relaunch would have made
        // the card hydrate.
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(app.restored_agent_attaches.is_empty());
        assert_eq!(app.terminal_starts_in_flight(), 1);
        assert_eq!(
            app.lifecycle.state(card),
            Some((&Exposure::Offscreen, &Residency::Hibernated))
        );
        assert_eq!(
            app.card_state(card).map(|state| &state.residency),
            Some(&Residency::Hibernated)
        );
        assert_eq!(app.notices.entries.len(), notices);
    }

    #[test]
    fn a_background_read_reselects_only_the_scope_its_project_holds() {
        use crate::model::{ChannelId, ProjectChannel, WorktreeId};
        use crate::sidebar::{SidebarProject, SidebarWorktree};

        let mut app = context_inspector_test_app(false);
        let [holding, other] = [ProjectId::new(), ProjectId::new()];
        for (project, root) in [(holding, "/repo"), (other, "/elsewhere")] {
            app.sidebar_model.projects.push(SidebarProject::new(
                project,
                RepositoryId::new(),
                "project".into(),
                root.into(),
                vec![SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    format!("{root}/worktrees/feature").into(),
                    Some("feature".into()),
                )],
                Vec::new(),
            ));
        }
        let launch = Path::new("/repo/worktrees/feature/src");
        assert!(app.project_owns_active_scope(holding, launch));
        assert!(!app.project_owns_active_scope(other, launch));
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: other,
                title: "channel".into(),
                ordinal: 0,
                shared: false,
                cards: Vec::new(),
                focused: None,
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        assert!(app.project_owns_active_scope(other, launch));
        assert!(!app.project_owns_active_scope(holding, launch));
    }

    #[test]
    fn restored_successful_terminal_is_completed_without_automatic_relaunch() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!("terminal fixture");
        };
        transition_terminal_to_relaunchable(session, terminal_exit(0, None));
        let identity = session.id;
        assert!(terminal_completed(session));
        drop(app.initialize_runtimes());
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
        assert!(app.status.is_none());
        assert!(
            app.lifecycle
                .reconcile(&HashSet::from([card]), &HashSet::new(), Instant::now())
                .is_empty()
        );
        let session = ended_terminal(app.card_state(card)).unwrap();
        assert_eq!(session.id, identity);
        // Relaunch remains an explicit action with a fresh host identity.
        assert_ne!(fresh_terminal_relaunch(session).id, identity);
    }

    /// Workspace previews freeze visit order until Alt is released, including
    /// channel identities that share their project's worktree directory.
    #[test]
    fn alt_tab_previews_workspace_history_and_commits_on_release_or_blur() {
        use crate::model::{ChannelId, ProjectChannel, WorktreeId};
        use crate::sidebar::{SidebarProject, SidebarWorktree};

        let mut app = context_inspector_test_app(false);
        let project = ProjectId::new();
        let channel = ChannelId::new();
        let cards: Vec<_> = ["/repo", "/repo/feature", "/repo"]
            .into_iter()
            .map(|path| {
                let mut card = terminal_card_state();
                let CardKind::Terminal(session) = &mut card.kind else {
                    unreachable!()
                };
                session.launch.working_directory = path.into();
                card
            })
            .collect();
        let channel_card = cards[2].id;
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards = cards;
        durable.channels.push(ProjectChannel {
            id: channel,
            project_id: project,
            title: "Migration".into(),
            ordinal: 0,
            shared: false,
            cards: vec![channel_card],
            focused: Some(channel_card),
        });
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            vec![
                SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    "/repo".into(),
                    Some("main".into()),
                ),
                SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    "/repo/feature".into(),
                    Some("feature".into()),
                ),
            ],
            Vec::new(),
        ));
        app.refresh_sidebar_channels();

        let destinations = sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state);
        let root = WorkspaceDestination::from_action(&destinations[0]).unwrap();
        let feature = WorkspaceDestination::from_action(&destinations[1]).unwrap();
        let channel = WorkspaceDestination::Channel(channel);
        // Sidebar order is root, feature, channel; visit order deliberately differs.
        for index in [0, 1, 2, 0] {
            drop(app.update(Message::Sidebar(destinations[index].clone())));
        }
        let next = |modifiers| Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            },
            captured: true,
        };
        let alt = keyboard::Modifiers::ALT;
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(channel));
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(feature));
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(alt | keyboard::Modifiers::SHIFT),
            captured: false,
        }));
        drop(app.update(next(alt | keyboard::Modifiers::SHIFT)));
        assert_eq!(app.active_workspace_destination(), Some(channel));
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty()),
            captured: false,
        }));
        // Intermediate feature preview did not become more recent than root.
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(root));
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(feature));
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            false,
        ))));
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(channel));
        app.settings_reset_confirm = true;
        drop(app.update(next(alt)));
        assert_eq!(app.active_workspace_destination(), Some(channel));
    }

    #[test]
    fn pane_diagnostics_toggle_keeps_the_inspector_open_and_ignores_stale_cards() {
        let mut app = context_inspector_test_app(true);
        let id = app.workspace.focused().unwrap();
        app.context_inspector_open = true;
        drop(app.handle_context_inspector_action(InspectorAction::ToggleDiagnostics(id)));
        assert_eq!(app.inspector_diagnostics_card, Some(id));
        assert!(app.context_inspector_open);
        drop(
            app.handle_context_inspector_action(InspectorAction::ToggleDiagnostics(CardId::new())),
        );
        assert_eq!(app.inspector_diagnostics_card, Some(id));
        drop(app.handle_context_inspector_action(InspectorAction::ToggleDiagnostics(id)));
        assert_eq!(app.inspector_diagnostics_card, None);
        assert!(app.context_inspector_open);
    }

    /// A killed session used to take its pane with it: `fail_runtime` replaced
    /// the terminal runtime with a failure surface, so the scrollback that said
    /// why the process died was discarded along with it.
    #[test]
    fn an_unclean_exit_keeps_the_terminal_surface_it_explains() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.lifecycle
            .register(card, Exposure::Visible, Residency::Hot, Instant::now());
        // A sentinel stands in for the live surface: the retirement must leave
        // whatever runtime the card holds exactly where it is.
        app.cards.insert(card, CardRuntime::Loading);

        app.retire_ended_terminal(card, 0, Some("SIGKILL"), false);

        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(app.lifecycle.state(card).is_none());
        assert_eq!(
            app.card_state(card).map(|state| &state.residency),
            Some(&Residency::Hibernated)
        );
        assert!(app.status.is_some());
        let session = ended_terminal(app.card_state(card)).expect("the terminal has ended");
        // A signalled exit is not a clean zero status, and it says so.
        assert!(!terminal_completed(session));
        assert_eq!(terminal_end_summary(session), "SIGKILL");

        app.retire_ended_terminal(card, 0, None, false);
        let session = ended_terminal(app.card_state(card)).expect("the terminal has ended");
        assert!(terminal_completed(session));
        assert_eq!(terminal_end_summary(session), "Completed");

        app.retire_ended_terminal(card, 3, None, false);
        let session = ended_terminal(app.card_state(card)).expect("the terminal has ended");
        assert_eq!(terminal_end_summary(session), "Exited with status 3");

        // A relaunch carries the exit forward as metadata. While the card is
        // hydrating that is a starting terminal, not an ended one, and it must
        // not be read back as read-only scrollback with a Restart button.
        app.card_state_mut(card).unwrap().residency = Residency::Hydrating { epoch: 1 };
        assert!(ended_terminal(app.card_state(card)).is_none());
        assert!(completed_terminal(app.card_state(card)).is_none());
    }

    #[test]
    fn only_success_without_a_signal_is_normal_completion() {
        let mut state = terminal_card_state();
        let CardKind::Terminal(session) = &mut state.kind else {
            panic!("terminal fixture");
        };
        for (code, signal, completed, summary) in [
            (Some(0), None, true, "Completed"),
            (Some(1), None, false, "Exited with status 1"),
            (None, Some("Terminated"), false, "Terminated"),
            (None, None, false, "Ended"),
        ] {
            transition_terminal_to_relaunchable(
                session,
                TerminalExit {
                    code,
                    signal: signal.map(str::to_owned),
                    occurred_at_unix_ms: 0,
                },
            );
            assert_eq!(terminal_completed(session), completed);
            assert_eq!(terminal_end_summary(session), summary);
        }
    }

    #[test]
    fn completed_managed_codex_reopens_its_native_owner_without_a_new_terminal_id() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!("terminal fixture");
        };
        let identity = session.id;
        session.launch.program = "codex".into();
        let endpoint = std::path::PathBuf::from("/daemon-temporary-root")
            .join(format!("cagents-codex-{identity}"))
            .join("control.sock");
        session.launch.arguments =
            vec!["--remote".into(), format!("unix://{}", endpoint.display())];
        session.owned_codex_listener = Some(crate::chat::OwnedCodexListener {
            socket: endpoint,
            allocator: HostSessionProof {
                host_session_id: "allocator".into(),
                boot_id: "boot".into(),
                host_incarnation: "previous-host".into(),
            },
        });
        transition_terminal_to_relaunchable(session, terminal_exit(0, None));
        // The fixture starts a card mid-hydration; this one has finished.
        app.card_state_mut(card).unwrap().residency = Residency::Hibernated;
        app.cards.insert(card, CardRuntime::Completed);
        assert!(app.completed_terminal_has_native_owner(card));
        // Dropping the task never connects to or launches a real agent.
        drop(app.retry_card(card));
        assert!(app.chat_start_pending(card));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Completed)));
        let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
            panic!("native attach has not completed yet");
        };
        assert_eq!(session.id, identity);
        assert_eq!(app.terminal_starts_in_flight(), 0);
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

    /// A row for each project, as session restoration builds them before any
    /// repository has been read.
    fn restored_sidebar_project(id: ProjectId, name: &str, repository: &Path) -> SidebarProject {
        SidebarProject::new(
            id,
            RepositoryId::new(),
            name.to_owned(),
            repository.to_path_buf(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn repository_load_result(
        project_id: ProjectId,
        result: Result<LoadedRepository, String>,
    ) -> Message {
        Message::RepositoryLoaded {
            open_cards: false,
            project_id,
            generation: 1,
            result: result.map(Box::new),
        }
    }

    #[test]
    fn one_unreadable_project_keeps_every_other_project_listed() {
        let mut app = context_inspector_test_app(false);
        let broken = ProjectId::new();
        let healthy = ProjectId::new();
        let healthy_path = PathBuf::from("/repositories/healthy");
        app.sidebar_model.projects = vec![
            restored_sidebar_project(broken, "broken", Path::new("/repositories/broken")),
            restored_sidebar_project(healthy, "healthy", &healthy_path),
        ];
        app.sidebar_model.data_state = SidebarDataState::Loading;
        app.repository_generations.insert(broken, 1);
        app.repository_generations.insert(healthy, 1);
        let loaded = LoadedRepository {
            project: ProjectState {
                kind: Default::default(),
                id: healthy,
                generation: 0,
                name: "healthy".into(),
                repository: healthy_path.clone(),
                existence: Existence::Open,
            },
            repository: Some(RepositoryInfo {
                id: RepositoryId::new(),
                selected_path: healthy_path.clone(),
                common_dir: healthy_path.join(".git"),
                git_dir: healthy_path.join(".git"),
                worktree_root: Some(healthy_path.clone()),
                is_bare: false,
            }),
            worktrees: Vec::new(),
            github: GitHubAvailability::NotAssociated,
            review_repository: None,
            head_repositories: HashMap::new(),
        };

        // The failure lands last on purpose. A failure followed by a success
        // would be rescued by the success setting the pane Ready, so only this
        // order can catch a failure that blanks the pane.
        drop(app.update(repository_load_result(healthy, Ok(loaded))));
        drop(app.update(repository_load_result(
            broken,
            Err("fatal: not a git repository".into()),
        )));

        assert_eq!(app.sidebar_model.data_state, SidebarDataState::Ready);
        assert_eq!(
            app.sidebar_model
                .projects
                .iter()
                .map(|project| (project.id, project.unreachable.clone()))
                .collect::<Vec<_>>(),
            vec![
                (broken, Some("fatal: not a git repository".to_owned())),
                (healthy, None),
            ]
        );
    }

    #[test]
    fn failed_sidebar_refresh_finishes_loading_with_an_error() {
        let mut app = context_inspector_test_app(false);
        let picked = ProjectId::new();
        app.sidebar_model.data_state = SidebarDataState::Loading;
        app.repository_generations.insert(picked, 1);

        drop(app.update(repository_load_result(picked, Err("refresh failed".into()))));

        assert!(app.sidebar_model.projects.is_empty());
        assert_eq!(
            app.sidebar_model.data_state,
            SidebarDataState::Error("refresh failed".into())
        );
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
            .focus_offset(cards[2], 600.0, workspace.camera().offset())
            .unwrap();

        assert!(synchronize_scroll);
        assert_eq!(workspace.focused(), Some(cards[2]));
        assert_eq!(workspace.camera().target(), expected_target);
        assert!(workspace.camera().is_animating());
    }

    pub(super) fn terminal_card_state() -> CardState {
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
                owned_codex_listener: None,
                recovery_origin: None,
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
    fn pane_clone_is_adjacent_and_never_shares_terminal_continuity() {
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let source = app.durable.cards[0].clone();
        let _unpolled = app.clone_focused_pane();
        let cloned = app.workspace.focused().unwrap();
        assert_ne!(cloned, source.id);
        assert_eq!(app.workspace.cards()[1].id, cloned);
        let original_state = app.card_state(source.id).unwrap();
        assert_eq!(original_state.id, source.id);
        assert_eq!(original_state.kind, source.kind);
        assert_eq!(original_state.width_fraction, source.width_fraction);
        assert_eq!(original_state.existence, source.existence);
        let copy = app.card_state(cloned).unwrap();
        assert_eq!(copy.width_fraction, source.width_fraction);
        let (CardKind::Terminal(original), CardKind::Terminal(copy)) = (&source.kind, &copy.kind)
        else {
            panic!("both panes must remain terminals");
        };
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.launch, original.launch);
        assert!(copy.continuity.live_proof().is_none());
        assert!(copy.resume_hint.is_none());
    }

    #[test]
    fn pane_clone_starts_a_new_chat_without_reusing_source_conversation() {
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let source_id = app.workspace.focused().unwrap();
        let mut recipe = launch_recipe("/tmp");
        recipe.program = "claude".into();
        let conversation = "a1b2c3d4-1111-4111-8111-123456789abc";
        recipe.arguments = vec!["--resume".into(), conversation.into(), "--bare".into()];
        let original = crate::chat::Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe,
                session_id: Some(conversation.into()),
            },
            proof: None,
            terminal: None,
        };
        app.card_state_mut(source_id).unwrap().kind = CardKind::Chat(original.clone());
        let _unpolled = app.clone_focused_pane();
        let cloned = app.workspace.focused().unwrap();
        assert_ne!(cloned, source_id);
        let CardKind::Chat(copy) = &app.card_state(cloned).unwrap().kind else {
            panic!("cloned conversation must stay native");
        };
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.launch.provider, original.launch.provider);
        assert_eq!(copy.launch.recipe.arguments, ["--bare"]);
        assert!(copy.launch.session_id.is_none());
        assert!(copy.proof.is_none());
        assert!(copy.terminal.is_none());
        assert_eq!(
            app.card_state(source_id).unwrap().kind,
            CardKind::Chat(original)
        );
    }

    fn diagnostic(message: &str) -> crate::lsp::LspDiagnostic {
        crate::lsp::LspDiagnostic {
            range: crate::lsp::LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1)),
            severity: crate::lsp::LspSeverity::Error,
            message: message.into(),
            source: None,
            code: None,
        }
    }

    fn completion_item(label: &str) -> crate::lsp::LspCompletionItem {
        crate::lsp::LspCompletionItem {
            label: label.into(),
            kind: None,
            detail: None,
            documentation: None,
            sort_text: None,
            filter_text: None,
            preselect: false,
            deprecated: false,
            edit: crate::lsp::LspCompletionEdit {
                range: crate::lsp::LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 0)),
                replace_range: None,
                new_text: label.into(),
                snippet: None,
            },
            additional_edits: Vec::new(),
        }
    }

    fn lsp_test_app() -> Cagents {
        let mut app = context_inspector_test_app(false);
        // No configured server, so nothing in these tests can spawn a process
        // or depend on one being installed on the machine running them.
        app.settings.language_servers = Vec::new();
        app.lsp = LspManager::new(Vec::new());
        app
    }

    fn repository_card(app: &mut Cagents, worktree: &str) -> CardId {
        let card = CardId::new();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(
                repository_widget::RepositoryRuntime::file_list_fixture(
                    RepositoryView {
                        selected_file: None,
                        project: ProjectId::new(),
                        repository: PathBuf::from(worktree),
                        worktree: PathBuf::from(worktree),
                    },
                    &["src/main.rs", "src/other.rs"],
                ),
            )),
        );
        card
    }

    fn lsp_state(app: &mut Cagents, card: CardId) -> &mut LspInspectorState {
        app.lsp_contexts.entry(card).or_default()
    }

    fn notice(app: &Cagents) -> Option<&str> {
        app.status.as_ref().map(|notice| notice.message.as_str())
    }

    /// A card is closed far more often than a file is. Leaving its document
    /// open would keep a server analysing and publishing for a file nobody can
    /// see, and would grow all three maps for the life of the session.
    #[test]
    fn closing_a_card_retires_everything_it_held_at_the_lsp_boundary() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        let descriptor = runtime.descriptor.clone();
        Arc::make_mut(&mut app.durable).cards.push(CardState {
            id: card,
            generation: 0,
            title: "Repository".into(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(descriptor),
        });
        app.workspace
            .add_card(
                WorkspaceCard::new(card, "Repository", CardWidth::Fraction(0.7)).unwrap(),
                0,
                true,
            )
            .unwrap();
        app.workspace.request_close(card).unwrap();
        let state = lsp_state(&mut app, card);
        state.document = Some(LspDocumentId(1));
        state.path = Some(PathBuf::from("/repo/a.rs"));
        app.lsp_documents.insert(LspDocumentId(1), card);
        app.lsp_pending_jumps.insert(
            card,
            PendingLspJump {
                path: PathBuf::from("/repo/b.rs"),
                at: LspPosition::new(0, 0),
                coordinates: LspCoordinates::Scalar,
            },
        );

        let _ = app.update(Message::CommitClose(card));

        assert!(!app.cards.contains_key(&card));
        assert!(!app.lsp_contexts.contains_key(&card));
        assert!(!app.lsp_documents.contains_key(&LspDocumentId(1)));
        assert!(!app.lsp_pending_jumps.contains_key(&card));
    }

    /// Diagnostics are unsolicited and replace each other wholesale, so a list
    /// on screen can outlive the text it describes. Jumping into a stale one
    /// puts the caret on a line the server never named.
    /// The two versions are equal only when both are known. An application
    /// that kept a count of its own would satisfy this predicate while the
    /// manager had silently moved on - a restart, a crash-respawn, or a second
    /// card opening the same file all bump the manager's version without any
    /// edit having been made.
    #[test]
    fn diagnostics_are_current_only_when_both_versions_are_known_and_equal() {
        assert!(diagnostics_are_current(Some(4), Some(4)));
        assert!(!diagnostics_are_current(Some(4), Some(5)));
        assert!(!diagnostics_are_current(Some(5), Some(4)));
        // No document: nothing published for it can still describe it.
        assert!(!diagnostics_are_current(Some(4), None));
        // Nothing published: there is no list to jump into.
        assert!(!diagnostics_are_current(None, Some(4)));
        assert!(!diagnostics_are_current(None, None));
    }

    /// The jump arm must consult that predicate rather than jumping blind.
    #[test]
    fn a_jump_into_stale_diagnostics_is_refused() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        // A published list, and a document the manager does not hold, which is
        // exactly the shape the predicate refuses.
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        lsp_state(&mut app, card).diagnostics_version = Some(6);

        let _ = app.update_lsp(InspectorAction::JumpToLspDiagnostic {
            card,
            at: LspPosition::new(3, 0),
        });
        assert_eq!(
            notice(&app),
            Some("these diagnostics describe an earlier version of the file")
        );
    }

    /// The completion list is addressed by position and a fresh response
    /// replaces it wholesale, so the label is the fingerprint that catches a
    /// list which changed under the press.
    #[test]
    fn a_completion_whose_label_moved_is_not_applied() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).context.completions = vec![completion_item("alpha")];

        let _ = app.update_lsp(InspectorAction::ApplyLspCompletion {
            card,
            index: 0,
            label: "beta".into(),
        });
        assert_eq!(
            notice(&app),
            Some("the completion list changed; nothing was applied")
        );
        // Refused, not consumed: the list the user is looking at is still there.
        assert_eq!(app.lsp_contexts[&card].context.completions.len(), 1);

        // A matching fingerprint gets as far as the buffer, which is a
        // different outcome from the refusal above.
        app.status = None;
        let _ = app.update_lsp(InspectorAction::ApplyLspCompletion {
            card,
            index: 0,
            label: "alpha".into(),
        });
        assert_eq!(
            notice(&app),
            Some("`alpha` could not be applied to this buffer")
        );
    }

    /// An index past the end of a list that shrank must not panic, and must not
    /// silently apply the last candidate either.
    #[test]
    fn a_completion_index_past_the_end_applies_nothing() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).context.completions = vec![completion_item("alpha")];

        let _ = app.update_lsp(InspectorAction::ApplyLspCompletion {
            card,
            index: 4,
            label: "alpha".into(),
        });
        assert_eq!(notice(&app), None);
        assert_eq!(app.lsp_contexts[&card].context.completions.len(), 1);
    }

    /// Two cards can hold two documents at once, and diagnostics arrive
    /// unsolicited for either. Routing them by anything but the document id
    /// puts one file's errors on another file's card.
    #[test]
    fn diagnostics_land_on_the_card_that_owns_the_document() {
        let mut app = lsp_test_app();
        let first = repository_card(&mut app, "/repo/first");
        let second = repository_card(&mut app, "/repo/second");
        lsp_state(&mut app, first).document = Some(LspDocumentId(1));
        lsp_state(&mut app, second).document = Some(LspDocumentId(2));
        app.lsp_documents.insert(LspDocumentId(1), first);
        app.lsp_documents.insert(LspDocumentId(2), second);

        app.apply_lsp_event(LspEvent::Diagnostics {
            doc: LspDocumentId(2),
            version: 1,
            items: vec![diagnostic("undefined name")],
        });

        assert_eq!(app.lsp_contexts[&second].context.diagnostics.len(), 1);
        assert_eq!(app.lsp_contexts[&second].diagnostics_version, Some(1));
        assert!(app.lsp_contexts[&first].context.diagnostics.is_empty());
        assert_eq!(app.lsp_contexts[&first].diagnostics_version, None);
    }

    /// An event for a document nobody holds any more has no surface to land on.
    #[test]
    fn an_event_for_a_closed_document_is_dropped() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        app.lsp_documents.insert(LspDocumentId(1), card);

        app.apply_lsp_event(LspEvent::Diagnostics {
            doc: LspDocumentId(9),
            version: 1,
            items: vec![diagnostic("undefined name")],
        });

        assert!(app.lsp_contexts[&card].context.diagnostics.is_empty());
    }

    /// A desync means the two sides disagreed about the text every answer was
    /// computed against. The buffer survives; the answers cannot.
    #[test]
    fn a_desync_drops_the_answers_and_keeps_the_diagnostics() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        app.lsp_documents.insert(LspDocumentId(1), card);
        let state = lsp_state(&mut app, card);
        state.document = Some(LspDocumentId(1));
        state.context.hover = Some("hover".into());
        state.context.completions = vec![completion_item("alpha")];
        state.context.locations = Some((crate::lsp::LocationKind::Definition, Vec::new()));
        state.context.diagnostics = vec![diagnostic("undefined name")];

        app.apply_lsp_event(LspEvent::DocumentDesynced {
            doc: LspDocumentId(1),
            version: 12,
            error: LspError::NoServer,
        });

        let state = &app.lsp_contexts[&card];
        assert!(state.context.hover.is_none());
        assert!(state.context.completions.is_empty());
        assert!(state.context.locations.is_none());
        // Diagnostics are the server's own standing answer, replaced on its
        // schedule, and are refused by version at the point of use instead.
        assert_eq!(state.context.diagnostics.len(), 1);
        assert!(state.context.notice.is_some());
    }

    /// A superseded request is the ordinary result of typing while one is in
    /// flight. Reporting it would put a notice on screen for every keystroke.
    #[test]
    fn a_cancelled_request_is_not_reported() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        app.lsp_documents.insert(LspDocumentId(1), card);

        app.apply_lsp_event(LspEvent::RequestFailed {
            request: crate::lsp::LspRequestId(1),
            doc: LspDocumentId(1),
            error: LspError::Cancelled,
        });
        assert!(app.lsp_contexts[&card].context.notice.is_none());

        app.apply_lsp_event(LspEvent::RequestFailed {
            request: crate::lsp::LspRequestId(2),
            doc: LspDocumentId(1),
            error: LspError::NoServer,
        });
        assert!(app.lsp_contexts[&card].context.notice.is_some());
    }

    /// Progress is transient by definition, so it occupies the notice slot and
    /// is cleared on `done` rather than accumulating a log nobody dismisses.
    #[test]
    fn progress_occupies_the_notice_slot_until_it_is_done() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        app.lsp_documents.insert(LspDocumentId(1), card);

        app.apply_lsp_event(LspEvent::Progress {
            doc: LspDocumentId(1),
            progress: crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(40),
                done: false,
            },
        });
        assert_eq!(
            app.lsp_contexts[&card].context.notice.as_deref(),
            Some("Indexing (40%)")
        );

        app.apply_lsp_event(LspEvent::Progress {
            doc: LspDocumentId(1),
            progress: crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(100),
                done: true,
            },
        });
        assert!(app.lsp_contexts[&card].context.notice.is_none());
    }

    /// A location arrives absolute and the card's file list is
    /// worktree-relative. A target the worktree does not contain has no row to
    /// open, and must not be guessed at.
    #[test]
    fn a_jump_outside_the_worktree_opens_nothing() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");

        let _ = app.update_lsp(InspectorAction::JumpToLspLocation {
            card,
            path: PathBuf::from("/elsewhere/other.rs"),
            at: LspPosition::new(1, 0),
            coordinates: LspCoordinates::Scalar,
        });

        assert_eq!(
            notice(&app),
            Some("that definition is outside this worktree")
        );
        assert!(!app.lsp_pending_jumps.contains_key(&card));
    }

    /// A jump into a file the card is not showing parks until that file is the
    /// open buffer: a column is only placeable against the text it indexes.
    #[test]
    fn a_jump_into_another_file_parks_until_it_is_open() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");

        let task = app.update_lsp(InspectorAction::JumpToLspLocation {
            card,
            path: PathBuf::from("/repo/src/other.rs"),
            at: LspPosition::new(4, 2),
            coordinates: LspCoordinates::Utf16,
        });

        // Parking without asking the card to open the file is a jump that never
        // lands: that open is the only thing that releases it, and the file
        // list it addresses is keyed on the worktree-relative path.
        assert!(matches!(
            task_app_message(task),
            Message::Repository(RepositoryAction::OpenFile { card: at, ref path })
                if at == card && path == Path::new("src/other.rs")
        ));
        let parked = app
            .lsp_pending_jumps
            .get(&card)
            .expect("the jump is parked on the open");
        // Parked absolute, because that is the side the *next* open reports and
        // the only side the two can be compared on.
        assert_eq!(parked.path, PathBuf::from("/repo/src/other.rs"));
        assert_eq!(parked.at, LspPosition::new(4, 2));
        assert_eq!(parked.coordinates, LspCoordinates::Utf16);
        assert_eq!(notice(&app), None);
    }

    /// Who holds the keyboard once a task's widget operations have run.
    ///
    /// Focus arrives as a widget operation rather than a message, so it is
    /// read the way the runtime reads one: run the task's operations over the
    /// tree the frame actually built, then ask that tree who holds focus.
    fn focused(app: &Cagents, task: Task<Message>) -> Option<widget::Id> {
        use cosmic::iced::advanced::layout;
        use cosmic::iced::advanced::widget::Tree;
        use cosmic::iced::advanced::widget::{Operation, operation};
        use cosmic::iced::{Size, advanced::Layout};

        let renderer = headless_renderer();
        let mut element: cosmic::Element<'_, Message> = cosmic::Application::view(app);
        let mut tree = Tree::new(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
        );

        // An empty task is not an error here: it is precisely the answer a
        // jump that forgot to focus gives, and the assertion that reads this
        // says so far better than an unwrap would.
        if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
            let executor = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("executor");
            while let Some(action) = executor.block_on(stream.next()) {
                let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
                    continue;
                };
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation,
                );
            }
        }

        // `find_focused` reports an `Id`; `operate` takes an operation over
        // `()`, so the answer comes back through a cell.
        let found: Arc<std::sync::Mutex<Option<widget::Id>>> = Default::default();
        let sink = Arc::clone(&found);
        let mut probe = operation::map(operation::focusable::find_focused(), move |id| {
            *sink.lock().expect("the probe is not shared across a panic") = Some(id);
        });
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        let _ = probe.finish();
        let answer = found.lock().expect("the probe survived").clone();
        answer
    }

    /// A jump that moves the caret must also hand the editor the keyboard.
    ///
    /// An unfocused `text_editor` draws no caret at all, so without this the
    /// application answers a click on a result with a frame indistinguishable
    /// from the one the user clicked in - measured live before this was wired:
    /// frame differencing over the caret's blink found a 2px column with the
    /// editor focused and zero pixel delta after a jump.
    #[test]
    fn a_jump_gives_the_editor_the_keyboard() {
        let (mut app, card) = editing_repository_app();

        // Into the buffer that is already open, which is the one jump that
        // needs no running server to place its caret.
        lsp_state(&mut app, card).path = Some(PathBuf::from("/repo/notes.txt"));
        let task = app.update_lsp(InspectorAction::JumpToLspLocation {
            card,
            path: PathBuf::from("/repo/notes.txt"),
            at: LspPosition::new(0, 0),
            coordinates: LspCoordinates::Scalar,
        });
        assert_eq!(
            focused(&app, task),
            Some(repository_widget::editor_focus_id(card)),
            "the jump left the editor unfocused, so its caret is invisible"
        );
    }

    /// The same, for the jump that has to open a file first.
    ///
    /// Go to definition and Find references usually land in a *different*
    /// file, so the caret cannot be placed when the answer arrives: the jump
    /// is parked, the file is opened, and the open answers it. That second
    /// path reaches the caret through `flush_pending_lsp_jump` rather than
    /// directly, and has to hand over the keyboard just the same - a caret
    /// placed in a buffer nobody focused is a caret nobody can see.
    #[test]
    fn a_jump_that_opens_a_file_gives_the_editor_the_keyboard_too() {
        let (mut app, card) = editing_repository_app();

        // Parked the way `jump_to_lsp_location` parks one, on the path the
        // card is about to open.
        let path = PathBuf::from("/repo/notes.txt");
        lsp_state(&mut app, card).path = Some(path.clone());
        app.lsp_pending_jumps.insert(
            card,
            PendingLspJump {
                path: path.clone(),
                at: LspPosition::new(0, 0),
                coordinates: LspCoordinates::Scalar,
            },
        );

        let task = app.flush_pending_lsp_jump(card, &path);

        assert!(
            !app.lsp_pending_jumps.contains_key(&card),
            "the open answered the jump, so it must not stay parked"
        );
        assert_eq!(
            focused(&app, task),
            Some(repository_widget::editor_focus_id(card)),
            "the jump placed its caret in a buffer it never focused, so the caret is invisible"
        );
    }

    /// A definition can resolve into a file the card has no row for - a
    /// generated or ignored source inside the worktree. `OpenFile` answers
    /// those by doing nothing, so parking on that open would wait forever and
    /// leave the user with no answer at all.
    #[test]
    fn a_jump_into_a_file_the_card_cannot_open_says_so() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");

        let _ = app.update_lsp(InspectorAction::JumpToLspLocation {
            card,
            path: PathBuf::from("/repo/target/generated.rs"),
            at: LspPosition::new(1, 0),
            coordinates: LspCoordinates::Scalar,
        });

        assert_eq!(
            notice(&app),
            Some("target/generated.rs is not in this worktree's file list")
        );
        assert!(!app.lsp_pending_jumps.contains_key(&card));
    }

    /// A jump is answered by the open it asked for or by nothing. Surviving an
    /// unrelated open would let the caret be yanked minutes later, when the
    /// user opens that file themselves, against text that has since changed.
    #[test]
    fn a_parked_jump_does_not_survive_a_different_file_opening() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        app.lsp_pending_jumps.insert(
            card,
            PendingLspJump {
                path: PathBuf::from("/repo/src/other.rs"),
                at: LspPosition::new(4, 2),
                coordinates: LspCoordinates::Scalar,
            },
        );

        // The flush is driven directly because these fixtures configure no
        // server, so `open_lsp_document` short-circuits into the disabled
        // status before reaching it - the half that
        // `a_file_with_no_server_reports_disabled_and_clears_a_parked_jump`
        // already covers.
        let _ = app.flush_pending_lsp_jump(card, Path::new("/repo/src/main.rs"));

        assert!(!app.lsp_pending_jumps.contains_key(&card));
    }

    /// The notice slot has two independent producers. An indexing report
    /// arriving a second later must not erase the one message telling the user
    /// their hover, locations and completions were thrown away.
    #[test]
    fn progress_does_not_erase_a_failure_the_user_has_not_read() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        app.lsp_documents.insert(LspDocumentId(1), card);

        app.apply_lsp_event(LspEvent::DocumentDesynced {
            doc: LspDocumentId(1),
            version: 3,
            error: LspError::NoServer,
        });
        let failure = app.lsp_contexts[&card]
            .context
            .notice
            .clone()
            .expect("the desync is reported");

        for done in [false, true] {
            app.apply_lsp_event(LspEvent::Progress {
                doc: LspDocumentId(1),
                progress: crate::lsp::LspProgress {
                    token: "indexing".into(),
                    title: Some("Indexing".into()),
                    message: None,
                    percentage: None,
                    done,
                },
            });
            assert_eq!(
                app.lsp_contexts[&card].context.notice.as_deref(),
                Some(failure.as_str()),
                "progress with done={done} replaced the failure"
            );
        }

        // Dismissing is the user saying they have read it; only then may
        // progress have the slot.
        let _ = app.update_lsp(InspectorAction::DismissLspResults(card));
        app.apply_lsp_event(LspEvent::Progress {
            doc: LspDocumentId(1),
            progress: crate::lsp::LspProgress {
                token: "indexing".into(),
                title: Some("Indexing".into()),
                message: None,
                percentage: Some(10),
                done: false,
            },
        });
        assert_eq!(
            app.lsp_contexts[&card].context.notice.as_deref(),
            Some("Indexing (10%)")
        );
    }

    /// Opening a file no configured server claims is a supported configuration,
    /// not a failure, and it must not leave an earlier file's jump parked.
    #[test]
    fn a_file_with_no_server_reports_disabled_and_clears_a_parked_jump() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        app.lsp_pending_jumps.insert(
            card,
            PendingLspJump {
                path: PathBuf::from("/repo/gone.rs"),
                at: LspPosition::new(1, 0),
                coordinates: LspCoordinates::Scalar,
            },
        );

        let _ = app.open_lsp_document(
            card,
            Path::new("/repo/notes.unknownext"),
            Path::new("/repo"),
            1,
            "text\n",
        );

        let state = &app.lsp_contexts[&card];
        assert_eq!(state.context.status, Some(LspStatus::Disabled));
        assert!(state.document.is_none());
        // Recorded even with no server: the file *is* open, and a later jump
        // compares against this path to decide whether it must open anything.
        assert_eq!(
            state.path.as_deref(),
            Some(Path::new("/repo/notes.unknownext"))
        );
        assert!(!app.lsp_pending_jumps.contains_key(&card));
    }

    /// Closing the buffer must retract the document from the routing map, or a
    /// later document reusing the card would receive the dead one's events.
    #[test]
    fn closing_a_document_retracts_it_from_the_routing_map() {
        let mut app = lsp_test_app();
        let card = repository_card(&mut app, "/repo");
        lsp_state(&mut app, card).document = Some(LspDocumentId(1));
        lsp_state(&mut app, card).path = Some(PathBuf::from("/repo/a.rs"));
        app.lsp_documents.insert(LspDocumentId(1), card);

        app.close_lsp_document(card);

        assert!(app.lsp_documents.is_empty());
        let state = &app.lsp_contexts[&card];
        assert!(state.document.is_none());
        assert!(state.path.is_none());
        assert_eq!(state.diagnostics_version, None);
    }

    /// Drive the close animation to completion through the production path.
    /// Tests have no frame clock, so without this every close is still
    /// mid-animation and the overlay stays mounted by design.
    fn settle_context_inspector_transition(app: &mut Cagents) {
        let _ =
            app.advance_context_inspector_transition(Instant::now() + DIALOG_TRANSITION_DURATION);
    }

    /// The shell re-runs one `RedrawRequested` through update and layout while
    /// any pass publishes a message, rebuilding the view in between, and warns
    /// once a frame has cost three such passes. Animated geometry therefore has
    /// to be a function of the frame: when it was a function of the wall clock
    /// the inspector was a few hundredths of a pixel wider on every pass, the
    /// workspace strip measured a new viewport width each time and reported it,
    /// and an animating frame never reached a fixed point.
    #[test]
    fn inspector_progress_is_the_same_for_every_pass_over_one_frame() {
        let mut app = context_inspector_test_app(true);
        let _ = app.update(Message::ToggleContextInspector);
        assert!(
            app.context_inspector_transition.is_some(),
            "the toggle has to start a transition for this to measure anything",
        );

        let first = app.context_inspector_progress();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            app.context_inspector_progress(),
            first,
            "a second pass over the same frame has to measure the same progress",
        );

        let frame = Instant::now() + DIALOG_TRANSITION_DURATION / 2;
        let _ = app.frame_clock.elapsed(frame);
        let advanced = app.context_inspector_progress();
        let target = app.context_inspector_transition.unwrap().to;
        assert!(
            (advanced - target).abs() < (first - target).abs(),
            "a later frame does advance the animation towards {target}: {first} then {advanced}",
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            app.context_inspector_progress(),
            advanced,
            "and every pass over that frame measures it identically",
        );
    }

    pub(super) fn context_inspector_test_app(with_focused_card: bool) -> Cagents {
        let card = with_focused_card.then(terminal_card_state);
        let card_id = card.as_ref().map(|card| card.id);
        let durable = Arc::new(WorkspaceState {
            cards: card.into_iter().collect(),
            focused: card_id,
            ..WorkspaceState::default()
        });
        let workspace = Cagents::rebuild_workspace(&durable);
        let settings = AppSettings::default();
        let (terminal_wake_tx, terminal_wake_rx) = async_channel::unbounded();
        let mut core = Core::default();
        core.window.show_headerbar = false;
        core.window.border_padding = Some(0);

        Cagents {
            workspace,
            durable,
            cards: card_id
                .map(|card_id| HashMap::from([(card_id, CardRuntime::Loading)]))
                .unwrap_or_default(),
            card_chrome: HashMap::new(),
            lifecycle: LifecycleManager::new(LifecycleBudget {
                max_hot_cards: settings.max_hot_cards,
                offscreen_grace: Duration::from_millis(settings.hibernation_delay_ms),
            }),
            lsp: LspManager::new(settings.language_servers.clone()),
            lsp_documents: HashMap::new(),
            lsp_contexts: HashMap::new(),
            lsp_pending_jumps: HashMap::new(),
            next_lsp_document: 0,
            lifecycle_sweep_deadline: None,
            lifecycle_sweep_generation: 0,
            terminal_wake_tx,
            terminal_wake_rx,
            hosted_terminals_rx: async_channel::unbounded().1,
            terminal_starts: std::sync::Mutex::new(HashMap::new()),
            pending_adoptions: Vec::new(),
            restored_agent_attaches: VecDeque::new(),
            relaunched_terminals: HashSet::new(),
            terminal_crash_notices: HashMap::new(),
            workspace_model: crate::workspace_model::WorkspaceModel::default(),
            workspace_model_opens: HashSet::new(),
            workspace_model_rx: async_channel::unbounded().1,
            // Unbounded and unread: a fixture publishes into a queue nobody
            // drains, which is what keeps a test off the daemon entirely.
            workspace_model_tx: async_channel::unbounded().0,
            activation_rx: async_channel::unbounded().1,
            notification_ui: notification_ui::State::default(),
            session: None,
            save_health: None,
            settings_draft: SettingsDraft::from_settings(&settings),
            settings_base: settings.clone(),
            settings_save_generation: 0,
            settings_ack_generation: 0,
            settings_reset_generation: None,
            settings_save_lane: Arc::new(std::sync::Mutex::new(None)),
            update_prepared: false,
            update_flush_pending: false,
            update_generation: 0,
            update_committed: false,
            exit: None,
            document_tone_stale: false,
            settings,
            #[cfg(feature = "integrated-browser")]
            browser_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_import: browser_import_ui::State::default(),
            terminal_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_factory: None,
            #[cfg(feature = "integrated-browser")]
            browser_parent_lifetime: None,
            window_scale_factor: 1.0,
            window_width: 1_600.0,
            window_height: 900.0,
            core,
            settings_path: None,
            uncertain_terminal_proofs: HashMap::new(),
            terminal_recovery_attempts: HashSet::new(),
            pending_terminal_handoffs: HashMap::new(),
            terminal_handoff_commit_inflight: false,
            settings_write_blocked: true,
            notices: NoticeLog::default(),
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, Vec::new()),
            sidebar_state: SidebarState::default(),
            project_ui: project_ui::State::default(),
            channel_ui: channel_ui::State::default(),
            sidebar_selection_ui: sidebar_selection_ui::State::default(),
            chat_ui: chat_ui::State::default(),
            update_ui: update_ui::State::default(),
            chrome_ui: chrome_ui::State::default(),
            pull_request_inspector: pull_request_ui::State::default(),
            branch_ui: branch_ui::State::default(),
            history_ui: history_ui::State::default(),
            refresh_ui: refresh_ui::State::default(),
            account_ui: account_ui::State::default(),
            automation_ui: crate::automation_ui::State::default(),
            active_workspace: None,
            worktree_focus: HashMap::new(),
            channel_offsets: HashMap::new(),
            worktree_offsets: HashMap::new(),
            worktree_owners: HashMap::new(),
            known_worktree_roots: HashSet::new(),
            workspace_view_generation: 0,
            workspace_scroll_id: widget::Id::new("workspace-strip-0"),
            github_availability: HashMap::new(),
            review_repositories: HashMap::new(),
            repository_generations: HashMap::new(),
            repository_pr_cancellations: HashMap::new(),
            repository_load_sequence: 0,
            worktree_dialog: None,
            settings_reset_confirm: false,
            pull_request_dialog: None,
            pull_request_dialog_generation: 0,
            dirty_repository_close: None,
            terminal_close: None,
            queued_card_closes: VecDeque::new(),
            terminal_close_attempts: HashSet::new(),
            terminal_close_generation: 0,
            closed_terminal_cards: HashSet::new(),
            worktree_operations: HashMap::new(),
            pending_panel_open: None,
            deferred_navigation: VecDeque::new(),
            frame_clock: FrameClock::default(),
            latency: crate::performance::InteractionLatency::default(),
            latency_focus: None,
            #[cfg(feature = "latency-harness")]
            latency_fixture: None,
            #[cfg(feature = "latency-harness")]
            latency_fixture_started: false,
            context_inspector_transition: None,
            inspector_diagnostics_card: None,
            context_inspector_open: true,
            context_tab: ContextTab::default(),
            context_inspector_overlay_active: false,
            context_inspector_restore_focus: Some(context_inspector_toggle_id()),
            settings_category: SettingsCategory::Appearance,
            settings_search: String::new(),
            tab_history: crate::tab_history::TabHistory::default(),
            workspace_history: crate::tab_history::TabHistory::default(),
            workspace_cycle_modifiers: keyboard::Modifiers::empty(),
            keyboard_modifiers: keyboard::Modifiers::empty(),
            settings_feedback: None,
            settings_pane_focused: false,
            reopen_utility: None,
            settings_pending: HashMap::new(),
            settings_generation: 0,
            settings_invalid: HashMap::new(),
            settings_save_error: None,
            status: None,
        }
    }

    #[test]
    fn pane_attention_tracks_semantic_events_and_hover_acknowledges_only_its_pane() {
        use crate::agent::{Activity, ActivityState, Provider};
        let mut app = context_inspector_test_app(true);
        app.context_inspector_open = false;
        let first = app.workspace.focused().unwrap();
        let second = terminal_card_state();
        let third = terminal_card_state();
        let (second_id, third_id) = (second.id, third.id);
        Arc::make_mut(&mut app.durable)
            .cards
            .extend([second, third]);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(1_000.0);
        app.workspace.native_scroll(120.5, 1_000.0);
        let activity = |state| {
            Some(Activity {
                provider: Provider::Omp,
                state,
            })
        };
        app.observe_agent_activity_for_card(first, activity(ActivityState::Working));
        app.observe_agent_activity_for_card(first, activity(ActivityState::Idle));
        assert!(!app.sidebar_state.has_card_update(first));
        app.observe_agent_activity_for_card(second_id, activity(ActivityState::Waiting));
        app.observe_agent_activity_for_card(third_id, activity(ActivityState::Working));
        app.observe_agent_activity_for_card(third_id, activity(ActivityState::Idle));
        assert!(app.sidebar_state.has_card_update(second_id));
        assert!(app.sidebar_state.has_card_update(third_id));
        let offset = app.workspace.camera().offset();
        let _ = app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::PointerEntered(second_id),
        });
        assert_eq!(app.workspace.focused(), Some(second_id));
        assert_eq!(app.workspace.camera().offset(), offset);
        assert!(!app.sidebar_state.has_card_update(second_id));
        assert!(app.sidebar_state.has_card_update(third_id));
        app.observe_agent_activity_for_card(second_id, activity(ActivityState::Waiting));
        assert!(!app.sidebar_state.has_card_update(second_id));
        app.settings.focus_follows_mouse = false;
        let _ = app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::PointerEntered(third_id),
        });
        assert_eq!(app.workspace.focused(), Some(second_id));
        assert!(app.sidebar_state.has_card_update(third_id));
    }
    #[test]
    fn an_agent_finishing_its_turn_marks_its_project_for_a_read() {
        use crate::agent::{Activity, ActivityState, Provider};
        use crate::model::{ChannelId, ProjectChannel, WorktreeId};
        use crate::sidebar::{SidebarProject, SidebarWorktree};

        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/tmp".into(),
            vec![SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/tmp".into(),
                Some("main".into()),
            )],
            Vec::new(),
        ));
        let activity = |state| {
            Some(Activity {
                provider: Provider::Claude,
                state,
            })
        };
        app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
        app.observe_agent_activity_for_card(card, activity(ActivityState::Waiting));
        assert!(!app.refresh_ui.is_stale(project));
        app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
        app.observe_agent_activity_for_card(card, activity(ActivityState::Idle));
        assert!(app.refresh_ui.is_stale(project));

        // A channel's agent works in its project without a worktree row.
        let channel_project = ProjectId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: ChannelId::new(),
                project_id: channel_project,
                title: "channel".into(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: None,
            });
        app.observe_agent_activity_for_card(card, activity(ActivityState::Working));
        app.observe_agent_activity_for_card(card, activity(ActivityState::Idle));
        assert!(app.refresh_ui.is_stale(channel_project));
    }

    /// A refresh tick reads one project at a time, only while the window is
    /// focused, and leaves a project whose checkout failed to its retry.
    #[test]
    fn explicit_picker_reads_prevent_focus_refresh_until_failure_or_cancellation() {
        for cancelled in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let repository = directory.path().canonicalize().unwrap();
            let project = ProjectId::new();
            let mut app = context_inspector_test_app(false);
            app.sidebar_model.projects.push(restored_sidebar_project(
                project,
                "Existing",
                &repository,
            ));
            app.sidebar_state.apply(&SidebarAction::OpenRepository {
                project_id: project,
                repository: repository.clone(),
            });
            app.core.set_main_window_id(Some(window::Id::RESERVED));
            let picked = ProjectId::new();
            drop(app.repository_load_task(repository.clone(), Some(picked), true));
            let generation = app.repository_generations[&picked];
            drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
                window::Id::RESERVED,
                true,
            ))));
            assert!(
                !app.repository_generations.contains_key(&project),
                "focus must not outrank the unresolved picker"
            );
            drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
            assert!(
                !app.repository_generations.contains_key(&project),
                "periodic polling must also defer during a slow picker read"
            );
            if cancelled {
                app.refresh_ui.projects[&picked].cancel();
            } else {
                drop(app.update(Message::RepositoryLoaded {
                    open_cards: true,
                    project_id: picked,
                    generation,
                    result: Err("picker failed".into()),
                }));
            }
            drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
            assert!(
                app.repository_generations.contains_key(&project),
                "normal refresh must resume after the open settles"
            );
            assert!(!app.refresh_ui.projects.contains_key(&picked));
        }
    }

    #[test]
    fn refresh_ticks_read_one_reachable_project_at_a_time_while_focused() {
        use crate::model::WorktreeId;
        use crate::sidebar::{SidebarProject, SidebarWorktree};

        let mut app = context_inspector_test_app(false);
        let projects = [ProjectId::new(), ProjectId::new(), ProjectId::new()];
        for (index, &project) in projects.iter().enumerate() {
            let root = PathBuf::from(format!("/nonexistent/project-{index}"));
            let mut row = SidebarProject::new(
                project,
                RepositoryId::new(),
                format!("project {index}"),
                root.clone(),
                vec![SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    root,
                    Some("main".into()),
                )],
                Vec::new(),
            );
            if index == 0 {
                row.unreachable = Some("fixture".into());
            }
            app.sidebar_model.projects.push(row);
        }
        let tick =
            |app: &mut Cagents| drop(app.update(Message::RefreshUi(refresh_ui::Action::Tick)));
        tick(&mut app);
        assert!(
            app.refresh_ui.projects.is_empty(),
            "unfocused windows read nothing"
        );

        app.core.set_main_window_id(Some(window::Id::RESERVED));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            true,
        ))));
        let reading = app.refresh_ui.projects.keys().copied().collect::<Vec<_>>();
        assert_eq!(reading.len(), 1);
        assert_ne!(reading[0], projects[0]);
        tick(&mut app);
        assert_eq!(
            app.refresh_ui.projects.keys().copied().collect::<Vec<_>>(),
            reading
        );
        // Once that read lands, the next tick moves on to the other one.
        app.refresh_ui.projects.clear();
        tick(&mut app);
        let next = app.refresh_ui.projects.keys().copied().collect::<Vec<_>>();
        assert_eq!(next.len(), 1);
        assert!(!next.contains(&reading[0]) && !next.contains(&projects[0]));
    }

    #[test]
    fn restored_duplicate_projects_preserve_worktrees_and_writer_generations() {
        let directory = tempfile::tempdir().unwrap();
        let common = directory.path().join(".git");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&common).unwrap();
        std::os::unix::fs::symlink(&common, &alias).unwrap();
        let original = ProjectState {
            kind: Default::default(),
            id: ProjectId::new(),
            generation: 7,
            name: "original".into(),
            repository: common.canonicalize().unwrap(),
            existence: Existence::Open,
        };
        let duplicate = ProjectState {
            kind: Default::default(),
            id: ProjectId::new(),
            generation: 3,
            repository: alias,
            ..original.clone()
        };
        let mut projects = vec![duplicate.clone(), original.clone()];
        let mut worktrees = [original.id, duplicate.id].map(|project_id| WorktreeState {
            id: crate::model::WorktreeId::new(),
            project_id,
            generation: 9,
            path: directory.path().join(project_id.to_string()),
            branch: Some("main".into()),
            existence: Existence::Open,
        });
        let expected = worktrees.clone().map(|mut worktree| {
            worktree.project_id = original.id;
            worktree
        });
        reconcile_restored_projects(&mut projects, &mut worktrees, &[original.id]);
        assert_eq!(projects, vec![original]);
        assert_eq!(worktrees, expected);
    }

    #[test]
    fn folder_projects_participate_in_workspace_history_alongside_git_and_channels() {
        let mut app = context_inspector_test_app(false);
        let folder = ProjectId::new();
        let git = ProjectId::new();
        let worktree = crate::model::WorktreeId::new();
        let channel = crate::model::ChannelId::new();
        app.sidebar_model.projects = vec![
            SidebarProject::new(
                folder,
                RepositoryId::new(),
                "Folder".into(),
                "/folder".into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
            SidebarProject::new(
                git,
                RepositoryId::new(),
                "Git".into(),
                "/git/.git".into(),
                vec![SidebarWorktree::from_durable(
                    worktree,
                    "/git".into(),
                    Some("main".into()),
                )],
                Vec::new(),
            ),
        ];
        app.sidebar_model.projects[0]
            .channels
            .push(sidebar::SidebarChannel::new(
                channel,
                "Channel".into(),
                false,
                None,
                false,
                1,
                true,
                1,
            ));
        let destinations = sidebar::workspace_cycle(&app.sidebar_model, &app.sidebar_state);
        let kinds: Vec<_> = destinations
            .iter()
            .filter_map(WorkspaceDestination::from_action)
            .collect();
        assert_eq!(
            kinds,
            vec![
                WorkspaceDestination::Folder(folder),
                WorkspaceDestination::Channel(channel),
                WorkspaceDestination::Worktree(worktree)
            ]
        );
        app.workspace_history
            .observe(Some(WorkspaceDestination::Channel(channel)));
        app.workspace_history
            .observe(Some(WorkspaceDestination::Folder(folder)));
        app.workspace_history
            .observe(Some(WorkspaceDestination::Worktree(worktree)));
        app.active_workspace = Some("/git".into());
        let previous = app
            .next_workspace(crate::workspace::FocusDirection::Right)
            .unwrap();
        assert_eq!(
            WorkspaceDestination::from_action(&previous),
            Some(WorkspaceDestination::Folder(folder))
        );
        app.active_workspace = Some("/folder".into());
        assert_eq!(
            app.active_workspace_destination(),
            Some(WorkspaceDestination::Folder(folder))
        );
        app.sidebar_model.projects.remove(0);
        assert!(
            app.next_workspace(crate::workspace::FocusDirection::Right)
                .is_some()
        );
    }

    #[test]
    fn folder_projects_select_a_directory_scope_and_keep_their_mode_on_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let loaded = load_project(&root, None, CancellationToken::default(), None).unwrap();
        assert_eq!(loaded.project.kind, ProjectKind::Folder);
        assert!(loaded.repository.is_none());
        assert!(loaded.worktrees.is_empty());
        let project = loaded.project.id;
        let mut app = context_inspector_test_app(false);
        app.repository_generations.insert(project, 1);
        drop(app.update(repository_load_result(project, Ok(loaded))));
        drop(app.select_worktree(root.clone(), false));
        assert_eq!(app.active_workspace.as_ref(), Some(&root));
        assert!(app.active_folder_scope());
        assert_eq!(app.context_tab, ContextTab::Files);
        let key = app.sidebar_model.worktree_at(&root).unwrap();
        assert!(app.sidebar_model.contains_worktree(&key));
        assert_eq!(key.project_id, project);
        assert!(
            app.repository_runtime(RepositoryView {
                selected_file: None,
                project,
                repository: root.clone(),
                worktree: root.clone(),
            })
            .is_directory_only()
        );
        drop(app.update(Message::SelectContextTab(ContextTab::Changes)));
        assert_eq!(app.context_tab, ContextTab::Files);

        std::fs::create_dir(root.join(".git")).unwrap();
        let refreshed = load_project(
            &root,
            Some(project),
            CancellationToken::default(),
            Some(ProjectKind::Folder),
        )
        .unwrap();
        assert_eq!(refreshed.project.kind, ProjectKind::Folder);
        assert_eq!(refreshed.project.repository, root);
        assert!(load_project(&root, None, CancellationToken::default(), None).is_err());
    }

    #[test]
    fn repeated_repository_add_reconciles_aliases_and_concurrent_linked_loads() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let linked = directory.path().join("linked");
        let alias = directory.path().join("alias");
        let git = |cwd: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(directory.path(), &["init", "-b", "main", "checkout"]);
        git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        std::fs::create_dir(root.join("subdir")).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();

        // These independent requests all finish before any result is accepted,
        // as when two picker loads race while another add is still in flight.
        let first = load_repository(&root, None, CancellationToken::default()).unwrap();
        let aliased =
            load_repository(&alias.join("subdir"), None, CancellationToken::default()).unwrap();
        let linked_load = load_repository(&linked, None, CancellationToken::default()).unwrap();
        assert_eq!(
            first.repository.as_ref().unwrap().id,
            aliased.repository.as_ref().unwrap().id
        );
        assert_eq!(
            first.repository.as_ref().unwrap().id,
            linked_load.repository.as_ref().unwrap().id
        );
        let canonical_root = root.canonicalize().unwrap();
        let canonical_linked = linked.canonicalize().unwrap();
        assert_eq!(
            aliased.repository.as_ref().unwrap().worktree_root.as_ref(),
            Some(&canonical_root)
        );
        assert_eq!(
            linked_load
                .repository
                .as_ref()
                .unwrap()
                .worktree_root
                .as_ref(),
            Some(&canonical_linked)
        );

        let mut app = context_inspector_test_app(false);
        let cards = [&canonical_root, &canonical_linked]
            .into_iter()
            .map(|path| {
                let mut card = terminal_card_state();
                if let CardKind::Terminal(session) = &mut card.kind {
                    session.launch.working_directory = path.clone();
                    session.continuity = TerminalContinuity::RelaunchAvailable {
                        recipe: session.launch.clone(),
                        last_exit: None,
                    };
                }
                app.cards.insert(card.id, CardRuntime::Loading);
                card
            })
            .collect::<Vec<_>>();
        let card_ids = cards.iter().map(|card| card.id).collect::<Vec<_>>();
        Arc::make_mut(&mut app.durable).cards = cards;
        let first_id = first.project.id;
        for loaded in [&first, &aliased, &linked_load] {
            app.repository_generations.insert(loaded.project.id, 1);
        }
        let complete = |loaded: LoadedRepository, generation| Message::RepositoryLoaded {
            open_cards: true,
            project_id: loaded.project.id,
            generation,
            result: Ok(Box::new(loaded)),
        };
        drop(app.update(complete(first.clone(), 1)));
        app.sidebar_model.projects[0].github_status = "newer pending refresh".into();
        let expected_project = app.sidebar_model.projects[0].clone();
        for loaded in [aliased, linked_load] {
            drop(app.update(complete(loaded, 1)));
            assert_eq!(app.sidebar_model.projects, vec![expected_project.clone()]);
            assert_eq!(
                app.durable
                    .cards
                    .iter()
                    .map(|card| card.id)
                    .collect::<Vec<_>>(),
                card_ids
            );
        }
        assert_eq!(app.active_workspace.as_ref(), Some(&canonical_linked));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card_ids[1]]);

        // Re-adding is selection, not refresh; neither it nor a stale explicit
        // refresh may clear newer project data or change its generation fence.
        app.repository_generations.insert(first_id, 2);
        drop(app.update(Message::RepositorySelected(Ok(root))));
        assert_eq!(app.repository_generations[&first_id], 2);
        drop(app.update(complete(first, 1)));
        assert_eq!(app.sidebar_model.projects, vec![expected_project]);
        assert_eq!(app.active_workspace.as_ref(), Some(&canonical_linked));
    }

    #[test]
    fn a_later_launch_can_request_focus_without_a_mapped_window() {
        let mut app = context_inspector_test_app(false);
        let _ = app.update(Message::ActivateUi { token: None });
        let _ = app.update(Message::ActivateUi {
            token: Some("wayland-token".into()),
        });
    }

    #[test]
    fn restored_terminal_retains_its_fraction_when_a_second_pane_opens() {
        let mut card = terminal_card_state();
        card.width_fraction = 0.125;
        let card_id = card.id;
        let durable = WorkspaceState {
            cards: vec![card],
            focused: Some(card_id),
            ..WorkspaceState::default()
        };

        let mut workspace = Cagents::rebuild_workspace(&durable);
        workspace.resize_viewport(3_840.0);

        assert_eq!(
            workspace.card(card_id).unwrap().width,
            CardWidth::Fraction(0.125)
        );
        assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 3_840.0);
        workspace.resize_viewport(800.0);
        assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 800.0);
        workspace.resize_viewport(3_840.0);
        assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 3_840.0);
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Second", CardWidth::Fraction(0.5)).unwrap(),
                1,
                false,
            )
            .unwrap();
        assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 768.0);
        assert_eq!(
            workspace.card(card_id).unwrap().width,
            CardWidth::Fraction(0.125)
        );
    }
    #[test]

    fn terminal_resize_updates_rendered_and_durable_width_within_bounds() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.workspace.resize_viewport(1_000.0);
        app.workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Second", CardWidth::Fraction(0.5)).unwrap(),
                1,
                false,
            )
            .unwrap();
        let limits = app.workspace.fractional_width_limits();

        let _ = app.update(Message::ResizeCard { card, width: 640.0 });
        assert_eq!(app.workspace.geometry().card(card).unwrap().width(), 640.0);
        assert_eq!(app.card_state(card).unwrap().width_fraction, 0.64);

        let _ = app.update(Message::ResizeCard { card, width: 10.0 });
        assert_eq!(
            app.workspace.geometry().card(card).unwrap().width(),
            limits.minimum
        );
        assert!((app.card_state(card).unwrap().width_fraction - 0.461_538_46).abs() < 0.000_001);

        let _ = app.update(Message::ResizeCard {
            card,
            width: 5_000.0,
        });
        assert_eq!(
            app.workspace.geometry().card(card).unwrap().width(),
            limits.maximum
        );
        assert_eq!(
            app.card_state(card).unwrap().width_fraction,
            limits.maximum / 1_000.0
        );
    }

    #[test]
    fn pane_width_cycle_survives_serialization_and_scope_rebuilds_for_every_kind() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let mut terminal = app.card_state(card).unwrap().kind.clone();
        if let CardKind::Terminal(session) = &mut terminal {
            session.launch.working_directory = PathBuf::from("/repo");
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
        }
        let url = Url::parse("about:blank").unwrap();
        let kinds = [
            terminal,
            CardKind::Repository(RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: PathBuf::from("/repo"),
                worktree: PathBuf::from("/repo"),
            }),
            CardKind::Browser(crate::model::BrowserSession {
                id: crate::model::BrowserId::new(),
                current_url: url.clone(),
                history: vec![url],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: crate::model::BrowserRestoreFidelity::ExactLive,
            }),
            CardKind::PullRequests(crate::model::PullRequestView {
                repository: PathBuf::from("/repo"),
                selected_number: None,
            }),
        ];
        for kind in kinds {
            app.card_state_mut(card).unwrap().kind = kind;
            app.card_state_mut(card).unwrap().width_fraction = 0.61;
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.workspace.resize_viewport(900.0);
            for fraction in [2.0 / 3.0, 1.0, 1.0 / 3.0, 0.5, 2.0 / 3.0] {
                let _ = app.update(Message::CyclePaneWidth);
                let saved = serde_json::to_vec(app.durable.as_ref()).unwrap();
                app.durable = Arc::new(serde_json::from_slice(&saved).unwrap());
                let _ = app.select_worktree(PathBuf::from("/other"), false);
                let _ = app.select_worktree(PathBuf::from("/repo"), false);
                assert_eq!(app.workspace.focused(), Some(card));
                assert_eq!(app.card_state(card).unwrap().width_fraction, fraction);
                assert_eq!(
                    app.workspace.card(card).unwrap().width,
                    CardWidth::Fraction(fraction)
                );
                assert_eq!(app.workspace.geometry().card(card).unwrap().width(), 900.0);
            }
        }
    }

    #[test]
    fn pointer_activation_keeps_the_clicked_child_keyboard_focus() {
        let mut app = context_inspector_test_app(true);
        let first = app.workspace.focused().unwrap();
        let second = WorkspaceCard::new(CardId::new(), "Editor", CardWidth::Fraction(0.5)).unwrap();
        let id = second.id;
        app.workspace.add_card(second, 1, false).unwrap();
        app.workspace.take_widget_focus_request();
        let (update, _) = update_workspace_viewport(
            &mut app.workspace,
            WorkspaceViewportEvent::PointerFocused(id),
        );
        assert_eq!(app.workspace.focused(), Some(id));
        assert_eq!(app.workspace.take_widget_focus_request(), None);
        let _ = app.apply_viewport(update, true, false);
        assert_ne!(app.workspace.focused(), Some(first));
    }

    pub(super) fn task_app_message(task: Task<Message>) -> Message {
        let mut stream = iced::runtime::task::into_stream(task).expect("task has an output");
        let action = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(stream.next())
            .expect("task produced an action");
        match action {
            iced::runtime::Action::Output(cosmic::Action::App(message)) => message,
            _ => panic!("task did not produce an application message"),
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
            selected_file: None,
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

    #[cfg(feature = "integrated-browser")]
    #[test]
    fn browser_created_before_factory_is_requeued_for_hydration() {
        let mut app = context_inspector_test_app(false);
        let _ = app.add_browser(Url::parse("https://example.com").unwrap());
        let card = app.workspace.focused().unwrap();
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Loading)));
        assert!(matches!(
            app.lifecycle.state(card),
            Some((_, Residency::Hydrating { .. }))
        ));
        app.register_deferred_browsers();
        assert!(matches!(
            app.lifecycle.state(card),
            Some((_, Residency::Hibernated))
        ));
        assert_eq!(
            app.card_state_mut(card).unwrap().residency,
            Residency::Hibernated
        );
    }

    #[cfg(feature = "integrated-browser")]
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
        assert!(can_register_deferred_browser(&state, true));

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
    fn control_tab_uses_history_and_respects_blocking_surfaces() {
        let mut app = context_inspector_test_app(false);
        let cards = vec![
            terminal_card_state(),
            terminal_card_state(),
            terminal_card_state(),
        ];
        let ids: Vec<_> = cards.iter().map(|card| card.id).collect();
        app.durable = Arc::new(WorkspaceState {
            focused: Some(ids[0]),
            cards,
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        for id in &ids {
            drop(app.update(Message::FocusCard(*id)));
        }
        let next = || Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Tab),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            },
            captured: true,
        };
        drop(app.update(next()));
        assert_eq!(app.workspace.focused(), Some(ids[1]));
        drop(app.update(next()));
        assert_eq!(app.workspace.focused(), Some(ids[0]));
        app.core.set_main_window_id(Some(window::Id::RESERVED));
        drop(app.update(Message::RefreshUi(refresh_ui::Action::Focused(
            window::Id::RESERVED,
            false,
        ))));
        drop(app.update(next()));
        assert_eq!(app.workspace.focused(), Some(ids[2]));
        app.settings_reset_confirm = true;
        drop(app.update(next()));
        assert_eq!(app.workspace.focused(), Some(ids[2]));
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
    fn default_pane_preferences_update_independently_and_reject_invalid_choices() {
        let settings = AppSettings::default();
        for field in [
            SettingsField::DefaultWorktreePane,
            SettingsField::DefaultChannelPane,
        ] {
            assert_eq!(
                settings_update(&settings, field, "native_chat"),
                SettingsDecision::Unchanged
            );
            let SettingsDecision::Persist(updated) = settings_update(&settings, field, "terminal")
            else {
                panic!("changing the default pane must request persistence");
            };
            let mut draft = SettingsDraft::from_settings(&settings);
            draft.set(field, "terminal".into());
            assert_eq!(draft, SettingsDraft::from_settings(&updated));
            if field == SettingsField::DefaultWorktreePane {
                assert_eq!(
                    updated.default_worktree_pane,
                    settings::DefaultPane::Terminal
                );
                assert_eq!(
                    updated.default_channel_pane,
                    settings::DefaultPane::NativeChat
                );
            } else {
                assert_eq!(
                    updated.default_worktree_pane,
                    settings::DefaultPane::NativeChat
                );
                assert_eq!(
                    updated.default_channel_pane,
                    settings::DefaultPane::Terminal
                );
            }
            assert!(matches!(
                settings_update(&updated, field, "unknown"),
                SettingsDecision::Invalid(_)
            ));
            draft.set(field, "unknown".into());
            assert_eq!(draft, SettingsDraft::from_settings(&updated));
        }
    }

    #[test]
    fn rapid_interface_scale_changes_use_the_last_preference_before_cosmic_catches_up() {
        let mut app = context_inspector_test_app(true);
        let original_width = app.window_width;
        // Deliberately do not execute the returned COSMIC actions: input can
        // arrive before the runtime has applied the previous scale action.
        let _ = app.apply_setting_input(SettingsField::UiScale, "1.25".into());
        assert!(app.context_inspector_is_overlay());
        let _ = app.apply_setting_input(SettingsField::UiScale, "1.5".into());
        assert!((app.window_width - original_width / 1.5).abs() < 0.01);
        let _ = app.apply_setting_input(SettingsField::UiScale, "NaN".into());
        assert!((app.window_width - original_width / 1.5).abs() < 0.01);
        assert_eq!(app.settings.ui_scale, 1.5);
        let _ = app.apply_setting_input(SettingsField::UiScale, "1.0".into());
        assert!((app.window_width - original_width).abs() < 0.01);
        assert!(!app.context_inspector_is_overlay());
    }

    #[test]
    fn settings_edits_report_field_validation_before_persistence() {
        let settings = AppSettings::default();
        assert!(matches!(
            settings_update(&settings, SettingsField::MaxHotCards, "0"),
            SettingsDecision::Invalid(message)
                if message == "Maximum hot columns must be a whole number, at least 1."
        ));
        assert!(matches!(
            settings_update(&settings, SettingsField::ManagedWorktreeDirectory, " "),
            SettingsDecision::Invalid(_)
        ));
        assert!(matches!(
            settings_update(&settings, SettingsField::DefaultColumnFraction, "not a number"),
            SettingsDecision::Invalid(message) if message.contains("number")
        ));
    }

    #[test]
    fn refused_values_are_explained_with_the_label_and_range_the_pane_shows() {
        let settings = AppSettings::default();
        let refusal = |field, value: &str| match settings_update(&settings, field, value) {
            SettingsDecision::Invalid(message) => message,
            decision => panic!("{value:?} was not refused: {decision:?}"),
        };
        // Out of range and unparsable read the same: both say what fits.
        assert_eq!(
            refusal(SettingsField::TerminalScrollSize, "11"),
            "Scroll size must be 1–10 lines."
        );
        assert_eq!(
            refusal(SettingsField::TerminalScrollSize, "lots"),
            "Scroll size must be 1–10 lines."
        );
        assert_eq!(
            refusal(SettingsField::TerminalScrollbackLines, "1000001"),
            "Scrollback must be 0–1,000,000 lines."
        );
        assert_eq!(
            refusal(SettingsField::TerminalFontFamily, "F".repeat(129).as_str()),
            "Font family must be at most 128 bytes."
        );
        assert_eq!(
            refusal(SettingsField::ManagedWorktreeDirectory, " "),
            "Enter a directory for managed worktrees."
        );
        for message in [
            refusal(SettingsField::MaxHotCards, "0"),
            refusal(SettingsField::HibernationDelayMs, "-1"),
            refusal(SettingsField::ShortcutNewTerminal, "Ctrl+"),
        ] {
            assert!(!message.contains('_'), "{message} quotes a settings key");
            assert!(message.ends_with('.'), "{message} is not a sentence");
        }
    }

    #[test]
    fn a_shortcut_clash_names_the_chord_and_the_action_that_holds_it() {
        let settings = AppSettings::default();
        let taken = settings.keyboard_shortcuts.new_channel[0].clone();
        // The edited action sorts before its rival here, so the table reports
        // the rival as the fault and the message must look past that.
        assert_eq!(
            settings_update(
                &settings,
                SettingsField::ShortcutNewTerminal,
                &format!("Ctrl+Alt+J, {taken}")
            ),
            SettingsDecision::Invalid(format!(
                "{taken} is already the shortcut for “New channel”."
            ))
        );
        let earlier = settings.keyboard_shortcuts.new_terminal[0].clone();
        assert_eq!(
            settings_update(&settings, SettingsField::ShortcutNewChannel, &earlier),
            SettingsDecision::Invalid(format!(
                "{earlier} is already the shortcut for “New terminal”."
            ))
        );
        assert_eq!(
            settings_update(
                &settings,
                SettingsField::ShortcutNewChannel,
                "Ctrl+Alt+K, Ctrl+Alt+J, ctrl+alt+j"
            ),
            SettingsDecision::Invalid("ctrl+alt+j is listed twice.".into())
        );
    }

    #[test]
    fn shortcut_fields_follow_the_order_of_the_shortcut_table() {
        let settings = AppSettings::default();
        for (index, field) in SettingsField::SHORTCUTS.into_iter().enumerate() {
            let SettingsDecision::Persist(edited) =
                settings_update(&settings, field, "Ctrl+Alt+Shift+9")
            else {
                panic!("{field:?} did not take a free chord");
            };
            assert_eq!(
                edited.keyboard_shortcuts.actions()[index],
                &vec!["Ctrl+Alt+Shift+9".to_owned()],
                "{field:?}"
            );
            assert_eq!(field.category(), SettingsCategory::Keyboard);
        }
        assert_eq!(grouped_digits(1_000_000), "1,000,000");
        assert_eq!(grouped_digits(999), "999");
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
    fn a_sweep_is_rearmed_only_for_an_earlier_wakeup() {
        let now = Instant::now();
        let soon = now + Duration::from_secs(5);
        let later = now + Duration::from_secs(30);

        // Nothing pending: the request is the only wakeup there is.
        assert!(sweep_needs_rearming(None, later));
        // An earlier grace must pull the wakeup in, or the card that owns it
        // waits for a deadline belonging to a card that went offscreen later.
        assert!(sweep_needs_rearming(Some(later), soon));
        // A pending timer already covers a later request and re-arms itself for
        // the remainder, so a second timer would only duplicate it.
        assert!(!sweep_needs_rearming(Some(soon), later));
        assert!(!sweep_needs_rearming(Some(soon), soon));
    }

    #[test]
    fn a_restored_title_is_normalised_before_it_reaches_any_surface() {
        // The state database predates the normaliser, and a process inside a
        // card can rewrite its own title before the write, so a restart must
        // not reinstate an unbounded or reordering one.
        let repository = |title: &str| CardState {
            id: CardId::new(),
            generation: 1,
            title: title.to_owned(),
            width_fraction: 0.7,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Repository(RepositoryView {
                selected_file: None,
                project: ProjectId::new(),
                repository: PathBuf::from("/repo"),
                worktree: PathBuf::from("/repo"),
            }),
        };
        let mut state = WorkspaceState {
            cards: vec![
                repository(&"A".repeat(20_000_000)),
                repository("safe\u{202e}gnitiops"),
                repository("\u{202e}\u{7}"),
            ],
            ..WorkspaceState::default()
        };

        normalize_restored_titles(&mut state);

        assert_eq!(
            state.cards[0].title.chars().count(),
            crate::workspace::MAX_CARD_NAME_CHARS
        );
        assert_eq!(state.cards[1].title, "safegnitiops");
        // Nothing legible survived, so the card is named for its kind rather
        // than losing its header, its accessible name and the window title.
        assert_eq!(state.cards[2].title, "Repository");
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

    fn discard_key() -> ActiveWorktreeKey {
        ActiveWorktreeKey {
            project_id: ProjectId::default(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: PathBuf::from("/repo/main"),
        }
    }

    fn discard_request(confirmed: bool) -> crate::repository::DiscardRequest {
        crate::repository::DiscardRequest {
            worktree: PathBuf::from("/repo/main"),
            tracked: Vec::new(),
            untracked: vec![RepoPath::parse("notes.txt").expect("a repository path")],
            confirmed,
        }
    }

    #[test]
    fn only_the_discard_confirmations_own_answers_survive_the_modal_guard() {
        // `Message::Sidebar` is refused while any dialog is up, because the
        // pane keeps its focus behind the overlay and an accessibility Click
        // is not captured at all. The discard confirmation is mounted by
        // `dialog` rather than by `sidebar::view`, so its Cancel and Discard
        // arrive down this same route: refusing them would strand the dialog
        // with no way out.
        for action in [
            SidebarAction::ConfirmDiscard {
                key: discard_key(),
                request: discard_request(true),
            },
            SidebarAction::CancelDiscard,
        ] {
            assert!(
                answers_discard_confirmation(&action),
                "{action:?} is one of the dialog's own two answers"
            );
            assert!(
                !user_workspace_message_blocked_while_modal(&Message::Sidebar(action.clone())),
                "{action:?} must also survive the deferred-navigation queue, which drops every \
                 message it decides a modal blocks"
            );
        }

        for action in [
            SidebarAction::RequestDiscard {
                key: discard_key(),
                request: discard_request(false),
            },
            SidebarAction::Stage {
                key: discard_key(),
                selection: Vec::new(),
            },
            SidebarAction::Commit {
                key: discard_key(),
                message: "wip".into(),
            },
            SidebarAction::Refresh,
        ] {
            assert!(
                !answers_discard_confirmation(&action),
                "{action:?} is a pane command and must stand down behind a dialog"
            );
            assert!(user_workspace_message_blocked_while_modal(
                &Message::Sidebar(action)
            ));
        }
    }

    #[test]
    fn a_pending_discard_reads_as_a_modal_and_mounts_its_own_confirmation() {
        let mut app = context_inspector_test_app(false);
        assert!(!app.modal_is_open());
        assert!(app.dialog().is_none());

        app.sidebar_state.apply(&SidebarAction::RequestDiscard {
            key: discard_key(),
            request: discard_request(false),
        });
        assert!(
            app.modal_is_open(),
            "the one dialog that is on screen because it is about to destroy work must not be \
             the one dialog the pane keeps its commands live behind"
        );
        assert!(
            app.dialog().is_some(),
            "a pending discard is mounted by `dialog`; nothing else mounts it"
        );

        app.sidebar_state.apply(&SidebarAction::CancelDiscard);
        assert!(!app.modal_is_open());
        assert!(app.dialog().is_none());
    }

    /// Every accessible label the dialog the runtime mounts exposes.
    ///
    /// libcosmic's `view_main` puts `Application::dialog` in a popover popup,
    /// and a popup is an overlay, so walking the frame's widget tree never
    /// reaches it. The dialog element itself is what the runtime asks for, so
    /// that is what these tests read.
    fn dialog_labels(app: &Cagents, renderer: &cosmic::Renderer) -> Vec<String> {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let mut element = app.dialog().expect("a mounted dialog");
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1600.0, 1200.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    /// A project with one deletable worktree beside its primary one, and a
    /// session to record the deletion against, which is what the pre-flight
    /// refusal asks for first.
    fn deletable_worktree_app() -> (Cagents, tempfile::TempDir, ProjectId, WorktreeId) {
        let mut app = context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(session);
        let project = ProjectId::new();
        let worktree = WorktreeId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![
                sidebar::SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    "/repo".into(),
                    Some("main".into()),
                ),
                sidebar::SidebarWorktree::from_durable(
                    worktree,
                    "/repo/feature".into(),
                    Some("feature".into()),
                ),
            ],
            Vec::new(),
        ));
        (app, directory, project, worktree)
    }

    /// "Delete worktree…" is a request for a confirmation, and a confirmation
    /// nobody can answer is the same as no command at all: the dialog the
    /// runtime mounts has to carry the question and its confirming button,
    /// and answering it has to start the deletion.
    #[test]
    fn deleting_a_worktree_mounts_its_confirmation_and_confirming_starts_the_deletion() {
        let renderer = headless_renderer();
        let (mut app, _directory, project_id, worktree_id) = deletable_worktree_app();

        let _ = app.update(Message::Sidebar(SidebarAction::ProjectCommand {
            project_id,
            worktree_id: Some(worktree_id),
            command: sidebar::ProjectCommand::Delete,
        }));
        assert!(
            app.modal_is_open(),
            "the command opens a modal rather than deleting outright"
        );
        let labels = dialog_labels(&app, &renderer);
        assert!(
            labels.iter().any(|label| label == "Delete worktree?"),
            "the confirmation the runtime mounts asks the question: {labels:?}"
        );
        assert!(
            labels.iter().any(|label| label == "Delete worktree"),
            "with nothing blocking it, the confirming button is offered: {labels:?}"
        );

        let _ = app.update(Message::ProjectUi(project_ui::Action::ConfirmDelete));
        assert!(
            app.project_ui
                .deleting_directory(Path::new("/repo/feature")),
            "confirming hands the worktree to the deletion path"
        );
        assert!(
            !app.modal_is_open() && app.dialog().is_none(),
            "and the dialog goes once it is confirmed: the row reports the rest (#264)"
        );
        let row = app.sidebar_model.projects[0]
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .unwrap();
        assert_eq!(row.deletion, Some(sidebar::RowDeletion::Deleting));
    }

    /// Both rails speak for one worktree. The context rail's Changes and Pull
    /// Request tabs read the indicators the navigation row draws, so a glance
    /// at either rail reports the same thing.
    #[test]
    fn the_context_rail_indicates_what_the_navigation_row_indicates() {
        let mut app = context_inspector_test_app(false);
        let project = ProjectId::new();
        let worktree = WorktreeId::new();
        let path = PathBuf::from("/repo/feature");
        let mut row = sidebar::SidebarWorktree::new(
            worktree,
            path.clone(),
            Some("feature".into()),
            false,
            false,
            Some(crate::repository::DirtyState {
                staged: 1,
                unstaged: 2,
                untracked: 0,
                conflicted: 0,
                submodule: 0,
            }),
            None,
            None,
        );
        row.head_repository =
            Some(GitHubRepoKey::new("github.com", None, "example", "cagents").unwrap());
        let mut pull_request = SidebarPullRequest::new(
            project,
            42,
            "Polish the rail".into(),
            Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
            "feature".into(),
            None,
            false,
            sidebar::CheckStatus::Pending,
        );
        pull_request.head_repository = row.head_repository.clone();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![
                sidebar::SidebarWorktree::from_durable(
                    WorktreeId::new(),
                    "/repo".into(),
                    Some("main".into()),
                ),
                row,
            ],
            vec![pull_request],
        ));
        app.sidebar_state.sync_active_worktree(
            &app.sidebar_model,
            &SidebarFocus::Repository {
                worktree: path.clone(),
            },
        );

        let navigation = sidebar::worktree_indicators(&app.sidebar_model.projects[0].worktrees[1]);
        assert_eq!(
            navigation.dirty,
            Some("3 changes"),
            "the navigation row counts the worktree's changes"
        );
        assert!(
            navigation.pending_checks,
            "and reports its pull request's checks as in flight"
        );
        assert_eq!(
            app.context_rail_indicators(),
            navigation,
            "the context rail indicates exactly what the navigation row indicates"
        );
    }

    #[test]
    fn a_mutation_that_did_not_do_all_of_what_it_was_asked_says_so() {
        let path = RepoPath::parse("src/app.rs").expect("a repository path");
        assert_eq!(
            mutation_report(
                "Stage",
                Ok(MutationOutcome::Applied {
                    paths: vec![path.clone()]
                })
            ),
            None,
            "a mutation that did everything asked reports nothing: the refreshed rows are the \
             report"
        );

        let partial = mutation_report(
            "Stage",
            Ok(MutationOutcome::PartiallyApplied {
                paths: vec![path.clone()],
                failures: vec![crate::repository::MutationFailure::Rejected {
                    reason: "pathspec did not match".into(),
                }],
            }),
        )
        .expect("a partial application is a failure with a known good half");
        assert!(partial.contains("1 path"), "{partial}");
        assert!(partial.contains("pathspec did not match"), "{partial}");

        let unknown = mutation_report(
            "Stage",
            Ok(MutationOutcome::OutcomeUnknown {
                failure: crate::repository::MutationFailure::Rejected {
                    reason: "git was killed".into(),
                },
            }),
        )
        .expect("an unknown outcome is not a success");
        assert!(unknown.contains("may or may not"), "{unknown}");
    }

    #[test]
    fn only_a_created_commit_clears_the_message_the_user_typed() {
        let oid = crate::repository::Oid::parse("ab".repeat(20)).expect("a valid object id");
        assert_eq!(
            commit_report(Ok(CommitOutcome::Created {
                oid: oid.clone(),
                warnings: Vec::new()
            })),
            (None, true)
        );

        let (report, committed) = commit_report(Ok(CommitOutcome::Created {
            oid,
            warnings: vec![crate::repository::MutationFailure::Rejected {
                reason: "post-commit hook failed".into(),
            }],
        }));
        assert!(committed, "the commit exists; the hook ran after it");
        assert!(
            report.expect("a failing hook is reported").contains("hook"),
            "a warning that is not shown is a commit the user thinks was clean"
        );

        let (report, committed) = commit_report(Ok(CommitOutcome::NotCreated {
            failure: crate::repository::MutationFailure::Rejected {
                reason: "there are no staged changes to commit".into(),
            },
        }));
        assert!(
            !committed,
            "a commit that was not created must keep the message: the text is the user's work"
        );
        assert!(report.is_some());
    }

    /// An app with one project whose GitHub state lists the given open pull
    /// requests, so a sidebar row can be resolved to a review target.
    fn pull_request_test_app(numbers: &[u64]) -> (Cagents, ProjectId) {
        let mut app = context_inspector_test_app(false);
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "project".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                WorktreeId::new(),
                "/repo".into(),
                Some("main".into()),
            )],
            Vec::new(),
        ));
        app.github_availability.insert(
            project,
            GitHubAvailability::Ready {
                repository_key: GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
                pull_requests: numbers
                    .iter()
                    .map(|number| {
                        (
                            crate::pull_request_review::tests::pull_request(*number),
                            Vec::new(),
                        )
                    })
                    .collect(),
                refreshed_at: 0,
            },
        );
        (app, project)
    }

    fn pull_request_cards(app: &Cagents) -> Vec<(CardId, Option<u64>)> {
        app.durable
            .cards
            .iter()
            .filter_map(|card| match &card.kind {
                CardKind::PullRequests(view) => Some((card.id, view.selected_number)),
                _ => None,
            })
            .collect()
    }

    fn reviewed_number(app: &Cagents, card: CardId) -> Option<u64> {
        match app.cards.get(&card) {
            Some(CardRuntime::PullRequests(pane)) => pane
                .review
                .target()
                .map(|target| target.pull_request.number),
            _ => None,
        }
    }

    fn open_pull_request(project_id: ProjectId, number: u64) -> Message {
        Message::Sidebar(SidebarAction::OpenPullRequest {
            project_id,
            number,
            url: Url::parse(&format!("https://github.com/o/r/pull/{number}")).unwrap(),
        })
    }

    #[test]
    fn lone_native_chat_uses_readable_width_without_a_compact_breakpoint_jump() {
        let preferred = CardWidth::Fraction(0.35);
        assert_eq!(lone_chat_width(800.0, preferred), 800.0);
        assert!((lone_chat_width(1440.0, preferred) - 504.0).abs() < 0.001);
        assert_eq!(lone_chat_width(320.0, CardWidth::Fixed(2000.0)), 320.0);
        assert_eq!(lone_chat_width(1440.0, CardWidth::Fraction(0.125)), 480.0);
        assert!(
            (lone_chat_width(961.0, preferred) - lone_chat_width(959.0, preferred)).abs() < 3.0
        );
    }

    #[test]
    fn a_sidebar_pull_request_opens_one_review_pane_per_repository() {
        let (mut app, project) = pull_request_test_app(&[7, 8]);

        let _ = app.update(open_pull_request(project, 7));
        let cards = pull_request_cards(&app);
        assert_eq!(cards.len(), 1, "one pull request card for the repository");
        let (card, selected) = cards[0];
        assert_eq!(selected, Some(7), "the card remembers its pull request");
        assert_eq!(reviewed_number(&app, card), Some(7));
        assert_eq!(app.workspace.focused(), Some(card));

        let _ = app.update(open_pull_request(project, 8));
        let cards = pull_request_cards(&app);
        assert_eq!(cards.len(), 1, "a second pull request reuses the card");
        assert_eq!(cards[0], (card, Some(8)));
        assert_eq!(reviewed_number(&app, card), Some(8));

        // A pull request the project's GitHub state does not list cannot be
        // reviewed here and is left to the browser.
        let _ = app.update(open_pull_request(project, 9));
        assert_eq!(pull_request_cards(&app)[0], (card, Some(8)));
        assert_eq!(reviewed_number(&app, card), Some(8));
    }

    #[test]
    fn the_link_setting_keeps_pull_requests_in_the_browser() {
        let (mut app, project) = pull_request_test_app(&[7]);
        app.settings.pull_request_open_target = PullRequestOpenTarget::Link;

        let _ = app.update(open_pull_request(project, 7));

        assert!(
            pull_request_cards(&app).is_empty(),
            "a link setting opens no review pane"
        );
    }

    #[test]
    fn a_restored_pull_request_card_reopens_its_pull_request_when_github_loads() {
        let (mut app, project) = pull_request_test_app(&[7]);
        let (card, _) = app.add_pull_requests_card("/repo".into());
        if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
            pane.pending_number = Some(7);
        }
        assert_eq!(reviewed_number(&app, card), None);

        let _ = app.open_pending_pull_requests(project);

        assert_eq!(reviewed_number(&app, card), Some(7));
        assert_eq!(pull_request_cards(&app), vec![(card, Some(7))]);

        // A card restored with a pull request that has since been merged or
        // closed stops waiting: the listing is settled and will not gain it.
        let (mut app, project) = pull_request_test_app(&[7]);
        let (card, _) = app.add_pull_requests_card("/repo".into());
        if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
            pane.pending_number = Some(42);
        }
        let _ = app.open_pending_pull_requests(project);
        assert_eq!(reviewed_number(&app, card), None);
        match app.cards.get(&card) {
            Some(CardRuntime::PullRequests(pane)) => assert_eq!(pane.pending_number, None),
            _ => panic!("the pull request card is missing"),
        }
    }

    #[test]
    fn a_restored_review_can_read_its_cache_when_the_sidebar_list_is_offline() {
        let (mut app, project) = pull_request_test_app(&[]);
        let key = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        app.github_availability.insert(
            project,
            GitHubAvailability::Failed {
                detail: "offline".into(),
            },
        );
        app.review_repositories.insert(project, key.clone());
        let (card, _) = app.add_pull_requests_card("/repo".into());
        if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
            pane.pending_number = Some(42);
        }
        drop(app.open_pending_pull_requests(project));
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get(&card) else {
            panic!("missing review")
        };
        assert_eq!(
            pane.review.source().map(|source| &source.repository),
            Some(&key)
        );
        assert_eq!(
            pane.pending_number, None,
            "exact cache lookup was dispatched"
        );
        assert!(
            app.github_availability[&project].repository_key().is_none(),
            "local read identity never grants sidebar mutation authority"
        );
    }

    /// The pane's surfaces each add and remove named controls (the refresh
    /// spinner, the viewed marks, the file toolbar), which is the condition
    /// that strands a named node; see the repository surface test above.
    #[test]
    fn moving_through_the_pull_request_pane_repaints_the_window() {
        use crate::pull_request_review::{Action, tests as review};
        use crate::repository::ChangeKind;
        use cosmic::iced::advanced::widget::Tree;

        let (mut app, _) = pull_request_test_app(&[7, 8]);
        let (card, _) = app.add_pull_requests_card("/repo".into());
        let mut renderer = headless_renderer();
        let mut tree = Tree::empty();
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        let review_action = |action| Message::PullRequestReview { card, action };
        let _ = app.update(review_action(Action::Select(7)));
        present(&app, &mut renderer, &mut tree);
        assert_frame_is_faithful(&app, &tree);

        if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
            review::finish_load(
                &mut pane.review,
                review::loaded(
                    vec![
                        review::entry("src/lib.rs", ChangeKind::Modified),
                        review::entry("src/new.rs", ChangeKind::Added),
                        review::entry("docs/guide.md", ChangeKind::Deleted),
                    ],
                    vec![review::remote_file("src/lib.rs", true)],
                ),
            );
        }
        for action in [
            Action::SelectFile(RepoPath::parse("src/new.rs").unwrap()),
            Action::ToggleViewed(RepoPath::parse("src/new.rs").unwrap()),
            Action::NextFile,
            Action::PreviousFile,
            Action::Refresh,
            Action::Close,
            Action::Select(8),
        ] {
            let _ = app.update(review_action(action));
            present(&app, &mut renderer, &mut tree);
            assert_frame_is_faithful(&app, &tree);
        }
    }

    #[test]
    fn thousand_file_review_mounts_a_bounded_tree_and_draws_repeated_frames() {
        use crate::pull_request_review::{Action, tests as review};
        use cosmic::iced::advanced::widget::Tree;
        let (mut app, project) = pull_request_test_app(&[1]);
        drop(app.update(open_pull_request(project, 1)));
        let card = pull_request_cards(&app)[0].0;
        let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) else {
            panic!("review pane")
        };
        let mut snapshot = review::loaded(
            (0..1000)
                .map(|i| {
                    review::entry(
                        &format!("src/file-{i:04}.rs"),
                        crate::repository::ChangeKind::Modified,
                    )
                })
                .collect(),
            (0..100)
                .map(|i| review::remote_file(&format!("src/file-{i:04}.rs"), false))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        review::finish_load(&mut pane.review, snapshot);
        let mut renderer = headless_renderer();
        let mut tree = Tree::empty();
        fn nodes(tree: &Tree) -> usize {
            1 + tree.children.iter().map(nodes).sum::<usize>()
        }
        let mut timings = Vec::new();
        for frame in 0..12 {
            let start = Instant::now();
            present(&app, &mut renderer, &mut tree);
            timings.push(start.elapsed());
            assert!(
                nodes(&tree) < 5000,
                "offscreen file widgets must not be mounted: {}",
                nodes(&tree)
            );
            drop(app.update(Message::PullRequestReview {
                card,
                action: Action::FilePage(
                    crate::pull_request_review::file_pages::PageAction::Viewport {
                        offset: frame as f32 * 70.0 * design::DENSE_ROW_HEIGHT,
                        height: 1000.0,
                    },
                ),
            }));
        }
        timings.sort();
        eprintln!(
            "1000-file review, full layout/draw: median {:?}, max {:?}, mounted nodes {}",
            timings[6],
            timings[11],
            nodes(&tree)
        );
    }

    /// Restoring is the shell's doing, not a keystroke aimed at a dialog, so
    /// a dialog on screen when the project loads must not swallow it.
    #[test]
    fn a_restored_pull_request_reopens_even_under_a_dialog() {
        let (mut app, project) = pull_request_test_app(&[7]);
        let (card, _) = app.add_pull_requests_card("/repo".into());
        if let Some(CardRuntime::PullRequests(pane)) = app.cards.get_mut(&card) {
            pane.pending_number = Some(7);
        }
        app.settings_reset_confirm = true;
        assert!(app.modal_is_open());

        let _ = app.open_pending_pull_requests(project);

        assert_eq!(reviewed_number(&app, card), Some(7));
    }

    #[test]
    fn choosing_a_listed_pull_request_in_the_pane_opens_it() {
        let (mut app, project) = pull_request_test_app(&[7]);
        let (card, _) = app.add_pull_requests_card("/repo".into());

        let _ = app.update(Message::PullRequestReview {
            card,
            action: crate::pull_request_review::Action::Select(7),
        });
        assert_eq!(reviewed_number(&app, card), Some(7));
        assert_eq!(pull_request_cards(&app), vec![(card, Some(7))]);

        let _ = app.update(Message::PullRequestReview {
            card,
            action: crate::pull_request_review::Action::Close,
        });
        assert_eq!(reviewed_number(&app, card), None);
        assert_eq!(
            pull_request_cards(&app),
            vec![(card, None)],
            "closing the review forgets the pull request"
        );
        let _ = project;
    }
}
