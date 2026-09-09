mod activity_ui;
mod chrome_ui;
mod daemon_stop;
#[cfg(feature = "latency-harness")]
mod latency_ui;
#[cfg(test)]
mod safety_tests;
mod settings_ui;
mod terminal_close;
mod worktree_operations;
mod worktree_ui;
pub use daemon_stop::run_daemon_stop_confirmation;
use settings_ui::settings_dialog;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsStr,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, LazyLock},
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
        ProjectId, ProjectState, RepositoryView, Residency, RuntimeError, TerminalContinuity,
        TerminalExit, TerminalId, TerminalSession, WorkspaceState, WorktreeState,
    },
    repository::{
        BaseRevision, CancellationToken, ChangeArea, CommitOutcome, CommitRequest, CommitSummary,
        CreateWorktreeRequest, DiffBasis, HistoryRequest, HistoryRevision, MutationOutcome,
        PushOutcome, PushRequest, RepoPath, RepositoryId, RepositoryInfo, RepositoryService,
        RepositorySnapshot, StageRequest, StatusSnapshot, UnstageRequest, ValidatedBranchName,
        WorktreeCreationOutcome, WorktreeRecord,
    },
    repository_widget::{
        self, LspIntent, RepositoryAction, RepositoryRuntime, RepositoryRuntimeStatus,
    },
    session::{SessionRuntime, SessionSaveHealth, SessionSaveHealthEvent},
    settings::{
        self, AppSettings, AppTheme, KeyboardShortcuts, LinkOpenDestination, LinkOpenGesture,
        TerminalCursorStyle,
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
        CardWidth, ViewportUpdate, VisibilityClass, VisibilityTransition, WheelDelta, Workspace,
        WorkspaceCard, WorkspaceConfig, normalize_card_name,
    },
    workspace_widget::{self, FrameClock, PanGesture, WorkspaceViewportEvent},
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

const TOOLBAR_FOCUS_ID: &str = "cagents-workspace-toolbar";
const NOTICE_ROW_HEIGHT: f32 = design::ROW_HEIGHT;
const CARD_HEADER_HEIGHT: f32 = design::HEADER_HEIGHT;
const BROWSER_TOOLBAR_HEIGHT: f32 = design::ROW_HEIGHT;
#[cfg(feature = "integrated-browser")]
const BROWSER_PROGRESS_HEIGHT: f32 = 2.0;
const ICON_CONTROL_SIZE: f32 = design::PRIMARY_CONTROL_SIZE;
const DIALOG_TRANSITION_DURATION: Duration = Duration::from_millis(160);
const CONTENT_MAX_WIDTH: f32 = 560.0;
const STATUS_SURFACE_MAX_WIDTH: f32 = 360.0;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SaveHealthState {
    database_path: Option<PathBuf>,
    revision: u64,
    health: SessionSaveHealth,
    last_successful_flush_at_unix_ms: Option<i64>,
    retry_requested: bool,
}

impl SaveHealthState {
    fn new(database_path: PathBuf) -> Self {
        Self {
            database_path: Some(database_path),
            revision: 0,
            health: SessionSaveHealth::Healthy,
            last_successful_flush_at_unix_ms: None,
            retry_requested: false,
        }
    }

    fn writer_unavailable(database_path: Option<PathBuf>) -> Self {
        Self {
            database_path,
            revision: 1,
            health: SessionSaveHealth::WriterUnavailable,
            last_successful_flush_at_unix_ms: None,
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
        self.retry_requested = false;
        true
    }

    fn request_retry(&mut self, session: &SessionRuntime) -> bool {
        if self.retry_requested || !matches!(self.health, SessionSaveHealth::RetryExhausted { .. })
        {
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
        !matches!(self.health, SessionSaveHealth::Healthy)
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
        widget::icon::from_name(severity.icon()).size(16).into(),
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

fn transient_notice_view(notice: &TransientNotice) -> Element<'_, Message> {
    let severity = notice.severity;
    notice_surface(
        severity,
        widget::text::body(&notice.message)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
        Some(
            widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                .id(widget::Id::new("status.dismiss"))
                .name(format!("Dismiss {} notice", severity.label()))
                .tooltip("Dismiss notice")
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press(Message::DismissStatus)
                .into(),
        ),
        true,
    )
}

fn save_health_view(health: &SaveHealthState) -> Option<Element<'_, Message>> {
    if !health.is_visible() {
        return None;
    }
    let path = health.database_path.as_deref().map_or_else(
        || "unavailable (the data directory could not be resolved)".to_owned(),
        |path| path.display().to_string(),
    );
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
                    Some(
                        widget::button::standard("Retry save")
                            .id(widget::Id::new("session-save.retry"))
                            .name("Retry saving the session")
                            .on_press(Message::RetrySessionSave)
                            .into(),
                    ),
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
            Some(
                widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                    .id(widget::Id::new("session-save.acknowledge-recovery"))
                    .name("Acknowledge session save recovery")
                    .tooltip("Acknowledge recovery")
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .on_press(Message::AcknowledgeSessionRecovery)
                    .into(),
            ),
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
            widget::text::body(title)
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            widget::text::caption(detail)
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
            widget::text::caption(guidance)
                .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                .into(),
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
                    },
                    (receiver, true),
                )),
            }
        },
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

#[cfg(feature = "integrated-browser")]
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
        CardKind::Browser(session) => {
            Cow::Owned(SanitizedLocation::from(&session.current_url).into_inner())
        }
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

fn terminal_transcript_path(id: TerminalId) -> Option<PathBuf> {
    instance::project_dirs().map(|dirs| {
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
            None => project
                .worktrees
                .iter()
                .find_map(|worktree| worktree.branch.clone()),
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct LoadedRepository {
    project: ProjectState,
    repository: RepositoryInfo,
    worktrees: Vec<(WorktreeRecord, Option<StatusSnapshot>)>,
    github: GitHubAvailability,
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
    Option<SaveHealthState>,
);

#[derive(Debug, Clone)]
struct WorktreeDialog {
    project_id: ProjectId,
    repository: PathBuf,
    branch: String,
    base: String,
    destination: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestDialog {
    repository: GitHubRepoKey,
    project_id: ProjectId,
    refresh_generation: u64,
    base: String,
    head: String,
    title: String,
    body: String,
    draft: bool,
    /// An uncertain remote outcome requires reconciliation before another create.
    retry_confirmation: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inspector {
    Settings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextTab {
    #[default]
    Context,
    Changes,
    Code,
}

impl ContextTab {
    const ALL: [Self; 3] = [Self::Context, Self::Changes, Self::Code];

    const fn label(self) -> &'static str {
        match self {
            Self::Context => "Context",
            Self::Changes => "Changes",
            Self::Code => "Code",
        }
    }
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
        Self::Appearance,
        Self::Terminal,
        Self::General,
        Self::Links,
        Self::Keyboard,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::General => "Performance",
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
            | RepositoryAction::SaveFinished { .. }
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
        | Message::ResizeCard { .. }
        | Message::ResizeCardFinished(_)
        | Message::CloseCard(_)
        | Message::Pan(PanGesture::Begin { .. } | PanGesture::Update { .. })
        | Message::Wheel(_)
        | Message::OpenBrowser(_)
        | Message::NewBrowser
        | Message::RetryCard(_)
        | Message::FocusAdjacent(_)
        | Message::FocusProjectSearch
        | Message::ToggleContextInspector
        | Message::SelectContextTab(_)
        | Message::ContextInspector(_)
        | Message::DismissStatus => true,
        Message::Repository(action) => !repository_action_allowed_while_modal(action),
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
    matches!(
        message,
        Message::NewTerminal
            | Message::NewBrowser
            | Message::OpenBrowser(_)
            | Message::FocusCard(_)
            | Message::FocusAdjacent(_)
            | Message::Wheel(_)
            | Message::Pan(PanGesture::Begin { .. } | PanGesture::Update { .. })
            | Message::ToggleContextInspector
            | Message::Repository(RepositoryAction::NewTerminal { .. })
            | Message::Repository(RepositoryAction::MarkdownLink { .. })
            | Message::Sidebar(
                SidebarAction::OpenRepositoryPicker
                    | SidebarAction::OpenRepository { .. }
                    | SidebarAction::OpenWorktree { .. }
                    | SidebarAction::OpenPullRequest { .. }
                    | SidebarAction::NewWorktree { .. }
                    | SidebarAction::CreatePullRequest { .. }
                    | SidebarAction::OpenSettings
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    AppTheme,
    UiScale,
    TerminalLineHeight,
    TerminalPadding,
    TerminalCursorStyle,
    InspectorWidth,
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

/// The shortcut rows exactly as typed.
///
/// A chord list edits as one comma-separated line, and reformatting it from the
/// parsed list on every keystroke would delete a separator the moment the user
/// typed it. The parsed form lives in `AppSettings`; this is only the text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShortcutDraft {
    new_terminal: String,
    close_focused_column: String,
    search_projects: String,
    previous_column: String,
    next_column: String,
}

impl ShortcutDraft {
    fn from_shortcuts(shortcuts: &KeyboardShortcuts) -> Self {
        Self {
            new_terminal: settings::format_chords(&shortcuts.new_terminal),
            close_focused_column: settings::format_chords(&shortcuts.close_focused_column),
            search_projects: settings::format_chords(&shortcuts.search_projects),
            previous_column: settings::format_chords(&shortcuts.previous_column),
            next_column: settings::format_chords(&shortcuts.next_column),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsDraft {
    app_theme: String,
    ui_scale: String,
    terminal_line_height: String,
    terminal_padding: String,
    terminal_cursor_style: String,
    inspector_width: String,
    max_hot_cards: String,
    hibernation_delay_ms: String,
    terminal_font_size: String,
    terminal_font_family: String,
    terminal_scrollback_lines: String,
    sidebar_width: String,
    default_column_fraction: String,
    link_open_destination: LinkOpenDestination,
    link_open_gesture: LinkOpenGesture,
    keyboard_shortcuts: ShortcutDraft,
}

impl SettingsDraft {
    fn from_settings(settings: &AppSettings) -> Self {
        Self {
            app_theme: match settings.app_theme {
                AppTheme::System => "system",
                AppTheme::Dark => "dark",
                AppTheme::Light => "light",
            }
            .into(),
            ui_scale: settings.ui_scale.to_string(),
            terminal_line_height: settings.terminal_line_height.to_string(),
            terminal_padding: settings.terminal_padding.to_string(),
            terminal_cursor_style: match settings.terminal_cursor_style {
                TerminalCursorStyle::Application => "application",
                TerminalCursorStyle::Block => "block",
                TerminalCursorStyle::Beam => "beam",
                TerminalCursorStyle::Underline => "underline",
            }
            .into(),
            inspector_width: settings.inspector_width.to_string(),
            max_hot_cards: settings.max_hot_cards.to_string(),
            hibernation_delay_ms: settings.hibernation_delay_ms.to_string(),
            terminal_font_size: settings.terminal_font_size.to_string(),
            terminal_font_family: settings.terminal_font_family.clone(),
            terminal_scrollback_lines: settings.terminal_scrollback_lines.to_string(),
            sidebar_width: settings.sidebar_width.to_string(),
            default_column_fraction: settings.default_column_fraction.to_string(),
            link_open_destination: settings.link_open_destination,
            link_open_gesture: settings.link_open_gesture,
            keyboard_shortcuts: ShortcutDraft::from_shortcuts(&settings.keyboard_shortcuts),
        }
    }

    fn set(&mut self, field: SettingsField, value: String) {
        match field {
            SettingsField::AppTheme => self.app_theme = value,
            SettingsField::UiScale => self.ui_scale = value,
            SettingsField::TerminalLineHeight => self.terminal_line_height = value,
            SettingsField::TerminalPadding => self.terminal_padding = value,
            SettingsField::TerminalCursorStyle => self.terminal_cursor_style = value,
            SettingsField::InspectorWidth => self.inspector_width = value,
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
        SettingsField::AppTheme => match value {
            "system" => {
                next.app_theme = AppTheme::System;
                Ok(())
            }
            "dark" => {
                next.app_theme = AppTheme::Dark;
                Ok(())
            }
            "light" => {
                next.app_theme = AppTheme::Light;
                Ok(())
            }
            _ => Err("Choose System, Dark, or Light.".into()),
        },
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
            .map_err(|_| "Terminal padding must be a number.".to_owned()),
        SettingsField::InspectorWidth => value
            .parse::<f32>()
            .map(|value| next.inspector_width = value)
            .map_err(|_| "Context panel width must be a number.".to_owned()),
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
            next.keyboard_shortcuts.new_terminal = settings::parse_chords(value);
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
    /// A worktree snapshot for the source-control panel.
    ///
    /// `key` is the binding the request was made against. The panel rebinds
    /// whenever the focused card names a different worktree, so a result whose
    /// key is no longer the bound one is a read of a tree nobody is looking at
    /// and is dropped rather than rendered.
    SourceControlLoaded {
        key: ActiveWorktreeKey,
        result: Result<Box<RepositorySnapshot>, String>,
    },
    /// One page of the bound worktree's history.
    SourceControlHistory {
        key: ActiveWorktreeKey,
        page: u32,
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
        project_id: ProjectId,
        generation: u64,
        result: Result<Box<LoadedRepository>, String>,
    },
    Repository(RepositoryAction),
    Sidebar(SidebarAction),
    FocusCard(CardId),
    ResizeCard {
        card: CardId,
        width: f32,
    },
    ResizeCardFinished(CardId),
    CloseCard(CardId),
    WindowCloseRequested(window::Id),
    CommitClose(CardId),
    ConfirmTerminalClose,
    CancelTerminalClose,
    TerminalCloseFinished {
        card: CardId,
        terminal: TerminalId,
        proof: HostSessionProof,
        outcome: terminal_close::CloseOutcome,
    },
    SaveAndCloseRepository(CardId),
    OverwriteAndCloseRepository(CardId),
    DiscardAndCloseRepository(CardId),
    CancelRepositoryClose,
    Viewport(WorkspaceViewportEvent),
    Pan(PanGesture),
    #[cfg(feature = "latency-harness")]
    LatencyRedraw {
        generation: crate::performance::Generation,
        at: Instant,
    },
    #[cfg(feature = "latency-harness")]
    LatencyFixtureStep,
    Wheel(WheelDelta),
    Frame(Instant),
    TerminalActivityExpired {
        card: CardId,
        timer: crate::model::TerminalActivityTimer,
    },
    LifecycleSweep,
    /// A sweep timer fired, carrying the generation it was armed for; a
    /// generation that has since been superseded is discarded.
    LifecycleSweepElapsed(u64),
    #[cfg(feature = "integrated-browser")]
    InitializeBrowserFactory,
    #[cfg(feature = "integrated-browser")]
    BrowserFactoryReady(Result<BrowserFactoryBinding, String>),
    WindowScaleFactorChanged(f32),
    WindowResized(f32),
    ActivateUi {
        token: Option<String>,
    },
    OpenBrowser(Url),
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
    KeyEvent(keyboard::Event),
    FocusAdjacent(crate::workspace::FocusDirection),
    FocusProjectSearch,
    ToggleContextInspector,
    SelectContextTab(ContextTab),
    ContextInspector(InspectorAction),
    CloseInspector,
    SelectSettingsCategory(SettingsCategory),
    SettingsChanged {
        field: SettingsField,
        value: String,
    },
    /// Records typing without adopting it; see [`setting_input_on_submit`].
    SettingsDraftChanged {
        field: SettingsField,
        value: String,
    },
    ResetSettings,
    SessionSaveHealthChanged(SessionSaveHealthEvent),
    Lsp(LspEvent),
    LanguageServersReaped,
    RetrySessionSave,
    AcknowledgeSessionRecovery,
    DismissStatus,
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
    /// `None` opens the editor; `Some` opens the diff on that side. The card
    /// has no third basis, so a conflict is shown against the working tree.
    diff_staged: Option<bool>,
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
                self.focused = DirtyCloseControl::Cancel;
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
    Terminal(TerminalSurface),
    #[cfg(feature = "integrated-browser")]
    Browser(BrowserRuntime),
    Repository(Box<RepositoryRuntime>),
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
    activation_rx: async_channel::Receiver<instance::UiActivation>,
    session: Option<SessionRuntime>,
    save_health: Option<SaveHealthState>,
    settings: AppSettings,
    // These owners are declared before `core`: application Drop empties and
    // joins them explicitly, and field drop order is the final backstop.
    #[cfg(feature = "integrated-browser")]
    browser_teardowns: Vec<JoinHandle<()>>,
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
    core: Core,
    settings_path: Option<PathBuf>,
    settings_write_blocked: bool,
    sidebar_model: SidebarModel,
    sidebar_state: SidebarState,
    active_workspace: Option<PathBuf>,
    worktree_focus: HashMap<PathBuf, CardId>,
    github_availability: HashMap<ProjectId, GitHubAvailability>,
    repository_generations: HashMap<ProjectId, u64>,
    worktree_dialog: Option<WorktreeDialog>,
    pull_request_dialog: Option<PullRequestDialog>,
    /// Opening or dismissing a newer draft supersedes in-flight draft recovery.
    pull_request_dialog_generation: u64,
    dirty_repository_close: Option<DirtyRepositoryClose>,
    terminal_close: Option<terminal_close::CloseIntent>,
    terminal_close_attempts: HashSet<TerminalId>,
    closed_terminal_cards: HashSet<CardId>,
    uncertain_terminal_proofs: HashMap<CardId, HostSessionProof>,
    terminal_activity: HashMap<CardId, activity_ui::ActivityPresentation>,
    activity_origin: Instant,
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
    inspector: Option<Inspector>,
    inspector_transition: Option<InspectorTransition>,
    context_inspector_open: bool,
    context_tab: ContextTab,
    /// The overlay state whose focus and native-surface transition has already
    /// been applied. The rendered mount also depends on workspace focus, so it
    /// cannot be derived solely from inspector-open and window-resize events.
    context_inspector_overlay_active: bool,
    context_inspector_restore_focus: Option<widget::Id>,
    settings_category: SettingsCategory,
    keyboard_modifiers: keyboard::Modifiers,
    settings_draft: SettingsDraft,
    settings_feedback: Option<SettingsFeedback>,
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
        Err(_) => open_system_url_with_opener(&url, "xdg-open").await,
    }
}

async fn open_system_url_with_opener(url: &Url, opener: impl AsRef<OsStr>) -> Result<(), String> {
    let presented = SanitizedLocation::from(url);
    let result = tokio::process::Command::new(opener)
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
    fn bootstrap() -> BootstrapResult {
        let (settings, settings_path, settings_write_blocked, status) =
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
        let default_database_path = SessionRuntime::default_database_path().ok();
        let (session, restored, projects, worktrees, save_health) =
            match SessionRuntime::open_default() {
                Ok((mut session, restored)) => {
                    let (projects, worktrees) = session.take_catalog();
                    let save_health = SaveHealthState::new(session.database_path().to_owned());
                    (
                        Some(session),
                        restored,
                        projects,
                        worktrees,
                        Some(save_health),
                    )
                }
                Err(_) => {
                    let save_health = SaveHealthState::writer_unavailable(default_database_path);
                    (None, None, Vec::new(), Vec::new(), Some(save_health))
                }
            };
        let mut restored = restored.unwrap_or_default();
        normalize_restored_titles(&mut restored);
        (
            restored,
            session,
            projects,
            worktrees,
            settings,
            settings_path,
            settings_write_blocked,
            status,
            save_health,
        )
    }

    fn rebuild_workspace(state: &WorkspaceState) -> Workspace {
        let mut workspace = Workspace::new(WorkspaceConfig::default()).unwrap_or_default();
        for card in state.cards.iter().filter(|card| card.existence.is_open()) {
            let width = if card.width_fraction.is_finite() && card.width_fraction > 0.0 {
                CardWidth::Fraction(card.width_fraction.min(1.0))
            } else {
                CardWidth::Fraction(0.7)
            };
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
        &mut self,
        path: PathBuf,
        project_id: Option<ProjectId>,
        open_cards: bool,
    ) -> Task<Message> {
        let project_id = project_id.unwrap_or_default();
        let generation = self.repository_generations.entry(project_id).or_default();
        *generation = generation.wrapping_add(1);
        let generation = *generation;
        self.github_availability
            .insert(project_id, GitHubAvailability::Loading);
        if let Some(project) = self
            .sidebar_model
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.github_status = GitHubAvailability::Loading.label().into();
            project.revoke_github();
        }
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(move || load_repository(&path, Some(project_id)))
                    .await
                    .map_err(|error| format!("repository loader join failed: {error}"))
                    .and_then(|result| result)
                    .map(Box::new);
            Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result,
            }
        })
    }

    #[cfg(feature = "integrated-browser")]
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

    #[cfg(feature = "integrated-browser")]
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
                    let mut runtime = RepositoryRuntime::new(view);
                    repository_tasks.push(repository_task(runtime.initial_load(card.id)));
                    self.cards
                        .insert(card.id, CardRuntime::Repository(Box::new(runtime)));
                }
                CardKind::PullRequests(view) => {
                    self.cards
                        .insert(card.id, CardRuntime::PullRequests(view.repository));
                }
            }
        }

        // Restored integrated browsers wait for the rendering factory. A
        // browserless build leaves their durable descriptors untouched and
        // renders an explicit external-open path instead.
        Task::batch([
            self.reconcile_lifecycle(&initially_visible, &initially_prefetch),
            Task::batch(repository_tasks),
        ])
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
        let scope = if self.active_workspace.as_deref() != Some(working_directory.as_path()) {
            self.select_worktree(working_directory.clone(), false)
        } else {
            Task::none()
        };
        let (index, durable_index) = self.terminal_insertion_indices();
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.insert(durable_index, card.clone());
        durable.focused = Some(card_id);
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
            scope,
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
        let mut runtime = RepositoryRuntime::new(view);
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
        self.show_workspace_for_action();
        match self.settings.link_open_destination {
            LinkOpenDestination::IntegratedBrowser => self.add_browser(url),
            LinkOpenDestination::SystemDefault => cosmic::task::future(async move {
                Message::ExternalLinkOpened(open_system_url(url).await)
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
        if let Some(scope) = &self.active_workspace
            && let Some(focused) = self.workspace.focused()
        {
            self.worktree_focus.insert(scope.clone(), focused);
        }
        if self.durable.active_worktree != self.active_workspace
            || self.durable.worktree_focus != self.worktree_focus
        {
            let durable = Arc::make_mut(&mut self.durable);
            durable.active_worktree.clone_from(&self.active_workspace);
            durable.worktree_focus.clone_from(&self.worktree_focus);
        }
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

        let surfaces_visible = self.workspace_surfaces_visible();
        match state.kind {
            CardKind::Terminal(session) => {
                self.cards.insert(card, CardRuntime::Loading);
                self.start_terminal(
                    card,
                    session,
                    state.exposure == Exposure::Visible && surfaces_visible,
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
                let mut runtime = RepositoryRuntime::new(view);
                let task = repository_task(runtime.initial_load(card));
                self.cards
                    .insert(card, CardRuntime::Repository(Box::new(runtime)));
                task
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
        workspace_widget::scroll_to(workspace_widget::workspace_scroll_id(), offset)
            .map(cosmic::Action::App)
    }

    fn apply_viewport(
        &mut self,
        update: ViewportUpdate,
        synchronize_scroll: bool,
        additional_durable_change: bool,
    ) -> Task<Message> {
        let context_overlay_task = self.sync_context_overlay_state();
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
            tasks.push(workspace_widget::focus_card(card).map(cosmic::Action::App));
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
            if visibility == VisibilityClass::Visible {
                visible.insert(id);
            } else if visibility == VisibilityClass::Overscan {
                let browser = match self.cards.get(&id) {
                    Some(CardRuntime::Terminal(_))
                    | Some(CardRuntime::Repository(_))
                    | Some(CardRuntime::PullRequests(_)) => false,
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
                        && let Some(proof) = terminal_continuity(self.card_state(card))
                            .and_then(TerminalContinuity::live_proof)
                    {
                        self.uncertain_terminal_proofs.insert(card, proof);
                    }
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

    #[cfg(feature = "integrated-browser")]
    fn handle_browser_event(&mut self, id: CardId, event: BrowserEvent) -> Task<Message> {
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

    fn card_view<'a>(&'a self, id: CardId, focused: bool) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let card = self.card_state(id);
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
        let header = self.column_header(id, focused);
        let header: Element<'a, Message> =
            if matches!(card.map(|card| &card.kind), Some(CardKind::Terminal(_))) {
                widget::column::with_children(vec![
                    header,
                    widget::text::caption(self.terminal_activity_label(id)).into(),
                ])
                .into()
            } else {
                header
            };

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

        // A card that kept model focus under an open dialog would take the
        // typing aimed at the dialog and write it into a shell, which runs
        // what it is given. The inspector additionally hides the workspace, so
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
            widget::column::with_children(vec![
                widget::divider::horizontal::light().into(),
                toolbar.into(),
            ])
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
            Some(CardRuntime::Terminal(handle)) => TerminalWidget::new(handle.clone())
                .font_size(self.settings.terminal_font_size)
                .font_family(&self.settings.terminal_font_family)
                .line_height(self.settings.terminal_line_height)
                .padding(self.settings.terminal_padding)
                .cursor_style(self.settings.terminal_cursor_style)
                .scale_factor(effective_surface_scale(
                    self.window_scale_factor,
                    self.settings.ui_scale,
                ))
                .model_focused(card_focused)
                .on_focus(Message::FocusCard(id))
                .on_command_error(move |error| Message::TerminalError {
                    card: id,
                    message: error.to_string(),
                })
                .into(),
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
            Some(CardRuntime::PullRequests(repository)) => widget::container(
                widget::container(
                    widget::column::with_children(vec![
                        widget::icon::from_name("vcs-merge-request-symbolic")
                            .size(32)
                            .into(),
                        widget::text::title3("No pull request selected").into(),
                        widget::text::body(
                            "Choose a pull request from Projects to open it in a browser column.",
                        )
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                        widget::text::caption(repository.to_string_lossy()).into(),
                        widget::button::standard("Search projects")
                            // Disabled rather than dead: the pane does not always
                            // give the search a control to focus.
                            .on_press_maybe(
                                sidebar::search_is_reachable(
                                    &self.sidebar_model,
                                    SidebarMode::for_width(self.window_width),
                                )
                                .then_some(Message::FocusProjectSearch),
                            )
                            .into(),
                    ])
                    .align_x(Alignment::Center)
                    .spacing(spacing.space_xs),
                )
                .class(cosmic::theme::Container::Card)
                .max_width(520)
                .padding(spacing.space_m),
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
                            widget::button::suggested(retry_label)
                                .id(card_control_id(id, "retry"))
                                .name(format!("{retry_label} {}", labels.type_label))
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
                widget::container(
                    widget::column::with_children(vec![
                        widget::progress_bar::circular::Circular::new()
                            .size(24.0)
                            .into(),
                        widget::text::title3(labels.starting).into(),
                        widget::text::caption("Preparing this workspace column…")
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

        let mut rows: Vec<Element<'a, Message>> = vec![header.into()];
        if let Some(toolbar) = browser_toolbar {
            rows.push(toolbar);
        } else {
            rows.push(widget::divider::horizontal::default().into());
        }
        rows.push(content);
        let surface: Element<'a, Message> = widget::container(
            widget::column::with_children(rows)
                .spacing(0)
                .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        let edge: Element<'a, Message> =
            if card.is_some_and(|state| matches!(&state.kind, CardKind::Terminal(_))) {
                let limits = self.workspace.fractional_width_limits();
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
                widget::container(widget::divider::vertical::default())
                    .center_x(Length::Fixed(crate::splitter::WIDTH))
                    .height(Length::Fill)
                    .into()
            };
        widget::row::with_children(vec![surface, edge])
            .spacing(0)
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
        };
        if let Some(action) = action {
            self.sidebar_state.apply(&action);
        }
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
            CardKind::Browser(_) | CardKind::PullRequests(_) => SidebarFocus::None,
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
        let Some(key) = self.sidebar_state.active_worktree().cloned() else {
            self.sidebar_model.source_control = None;
            return Task::none();
        };
        self.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
        Self::source_control_snapshot_task(key)
    }

    /// Asks for the first page of history once, and only once the `History`
    /// section is showing: a page is a `git log` walk, and a section nobody
    /// opened is not worth one.
    fn load_pending_history(&mut self) -> Task<Message> {
        if !self.sidebar_state.section_expanded(PanelSection::History) {
            return Task::none();
        }
        let Some(source) = self.sidebar_model.source_control.as_mut() else {
            return Task::none();
        };
        if !source.wants_history() || source.history_requested() {
            return Task::none();
        }
        source.mark_history_requested();
        Self::source_control_history_task(source.key.clone(), 0)
    }

    /// The open repository card showing this worktree, if one is open.
    fn repository_card_for(&self, worktree: &Path) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
                    && matches!(&card.kind, CardKind::Repository(view) if view.worktree == worktree)
            })
            .map(|card| card.id)
    }

    /// Routes one panel row to the worktree's repository card.
    ///
    /// The card is opened if the worktree has none. Either way the navigation
    /// is parked in [`Self::pending_panel_open`] and performed when that card's
    /// file list lands, because the card resolves a path against the list it
    /// holds and the sidebar's rows come from a snapshot the list may predate.
    fn route_panel_open(
        &mut self,
        key: &ActiveWorktreeKey,
        path: &RepoPath,
        diff_staged: Option<bool>,
    ) -> Task<Message> {
        self.show_workspace_for_action();
        let path = path.as_path().to_path_buf();
        match self.repository_card_for(&key.canonical_path) {
            Some(card) => {
                self.pending_panel_open = Some(PendingPanelOpen {
                    card,
                    path,
                    diff_staged,
                });
                cosmic::task::message(Message::Repository(RepositoryAction::RefreshFiles { card }))
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
                    self.pending_panel_open = Some(PendingPanelOpen {
                        card,
                        path,
                        diff_staged,
                    });
                }
                task
            }
        }
    }

    /// Performs a parked navigation now that `card` has listed its files.
    ///
    /// The three actions are chained rather than batched: the card decides
    /// whether it may show a diff from the selection the open just made, so
    /// they must arrive in this order.
    /// Hands the card's buffer lifecycle to the language-server manager.
    ///
    /// The widget records what happened to the buffer; nothing in it can talk
    /// to a process. Draining here is what turns those records into protocol
    /// traffic, and it must happen after *every* repository action, because an
    /// `Open` that is never drained leaves a document the server has not been
    /// told about while the application shows results for it.
    fn drain_lsp_intents(&mut self, card: CardId) -> Task<Message> {
        let intents = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => runtime.take_lsp_intents(),
            _ => Vec::new(),
        };
        if intents.is_empty() {
            return Task::none();
        }
        let mut tasks = Vec::new();
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
                    // The widget cannot report *where* the text differs, so the
                    // manager is given an empty edit slice and sends the whole
                    // document. Every answer computed against the old text is
                    // dropped in the same breath: a completion range or a hover
                    // span that predates a keystroke describes characters the
                    // buffer no longer has.
                    self.lsp.change(doc, version, &[], &text);
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
                Ok(url) => self.open_link(url),
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
        let mut task = cosmic::task::message(Message::Repository(RepositoryAction::OpenFile {
            card,
            path: open.path,
        }));
        if let Some(staged) = open.diff_staged {
            // A staged row is what the index holds against HEAD; an unstaged
            // one is what the worktree holds against the index. The panel
            // reports which list the row came from, and these are the two
            // bases those lists are computed from.
            let basis = if staged {
                DiffBasis::HeadToIndex
            } else {
                DiffBasis::IndexToWorktree
            };
            task = task
                .chain(cosmic::task::message(Message::Repository(
                    RepositoryAction::ApplySourceSurface { card, diff: true },
                )))
                .chain(cosmic::task::message(Message::Repository(
                    RepositoryAction::ApplyDiffBasis { card, basis },
                )));
        }
        task
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
        Self::source_control_snapshot_task(key.clone())
    }

    fn source_control_snapshot_task(key: ActiveWorktreeKey) -> Task<Message> {
        let worktree = key.canonical_path.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                RepositoryService::system("git")
                    .snapshot(&worktree, &CancellationToken::default())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| format!("source-control reader join failed: {error}"))
            .and_then(|result| result)
            .map(Box::new);
            Message::SourceControlLoaded { key, result }
        })
    }

    fn source_control_history_task(key: ActiveWorktreeKey, page: u32) -> Task<Message> {
        let request = HistoryRequest {
            worktree: key.canonical_path.clone(),
            revision: HistoryRevision::Head,
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
            Message::SourceControlHistory { key, page, result }
        })
    }

    /// Starts one mutation against the bound worktree.
    ///
    /// Refused outright when the panel has moved on or a command is already
    /// running: two `git add` runs against one index corrupt each other's
    /// `index.lock`, and the panel's own controls stand down while one is in
    /// flight, so this is the guard for a press that raced the frame.
    fn begin_mutation(&mut self, key: &ActiveWorktreeKey, kind: MutationKind) -> bool {
        if !self.source_control_is_bound(key) {
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
                #[cfg(feature = "integrated-browser")]
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
            let visible = self.workspace_surfaces_visible();
            self.set_workspace_surfaces_visible(visible);
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
        self.inspector.is_some()
            || self.worktree_dialog.is_some()
            || self.pull_request_dialog.is_some()
            || self.dirty_repository_close.is_some()
            || self.terminal_close.is_some()
            // Mounted by `dialog` from the sidebar's own state, so nothing
            // beneath it can learn about it any other way. Leaving it out is
            // how the one dialog that is on screen *because* it is about to
            // destroy work becomes the one dialog the pane keeps its commands
            // live behind.
            || self.sidebar_state.pending_discard().is_some()
    }

    fn context_inspector_is_overlay(&self) -> bool {
        matches!(
            inspector::mount_for_width(
                self.window_width,
                self.context_inspector_open,
                self.workspace.focused().is_some() || self.active_workspace.is_some(),
            ),
            Some(InspectorLayout::Overlay)
        )
    }

    fn workspace_inputs_blocked(&self) -> bool {
        self.modal_is_open() || self.context_inspector_is_overlay()
    }

    fn workspace_surfaces_visible(&self) -> bool {
        workspace_surfaces_should_render(
            self.inspector.is_some(),
            self.context_inspector_is_overlay(),
        )
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
            iced::widget::operation::focus(inspector::close_focus_id()).map(cosmic::Action::App)
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
        self.context_inspector_open = open;
        self.sync_context_overlay_state()
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
            Some(CardRuntime::Terminal(_)) | Some(CardRuntime::PullRequests(_)) => {
                InspectorRuntimeStatus::Ready
            }
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
            InspectorAction::Resize { width } => {
                self.settings.inspector_width =
                    width.clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH);
                self.settings_draft.inspector_width = self.settings.inspector_width.to_string();
                Task::none()
            }
            InspectorAction::ResizeFinished => {
                if let Err(error) = self.save_settings_now() {
                    self.status = Some(TransientNotice::error(format!(
                        "could not save inspector width: {error}"
                    )));
                }
                Task::none()
            }
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
                        InspectorAction::OpenBrowser(url) => {
                            cosmic::task::message(Message::OpenBrowser(url))
                        }
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

    /// Exits once the language servers have actually been reaped.
    ///
    /// `shutdown` only *asks*: the processes are stopped by session tasks on
    /// the tokio runtime, and `iced::exit()` takes that runtime away. Waiting
    /// off the update thread and exiting on the answer is what keeps a
    /// `rust-analyzer` - and the proc-macro server it owns - from outliving the
    /// application that spawned it. The wait is bounded by the module's own
    /// reap timeout, so a server that refuses to die cannot hold the exit.
    fn exit_after_language_servers(&mut self) -> Task<Message> {
        let reaped = self.lsp.begin_shutdown();
        cosmic::task::future(async move {
            reaped.await;
            Message::LanguageServersReaped
        })
    }

    fn activate_main_window(&self, token: Option<String>) -> Task<Message> {
        iced::Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Activate(
            token.unwrap_or_default(),
        )))
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
        self.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            remaining: cards,
            intent,
            focused: DirtyCloseControl::Cancel,
            restore_card,
        });
        iced::widget::operation::focus(dirty_close_control_id(card, DirtyCloseControl::Cancel))
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
            close.focused = DirtyCloseControl::Cancel;
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(
                card,
                DirtyCloseControl::Cancel,
            ))
            .map(cosmic::Action::App);
        }
        if let Some(card) = close.next_unresolved(|card| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        }) {
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(
                card,
                DirtyCloseControl::Cancel,
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
            .map(workspace_widget::card_focus_id)
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
        });
        iced::widget::operation::focus(restore_focus).map(cosmic::Action::App)
    }

    fn nested_modal_focus_target(&self) -> Option<widget::Id> {
        if self.worktree_dialog.is_some() {
            Some(worktree_dialog_focus_id())
        } else if self.inspector.is_some() {
            Some(settings_close_id())
        } else if self.pull_request_dialog.is_some() {
            Some(pull_request_dialog_focus_id())
        } else if self.context_inspector_is_overlay() {
            Some(inspector::close_focus_id())
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
            || self.inspector.is_some()
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
            iced::widget::operation::focus(inspector::close_focus_id()).map(cosmic::Action::App)
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

    /// Opens `inspector` from closed, and moves keyboard focus into it.
    ///
    /// The dialog is a modal popover, and the modal captures pointer events
    /// only, so focus would otherwise stay on the control that opened it -
    /// where Enter re-fires that control. That control cannot be reached
    /// while the dialog is up, because `update` refuses a pane command for
    /// as long as one is open, so this only ever runs from closed.
    fn open_inspector(&mut self, inspector: Inspector) -> Task<Message> {
        self.inspector = Some(inspector);
        self.inspector_transition = Some(InspectorTransition {
            started_at: Instant::now(),
            from: 0.0,
            to: 1.0,
        });
        self.set_workspace_surfaces_visible(false);
        self.frame_clock.reset();
        iced::widget::operation::focus(settings_close_id()).map(cosmic::Action::App)
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

    fn advance_inspector_transition(&mut self, now: Instant) -> Task<Message> {
        let Some(transition) = self.inspector_transition else {
            return Task::none();
        };
        if transition.is_complete(now) {
            if transition.to == 0.0 {
                self.inspector = None;
                let visible = self.workspace_surfaces_visible();
                self.set_workspace_surfaces_visible(visible);
            }
            self.inspector_transition = None;
        }
        self.focus_context_overlay_if_exposed()
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

    fn synchronize_appearance(&mut self, previous_scale: f32) -> Task<Message> {
        // COSMIC applies its scale action asynchronously. Use the previous
        // persisted preference, not Core's possibly not-yet-updated value.
        self.window_width *= previous_scale / self.settings.ui_scale;
        #[cfg(feature = "integrated-browser")]
        for runtime in self.cards.values() {
            if let CardRuntime::Browser(browser) = runtime {
                let _ = browser.handle.set_scale_factor(effective_surface_scale(
                    self.window_scale_factor,
                    self.settings.ui_scale,
                ));
            }
        }
        let theme = match self.settings.app_theme {
            AppTheme::System => {
                cosmic::Theme::system(Arc::new(self.core.system_theme().cosmic().clone()))
            }
            AppTheme::Dark => cosmic::Theme::dark(),
            AppTheme::Light => cosmic::Theme::light(),
        };
        let overlay = self.sync_context_overlay_state();
        Task::batch([
            cosmic::command::set_theme(theme),
            overlay,
            Task::done(cosmic::Action::Cosmic(cosmic::app::Action::ScaleFactor(
                self.settings.ui_scale,
            ))),
        ])
    }

    fn apply_setting_input(&mut self, field: SettingsField, value: String) -> Task<Message> {
        self.settings_draft.set(field, value.clone());
        let previous_scale = self.settings.ui_scale;
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
                    SettingsField::AppTheme | SettingsField::UiScale => {
                        self.synchronize_appearance(previous_scale)
                    }
                    SettingsField::MaxHotCards => cosmic::task::message(Message::LifecycleSweep),
                    SettingsField::HibernationDelayMs => {
                        // Reconciled here rather than through a queued
                        // `LifecycleSweep`, so the manager's exposure is
                        // current before the new grace is re-derived from it.
                        let sweep = self.lifecycle_sweep();
                        Task::batch([sweep, self.reschedule_offscreen_lifecycle_sweep()])
                    }
                    SettingsField::TerminalFontSize
                    | SettingsField::TerminalFontFamily
                    | SettingsField::TerminalScrollbackLines
                    | SettingsField::TerminalLineHeight
                    | SettingsField::TerminalPadding
                    | SettingsField::TerminalCursorStyle
                    | SettingsField::InspectorWidth
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
        let previous_scale = self.settings.ui_scale;
        self.settings = AppSettings::default();
        self.settings_draft = SettingsDraft::from_settings(&self.settings);
        self.card_chrome.clear();
        self.synchronize_lifecycle_budget();
        // Reset restores the built-in server list, which is a different set of
        // processes whenever the user had configured their own.
        let resync = self.synchronize_language_servers();
        self.settings_write_blocked = false;
        self.settings_feedback = Some(match self.save_settings_now() {
            Ok(()) => SettingsFeedback::Saved,
            Err(error) => SettingsFeedback::Error(error),
        });
        let sweep = self.lifecycle_sweep();
        let appearance = self.synchronize_appearance(previous_scale);
        Task::batch([
            resync,
            sweep,
            appearance,
            self.reschedule_offscreen_lifecycle_sweep(),
        ])
    }
}

impl Application for Cagents {
    type Executor = cosmic::executor::Default;
    type Flags = Option<async_channel::Receiver<instance::UiActivation>>;
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

    fn init(core: Core, activation_rx: Self::Flags) -> (Self, Task<Message>) {
        #[cfg(feature = "latency-harness")]
        let latency_fixture = crate::latency_fixture::Fixture::from_env();
        let (
            durable,
            session,
            projects,
            worktrees,
            settings,
            settings_path,
            settings_write_blocked,
            status,
            save_health,
        ) = {
            #[cfg(feature = "latency-harness")]
            if latency_fixture.is_some() {
                Self::fixture_bootstrap()
            } else {
                Self::bootstrap()
            }
            #[cfg(not(feature = "latency-harness"))]
            Self::bootstrap()
        };
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
            activation_rx,
            session,
            save_health,
            settings,
            #[cfg(feature = "integrated-browser")]
            browser_teardowns: Vec::new(),
            terminal_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_factory: None,
            #[cfg(feature = "integrated-browser")]
            browser_parent_lifetime: None,
            window_scale_factor,
            window_width: 1_600.0,
            settings_path,
            settings_write_blocked,
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, sidebar_projects),
            sidebar_state: SidebarState::default(),
            active_workspace: None,
            worktree_focus: HashMap::new(),
            github_availability: HashMap::new(),
            repository_generations: HashMap::new(),
            worktree_dialog: None,
            pull_request_dialog: None,
            pull_request_dialog_generation: 0,
            dirty_repository_close: None,
            terminal_close: None,
            terminal_close_attempts: HashSet::new(),
            closed_terminal_cards: HashSet::new(),
            uncertain_terminal_proofs: HashMap::new(),
            terminal_activity: HashMap::new(),
            activity_origin: Instant::now(),
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
            inspector: None,
            inspector_transition: None,
            context_inspector_open: true,
            context_tab: ContextTab::default(),
            context_inspector_overlay_active: false,
            // The initial estimate is wide, but the first compositor resize
            // can immediately mount the default-open inspector as a modal.
            // Keep a valid post-close target ready for that narrow startup.
            context_inspector_restore_focus: Some(context_inspector_toggle_id()),
            settings_category: SettingsCategory::Appearance,
            keyboard_modifiers: keyboard::Modifiers::empty(),
            settings_draft,
            settings_feedback: None,
            status: status.map(TransientNotice::error),
            lifecycle_sweep_deadline: None,
            lifecycle_sweep_generation: 0,
        };
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
            app.settings.inspector_width + crate::splitter::WIDTH
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
        let mut tasks = vec![
            scope,
            app.rehydrate_worktree_operations(),
            app.initialize_runtimes(),
            app.synchronize_appearance(1.0),
        ];
        #[cfg(feature = "integrated-browser")]
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
        if !self.workspace.camera().is_animating() && self.inspector_transition.is_none() {
            // A new animation starts from its first frame, not from time spent
            // idle while the window-frame subscription was disconnected.
            self.frame_clock.reset();
        }
        // Camera-only events cannot change labels, selected worktrees, or
        // repository bindings. Keep per-frame work proportional to visible
        // columns rather than rebuilding every card's chrome.
        let camera_only = matches!(
            &message,
            Message::Frame(_) | Message::Viewport(_) | Message::Pan(_) | Message::Wheel(_)
        );
        let task = match message {
            Message::NewTerminal => {
                self.show_workspace_for_action();
                match self.terminal_directory() {
                    Some(directory) => self.add_terminal(directory),
                    None => cosmic::task::message(Message::OpenRepository),
                }
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
                        // A worker can publish its first event before this start
                        // result reaches the UI. Adopt and drain it before sending
                        // commands, which could otherwise mask a permanent failure
                        // as a closed visibility channel.
                        self.cards.insert(card, CardRuntime::Terminal(handle));
                        self.drain_terminal(card, 0);
                        let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                            return cosmic::task::message(Message::LifecycleSweep);
                        };
                        let visible = terminal_start_visibility(
                            self.durable.cards.iter().find(|state| state.id == card),
                        )
                        .expect("accepted terminal start has an open terminal card")
                            && self.workspace_surfaces_visible();
                        let visibility = if visible {
                            handle.reveal()
                        } else {
                            handle.hide()
                        };
                        match visibility {
                            Ok(()) => {
                                if self.finish_initial_hydration(card, Ok(())) {
                                    let delay =
                                        Duration::from_millis(self.settings.hibernation_delay_ms);
                                    tasks.push(self.request_lifecycle_sweep(delay));
                                }
                            }
                            Err(error) => {
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
                if self
                    .card_state(card)
                    .is_some_and(|state| state.exposure == Exposure::Offscreen)
                {
                    tasks.push(self.reconcile_scoped_lifecycle());
                }
                self.persist();
                Task::batch(tasks)
            }
            Message::TerminalActivityExpired { card, timer } => {
                self.expire_terminal_activity(card, timer)
            }
            Message::TerminalWake { card, generation } => {
                let started =
                    tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::DEBUG)
                        .then(Instant::now);
                self.drain_terminal(card, generation);
                if let Some(started) = started {
                    let activity_sequence = match self.cards.get(&card) {
                        Some(CardRuntime::Terminal(handle)) => {
                            handle.activity().map_or(0, |fact| fact.sequence)
                        }
                        _ => 0,
                    };
                    tracing::debug!(target: "cagents::terminal_pipeline", stage = "ui_apply", elapsed_ns = started.elapsed().as_nanos() as u64, generation, activity_sequence);
                }
                Task::none()
            }
            Message::TerminalError { card: _, message } => {
                self.status = Some(TransientNotice::error(message));
                Task::none()
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
                    self.repository_load_task(path, project_id, true)
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
                        let mut sidebar_project = SidebarProject::from_repository(
                            &loaded.project,
                            &loaded.repository,
                            worktrees,
                            pull_requests,
                        );
                        sidebar_project.github_ready = loaded.github.repository_key().is_some();
                        if !sidebar_project.github_ready {
                            sidebar_project.revoke_github();
                        }
                        sidebar_project.github_status = loaded.github.label().into();
                        self.github_availability
                            .insert(project_id, loaded.github.clone());
                        self.sidebar_model
                            .projects
                            .retain(|project| project.id != loaded.project.id);
                        self.sidebar_model.projects.push(sidebar_project);
                        self.sync_worktree_operation_rows();
                        self.sidebar_model.data_state = SidebarDataState::Ready;
                        let worktree = loaded
                            .worktrees
                            .first()
                            .map(|(record, _)| record.path.clone())
                            .unwrap_or_else(|| loaded.repository.selected_path.clone());
                        if open_cards {
                            task = self.select_worktree(worktree, true);
                        }
                    }
                    Err(error) => {
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
                        self.status = Some(TransientNotice::error(error.clone()));
                        finish_repository_load_failure(&mut self.sidebar_model.data_state, error);
                    }
                }
                task
            }
            Message::Repository(RepositoryAction::NewTerminal { card }) => {
                self.show_workspace_for_action();
                match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) => {
                        self.add_terminal(runtime.descriptor.worktree.clone())
                    }
                    _ => Task::none(),
                }
            }
            Message::Repository(RepositoryAction::MarkdownLink { uri, .. }) => {
                match Url::parse(&uri) {
                    Ok(url) => self.open_link(url),
                    Err(_) => {
                        self.status = Some(TransientNotice::error(
                            "could not open Markdown link: invalid absolute URL",
                        ));
                        Task::none()
                    }
                }
            }
            Message::Repository(action) => {
                if self.modal_is_open() && !repository_action_allowed_while_modal(&action) {
                    return Task::none();
                }
                let card = action.card();
                let save_finished = matches!(action, RepositoryAction::SaveFinished { .. });
                // A parked source-control navigation is answered by the very
                // file list it is waiting on, and only after the card has taken
                // that list.
                let files_loaded = matches!(action, RepositoryAction::FilesLoaded { .. });
                let closing_card = self.dirty_repository_close.as_ref().map(|close| close.card);
                let task = match self.cards.get_mut(&card) {
                    Some(CardRuntime::Repository(runtime)) => {
                        repository_task(runtime.update(action))
                    }
                    _ => Task::none(),
                };
                // Every buffer transition the action caused is recorded on the
                // runtime and has so far reached nothing. Draining here rather
                // than at the individual actions is what makes that true of all
                // of them, including the ones a later action adds.
                let mut tasks = vec![task, self.drain_lsp_intents(card)];
                if files_loaded {
                    tasks.push(self.flush_pending_panel_open(card));
                }
                if save_finished && closing_card == Some(card) {
                    tasks.push(self.advance_dirty_close());
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
                self.sidebar_state.apply(&action);
                match action {
                    SidebarAction::CancelWorktreeOperation(id) => self.cancel_worktree_operation(id),
                    SidebarAction::OpenRepositoryPicker => {
                        cosmic::task::message(Message::OpenRepository)
                    }
                    SidebarAction::OpenRepository {
                        project_id,
                        repository,
                    } => {
                        self.show_workspace_for_action();
                        let path = self.sidebar_model.projects.iter()
                            .find(|project| project.id == project_id)
                            .and_then(|project| project.worktrees.first())
                            .map_or(repository, |worktree| worktree.path.clone());
                        self.select_worktree(path, true)
                    }
                    SidebarAction::OpenWorktree { path, .. } => {
                        self.show_workspace_for_action();
                        self.select_worktree(path, true)
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
                        iced::widget::operation::focus(worktree_dialog_focus_id())
                            .map(cosmic::Action::App)
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
                        self.pull_request_dialog_generation =
                            self.pull_request_dialog_generation.wrapping_add(1);
                        self.pull_request_dialog = Some(PullRequestDialog {
                            project_id,
                            refresh_generation: self.repository_generations.get(&project_id).copied().unwrap_or_default(),
                            repository,
                            base: "main".into(),
                            head,
                            title: String::new(),
                            body: String::new(),
                            draft: false,
                            retry_confirmation: None,
                        });
                        iced::widget::operation::focus(pull_request_dialog_focus_id())
                            .map(cosmic::Action::App)
                    }
                    SidebarAction::OpenSettings => {
                        self.settings_category = SettingsCategory::Appearance;
                        self.settings_draft = SettingsDraft::from_settings(&self.settings);
                        self.settings_feedback = None;
                        self.open_inspector(Inspector::Settings)
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
                    // Expanding `History` is the only thing that asks for the
                    // first page; the tail of `update` sees the new toggle and
                    // issues the walk.
                    SidebarAction::ToggleSection { .. }
                    | SidebarAction::ToggleSourceControlPanel { .. }
                    | SidebarAction::FilesQueryChanged { .. }
                    | SidebarAction::ToggleDirectory { .. }
                    | SidebarAction::CommitMessageChanged { .. }
                    | SidebarAction::PublishRemoteChanged { .. }
                    | SidebarAction::PublishBranchChanged { .. }
                    | SidebarAction::CancelPublish
                    // `apply` already cleared the pending discard, and the
                    // dialog it was driving is gone with it.
                    | SidebarAction::CancelDiscard
                    // Raising the confirmation is the whole of this intent:
                    // `apply` recorded the unconfirmed request and `dialog`
                    // mounts it.
                    | SidebarAction::RequestDiscard { .. } => Task::none(),
                    SidebarAction::RefreshWorktree { key } => self.reload_source_control(&key),
                    SidebarAction::OpenFile { key, path } => {
                        self.route_panel_open(&key, &path, None)
                    }
                    SidebarAction::OpenChange { key, path, area } => {
                        // The card has two diff bases. A conflict has no staged
                        // side to show, so it is opened against the working
                        // tree like any other unstaged change.
                        let staged = area == ChangeArea::Staged;
                        self.route_panel_open(&key, &path, Some(staged))
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
                            source.mark_history_requested();
                        }
                        Self::source_control_history_task(key, page)
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
            Message::ResizeCard { card, width } => {
                if !self
                    .card_state(card)
                    .is_some_and(|state| matches!(&state.kind, CardKind::Terminal(_)))
                {
                    return Task::none();
                }
                let viewport = self.workspace.viewport_width();
                if viewport <= f32::EPSILON {
                    return Task::none();
                }
                let limits = self.workspace.fractional_width_limits();
                let maximum = limits.maximum;
                let minimum = limits.minimum;
                let width = width.clamp(minimum, maximum);
                let fraction = width / viewport;
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
            Message::ResizeCardFinished(card) => {
                if self.card_state(card).is_some() {
                    self.persist();
                }
                Task::none()
            }
            Message::CloseCard(_) if self.dirty_repository_close.is_some() => Task::none(),
            Message::CloseCard(id) if matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()) => {
                self.begin_dirty_close(DirtyCloseIntent::Card(id), VecDeque::from([id]))
            }
            Message::WindowCloseRequested(window) => {
                if self.dirty_repository_close.is_some() {
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
            Message::CloseCard(id) => self.begin_card_close(id),
            Message::ConfirmTerminalClose => self.confirm_terminal_close(),
            Message::CancelTerminalClose => self.cancel_terminal_close(),
            Message::TerminalCloseFinished {
                card,
                terminal,
                proof,
                outcome,
            } => self.terminal_close_finished(card, terminal, proof, outcome),
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
                self.retire_lsp_card(id);
                if let Some(runtime) = self.cards.remove(&id) {
                    match runtime {
                        CardRuntime::Terminal(_handle) => {
                            // Explicit confirmation already obtained the daemon acknowledgement.
                            // Dropping the surface only detaches; never send a second shutdown.
                        }
                        #[cfg(feature = "integrated-browser")]
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
                let inspector_task = self.advance_inspector_transition(frame);
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
            #[cfg(feature = "integrated-browser")]
            Message::InitializeBrowserFactory => self.browser_factory_task(),
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
            Message::WindowResized(width) => {
                if width.is_finite() && width > 0.0 {
                    self.window_width = width;
                }
                if self.focused_repository_is_saving() && self.context_inspector_is_overlay() {
                    self.context_inspector_open = false;
                }
                self.sync_context_overlay_state()
            }
            Message::ActivateUi { token } => self.activate_main_window(token),
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
                        let submit = self.start_worktree_operation(dialog.project_id, request);
                        Task::batch([submit, self.focus_context_overlay_if_exposed()])
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        self.worktree_dialog = Some(dialog);
                        iced::widget::operation::focus(worktree_dialog_focus_id())
                            .map(cosmic::Action::App)
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
                if self.repository_generations.get(&dialog.project_id).copied()
                    != Some(dialog.refresh_generation)
                    || self
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
            Message::CancelDialog => {
                if self.worktree_dialog.take().is_none()
                    && self.pull_request_dialog.take().is_some()
                {
                    self.pull_request_dialog_generation =
                        self.pull_request_dialog_generation.wrapping_add(1);
                }
                if let Some(target) = self.nested_modal_focus_target() {
                    return iced::widget::operation::focus(target).map(cosmic::Action::App);
                }
                self.focus_context_overlay_if_exposed()
            }
            Message::KeyEvent(event) => {
                if let keyboard::Event::ModifiersChanged(modifiers) = &event {
                    self.keyboard_modifiers = *modifiers;
                }
                if is_escape_press(&event) {
                    if self.terminal_close.is_some() {
                        return self.cancel_terminal_close();
                    }
                    if self.dirty_repository_close.is_some() {
                        if self.dirty_repository_close.as_ref().is_some_and(|close| {
                            matches!(self.cards.get(&close.card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
                        }) {
                            return Task::none();
                        }
                        return cosmic::task::message(Message::CancelRepositoryClose);
                    }
                    if self.inspector.is_some() {
                        return cosmic::task::message(Message::CloseInspector);
                    }
                    if self.worktree_dialog.is_some() || self.pull_request_dialog.is_some() {
                        return cosmic::task::message(Message::CancelDialog);
                    }
                    if self.context_inspector_is_overlay() {
                        return cosmic::task::message(Message::ContextInspector(
                            InspectorAction::Close,
                        ));
                    }
                }
                if is_tab_press(&event).is_some() && self.terminal_close.is_some() {
                    return self.cycle_terminal_close_focus();
                }
                if let Some(backwards) = is_tab_press(&event)
                    && self.dirty_repository_close.is_some()
                {
                    return self.cycle_dirty_close_focus(backwards);
                }
                // A chord typed at a dialog is not the workspace's to act on:
                // Ctrl+W must not close the card behind the dialog. The dialog
                // is an overlay, so it has already seen the keystroke by the
                // time this runs, and what it does with one is its own affair.
                if self.workspace_inputs_blocked() {
                    return Task::none();
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
                self.show_workspace_for_action();
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
                self.context_tab = tab;
                Task::none()
            }
            Message::ContextInspector(_) if self.modal_is_open() => Task::none(),
            Message::ContextInspector(action) => self.handle_context_inspector_action(action),
            Message::CloseInspector => {
                self.close_inspector();
                Task::none()
            }
            Message::SelectSettingsCategory(category) => {
                self.settings_category = category;
                Task::none()
            }
            Message::SettingsChanged { field, value } => self.apply_setting_input(field, value),
            Message::SettingsDraftChanged { field, value } => {
                self.settings_draft.set(field, value);
                self.settings_feedback = None;
                Task::none()
            }
            Message::ResetSettings => self.reset_settings(),
            Message::Lsp(event) => {
                self.apply_lsp_event(event);
                Task::none()
            }
            Message::LanguageServersReaped => iced::exit().map(cosmic::Action::App),
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
            Message::DismissStatus => {
                self.status = None;
                Task::none()
            }
            Message::SourceControlLoaded { key, result } => {
                if !self.source_control_is_bound(&key) {
                    return Task::none();
                }
                let mut next = match result {
                    Ok(snapshot) => SourceControlModel::ready(key, &snapshot),
                    Err(error) => SourceControlModel::failed(key, error),
                };
                // The reload replaces the rows and nothing else: an in-flight
                // mutation, its last failure, and the history already walked
                // belong to the binding, not to the page of Git output.
                if let Some(previous) = self.sidebar_model.source_control.as_ref() {
                    previous.carry_mutation_into(&mut next);
                }
                self.sidebar_model.source_control = Some(next);
                Task::none()
            }
            Message::SourceControlHistory { key, page, result } => {
                if !self.source_control_is_bound(&key) {
                    return Task::none();
                }
                match result {
                    Ok(commits) => {
                        if let Some(source) = self.sidebar_model.source_control.as_mut() {
                            // A page that arrives out of order would append the
                            // same commits twice, so only the page the walk is
                            // waiting for is taken.
                            if source.next_history_page == page {
                                source.append_history(&commits);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(source) = self.sidebar_model.source_control.as_mut() {
                            source.set_error(Some(format!("Could not read history: {error}")));
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
                if !self.source_control_is_bound(&key) {
                    return Task::none();
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
                self.reload_source_control(&key)
            }
        };
        if self.latency_focus != self.workspace.focused() {
            self.latency.abandon_pending();
        }
        self.refresh_terminal_activity();
        if camera_only {
            return Task::batch([task, self.release_deferred_navigation_if_unblocked()]);
        }
        self.synchronize_sidebar_selection();
        let binding = self.rebind_source_control();
        // `view` cannot rebuild the chrome cache itself, so refresh it here.
        let chrome = self.sync_chrome();
        let deferred = self.release_deferred_navigation_if_unblocked();
        Task::batch([task, binding, chrome, deferred])
    }

    fn subscription(&self) -> Subscription<Message> {
        let operation_subscriptions = self.worktree_progress_subscriptions();
        let mut subscriptions = vec![
            Subscription::run_with(
                TerminalWakeSource(self.terminal_wake_rx.clone()),
                terminal_wake_stream,
            )
            .map(|wake| Message::TerminalWake {
                card: wake.card,
                generation: wake.generation,
            }),
            Subscription::run_with(
                UiActivationSource(self.activation_rx.clone()),
                ui_activation_stream,
            )
            .map(|activation| Message::ActivateUi {
                token: activation.token,
            }),
            event::listen_with(|event, _status, event_window| match event {
                iced::Event::Keyboard(event) => Some(Message::KeyEvent(event)),
                iced::Event::Window(window::Event::Rescaled(scale_factor)) => {
                    Some(Message::WindowScaleFactorChanged(scale_factor))
                }
                iced::Event::Window(window::Event::Resized(size)) => {
                    Some(Message::WindowResized(size.width))
                }
                iced::Event::Window(window::Event::CloseRequested) => {
                    Some(Message::WindowCloseRequested(event_window))
                }
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
        subscriptions.extend(self.activity_subscriptions());
        if let Some(window) = self.core.main_window_id()
            && (self.workspace.camera().is_animating() || self.inspector_transition.is_some())
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
        if self.terminal_close.is_some() {
            return self.terminal_close_dialog();
        }
        let spacing = cosmic::theme::spacing();
        // First, because a discard is the one dialog on screen precisely
        // because it is about to destroy work that exists nowhere else.
        if let Some(pending) = self.sidebar_state.pending_discard() {
            return Some(sidebar::discard_confirmation_dialog(pending).map(Message::Sidebar));
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
            return Some(
                widget::container(widget::column::with_children(rows).spacing(spacing.space_xs))
                    .id(widget::Id::new(format!("dialog.dirty-close.{card}")))
                    .class(cosmic::theme::Container::Dialog(false))
                    .padding(spacing.space_m)
                    .max_width(560)
                    .into(),
            );
        }
        if let Some(dialog) = self.worktree_dialog.as_ref() {
            return Some(
                widget::container(
                    widget::column::with_children(vec![
                        widget::text::title3("Create worktree").into(),
                        widget::column::with_children(vec![
                            widget::text::caption("Branch").into(),
                            widget::text_input("Branch", &dialog.branch)
                                .id(worktree_dialog_focus_id())
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
                    self.window_width,
                    progress,
                ),
            });
        }

        let dialog = self.pull_request_dialog.as_ref()?;
        let mut rows: Vec<Element<'_, Message>> = vec![
            widget::text::title3("Create pull request").into(),
            widget::text::caption(dialog.repository.gh_selector()).into(),
            widget::column::with_children(vec![
                widget::text::caption("Title").into(),
                widget::text_input("Title", &dialog.title)
                    .id(pull_request_dialog_focus_id())
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
                        (dialog.retry_confirmation != Some(false)
                            && self.repository_generations.get(&dialog.project_id)
                                == Some(&dialog.refresh_generation)
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
        Some(
            widget::container(widget::column::with_children(rows).spacing(spacing.space_xs))
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
        self.workflow_header()
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

        let save_health = self.save_health.as_ref().and_then(save_health_view);
        let notice = self.status.as_ref().map(transient_notice_view);

        let workspace: Element<'_, Message> = if self.workspace.cards().is_empty() {
            let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let action_buttons = || {
                vec![
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
                    widget::icon::from_name("utilities-terminal-symbolic")
                        .size(32)
                        .into(),
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

        let mut main_children: Vec<Element<'_, Message>> = Vec::with_capacity(5);
        if let Some(save_health) = save_health {
            main_children.push(save_health);
            main_children.push(widget::divider::horizontal::default().into());
        }
        if let Some(notice) = notice {
            main_children.push(notice);
            main_children.push(widget::divider::horizontal::default().into());
        }
        main_children.push(workspace);
        let main_content: Element<'_, Message> = widget::column::with_children(main_children)
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        let context_model = (self.context_inspector_open
            && self.context_tab != ContextTab::Changes)
            .then(|| self.workspace.focused())
            .flatten()
            .and_then(|card| {
                self.card_state(card).and_then(|state| {
                    if self.context_tab == ContextTab::Code
                        && !matches!(state.kind, CardKind::Repository(_))
                    {
                        return None;
                    }
                    let repository = match self.cards.get(&card) {
                        Some(CardRuntime::Repository(runtime))
                            if self.context_tab == ContextTab::Code =>
                        {
                            Some(runtime.inspector_context())
                        }
                        _ => None,
                    };
                    let model = inspector::model_for_card(
                        state,
                        self.inspector_runtime_status(card),
                        &self.sidebar_model.projects,
                        repository.as_ref(),
                    );
                    Some(if self.context_tab == ContextTab::Code {
                        match self.lsp_context(card) {
                            Some(lsp) => inspector::with_lsp(model, card, lsp),
                            None => model,
                        }
                    } else {
                        model
                    })
                })
            });
        let surface: Element<'_, Message> = if self.context_inspector_open
            && (self.active_workspace.is_some() || self.workspace.focused().is_some())
        {
            let width = self
                .settings
                .inspector_width
                .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH)
                .min((window_width - crate::splitter::WIDTH).max(0.0));
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
            let pane = self.context_panel(context_model, width);
            let inspector_surface: Element<'_, Message> =
                widget::row::with_children(vec![splitter, pane])
                    .spacing(0)
                    .height(Length::Fill)
                    .into();
            match inspector::mount_for_width(window_width, true, true)
                .expect("an open contextual inspector with a model has a mount")
            {
                InspectorLayout::Docked => {
                    widget::row::with_children(vec![sidebar, main_content, inspector_surface])
                        .spacing(0)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into()
                }
                InspectorLayout::Overlay => {
                    let background = widget::row::with_children(vec![sidebar, main_content])
                        .spacing(0)
                        .width(Length::Fill)
                        .height(Length::Fill);
                    let dismiss = iced::widget::opaque(
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
                    let foreground =
                        iced::widget::Stack::with_children(vec![dismiss, aligned.into()])
                            .width(Length::Fill)
                            .height(Length::Fill);
                    inspector::ModalOverlay::new(background, foreground).into()
                }
            }
        } else {
            widget::row::with_children(vec![sidebar, main_content])
                .height(Length::Fill)
                .width(Length::Fill)
                .into()
        };
        let surface = crate::interaction::drive_animation(
            surface,
            self.workspace.camera().is_animating() || self.inspector_transition.is_some(),
        );
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
        if !self.settings_write_blocked
            && let Some(path) = &self.settings_path
            && let Err(error) = settings::save(path, &self.settings)
        {
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

        // Exact Wayland parent invariant:
        // 1. request and join every browser still owned by a card;
        // 2. join every off-UI-path card-close reaper;
        // 3. drop the raw-handle factory/Wayland host;
        // 4. deactivate/drop the parent lifetime guard;
        // 5. only then may Rust drop `core` and its parent window.
        #[cfg(feature = "integrated-browser")]
        for runtime in std::mem::take(&mut self.cards).into_values() {
            if let CardRuntime::Browser(mut browser) = runtime {
                browser.handle.shutdown();
            }
        }
        for teardown in self.terminal_teardowns.drain(..) {
            let _ = teardown.join();
        }
        #[cfg(feature = "integrated-browser")]
        for teardown in self.browser_teardowns.drain(..) {
            let _ = teardown.join();
        }
        #[cfg(feature = "integrated-browser")]
        self.browser_factory.take();
        #[cfg(feature = "integrated-browser")]
        if let Some(parent_lifetime) = self.browser_parent_lifetime.take() {
            parent_lifetime.deactivate();
        }
    }
}

/// The settings dialog's first focusable control, focused when it opens.
fn settings_close_id() -> widget::Id {
    widget::Id::new("dialog.settings.close")
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

/// The worktree dialog's first field, focused when it opens.
fn worktree_dialog_focus_id() -> widget::Id {
    widget::Id::new("dialog.worktree.branch")
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
    let github_availability = (|| {
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
        let pull_requests = github
            .list(
                &ListPullRequestsRequest {
                    repository: key.clone(),
                    state: PullRequestStateFilter::Open,
                    limit: sidebar::PULL_REQUEST_LIST_LIMIT,
                },
                &cancellation,
            )
            .map_err(GitHubAvailability::from_error)?;
        let checks = github.checks_for_pull_requests(&key, &pull_requests, &cancellation);
        Ok(GitHubAvailability::Ready {
            repository_key: key,
            pull_requests: pull_requests.into_iter().zip(checks).collect(),
            refreshed_at: unix_millis(),
        })
    })()
    .unwrap_or_else(|unavailable| unavailable);
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
        github: github_availability,
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

const fn workspace_surfaces_should_render(
    settings_inspector_open: bool,
    context_inspector_overlay: bool,
) -> bool {
    !settings_inspector_open && !context_inspector_overlay
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
    use cosmic::iced::futures::StreamExt;

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
            "first"
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

    fn health_event(
        revision: u64,
        health: SessionSaveHealth,
        last_successful_flush_at_unix_ms: Option<i64>,
    ) -> SessionSaveHealthEvent {
        SessionSaveHealthEvent {
            revision,
            health,
            last_successful_flush_at_unix_ms,
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
                })
            );
            assert_eq!(stream.next().await, None);
        });
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

    fn launch_recipe(working_directory: impl Into<PathBuf>) -> LaunchRecipe {
        LaunchRecipe {
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
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let opener = directory.path().join("fake-opener");
        std::fs::write(
            &opener,
            concat!(
                "#!/bin/sh\n",
                "readlink /proc/$$/fd/1 > \"${0}.stdout-target\"\n",
                "readlink /proc/$$/fd/2 > \"${0}.stderr-target\"\n",
                "printf '%s\\n' \"$1\" >&2\n",
                "exit 42\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o700)).unwrap();

        let raw =
            "https://alice:password@example.test:8443/private%20path?oauth=secret#access-token";
        let url = Url::parse(raw).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(open_system_url_with_opener(&url, &opener))
            .unwrap_err();

        assert_eq!(
            std::fs::read_to_string(opener.with_extension("stdout-target")).unwrap(),
            "/dev/null\n"
        );
        assert_eq!(
            std::fs::read_to_string(opener.with_extension("stderr-target")).unwrap(),
            "/dev/null\n"
        );
        assert!(error.contains("https://example.test:8443/private%20path"));
        assert!(error.contains("system opener exited"));
        assert!(error.contains("42"));
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
        assert_eq!(close.focused, DirtyCloseControl::Cancel);
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
    fn headless_renderer() -> cosmic::Renderer {
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
        let mut fresh = frame(app);
        assert_tree_is_faithful(
            tree,
            &cosmic::iced::advanced::widget::Tree::new(fresh.as_widget_mut()),
        );
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
            Message::FocusCard(CardId::new()),
            Message::FocusAdjacent(crate::workspace::FocusDirection::Right),
            Message::Pan(PanGesture::Begin { pointer_x: 10.0 }),
            Message::Pan(PanGesture::Update { pointer_x: 20.0 }),
            Message::Wheel(WheelDelta::Pixels { x: 1.0, y: 0.0 }),
            Message::ToggleContextInspector,
            Message::Repository(RepositoryAction::NewTerminal { card: focused }),
        ];
        assert!(
            messages
                .iter()
                .all(workspace_navigation_blocked_during_save)
        );
        assert!(!workspace_navigation_blocked_during_save(&Message::Pan(
            PanGesture::End
        )));

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
            refresh_generation: 0,
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
            base: "release".into(),
            head: "topic/authored-change".into(),
            title: "Keep the authored title".into(),
            body: "A carefully authored description.\n\nWith details for reviewers.".into(),
            draft: true,
            retry_confirmation: None,
        }
    }

    fn authorize_pull_request_test(app: &mut Cagents, dialog: &PullRequestDialog) {
        app.repository_generations
            .insert(dialog.project_id, dialog.refresh_generation);
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
        let card = CardId::new();
        assert_eq!(
            dirty_close_control_id(card, DirtyCloseControl::Cancel),
            dirty_close_control_id(card, DirtyCloseControl::Cancel)
        );
        assert_ne!(
            dirty_close_control_id(card, DirtyCloseControl::Cancel),
            dirty_close_control_id(card, DirtyCloseControl::Save)
        );
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
    fn first_frame_after_immediate_settings_close_releases_deferred_navigation() {
        let mut app = context_inspector_test_app(false);
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let key = crate::github::PullRequestKey::new(repository, 45).unwrap();
        app.deferred_navigation
            .push_back(Message::PullRequestCreated {
                dialog: pull_request_test_dialog(),
                generation: 0,
                result: Ok(PullRequestCreationOutcome::Created {
                    key,
                    url: Url::parse("https://github.com/owner/repo/pull/45").unwrap(),
                    pull_request: None,
                    normalization_warning: None,
                }),
            });
        app.inspector = Some(Inspector::Settings);
        app.inspector_transition = Some(InspectorTransition {
            started_at: Instant::now() - DIALOG_TRANSITION_DURATION,
            from: 0.05,
            to: 0.0,
        });
        app.frame_clock.reset();

        let _released = app.update(Message::Frame(Instant::now()));

        assert!(app.inspector.is_none());
        assert!(app.inspector_transition.is_none());
        assert!(app.deferred_navigation.is_empty());
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
    fn dirty_close_restores_the_actual_underlying_modal_focus_target() {
        let mut app = context_inspector_test_app(true);
        let project = ProjectId::new();
        app.worktree_dialog = Some(WorktreeDialog {
            project_id: project,
            repository: PathBuf::from("/repo"),
            branch: String::new(),
            base: "main".into(),
            destination: "/worktrees".into(),
        });
        assert_eq!(
            app.nested_modal_focus_target().unwrap().to_string(),
            worktree_dialog_focus_id().to_string()
        );

        app.worktree_dialog = None;
        app.inspector = Some(Inspector::Settings);
        assert_eq!(
            app.nested_modal_focus_target().unwrap().to_string(),
            settings_close_id().to_string()
        );

        app.inspector = None;
        app.pull_request_dialog = Some(PullRequestDialog {
            project_id: ProjectId::new(),
            refresh_generation: 0,
            repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
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
            inspector::close_focus_id().to_string()
        );
    }

    #[test]
    fn column_navigator_appears_only_when_movement_is_possible() {
        assert!(!should_show_card_navigator(0));
        assert!(!should_show_card_navigator(1));
        assert!(should_show_card_navigator(2));
    }

    #[test]
    fn native_surfaces_stay_hidden_until_every_overlay_is_closed() {
        assert!(workspace_surfaces_should_render(false, false));
        assert!(!workspace_surfaces_should_render(true, false));
        assert!(!workspace_surfaces_should_render(false, true));
        assert!(!workspace_surfaces_should_render(true, true));
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
        let _focus_close = app.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0));
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
        let close = task_app_message(app.update(Message::KeyEvent(escape)));
        assert!(matches!(
            close,
            Message::ContextInspector(InspectorAction::Close)
        ));
        let _restore_focus = app.update(close);
        assert!(!app.context_inspector_open);
        assert!(!app.context_inspector_overlay_active);
        assert!(!app.workspace_inputs_blocked());
        assert!(app.context_inspector_restore_focus.is_none());
        assert_eq!(app.core.window.show_headerbar, headerbar_enabled);

        // Panel tabs change the tools, not workspace focus or the overlay
        // boundary, and remain available in production builds.
        let focused = app.workspace.focused();
        let _ = app.update(Message::ToggleContextInspector);
        for tab in [ContextTab::Changes, ContextTab::Code, ContextTab::Context] {
            let _ = app.update(Message::SelectContextTab(tab));
            assert_eq!(app.context_tab, tab);
            assert_eq!(app.workspace.focused(), focused);
            assert!(app.context_inspector_overlay_active);
            assert_eq!(app.core.window.show_headerbar, headerbar_enabled);
        }
        let _ = app.open_inspector(Inspector::Settings);
        let _ = app.update(Message::SelectContextTab(ContextTab::Code));
        assert_eq!(app.context_tab, ContextTab::Context);
        app.inspector = None;
        app.inspector_transition = None;
        let _ = app.update(Message::ContextInspector(InspectorAction::Close));

        // Re-entering from a live docked inspector captures afresh instead of
        // depending on the startup value consumed by the first Escape.
        let _wide = app.update(Message::WindowResized(design::WIDE_BREAKPOINT));
        let _open_docked = app.update(Message::ToggleContextInspector);
        assert!(!app.context_inspector_is_overlay());
        app.context_inspector_restore_focus = None;
        let _focus_close = app.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0));
        assert!(app.context_inspector_is_overlay());
        assert_eq!(
            app.context_inspector_restore_focus
                .as_ref()
                .map(ToString::to_string),
            Some(context_inspector_toggle_id().to_string())
        );

        // A real first-card update can create the overlay without any resize:
        // narrow startup begins empty, then the focused context arrives.
        let mut startup = context_inspector_test_app(false);
        let _narrow = startup.update(Message::WindowResized(design::WIDE_BREAKPOINT - 1.0));
        assert!(!startup.context_inspector_is_overlay());
        assert!(!startup.context_inspector_overlay_active);
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

        let close = task_app_message(startup.update(Message::KeyEvent(
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
        )));
        let _restore_focus = startup.update(close);
        assert!(!startup.context_inspector_open);
        assert!(!startup.context_inspector_overlay_active);
        assert!(startup.context_inspector_restore_focus.is_none());

        // Losing the final context is another real overlay exit: it restores
        // the header target even though the open preference itself stays on.
        let _focus_close = startup.update(Message::ToggleContextInspector);
        assert!(startup.context_inspector_overlay_active);
        let final_card = startup.workspace.focused().unwrap();
        startup.workspace.request_close(final_card).unwrap();
        let _restore_focus = startup.update(Message::CommitClose(final_card));
        assert!(startup.workspace.focused().is_none());
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

        let _finished = app.update(Message::ContextInspector(InspectorAction::ResizeFinished));
        let saved = std::fs::read_to_string(path).unwrap();
        assert!(saved.contains("inspector_width = 384.0"));
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
    fn daemon_rejection_before_terminal_started_remains_relaunchable() {
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
        // The subscription can deliver this edge before the start task returns.
        let _ = app.update(Message::TerminalWake {
            card,
            generation: wake.generation,
        });
        let _ = app.update(Message::TerminalStarted {
            card,
            result: Ok(TerminalSurface::from(handle)),
        });
        assert!(matches!(
            &app.card_state(card).unwrap().kind,
            CardKind::Terminal(TerminalSession {
                continuity: TerminalContinuity::RelaunchAvailable { .. },
                ..
            })
        ));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));

        let _ = app.retry_card(card);
        let CardKind::Terminal(replacement) = &app.card_state(card).unwrap().kind else {
            panic!("retry must preserve the terminal card");
        };
        assert_ne!(replacement.id, old_id);
        assert!(matches!(
            terminal_start_mode(replacement),
            Ok(TerminalStartMode::Spawn(_))
        ));
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
        let core = Core::default();

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
            activation_rx: async_channel::unbounded().1,
            session: None,
            save_health: None,
            settings_draft: SettingsDraft::from_settings(&settings),
            settings,
            #[cfg(feature = "integrated-browser")]
            browser_teardowns: Vec::new(),
            terminal_teardowns: Vec::new(),
            #[cfg(feature = "integrated-browser")]
            browser_factory: None,
            #[cfg(feature = "integrated-browser")]
            browser_parent_lifetime: None,
            window_scale_factor: 1.0,
            window_width: 1_600.0,
            core,
            settings_path: None,
            uncertain_terminal_proofs: HashMap::new(),
            settings_write_blocked: true,
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, Vec::new()),
            sidebar_state: SidebarState::default(),
            active_workspace: None,
            worktree_focus: HashMap::new(),
            github_availability: HashMap::new(),
            repository_generations: HashMap::new(),
            worktree_dialog: None,
            pull_request_dialog: None,
            pull_request_dialog_generation: 0,
            dirty_repository_close: None,
            terminal_close: None,
            terminal_close_attempts: HashSet::new(),
            closed_terminal_cards: HashSet::new(),
            terminal_activity: HashMap::new(),
            activity_origin: Instant::now(),
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
            inspector: None,
            inspector_transition: None,
            context_inspector_open: true,
            context_tab: ContextTab::default(),
            context_inspector_overlay_active: false,
            context_inspector_restore_focus: Some(context_inspector_toggle_id()),
            settings_category: SettingsCategory::Appearance,
            keyboard_modifiers: keyboard::Modifiers::empty(),
            settings_feedback: None,
            status: None,
        }
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
    fn restored_terminal_preserves_a_minimum_width_fraction_on_wide_viewports() {
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
        assert_eq!(workspace.geometry().card(card_id).unwrap().width(), 480.0);
    }
    #[test]

    fn terminal_resize_updates_rendered_and_durable_width_within_bounds() {
        let mut app = context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.workspace.resize_viewport(1_000.0);
        let limits = app.workspace.fractional_width_limits();

        let _ = app.update(Message::ResizeCard { card, width: 640.0 });
        assert_eq!(app.workspace.geometry().card(card).unwrap().width(), 640.0);
        assert_eq!(app.card_state(card).unwrap().width_fraction, 0.64);

        let _ = app.update(Message::ResizeCard { card, width: 10.0 });
        assert_eq!(
            app.workspace.geometry().card(card).unwrap().width(),
            limits.minimum
        );
        assert_eq!(
            app.card_state(card).unwrap().width_fraction,
            limits.minimum / 1_000.0
        );

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

    fn task_app_message(task: Task<Message>) -> Message {
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
}
