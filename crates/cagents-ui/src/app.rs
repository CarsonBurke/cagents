mod account_ui;
mod agent_ui;
mod automation_approval;
mod automation_ui;
mod branch_ui;
#[cfg(feature = "integrated-browser")]
mod browser_import_ui;
#[cfg(test)]
mod channel_open_workload;
mod channel_ui;
mod chat_close;
mod chat_ui;
mod chrome_ui;
mod context_sections;
mod daemon_health_ui;
mod daemon_recovery;
mod daemon_stop;
mod device_ui;
pub(crate) mod dialog_ui;
#[cfg(test)]
mod dispatch_tests;
mod inspector_activity;
use crate::file_manager;
#[cfg(test)]
use crate::file_manager::open_system_url_with_opener;
#[cfg(feature = "integrated-browser")]
mod browser_backend;
mod browser_host;
mod file_ui;
#[cfg(any(debug_assertions, feature = "development"))]
mod fixture_seed;
mod git_ui;
mod history_ui;
#[cfg(feature = "latency-harness")]
mod latency_ui;
mod notification_ui;
mod operation_recovery_ui;
mod palette_ui;
mod pane_inspector;
mod pr_checkout_ui;
mod project_ui;
#[cfg(any(test, feature = "development"))]
mod projection_oracle;
mod pull_request_ui;
#[cfg(test)]
mod question_tests;
mod refresh_ui;
mod repository_loaded;
mod repository_ui;
mod request_failure;
mod resume_ui;
mod review_request_ui;
#[cfg(test)]
mod safety_tests;
mod saved_terminal;
mod session_recovery;
mod settings_ui;
mod shared_views;
mod sidebar_selection_ui;
mod sidebar_ui;
mod source_control_ui;
mod startup_gate;
mod startup_restore;
mod terminal_adoption;
mod terminal_close;
mod terminal_end_ui;
mod terminal_failure;
#[cfg(test)]
mod terminal_output_tests;
mod terminal_save;
mod terminal_ui;
mod update_resume;
mod update_ui;
mod work_ui;
mod workspace_model_sync;
#[cfg(test)]
use workspace_model_sync::{TestModelRequests, test_model_channel};
mod workspace_switcher;
mod worktree_dialog;
mod worktree_operations;
mod worktree_ui;
pub use daemon_stop::run_daemon_stop_confirmation;

/// Fills the `slot` window's empty session store from a fixture plan and
/// returns what it wrote as one JSON line. Refused by production builds and
/// while that window is open. See docs/fixture-workspace.md.
pub fn seed_fixture_window(
    plan: &Path,
    slot: &str,
    workspace_model_identity: Option<uuid::Uuid>,
) -> Result<String, String> {
    #[cfg(any(debug_assertions, feature = "development"))]
    {
        let seeded = fixture_seed::seed(plan, slot, workspace_model_identity)?;
        serde_json::to_string(&seeded).map_err(|error| error.to_string())
    }
    #[cfg(not(any(debug_assertions, feature = "development")))]
    {
        let _ = (plan, slot, workspace_model_identity);
        Err(crate::fixture_workspace::refusal())
    }
}
use worktree_dialog::{
    WorktreeDialog, WorktreeDraft, WorktreeField, WorktreeInputError, worktree_dialog_focus_id,
};

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    ops::ControlFlow,
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
    design::{self, InspectorLayout, space},
    github::{
        CreatePullRequestRequest, GitHubAvailability, GitHubRepoKey, GitHubService,
        PullRequestCreationOutcome,
    },
    inspector::{self, InspectorAction},
    instance,
    lifecycle::{LifecycleAction, LifecycleBudget, LifecycleManager},
    location::SanitizedLocation,
    lsp::{LspCoordinates, LspDocumentId, LspError, LspEvent, LspManager, LspPosition, LspStatus},
    model::{
        CardId, CardKind, CardState, Existence, Exposure, HostSessionProof, LaunchRecipe,
        ProjectId, ProjectKind, ProjectState, RepositoryView, Residency, RuntimeError,
        TerminalContinuity, TerminalExit, TerminalId, TerminalSession, WorkspaceDestination,
        WorkspaceState, WorktreeState,
    },
    pull_request_review,
    repository::{
        CancellationToken, ChangeArea, CommitOutcome, CommitRequest, CommitSummary,
        CreateWorktreeRequest, DiffBasis, HistoryRequest, HistoryRevision, MutationOutcome,
        PushOutcome, PushRequest, RepoPath, RepositoryId, RepositoryInfo, RepositoryService,
        RepositorySnapshot, StageRequest, StatusSnapshot, UnstageRequest, WorktreeCreationOutcome,
        WorktreeRecord,
    },
    repository_widget::{self, LspIntent, RepositoryAction, RepositoryRuntime},
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
    ui_controls::Icon,
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

const APPLICATION_ICON_PNG: &[u8] = include_bytes!("../../../packaging/dev.cagents.Cagents.png");
const APPLICATION_ICON_SIZE: u32 = 128;

/// The name shown to a person, as opposed to `APP_ID`, which only matches the
/// window to its desktop entry.
const APPLICATION_NAME: &str = instance::APPLICATION_NAME;

/// Separates what the window is showing from the application name, following
/// the desktop convention of "<document> — <application>".
const WINDOW_TITLE_SEPARATOR: &str = " — ";

/// Embedded artwork for initial window creation, independent of icon themes.
pub fn application_window_icon() -> window::Icon {
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

#[cfg(feature = "integrated-browser")]
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
    save_failure: Option<terminal_save::Failure>,
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
    fn icon(self) -> Icon {
        match self {
            Self::Success => Icon::Check,
            Self::Warning => Icon::Warning,
            Self::Error => Icon::Error,
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

/// The operation, and the object it acted on, that a notice reports on.
///
/// A notice raised under a key takes the place of whatever that key last
/// raised: a retry that fails again does not stack a second report, and one
/// that succeeds replaces the failure with its confirmation. Resolving a key
/// withdraws its notice without announcing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoticeKey {
    /// Opening the conversation in this card, or switching its terminal to one.
    ChatAttach(CardId),
    /// Saving the account that one sign-in produced.
    SignIn(uuid::Uuid),
    /// Adding an existing account home from the accounts table.
    AccountLink,
    /// Removing or forgetting this account.
    AccountRemove(crate::account::AccountId),
    /// Telling panes whether they are on screen.
    SurfaceVisibility,
    /// Committing this exact project/worktree/path identity.
    GitCommit(ActiveWorktreeKey),
    /// Saving this chat's return to a terminal.
    ChatReturn(CardId),
    /// Closing this chat, which the daemon refused.
    ChatClose(CardId),
    /// Finding the integrated browser, which the last check could not.
    #[cfg(feature = "integrated-browser")]
    BrowserUnavailable,
    /// Saving the reservation for this already running terminal.
    TerminalSave(CardId, TerminalId),
    /// Starting or restoring the terminal in this card.
    TerminalStart(CardId),
    /// Restoring this card from the window's saved state at startup.
    CardRestore(CardId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransientNotice {
    severity: NoticeSeverity,
    /// What happened, in the reader's terms.
    message: String,
    /// The underlying reason as the failing component reported it, for a
    /// bug report or a search; drawn beneath the message.
    details: Option<String>,
    key: Option<NoticeKey>,
}

/// Notices retained for review. Bounded because nothing else bounds them: the
/// shell raises a notice from almost a hundred call sites, several of them on
/// per-frame or per-poll paths.
const NOTICE_LOG_CAPACITY: usize = 24;

/// How long a confirmation stays before it retires on its own: long enough to
/// read one sentence, short enough that a success glyph in the header still
/// means something just happened.
const NOTICE_SUCCESS_LIFETIME: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoggedNotice {
    id: u64,
    notice: TransientNotice,
    /// Repeats of an identical message collapse onto one entry rather than
    /// flooding the log; a failing poll would otherwise evict everything else.
    repeats: u32,
    /// When a confirmation retires. Warnings and errors have none: they wait
    /// for their key to resolve or for the reader to dismiss them.
    expires_at: Option<Instant>,
}

/// The shell's notice history: a bounded ring plus the identity counter that
/// lets a dismiss request name exactly one entry.
#[derive(Debug, Default)]
struct NoticeLog {
    entries: VecDeque<LoggedNotice>,
    next_id: u64,
    /// Since when confirmations have stopped counting down, because nobody
    /// could be reading them: the window is in the background, or the reader
    /// has the notices open. A confirmation gets its whole lifetime in front
    /// of someone who can see it.
    paused_at: Option<Instant>,
}

impl NoticeLog {
    fn record(&mut self, notice: TransientNotice) {
        self.record_at(notice, Instant::now());
    }

    fn record_at(&mut self, notice: TransientNotice, now: Instant) {
        // Recorded while paused, a confirmation's countdown starts with the
        // resume: `resume` moves every deadline on by the pause's length.
        let counted_from = self.paused_at.unwrap_or(now);
        let expires_at = (notice.severity == NoticeSeverity::Success)
            .then(|| counted_from + NOTICE_SUCCESS_LIFETIME);
        // A keyed notice takes over its key's entry, whatever either says, so
        // a key never holds more than one. An unkeyed one only collapses onto
        // an identical message. Scanning the whole log rather than only its
        // tail is what makes that survive interleaving: a poll that raises two
        // failures in turn would otherwise alternate them forever and fill the
        // log with copies. The repeat moves to the back because the user cares
        // that it is happening now, and `recent` reads newest first.
        let previous = self
            .entries
            .iter()
            .position(|entry| match notice.key.as_ref() {
                Some(key) => entry.notice.key.as_ref() == Some(key),
                None => entry.notice == notice,
            });
        if let Some(index) = previous {
            let mut entry = self.entries.remove(index).expect("index came from the log");
            if entry.notice == notice {
                entry.repeats = entry.repeats.saturating_add(1);
                entry.expires_at = expires_at;
                self.entries.push_back(entry);
                return;
            }
            // Replaced: the new report gets an identity of its own, so a
            // Dismiss aimed at the old one cannot remove it.
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.entries.push_back(LoggedNotice {
            id: self.next_id,
            notice,
            repeats: 1,
            expires_at,
        });
        while self.entries.len() > NOTICE_LOG_CAPACITY {
            // The notice just recorded always stays: it is what the reader is
            // being told now. Among the rest the oldest entry of the lowest
            // severity goes first, so a run of confirmations or warnings
            // cannot push out a failure that nothing has resolved.
            let earlier = self.entries.len() - 1;
            let index = self
                .entries
                .iter()
                .take(earlier)
                .enumerate()
                .min_by_key(|(_, entry)| entry.notice.severity)
                .map(|(index, _)| index)
                .expect("an overflowing log holds more than the new entry");
            self.entries.remove(index);
        }
    }

    /// Stops confirmations counting down from `now`; see `paused_at`.
    fn pause(&mut self, now: Instant) {
        self.paused_at.get_or_insert(now);
    }

    /// Lets confirmations count down again from where they stopped.
    fn resume(&mut self, now: Instant) {
        let Some(since) = self.paused_at.take() else {
            return;
        };
        let paused = now.saturating_duration_since(since);
        for expiry in self
            .entries
            .iter_mut()
            .filter_map(|entry| entry.expires_at.as_mut())
        {
            *expiry += paused;
        }
    }

    /// Whether `key` has a report standing.
    fn has(&self, key: NoticeKey) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.notice.key.as_ref() == Some(&key))
    }

    /// Withdraws whatever `key` last reported, because its condition no
    /// longer holds.
    fn resolve(&mut self, key: NoticeKey) {
        self.entries
            .retain(|entry| entry.notice.key.as_ref() != Some(&key));
    }

    /// Retires every confirmation due by `now`. Nothing is due while paused,
    /// which also discards a timer that fired just before the pause began.
    fn expire(&mut self, now: Instant) {
        if self.paused_at.is_some() {
            return;
        }
        self.entries
            .retain(|entry| entry.expires_at.is_none_or(|expiry| expiry > now));
    }

    /// When the next confirmation is due to retire: never while paused, and
    /// never with no confirmation waiting, so neither arms a timer.
    fn next_expiry(&self) -> Option<Instant> {
        if self.paused_at.is_some() {
            return None;
        }
        self.entries
            .iter()
            .filter_map(|entry| entry.expires_at)
            .min()
    }

    fn dismiss(&mut self, id: u64) {
        self.entries.retain(|entry| entry.id != id);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    /// Newest first: the log reads downward from the most recent event.
    ///
    /// Only confirmations retire on a timer. A failure the user never saw is
    /// worse than one that waits: the header carries its severity until the
    /// operation that raised it resolves it or the reader dismisses it.
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

fn terminal_handoff_pending_notice(error: String) -> TransientNotice {
    TransientNotice::error(format!(
        "Terminal handoff acknowledgement is pending; it will retry after saving recovers or the window refreshes: {error}"
    ))
}

/// A card the window's saved state could not restore. The notice stands until
/// the card opens here anyway, as a pane another window or the daemon still
/// holds does, and then the copy that arrived replaces the unreadable one.
fn card_restore_notice(warning: &cagents::persistence::RestoreWarning) -> TransientNotice {
    use cagents::persistence::RestoreProblem;
    let title = &warning.title;
    match &warning.problem {
        RestoreProblem::Missing => TransientNotice::warning(format!(
            "Could not restore “{title}”: its saved record is missing."
        )),
        RestoreProblem::Unreadable(reason) => TransientNotice::warning(format!(
            "Could not restore “{title}”. Its saved data is kept."
        ))
        .with_details(reason.clone()),
    }
    .with_key(NoticeKey::CardRestore(warning.card))
}

fn chat_return_unsaved_notice(card: CardId) -> TransientNotice {
    TransientNotice::error("Return to terminal is not saved")
        .with_details(
            "The terminal keeps running in this pane, but the change is not saved. If saving does not recover, close the pane: that ends the terminal and clears this notice.",
        )
        .with_key(NoticeKey::ChatReturn(card))
}

impl TransientNotice {
    fn new(severity: NoticeSeverity, message: impl Into<String>) -> Self {
        Self {
            severity,
            message: message.into(),
            details: None,
            key: None,
        }
    }

    /// Ties the notice to the operation that raised it; see [`NoticeKey`].
    fn with_key(mut self, key: NoticeKey) -> Self {
        self.key = Some(key);
        self
    }

    fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
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
    value.map_or_else(|| "none in this run".to_owned(), unix_time_label)
}

fn notice_surface<'a>(
    severity: NoticeSeverity,
    body: Element<'a, Message>,
    action: Option<Element<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    let mut children = vec![severity.icon().handle().icon().size(16).into(), body];
    if let Some(action) = action {
        children.push(action);
    }
    let mut surface = widget::container(
        widget::row::with_children(children)
            .align_y(Alignment::Center)
            .spacing(space::S),
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
        Padding::from([0.0, space::M])
    } else {
        Padding::from(space::M)
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
    notice_detail_text(severity, text).into()
}

/// A notice's underlying reason, kept to a few lines so one long error chain
/// cannot crowd the rest of the notices out of the popup. The whole of it is
/// in the log, which records every warning and error with its details.
fn notice_surface_details<'a>(
    severity: NoticeSeverity,
    text: impl Into<Cow<'a, str>> + 'a,
) -> Element<'a, Message> {
    notice_detail_text(severity, text)
        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
            EllipsizeHeightLimit::Lines(NOTICE_DETAIL_LINES),
        ))
        .into()
}

const NOTICE_DETAIL_LINES: usize = 4;

fn notice_detail_text<'a>(
    severity: NoticeSeverity,
    text: impl Into<Cow<'a, str>> + 'a,
) -> widget::SelectableText<'a> {
    widget::selectable_text(text)
        .size(design::TEXT_CAPTION)
        .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
        .width(Length::Fill)
        .class(cosmic::theme::Text::Color(severity.detail_color()))
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

/// A notice's action that is busy while its work runs: it then shows
/// `busy_label` and can't be pressed again. It keeps the room of the wider
/// label throughout, so the notice's other controls stay where they are.
fn notice_surface_busy_action<'a>(
    severity: NoticeSeverity,
    label: &'a str,
    busy_label: &'a str,
    busy: bool,
    accessible_name: &'a str,
    id: widget::Id,
    message: Message,
) -> Element<'a, Message> {
    crate::ui_controls::button::stock_instead(label, busy_label, busy)
        .id(id)
        .name(if busy { busy_label } else { accessible_name })
        .class(notice_surface_button(severity))
        .on_press_maybe((!busy).then_some(message))
        .into()
}

/// The save-health notices: the writer lane's state, and the writes storage
/// refused, which the lane reports beside its state rather than as one.
/// `card_title` names the card a refused terminal identity belongs to.
fn save_health_views<'a>(
    health: &'a SaveHealthState,
    card_title: impl Fn(CardId) -> Option<&'a str>,
    recovery: Option<Element<'a, Message>>,
) -> Vec<Element<'a, Message>> {
    let path = health.database_path.as_deref().map_or_else(
        || "unavailable (the data directory could not be resolved)".to_owned(),
        |path| path.display().to_string(),
    );
    let refused = save_quarantine_view(health, &path, card_title, recovery);
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
        WriteSubject::Workspace { card: None } => "Window layout and panes".to_owned(),
        WriteSubject::Workspace { card: Some(card) } => card_title(*card).map_or_else(
            || "Window layout and panes (one pane was refused)".to_owned(),
            |title| format!("Window layout and panes (pane “{title}” was refused)"),
        ),
        WriteSubject::Pane { card } => card_title(*card).map_or_else(
            || "Latest changes to a pane (storage keeps its last saved state)".to_owned(),
            |title| {
                format!("Latest changes to pane “{title}” (storage keeps its last saved state)")
            },
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
            || "A chat pane's new conversation".to_owned(),
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
        WriteSubject::CardIdentity { .. }
            | WriteSubject::Workspace { card: Some(_) }
            | WriteSubject::Pane { .. }
    ) {
        line.push_str(" · closing the pane clears it");
    }
    match write.held_back {
        0 => {}
        1 => line.push_str(" · 1 worktree waits on it"),
        count => line.push_str(&format!(" · {count} worktrees wait on it")),
    }
    // The rule is what to report; on its own line it stays apart from the
    // remedies above, and from the next write's subject below.
    line.push_str(&format!("\nRule: {}", write.check));
    line
}

fn save_quarantine_view<'a>(
    health: &'a SaveHealthState,
    path: &str,
    card_title: impl Fn(CardId) -> Option<&'a str>,
    recovery: Option<Element<'a, Message>>,
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
    // A requested retry keeps its button, busy, so the recovery action
    // beside it stays put and the button itself says the retry is under
    // way; with the retry on the lane's notice, this one says so.
    let guidance = if health.retry_requested && lane_offers_retry {
        "Retry requested; waiting for the session writer."
    } else {
        QUARANTINE_GUIDANCE
    };
    let action =
        ((health.offers_retry() || health.retry_requested) && !lane_offers_retry).then(|| {
            notice_surface_busy_action(
                NoticeSeverity::Error,
                "Retry save",
                "Retrying save…",
                health.retry_requested,
                "Offer the refused changes to storage again",
                widget::Id::new("session-save.retry"),
                Message::RetrySessionSave,
            )
        });
    let actions: Vec<_> = action.into_iter().chain(recovery).collect();
    let action = (!actions.is_empty()).then(|| {
        widget::row::with_children(actions)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into()
    });
    let mut body = vec![
        notice_body(title),
        notice_surface_detail(NoticeSeverity::Error, detail.join("\n")),
        notice_surface_detail(NoticeSeverity::Error, guidance),
    ];
    // Keep the explanation readable in the bounded Notices popover. Multiple
    // actions alongside it would reserve almost the entire text width.
    if let Some(action) = action {
        body.push(action);
    }
    Some(notice_surface(
        NoticeSeverity::Error,
        widget::column::with_children(body)
            .spacing(space::XS)
            .width(Length::Fill)
            .into(),
        None,
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
            // A requested retry keeps its button, busy, so the notice
            // doesn't jump while the writer works, and the button says
            // the retry is under way.
            let guidance = category.guidance();
            let action = Some(notice_surface_busy_action(
                NoticeSeverity::Error,
                "Retry save",
                "Retrying save…",
                health.retry_requested,
                "Retry saving the session",
                widget::Id::new("session-save.retry"),
                Message::RetrySessionSave,
            ));
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
                widget::button::icon(Icon::Close)
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
    Some(notice_surface(
        severity,
        widget::column::with_children(vec![
            notice_body(title),
            notice_surface_detail(severity, detail),
            notice_surface_detail(severity, guidance),
        ])
        .spacing(space::XS)
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

/// How much of the workspace projection an update has to rebuild. Busy
/// terminals wake about once per frame each, so a wake that changed nothing a
/// projection reads must not pay for every card's sidebar, chrome and
/// protection bookkeeping.
///
/// Every update projects `Full` unless what handled its message declares
/// less: a handler that knows what it changed returns its projection beside
/// its task, as [`Cagents::update_chat_ui`] and [`Cagents::update_chrome_ui`]
/// do, and `update` projects that instead. A message nothing declares for keeps the
/// full projection, so a new or changed handler is correct before it is
/// fast. Tests check every narrower declaration against the full projection
/// (`projection_oracle`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Projection {
    /// Nothing any projection reads changed: grid output, repaint ticks.
    Unchanged,
    /// Only a runtime-reported card title changed. Card chrome and the window
    /// title are the only projections that read it.
    Chrome,
    /// Stable shared presentation, including retained local scope members.
    ScopedChrome(Vec<CardId>),
    /// Only a terminal's transcript probe changed, or its retry time came.
    /// Nothing but the probe stage reads probes, and that stage starts the
    /// probes now due.
    TranscriptProbes,
    /// Anything else, including every user action.
    Full,
}

impl Projection {
    /// The variant's name, which the UI thread trace reports.
    fn name(&self) -> &'static str {
        match self {
            Self::Unchanged => "Unchanged",
            Self::Chrome => "Chrome",
            Self::ScopedChrome(_) => "ScopedChrome",
            Self::TranscriptProbes => "TranscriptProbes",
            Self::Full => "Full",
        }
    }

    /// The projection that brings both declarations' changes up to date:
    /// either one when the other changed nothing, one declared twice, and
    /// otherwise the full projection.
    fn widen(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unchanged, projection) | (projection, Self::Unchanged) => projection,
            (projection, other) if projection == other => projection,
            _ => Self::Full,
        }
    }
}

/// What the messages dispatched since the last projection owe it. `update`
/// owes what its own message's handler declares, and each message a handler
/// dispatches within it adds its own, so the update projects once for all
/// of them (`Cagents::dispatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwedProjection {
    projection: Projection,
    /// The full projection is owed only for the camera: panning and
    /// resizing the workspace, which move no label or binding.
    camera_only: bool,
    /// A message that moves the focus or the sidebar's rows owes the
    /// sidebar's row selection a reconciliation.
    reconcile_row_selection: bool,
    /// The shared workspace model changed, so the replica snapshot is
    /// materialized before chrome reads it.
    model_update: bool,
    /// The window's saved view changed. It is captured once the projection
    /// has rebound what the view shows, such as the source-control panel's
    /// worktree and its filter, so the save never holds the binding being
    /// replaced.
    save_view: bool,
}

impl OwedProjection {
    /// The full projection, owed for more than the camera.
    fn full() -> Self {
        Self {
            projection: Projection::Full,
            camera_only: false,
            reconcile_row_selection: false,
            model_update: false,
            save_view: false,
        }
    }

    /// This projection, then a save of the window's view.
    fn saving_view(self) -> Self {
        Self {
            save_view: true,
            ..self
        }
    }

    /// What two dispatched messages owe between them.
    fn merge(self, other: Self) -> Self {
        let camera_only = self.owes_only_camera() && other.owes_only_camera();
        let projection = self.projection.widen(other.projection);
        Self {
            camera_only: camera_only && projection == Projection::Full,
            projection,
            reconcile_row_selection: self.reconcile_row_selection || other.reconcile_row_selection,
            model_update: self.model_update || other.model_update,
            save_view: self.save_view || other.save_view,
        }
    }

    /// Whether this owes nothing beyond what moving the camera does.
    fn owes_only_camera(&self) -> bool {
        self.projection == Projection::Unchanged || self.camera_only
    }
}

/// What `update` notes before dispatching its message, for the projection
/// and the postlude that follow it.
struct UpdateEntry {
    notice_control: Option<NoticeSeverity>,
    /// The update's workspace pipeline timing is on, so its chrome
    /// projection times its phases too.
    timed: bool,
    settled_projection: Task<Message>,
    #[cfg(any(test, feature = "development"))]
    oracle_message: Option<Box<str>>,
}

/// What an extracted `update` handler returns; its arm takes it with `handled!`.
type Handled = std::ops::ControlFlow<Task<Message>, Task<Message>>;

/// Takes an extracted `update` handler's result. `Break` returns from the
/// dispatch closure at once, skipping the projection, exactly as a `return`
/// written inside the arm would; `Continue` is the arm's value.
macro_rules! handled {
    ($handler:expr) => {
        match $handler {
            std::ops::ControlFlow::Break(task) => return task,
            std::ops::ControlFlow::Continue(task) => task,
        }
    };
}

/// Evaluates a stage of an update's projection, timed in the UI thread trace.
macro_rules! projection_stage {
    ($stage:literal, $work:expr) => {{
        let _stage = crate::performance::ui_trace::projection_stage($stage);
        $work
    }};
}

/// Runtime-reported terminal titles can animate several times a second
/// (spinners, step counters). The durable card follows each one at once, so
/// chrome stays current, but saving it, and publishing it to shared views,
/// waits for this window: a burst of reported titles costs at most one save
/// per window, and the last title is saved within one window of the burst
/// ending. Any earlier save carries the titles with it.
const REPORTED_TITLE_SAVE_DELAY: Duration = Duration::from_secs(3);

/// Saving of runtime-reported titles, deferred by [`REPORTED_TITLE_SAVE_DELAY`].
#[derive(Debug, Default)]
struct ReportedTitleSave {
    /// Cards whose reported title changed since the last save that published
    /// titles. Shared views hold those titles back until that save.
    cards: HashSet<CardId>,
    /// A `ReportedTitleSaveDue` timer is outstanding. Kept apart from
    /// `cards` so a burst interleaved with other saves still arms one timer.
    scheduled: bool,
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

/// Inclusive UI-thread wall time for a shared-model notification, including
/// dispatch and projection. This does not measure rendering or presentation.
struct WorkspaceUpdateTiming(std::time::Instant, &'static str);

thread_local! {
    static SNAPSHOT_CAPTURE_PARENT: std::cell::Cell<&'static str> = const { std::cell::Cell::new("outside_update") };
}

struct SnapshotCaptureParent(Option<&'static str>);
impl SnapshotCaptureParent {
    fn enter(message: &Message) -> Self {
        if !tracing::enabled!(target: "cagents::workspace_pipeline", tracing::Level::DEBUG) {
            return Self(None);
        }
        let parent = match message {
            Message::WorkspaceModelObserved(_) => "observer",
            Message::ReplicaCaptureDue(_) => "replica_timer",
            _ => "other_update",
        };
        Self(Some(
            SNAPSHOT_CAPTURE_PARENT.with(|active| active.replace(parent)),
        ))
    }
}
impl Drop for SnapshotCaptureParent {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            SNAPSHOT_CAPTURE_PARENT.with(|active| active.set(previous));
        }
    }
}

impl Drop for WorkspaceUpdateTiming {
    fn drop(&mut self) {
        tracing::debug!(
            target: "cagents::workspace_pipeline",
            stage = self.1,
            elapsed_ns = self.0.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            "Applied a shared workspace notification"
        );
    }
}

/// Opt-in phase timing for a selected projection, or for a save and the
/// complete projection it publishes. Disabled tracing does not read the clock
/// or allocate, and phase fields do not replace inclusive time.
struct WorkspaceProjectionTiming(Option<std::time::Instant>);

impl WorkspaceProjectionTiming {
    fn new(timed: bool) -> Self {
        Self(
            (timed
                && tracing::enabled!(target: "cagents::workspace_pipeline", tracing::Level::DEBUG))
            .then(std::time::Instant::now),
        )
    }

    fn mark(&mut self, phase: &'static str) {
        let Some(started) = self.0.as_mut() else {
            return;
        };
        let now = std::time::Instant::now();
        tracing::debug!(target: "cagents::workspace_pipeline", stage = phase,
            phase_us = now.duration_since(*started).as_micros().min(u64::MAX as u128) as u64,
            "Projected shared presentation phase");
        *started = now;
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

/// The channels holding `card`. An open card's unread update and waiting
/// agent mark every one of their sidebar rows.
fn channels_holding(
    channels: &[crate::model::ProjectChannel],
    card: CardId,
) -> impl Iterator<Item = &crate::model::ProjectChannel> {
    channels
        .iter()
        .filter(move |channel| channel.cards.contains(&card))
}

fn terminal_completed(session: &TerminalSession) -> bool {
    matches!(&session.continuity,
        TerminalContinuity::RelaunchAvailable { last_exit: Some(exit), .. }
        | TerminalContinuity::Exited { last_exit: exit }
        if exit.code == Some(0) && exit.signal.is_none())
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
        CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
            Residency::Hot
        }
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

/// Where a pane is and what its runtime holds, for this window's run only.
///
/// Storage never kept either: a restored card is offscreen and cold until this
/// run's lifecycle says otherwise. Keeping them out of [`WorkspaceState`]
/// keeps every scroll, channel switch and hydration from copying and saving
/// the whole workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresence {
    exposure: Exposure,
    residency: Residency,
}

impl PanePresence {
    const ABSENT: Self = Self {
        exposure: Exposure::Offscreen,
        residency: Residency::Cold,
    };
}

impl Default for PanePresence {
    fn default() -> Self {
        Self::ABSENT
    }
}

/// How long a change of where this window is - its focus, channel or
/// worktree - waits to be saved. Navigation comes in bursts: a held key, a
/// pointer moving across one pane after another, a person clicking down the
/// sidebar. Each save projects every shared view and copies the workspace and
/// every chat draft on the UI thread, so a burst is saved once, at its first
/// move plus this delay. Any other save in between carries it along, and
/// nothing outside this window reads it before then. A channel name another
/// window gave, which the shared registry already holds, waits the same way.
const NAVIGATION_SAVE_DELAY: Duration = Duration::from_secs(1);

/// What a save writes that a projection reads.
///
/// The navigation it saves was projected when it happened. A save then
/// snapshots the strip into the active channel, which can reorder its panes
/// and drops closed cards from every channel, and prunes pending terminal
/// creations; worktree protections read where editors are
/// (`repository_navigation`). It also writes where the window is
/// (`active_worktree`, `worktree_focus`), its history, drafts and shared-view
/// bookkeeping, none of which a projection reads. When none of these changed,
/// the save leaves nothing to project.
struct SavedProjectionInputs {
    channels: Vec<crate::model::ProjectChannel>,
    focused: Option<CardId>,
    pending_terminal_creations: HashSet<CardId>,
    repository_navigation: HashMap<CardId, crate::model::RepositoryNavigation>,
}

impl SavedProjectionInputs {
    fn capture(app: &Cagents) -> Self {
        Self {
            channels: app.durable.channels.clone(),
            focused: app.durable.focused,
            pending_terminal_creations: app.durable.pending_terminal_creations.clone(),
            repository_navigation: app.durable.repository_navigation.clone(),
        }
    }

    fn unchanged(&self, app: &Cagents) -> bool {
        let durable = &app.durable;
        self.channels == durable.channels
            && self.focused == durable.focused
            && self.pending_terminal_creations == durable.pending_terminal_creations
            && self.repository_navigation == durable.repository_navigation
    }
}

/// What a save deferred by [`NAVIGATION_SAVE_DELAY`] has to do when it falls
/// due. Ordered so that merging two requests keeps the larger one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeferredSave {
    /// Hand the writer this window's snapshot. The change is to state no
    /// shared view reads, such as a channel's name or where an editor is, so
    /// projecting every view would publish nothing; with a long saved
    /// history, that projection is nearly all of a save's time on the UI
    /// thread.
    Snapshot,
    /// Save as [`Cagents::persist`] does, publishing every view: a change of
    /// channel, worktree or focus can move panes between views.
    Views,
}

#[derive(Default)]
struct ReplicaCapture {
    pending: Option<ReplicaSnapshotDebt>,
    generation: u64,
    timer_armed: bool,
    rejected: bool,
    #[cfg(test)]
    captures: usize,
    #[cfg(test)]
    offers: usize,
}

struct ReplicaSnapshotDebt {
    changes: crate::session::SnapshotChanges,
    registry: uuid::Uuid,
    workspace: crate::model::WorkspaceId,
    writer: crate::session::WriterIdentity,
}

/// Whether a focus change reads the newly focused pane's update at once, or
/// leaves that to the attention dwell once the pane has stayed visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusRead {
    Now,
    AfterDwell,
}

/// What an update changed in the durable workspace beside focus, which decides
/// when [`Cagents::apply_viewport`] hands the workspace to the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableChange {
    /// Nothing: a change of focus alone is saved within
    /// [`NAVIGATION_SAVE_DELAY`].
    None,
    /// The channel or worktree this window shows, or a utility pane shown in
    /// it, saved within [`NAVIGATION_SAVE_DELAY`] like focus.
    Navigation,
    /// Anything else, saved now.
    Content,
}

const WORKSPACE_HISTORY_LIMIT: usize = 128;

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

fn card_control_id(id: CardId, control: &str) -> widget::Id {
    widget::Id::new(format!("card.{id}.{control}"))
}

fn card_accessible_name(type_label: &str, title: &str, caption: &str, focused: bool) -> String {
    format!(
        "{type_label} pane {title}, {caption}, {}",
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
    format!("Close {type_label} pane {title}")
}

fn card_kind_labels(kind: Option<&CardKind>) -> CardLabels {
    match kind {
        Some(CardKind::Terminal(_)) => CardLabels {
            type_label: "Terminal",
            icon: Icon::Terminal,
            starting: "Starting terminal…",
            unavailable: "Terminal unavailable",
        },
        Some(CardKind::Chat(_)) => CardLabels {
            type_label: "Chat",
            icon: Icon::Chat,
            starting: "Connecting to agent…",
            unavailable: "Agent connection unavailable",
        },
        Some(CardKind::Browser(_)) => CardLabels {
            type_label: "Browser",
            icon: Icon::Browser,
            starting: "Restoring browser…",
            unavailable: "Browser unavailable",
        },
        Some(CardKind::Repository(_)) => CardLabels {
            type_label: "Repository",
            icon: Icon::Folder,
            starting: "Loading repository…",
            unavailable: "Repository unavailable",
        },
        Some(CardKind::PullRequests(_)) => CardLabels {
            type_label: "Pull requests",
            icon: Icon::PullRequest,
            starting: "Loading pull requests…",
            unavailable: "Pull requests unavailable",
        },
        Some(CardKind::Settings) => CardLabels {
            type_label: "Settings",
            icon: Icon::Settings,
            starting: "Loading settings…",
            unavailable: "Settings unavailable",
        },
        Some(CardKind::Accounts) => CardLabels {
            type_label: "Accounts",
            icon: Icon::Avatar,
            starting: "Loading accounts…",
            unavailable: "Accounts unavailable",
        },
        Some(CardKind::Automations) => CardLabels {
            type_label: "Automations",
            icon: Icon::Alarm,
            starting: "Loading automations…",
            unavailable: "Automations unavailable",
        },
        Some(CardKind::Work) => CardLabels {
            type_label: "Work",
            icon: Icon::Board,
            starting: "Loading the Work board…",
            unavailable: "Work board unavailable",
        },
        None => CardLabels {
            type_label: "Pane",
            icon: Icon::Grid,
            starting: "Starting pane…",
            unavailable: "Pane unavailable",
        },
    }
}

fn card_kind_caption(kind: &CardKind) -> Cow<'_, str> {
    CaptionSource::of_kind(kind).caption()
}

/// What a card's caption is formatted from.
///
/// The update projection rebuilds every card's chrome, so it compares this
/// with the source the cached caption was built from: an unchanged caption
/// costs no path formatting or allocation.
#[derive(Debug, Clone, PartialEq)]
enum CaptionSource<'a> {
    Directory(Cow<'a, Path>),
    /// A terminal brought back by an upgrade, in its launch directory.
    Relaunched(Cow<'a, Path>),
    /// The file an editor shows, relative to its worktree.
    File {
        worktree: Cow<'a, Path>,
        path: Cow<'a, Path>,
    },
    Location(Cow<'a, Url>),
    /// Neither pane belongs to a place: both apply to the whole application.
    ApplicationWide,
}

impl<'a> CaptionSource<'a> {
    fn of_kind(kind: &'a CardKind) -> Self {
        match kind {
            CardKind::Terminal(session) => {
                Self::Directory(Cow::Borrowed(&session.launch.working_directory))
            }
            CardKind::Chat(session) => {
                Self::Directory(Cow::Borrowed(&session.launch.recipe.working_directory))
            }
            CardKind::Browser(session) => Self::Location(Cow::Borrowed(&session.current_url)),
            CardKind::PullRequests(view) => Self::Directory(Cow::Borrowed(&view.repository)),
            CardKind::Repository(view) => Self::Directory(Cow::Borrowed(&view.worktree)),
            CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
                Self::ApplicationWide
            }
        }
    }

    fn caption(&self) -> Cow<'a, str> {
        match self {
            Self::Directory(directory) => Cow::Owned(display_path(directory).into_owned()),
            Self::Relaunched(directory) => Cow::Owned(format!(
                "Relaunched after upgrade · {}",
                display_path(directory)
            )),
            Self::File { worktree, path } => {
                Cow::Owned(display_path(&worktree.join(path)).into_owned())
            }
            Self::Location(url) => Cow::Owned(SanitizedLocation::from(url.as_ref()).into_inner()),
            Self::ApplicationWide => Cow::Borrowed("Application-wide"),
        }
    }

    fn into_owned(self) -> CaptionSource<'static> {
        match self {
            Self::Directory(directory) => {
                CaptionSource::Directory(Cow::Owned(directory.into_owned()))
            }
            Self::Relaunched(directory) => {
                CaptionSource::Relaunched(Cow::Owned(directory.into_owned()))
            }
            Self::File { worktree, path } => CaptionSource::File {
                worktree: Cow::Owned(worktree.into_owned()),
                path: Cow::Owned(path.into_owned()),
            },
            Self::Location(url) => CaptionSource::Location(Cow::Owned(url.into_owned())),
            Self::ApplicationWide => CaptionSource::ApplicationWide,
        }
    }
}

/// A path as a person would write it, with the home directory as `~`.
///
/// Every path this application shows lives under the user's home, so the
/// absolute prefix is noise that costs the readable tail its space when the
/// label has to ellipsize.
pub(crate) fn display_path(path: &Path) -> Cow<'_, str> {
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

/// The process's starting directory, resolved once.
///
/// Nothing in this application calls `set_current_dir`, so this value cannot
/// change while the process runs. Views that re-run on every redraw read it,
/// so resolving it there would cost a blocking `getcwd` per frame.
static STARTING_DIRECTORY: LazyLock<PathBuf> = LazyLock::new(|| {
    crate::device::home()
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
});

/// A terminal that has never run, under a new identity, waiting to launch `launch`.
pub(super) fn fresh_terminal_session(launch: LaunchRecipe) -> TerminalSession {
    let id = TerminalId::new();
    TerminalSession {
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
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
        predecessor: Some(session.id),
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
        if !crate::device::is_remote()
            && let Ok(canonical) = project.repository.canonicalize()
        {
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
            let width_changed = sample.width != workspace.viewport_width();
            let update = if sample.user_input {
                workspace.native_scroll(sample.offset, sample.width)
            } else {
                workspace.native_layout_scroll(sample.offset, sample.width)
            };
            let synchronize = !sample.user_input
                && (width_changed
                    || (sample.offset - workspace.camera().offset()).abs() > f32::EPSILON);
            (update, synchronize)
        }
        WorkspaceViewportEvent::Resized(width) => (workspace.resize_viewport(width), true),
        // Pointer activation hands the keyboard over itself, through
        // `focus_pane_keyboard_after_entry` and `_after_press`, which leave a
        // control the person chose where it is. The workspace's own request
        // would focus the pane's control regardless.
        WorkspaceViewportEvent::PointerEntered(card) => {
            let update = workspace
                .focus_without_scrolling(card)
                .unwrap_or_else(|_| ViewportUpdate::unchanged());
            workspace.take_widget_focus_request();
            (update, false)
        }
        WorkspaceViewportEvent::PointerFocused { card, .. } => {
            let update = workspace
                .focus(card)
                .unwrap_or_else(|_| ViewportUpdate::unchanged());
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
    restore_warnings: Vec<cagents::persistence::RestoreWarning>,
    /// Settings the file holds that this build could not use. Advisory.
    settings_warnings: Vec<String>,
}

/// Loaded before iced starts: session restoration and file locks never wait on
/// the native event loop. Only successfully restored launches can report ready.
pub struct Startup {
    bootstrap: Bootstrap,
    window_view: Option<WindowView>,
}

#[derive(Debug, Clone, Default)]
struct PullRequestDescription(iced::widget::text_editor::Content);

impl From<String> for PullRequestDescription {
    fn from(value: String) -> Self {
        Self(iced::widget::text_editor::Content::with_text(&value))
    }
}

impl From<&str> for PullRequestDescription {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

impl PartialEq for PullRequestDescription {
    fn eq(&self, other: &Self) -> bool {
        self.0.text() == other.0.text()
    }
}
impl Eq for PullRequestDescription {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestDialog {
    repository: GitHubRepoKey,
    project_id: ProjectId,
    /// The worktree this draft was opened against, when it was opened from one.
    worktree: Option<PathBuf>,
    base: String,
    head: String,
    title: String,
    body: PullRequestDescription,
    draft: bool,
    /// An uncertain remote outcome requires reconciliation before another create.
    retry_confirmation: Option<bool>,
}

impl PullRequestDialog {
    fn can_submit(&self) -> bool {
        !self.title.trim().is_empty()
            && !self.head.trim().is_empty()
            && !self.base.trim().is_empty()
            && self.retry_confirmation != Some(false)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WindowView {
    #[serde(default)]
    user_titles: HashMap<CardId, String>,
    #[serde(default)]
    projects_sidebar_collapsed: bool,
    sidebar_width: f32,
    inspector_width: f32,
    /// Whether the window's one inspector was open, from before each pane
    /// had its own; still written, for an older version reading this back.
    #[serde(default)]
    context_inspector_open: bool,
    /// The panes whose inspectors are open. Absent from a window saved
    /// before each pane had its own. Still written, for an older version
    /// reading this back; this one reads `inspector_choices` when present.
    #[serde(default)]
    open_inspectors: Option<Vec<CardId>>,
    /// The panes whose inspectors the user opened or closed. Absent from a
    /// window saved while inspectors were closed until opened, where a
    /// closed one was not a choice.
    #[serde(default)]
    inspector_choices: Option<pane_inspector::Choices>,
    #[serde(default)]
    context_sections: context_sections::Sections,
    sidebar: sidebar::SidebarView,
    worktree_offsets: HashMap<PathBuf, f32>,
    /// Each channel's strip offset. Absent from a window saved before
    /// channels kept their own.
    #[serde(default)]
    channel_offsets: HashMap<crate::model::ChannelId, f32>,
    /// Absent from a window saved before Settings kept its page.
    #[serde(default, deserialize_with = "sidebar::known_or_default")]
    settings_category: Option<SettingsCategory>,
    #[serde(default)]
    settings_search: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SettingsCategory {
    Notifications,
    General,
    Appearance,
    Terminal,
    Editor,
    NativeChat,
    Inspector,
    Links,
    Keyboard,
    Accounts,
    About,
}

impl SettingsCategory {
    const ALL: [Self; 11] = [
        Self::General,
        Self::Notifications,
        Self::Appearance,
        Self::Editor,
        Self::NativeChat,
        Self::Inspector,
        Self::Terminal,
        Self::Links,
        Self::Keyboard,
        Self::Accounts,
        Self::About,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Notifications => "Notifications",
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Editor => "Editor",
            Self::NativeChat => "Native chat",
            Self::Inspector => "Inspector",
            Self::Links => "Browser & links",
            Self::Keyboard => "Keyboard",
            Self::Accounts => "Accounts",
            Self::About => "About",
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

#[cfg(feature = "integrated-browser")]
fn browser_shortcut(card: CardId, event: &keyboard::Event) -> Option<Message> {
    use keyboard::{Key, key::Named};
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return None;
    };
    if modifiers.logo() || modifiers.shift() {
        return None;
    }
    if modifiers.control() && !modifiers.alt() {
        return match key
            .to_latin(*physical_key)
            .map(|key| key.to_ascii_lowercase())
        {
            Some('l') => Some(Message::FocusBrowserAddress(card)),
            Some('r') => Some(Message::BrowserReload(card)),
            _ => None,
        };
    }
    match key.as_ref() {
        Key::Named(Named::ArrowLeft) if modifiers.alt() && !modifiers.control() => {
            Some(Message::BrowserBack(card))
        }
        Key::Named(Named::ArrowRight) if modifiers.alt() && !modifiers.control() => {
            Some(Message::BrowserForward(card))
        }
        Key::Named(Named::F5) if modifiers.is_empty() => Some(Message::BrowserReload(card)),
        _ => None,
    }
}

/// The first native size is reported as Opened, before any resize. Both
/// events carry logical dimensions and replace the pre-map estimate.
fn window_geometry_message(event: &window::Event) -> Option<Message> {
    match event {
        window::Event::Opened { size, .. } | window::Event::Resized(size) => {
            Some(Message::WindowResized(size.width, size.height))
        }
        _ => None,
    }
}

/// Whether a Wayland compositor fills the screen with a window or tiles its
/// right edge, either of which removes the shell's resize border along it.
fn window_edge_message(event: &iced::Event, window: window::Id) -> Option<Message> {
    use cosmic::cctk::sctk::reexports::csd_frame::WindowState;
    use iced::event::{PlatformSpecific, wayland};

    match event {
        iced::Event::PlatformSpecific(PlatformSpecific::Wayland(wayland::Event::Window(
            wayland::WindowEvent::WindowState(state),
        ))) => Some(Message::WindowEdgeState {
            window,
            filled: state.intersects(WindowState::MAXIMIZED | WindowState::FULLSCREEN),
            right_edge_tiled: state.contains(WindowState::TILED_RIGHT),
        }),
        _ => None,
    }
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
            | RepositoryAction::FileLoadedAt { .. }
            | RepositoryAction::DocumentChanged { .. }
            | RepositoryAction::DocumentReloaded { .. }
            | RepositoryAction::DiffLoaded { .. }
            | RepositoryAction::DiffBasisResolved { .. }
            | RepositoryAction::SaveFinished { .. }
            | RepositoryAction::RetainedSaveFinished { .. }
            // A dropped page render leaves the viewer reading "Rendering
            // page" for a page it will never draw, because the fence that
            // accepts the result is the only thing that clears it.
            | RepositoryAction::PdfPageRendered { .. }
            | RepositoryAction::WorkingDiffDue { .. }
            | RepositoryAction::WorkingDiffLoaded { .. }
            | RepositoryAction::FindLoaded { .. }
            | RepositoryAction::FindReplacementLoaded { .. }
            | RepositoryAction::AutoSaveDue { .. }
            // Refusing these strands the card's file list and markdown preview
            // behind their own debounce until the next self-heal.
            | RepositoryAction::FileListUnchanged { .. }
            | RepositoryAction::MarkdownDue { .. }
            | RepositoryAction::DiagramsRendered { .. }
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
        Ok(PushOutcome::OutcomeUnknown {
            failure: crate::repository::MutationFailure::Reconciliation { reason },
        }) => Some(format!(
            "Push completed, but its published commit is uncertain: {reason}. Refresh to see what Git holds."
        )),
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
        Message::TerminalFailure(action) => action.is_user_action(),
        Message::SavedTerminal(action) => action.is_user_action(),
        Message::NewTerminal
        | Message::OpenRepository
        | Message::FocusCard(_)
        | Message::MovePaneToChannel { .. }
        | Message::PaneDropToChannel { .. }
        | Message::TerminalReviewMode { .. }
        | Message::TerminalDropped { .. }
        | Message::ResizeCard { .. }
        | Message::ResizeCardFinished(_)
        | Message::CyclePaneWidth
        | Message::CloseCard(_)
        | Message::OpenBrowser(_)
        | Message::OpenBrowserInSystem(_)
        | Message::OpenLink(..)
        | Message::OpenDocumentLink(..)
        | Message::PreviewLocalFile(_)
        | Message::OpenTerminalOutput(_)
        | Message::NewBrowser
        | Message::RetryCard(_)
        | Message::FocusAdjacent(_)
        | Message::FocusProjectSearch
        | Message::ToggleContextInspector
        | Message::TogglePaneInspector(_)
        | Message::ClosePaneInspector(_)
        | Message::ReviewFilePanelToggle { .. }
        | Message::ReviewFilePanelResize { .. }
        | Message::ReviewFilePanelResizeFinished
        | Message::ContextSection(_)
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
        Message::TerminalFind(action) => !matches!(action, terminal_ui::Action::ConfirmPaste(..) | terminal_ui::Action::Focus(..) | terminal_ui::Action::DismissContext(..) | terminal_ui::Action::Refresh(..)),
        Message::ChatUi(action) => !action.allowed_while_modal(),
        Message::ChromeUi(action) => !matches!(action, chrome_ui::Action::Close),
        Message::ChannelUi(action) => !action.answers_own_dialog(),
        Message::SidebarSelection(_) => false,
        Message::AccountUi(action) => action.blocked_while_modal(),
        Message::AutomationUi(action) => !action.background() && !action.answers_draft_dialog(),
        Message::WorkUi(action) => !action.background(),
        Message::UpdateUi(update_ui::Action::Apply) => true,
        Message::PullRequestUi(action) => !action.allowed_while_modal(),
        Message::PullRequestReview { action, .. } => !action.allowed_while_modal(),
        Message::Sidebar(action) => !answers_discard_confirmation(action),
        #[cfg(feature = "integrated-browser")]
        Message::BrowserBack(_)
        | Message::BrowserForward(_)
        | Message::BrowserReload(_)
        | Message::BrowserStop(_)
        | Message::FocusBrowserAddress(_)
        | Message::BrowserAddressChanged { .. }
        | Message::BrowserNavigate(_) => true,
        _ => false,
    }
}

/// Whether `message` may change which projects and worktrees attention
/// covers, so a pending notification activation no longer applies.
fn changes_attention_scope(message: &Message) -> bool {
    matches!(
        message,
        Message::RepositoryLoaded { result: Ok(_), .. }
            | Message::DeviceProjectLoaded { result: Ok(_), .. }
            | Message::AttachedWorktreePrsResolved { .. }
            | Message::GitUi(_)
    )
}

/// Timers that exist only to repaint elapsed time or a progress animation.
/// Their handlers change nothing a workspace projection reads.
fn repaint_tick(message: &Message) -> bool {
    match message {
        Message::ChatUi(chat_ui::Action::Tick) => true,
        #[cfg(feature = "integrated-browser")]
        Message::BrowserProgressTick => true,
        _ => false,
    }
}

/// Background polls, and their answers, whose handlers keep to their own
/// state: the automation request reader's, and the accounts' figures, which
/// only the view reads. No sidebar row, card chrome, worktree protection or
/// source-control binding depends on them, so rebuilding those projections
/// for every poll would spend several milliseconds of the UI thread on
/// nothing.
fn background_poll(message: &Message) -> bool {
    matches!(
        message,
        Message::AutomationApproval(
            automation_approval::Action::Tick | automation_approval::Action::Read { .. }
        ) | Message::AccountUi(account_ui::Action::Tick | account_ui::Action::OmpRead(_))
    )
}

fn user_message_blocked_during_update(message: &Message) -> bool {
    user_workspace_message_blocked_while_modal(message)
        || matches!(
            message,
            Message::KeyEvent { .. }
                | Message::Viewport { .. }
                | Message::NewWindow
                | Message::WindowCloseRequested(_)
                | Message::CancelTransientWorkClose
                | Message::ReturnToDeviceBeforeClose
                | Message::ReturnToAutomationBeforeClose
                | Message::DiscardAutomationAndClose(_)
                | Message::CommitClose(_)
                | Message::ConfirmTerminalClose
                | Message::CancelTerminalClose
                | Message::ConfirmChatClose
                | Message::CancelChatClose
                | Message::DontAskAgain(..)
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
                | Message::PullRequestBodyCursor(_)
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
    if let Message::SavedTerminal(action) = message {
        return action.is_user_action();
    }
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
                pull_request_ui::Action::Open { .. }
                    | pull_request_ui::Action::OpenCheck(_)
                    | pull_request_ui::Action::OpenInBrowser(_)
            )
            | Message::ChromeUi(chrome_ui::Action::Launch(_))
            | Message::MovePaneToChannel { .. }
            | Message::PaneDropToChannel { .. }
            | Message::FocusCard(_)
            | Message::TerminalReviewMode { .. }
            | Message::FocusAdjacent(_)
            | Message::Viewport {
                event: WorkspaceViewportEvent::PointerFocused { .. }
                    | WorkspaceViewportEvent::PointerEntered(_),
                ..
            }
            | Message::ToggleContextInspector
            | Message::TogglePaneInspector(_)
            | Message::ClosePaneInspector(_)
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
                    | SidebarAction::OpenCard { .. }
                    | SidebarAction::CreatePullRequest { .. }
                    | SidebarAction::OpenSettings
                    | SidebarAction::OpenAutomations
                    | SidebarAction::OpenWork
            )
    )
}

/// Messages that may run while background repository loads wait for their
/// shared reconciliation: further loads, which join it, and the reconciliation.
fn defers_repository_reconciliation(message: &Message) -> bool {
    matches!(
        message,
        Message::RepositoryLoaded {
            open_cards: false,
            ..
        } | Message::RepositoryReconciliationDue
    )
}

fn completion_navigates(message: &Message) -> bool {
    matches!(
        message,
        Message::PullRequestCheckoutReady { .. }
            | Message::RepositoryLoaded {
                open_cards: true,
                result: Ok(_),
                ..
            }
            | Message::PrWorktreeCreated { .. }
            | Message::WorktreeCreated {
                result: Ok(WorktreeCreationOutcome::Created { .. }),
                ..
            }
            | Message::PullRequestCreated {
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
    WorkStaleAfterDays,
    TerminalFontSize,
    TerminalFontFamily,
    ConfirmMultilinePaste,
    BrowserSearchUrl,
    EditorFontFamily,
    EditorFontSize,
    TerminalScrollbackMib,
    ManagedWorktreeDirectory,
    AutoDeleteMergedWorktrees,
    WorktreeSort,
    SidebarWorktrees,
    DefaultWorktreePane,
    DefaultChannelPane,
    DefaultColumnFraction,
    LinkOpenDestination,
    EditorAutoSave,
    EditorWrapLines,
    DiffHideUnchanged,
    ChatEnterSends,
    ChatExpandToolGroups,
    ChatContentFraction,
    DefaultNativePaneFraction,
    ShowStatusBar,
    AccountUsageDisplay,
    ShowWorktreeChangesIcons,
    ShowMentionedPullRequests,
    InspectorOpenByDefault,
    InspectorShowActivity,
    InspectorShowWork,
    InspectorWidth,
    RelaunchAgentsOnUpgrade,
    CheckForUpdates,
    EditorSyntaxTheme,
    Confirm(settings::Confirmation),
    FocusFollowsMouse,
    LinkOpenGesture,
    PullRequestOpenTarget,
    ShortcutNewTerminal,
    ShortcutNewChannel,
    ShortcutClonePane,
    ShortcutCloseFocusedColumn,
    ShortcutSearchProjects,
    ShortcutFindInPane,
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
    ShortcutCommandPalette,
    ShortcutNewThread,
    ShortcutChatPermissions,
    ShortcutChatModel,
    ShortcutChatAccount,
    ShortcutToggleInspector,
    ShortcutOpenSettings,
    ShortcutMovePaneLeft,
    ShortcutMovePaneRight,
    ShortcutReopenPane,
    ShortcutFocusPane1,
    ShortcutFocusPane2,
    ShortcutFocusPane3,
    ShortcutFocusPane4,
    ShortcutFocusPane5,
    ShortcutFocusPane6,
    ShortcutFocusPane7,
    ShortcutFocusPane8,
    ShortcutFocusPane9,
    ShortcutToggleProjects,
    ShortcutRenamePane,
}

impl SettingsField {
    /// The shortcut table in `KeyboardShortcuts::actions` order.
    const SHORTCUTS: [Self; 38] = [
        Self::ShortcutNewTerminal,
        Self::ShortcutNewChannel,
        Self::ShortcutClonePane,
        Self::ShortcutCloseFocusedColumn,
        Self::ShortcutSearchProjects,
        Self::ShortcutFindInPane,
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
        Self::ShortcutCommandPalette,
        Self::ShortcutNewThread,
        Self::ShortcutChatPermissions,
        Self::ShortcutChatModel,
        Self::ShortcutChatAccount,
        Self::ShortcutToggleInspector,
        Self::ShortcutOpenSettings,
        Self::ShortcutMovePaneLeft,
        Self::ShortcutMovePaneRight,
        Self::ShortcutReopenPane,
        Self::ShortcutFocusPane1,
        Self::ShortcutFocusPane2,
        Self::ShortcutFocusPane3,
        Self::ShortcutFocusPane4,
        Self::ShortcutFocusPane5,
        Self::ShortcutFocusPane6,
        Self::ShortcutFocusPane7,
        Self::ShortcutFocusPane8,
        Self::ShortcutFocusPane9,
        Self::ShortcutToggleProjects,
        Self::ShortcutRenamePane,
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
            Self::Notifications(Field::MessagePreviews) => "Show message previews",
            Self::AppTheme => "App theme",
            Self::UiScale => "Interface scale",
            Self::TerminalLineHeight => "Line height",
            Self::TerminalPadding => "Inner padding",
            Self::TerminalScrollSize => "Scroll size",
            Self::TerminalScrollSpeed => "Scroll speed",
            Self::TerminalCursorStyle => "Cursor shape",
            Self::TerminalTheme => "Color theme",
            Self::MaxHotCards => "Maximum hot panes",
            Self::HibernationDelayMs => "Hibernation delay (ms)",
            Self::WorkStaleAfterDays => "Stale after (days)",
            Self::TerminalFontSize => "Font size",
            Self::TerminalFontFamily => "Font family",
            Self::TerminalScrollbackMib => "Scrollback (MiB)",
            Self::ManagedWorktreeDirectory => "Managed worktree directory",
            Self::WorktreeSort => "Worktree order",
            Self::SidebarWorktrees => "Checkouts in sidebar",
            Self::DefaultWorktreePane => "Default worktree pane",
            Self::DefaultChannelPane => "Default channel pane",
            Self::DefaultColumnFraction => "New pane width",
            Self::LinkOpenDestination => "Open links in",
            Self::EditorAutoSave => "Auto-save",
            Self::EditorWrapLines => "Word wrap",
            Self::DiffHideUnchanged => "Hide unchanged diff lines",
            Self::EditorFontFamily => "Editor font family",
            Self::EditorFontSize => "Editor font size",
            Self::BrowserSearchUrl => "Browser search URL",
            Self::ConfirmMultilinePaste => "Confirm multiline paste",
            Self::ShortcutToggleProjects => "Toggle Projects sidebar",
            Self::ShortcutRenamePane => "Rename focused pane",
            Self::ShortcutCommandPalette => "Command palette",
            Self::ShortcutNewThread => "New thread",
            Self::ShortcutChatPermissions => "Choose chat permission mode",
            Self::ShortcutChatModel => "Choose chat model",
            Self::ShortcutChatAccount => "Choose chat account",
            Self::ShortcutToggleInspector => "Toggle inspector",
            Self::ShortcutOpenSettings => "Open settings",
            Self::ShortcutMovePaneLeft => "Move pane left",
            Self::ShortcutMovePaneRight => "Move pane right",
            Self::ShortcutReopenPane => "Reopen closed pane",
            Self::ShortcutFocusPane1 => "Focus pane 1",
            Self::ShortcutFocusPane2 => "Focus pane 2",
            Self::ShortcutFocusPane3 => "Focus pane 3",
            Self::ShortcutFocusPane4 => "Focus pane 4",
            Self::ShortcutFocusPane5 => "Focus pane 5",
            Self::ShortcutFocusPane6 => "Focus pane 6",
            Self::ShortcutFocusPane7 => "Focus pane 7",
            Self::ShortcutFocusPane8 => "Focus pane 8",
            Self::ShortcutFocusPane9 => "Focus pane 9",
            Self::ChatExpandToolGroups => "Expand tool groups by default",
            Self::ChatContentFraction => "Chat content width",
            Self::DefaultNativePaneFraction => "Native pane minimum width",
            Self::ChatEnterSends => "Chat send key",
            Self::ShowStatusBar => "Status bar",
            Self::AccountUsageDisplay => "Account usage",
            Self::ShowWorktreeChangesIcons => "Worktree changes icons",
            Self::ShowMentionedPullRequests => "Mentioned pull requests",
            Self::InspectorOpenByDefault => "Open with each pane",
            Self::InspectorShowActivity => "Activity",
            Self::InspectorShowWork => "Work",
            Self::InspectorWidth => "Inspector width",
            Self::AutoDeleteMergedWorktrees => "Notify about merged worktrees",
            Self::RelaunchAgentsOnUpgrade => "Unresumable agents",
            Self::CheckForUpdates => "Automatic checks",
            Self::EditorSyntaxTheme => "Syntax theme",
            Self::Confirm(settings::Confirmation::StopWorkingChat) => "Working chats",
            Self::Confirm(settings::Confirmation::CloseActiveTerminal) => "Active processes",
            Self::FocusFollowsMouse => "Pane focus",
            Self::LinkOpenGesture => "System browser gesture",
            Self::PullRequestOpenTarget => "Open pull requests in",
            Self::ShortcutNewTerminal => "New terminal",
            Self::ShortcutNewChannel => "New channel",
            Self::ShortcutClonePane => "Clone focused pane",
            Self::ShortcutCloseFocusedColumn => "Close focused pane",
            Self::ShortcutSearchProjects => "Search projects",
            Self::ShortcutFindInPane => "Find in pane",
            Self::ShortcutPreviousColumn => "Previous pane",
            Self::ShortcutNextColumn => "Next pane",
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
            Self::AccountUsageDisplay => SettingsCategory::Accounts,
            Self::AppTheme
            | Self::UiScale
            | Self::ShowStatusBar
            | Self::ShowWorktreeChangesIcons
            | Self::AutoDeleteMergedWorktrees
            | Self::DefaultColumnFraction => SettingsCategory::Appearance,
            Self::TerminalLineHeight
            | Self::TerminalPadding
            | Self::TerminalScrollSize
            | Self::TerminalScrollSpeed
            | Self::TerminalCursorStyle
            | Self::TerminalTheme
            | Self::TerminalFontSize
            | Self::TerminalFontFamily
            | Self::TerminalScrollbackMib
            | Self::ConfirmMultilinePaste
            | Self::RelaunchAgentsOnUpgrade => SettingsCategory::Terminal,
            Self::MaxHotCards
            | Self::HibernationDelayMs
            | Self::WorkStaleAfterDays
            | Self::ManagedWorktreeDirectory
            | Self::WorktreeSort
            | Self::SidebarWorktrees
            | Self::DefaultWorktreePane
            | Self::DefaultChannelPane
            | Self::Confirm(_)
            | Self::FocusFollowsMouse => SettingsCategory::General,
            Self::ChatEnterSends | Self::ChatExpandToolGroups | Self::ChatContentFraction => {
                SettingsCategory::NativeChat
            }
            Self::DefaultNativePaneFraction => SettingsCategory::Appearance,
            Self::InspectorOpenByDefault
            | Self::InspectorShowActivity
            | Self::InspectorShowWork
            | Self::InspectorWidth
            | Self::ShowMentionedPullRequests => SettingsCategory::Inspector,
            Self::EditorAutoSave
            | Self::EditorWrapLines
            | Self::EditorSyntaxTheme
            | Self::EditorFontFamily
            | Self::EditorFontSize
            | Self::DiffHideUnchanged => SettingsCategory::Editor,
            Self::CheckForUpdates => SettingsCategory::About,
            Self::LinkOpenDestination
            | Self::LinkOpenGesture
            | Self::PullRequestOpenTarget
            | Self::BrowserSearchUrl => SettingsCategory::Links,
            Self::ShortcutNewTerminal
            | Self::ShortcutToggleProjects
            | Self::ShortcutRenamePane
            | Self::ShortcutCommandPalette
            | Self::ShortcutNewThread
            | Self::ShortcutChatPermissions
            | Self::ShortcutChatModel
            | Self::ShortcutChatAccount
            | Self::ShortcutToggleInspector
            | Self::ShortcutOpenSettings
            | Self::ShortcutMovePaneLeft
            | Self::ShortcutMovePaneRight
            | Self::ShortcutReopenPane
            | Self::ShortcutFocusPane1
            | Self::ShortcutFocusPane2
            | Self::ShortcutFocusPane3
            | Self::ShortcutFocusPane4
            | Self::ShortcutFocusPane5
            | Self::ShortcutFocusPane6
            | Self::ShortcutFocusPane7
            | Self::ShortcutFocusPane8
            | Self::ShortcutFocusPane9
            | Self::ShortcutNewChannel
            | Self::ShortcutClonePane
            | Self::ShortcutCloseFocusedColumn
            | Self::ShortcutSearchProjects
            | Self::ShortcutFindInPane
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
    toggle_projects: String,
    rename_pane: String,
    command_palette: String,
    new_native_thread: String,
    chat_permissions: String,
    chat_model: String,
    chat_account: String,
    toggle_inspector: String,
    open_settings: String,
    move_pane_left: String,
    move_pane_right: String,
    reopen_pane: String,
    focus_pane_1: String,
    focus_pane_2: String,
    focus_pane_3: String,
    focus_pane_4: String,
    focus_pane_5: String,
    focus_pane_6: String,
    focus_pane_7: String,
    focus_pane_8: String,
    focus_pane_9: String,
    new_terminal: String,
    new_channel: String,
    clone_pane: String,
    close_focused_column: String,
    search_projects: String,
    find_in_pane: String,
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
            toggle_projects: settings::format_chords(&shortcuts.toggle_projects),
            rename_pane: settings::format_chords(&shortcuts.rename_pane),
            command_palette: settings::format_chords(&shortcuts.command_palette),
            new_native_thread: settings::format_chords(&shortcuts.new_native_thread),
            chat_permissions: settings::format_chords(&shortcuts.chat_permissions),
            chat_model: settings::format_chords(&shortcuts.chat_model),
            chat_account: settings::format_chords(&shortcuts.chat_account),
            toggle_inspector: settings::format_chords(&shortcuts.toggle_inspector),
            open_settings: settings::format_chords(&shortcuts.open_settings),
            move_pane_left: settings::format_chords(&shortcuts.move_pane_left),
            move_pane_right: settings::format_chords(&shortcuts.move_pane_right),
            reopen_pane: settings::format_chords(&shortcuts.reopen_pane),
            focus_pane_1: settings::format_chords(&shortcuts.focus_pane_1),
            focus_pane_2: settings::format_chords(&shortcuts.focus_pane_2),
            focus_pane_3: settings::format_chords(&shortcuts.focus_pane_3),
            focus_pane_4: settings::format_chords(&shortcuts.focus_pane_4),
            focus_pane_5: settings::format_chords(&shortcuts.focus_pane_5),
            focus_pane_6: settings::format_chords(&shortcuts.focus_pane_6),
            focus_pane_7: settings::format_chords(&shortcuts.focus_pane_7),
            focus_pane_8: settings::format_chords(&shortcuts.focus_pane_8),
            focus_pane_9: settings::format_chords(&shortcuts.focus_pane_9),
            new_terminal: settings::format_chords(&shortcuts.new_terminal),
            new_channel: settings::format_chords(&shortcuts.new_channel),
            clone_pane: settings::format_chords(&shortcuts.clone_pane),
            close_focused_column: settings::format_chords(&shortcuts.close_focused_column),
            search_projects: settings::format_chords(&shortcuts.search_projects),
            find_in_pane: settings::format_chords(&shortcuts.find_in_pane),
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
    work_stale_after_days: String,
    terminal_font_size: String,
    terminal_font_family: String,
    editor_font_family: String,
    editor_font_size: String,
    confirm_multiline_paste: bool,
    browser_search_url: String,
    terminal_scrollback_mib: String,
    managed_worktree_directory: String,
    auto_delete_merged_worktrees: bool,
    worktree_sort: String,
    sidebar_worktrees: settings::SidebarWorktrees,
    default_worktree_pane: settings::DefaultPane,
    default_channel_pane: settings::DefaultPane,
    default_column_fraction: String,
    editor_auto_save: bool,
    editor_wrap_lines: bool,
    diff_hide_unchanged: bool,
    editor_syntax_theme: settings::EditorSyntaxTheme,
    /// Whether each routine question is asked, as `AppSettings::asks` reads it.
    confirmations: BTreeMap<settings::Confirmation, bool>,
    chat_enter_sends: bool,
    chat_expand_tool_groups: bool,
    chat_content_fraction: String,
    default_native_pane_fraction: String,
    show_status_bar: bool,
    account_usage_display: settings::AccountUsageDisplay,
    show_worktree_changes_icons: bool,
    show_mentioned_pull_requests: bool,
    inspector_open_by_default: bool,
    inspector_show_activity: bool,
    inspector_show_work: bool,
    inspector_width: String,
    relaunch_agents_on_upgrade: bool,
    check_for_updates: bool,
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
            work_stale_after_days: settings.work_stale_after_days.to_string(),
            terminal_font_size: settings.terminal_font_size.to_string(),
            terminal_font_family: settings.terminal_font_family.clone(),
            editor_font_family: settings.editor_font_family.clone().unwrap_or_default(),
            editor_font_size: settings
                .editor_font_size
                .map(|size| size.to_string())
                .unwrap_or_default(),
            terminal_scrollback_mib: settings.terminal_scrollback_mib.to_string(),
            managed_worktree_directory: settings.managed_worktree_directory.clone(),
            auto_delete_merged_worktrees: settings.auto_delete_merged_worktrees,
            worktree_sort: match settings.worktree_sort {
                settings::WorktreeSort::Newest => "newest",
                settings::WorktreeSort::Oldest => "oldest",
                settings::WorktreeSort::Alphabetical => "alphabetical",
            }
            .into(),
            sidebar_worktrees: settings.sidebar_worktrees,
            default_column_fraction: settings.default_column_fraction.to_string(),
            default_worktree_pane: settings.default_worktree_pane,
            default_channel_pane: settings.default_channel_pane,
            editor_auto_save: settings.editor_auto_save,
            editor_wrap_lines: settings.editor_wrap_lines,
            diff_hide_unchanged: settings.diff_hide_unchanged,
            confirmations: settings::Confirmation::ALL
                .into_iter()
                .map(|kind| (kind, settings.asks(kind)))
                .collect(),
            confirm_multiline_paste: settings.confirm_multiline_paste,
            browser_search_url: settings.browser_search_url.clone(),
            chat_enter_sends: settings.chat_enter_sends,
            chat_expand_tool_groups: settings.chat_expand_tool_groups,
            chat_content_fraction: settings.chat_content_fraction.to_string(),
            default_native_pane_fraction: settings.default_native_pane_fraction.to_string(),
            show_status_bar: settings.show_status_bar,
            account_usage_display: settings.account_usage_display,
            show_worktree_changes_icons: settings.show_worktree_changes_icons,
            show_mentioned_pull_requests: settings.show_mentioned_pull_requests,
            inspector_open_by_default: settings.inspector_open_by_default,
            inspector_show_activity: settings.inspector_show_activity,
            inspector_show_work: settings.inspector_show_work,
            inspector_width: settings.inspector_width.to_string(),
            relaunch_agents_on_upgrade: settings.relaunch_agents_on_upgrade,
            check_for_updates: settings.check_for_updates,
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
            SettingsField::WorkStaleAfterDays => self.work_stale_after_days = value,
            SettingsField::TerminalFontSize => self.terminal_font_size = value,
            SettingsField::TerminalFontFamily => self.terminal_font_family = value,
            SettingsField::ConfirmMultilinePaste => self.confirm_multiline_paste = value == "true",
            SettingsField::BrowserSearchUrl => self.browser_search_url = value,
            SettingsField::EditorFontFamily => self.editor_font_family = value,
            SettingsField::EditorFontSize => self.editor_font_size = value,
            SettingsField::TerminalScrollbackMib => self.terminal_scrollback_mib = value,
            SettingsField::ManagedWorktreeDirectory => self.managed_worktree_directory = value,
            SettingsField::AutoDeleteMergedWorktrees => {
                self.auto_delete_merged_worktrees = value == "true"
            }
            SettingsField::WorktreeSort => self.worktree_sort = value,
            SettingsField::SidebarWorktrees => {
                if let Some(mode) = settings::SidebarWorktrees::from_key(&value) {
                    self.sidebar_worktrees = mode;
                }
            }
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
            SettingsField::DiffHideUnchanged => self.diff_hide_unchanged = value == "true",
            SettingsField::Confirm(kind) => {
                self.confirmations.insert(kind, value == "true");
            }
            SettingsField::ChatEnterSends => self.chat_enter_sends = value == "true",
            SettingsField::ChatExpandToolGroups => self.chat_expand_tool_groups = value == "true",
            SettingsField::ChatContentFraction => self.chat_content_fraction = value,
            SettingsField::DefaultNativePaneFraction => self.default_native_pane_fraction = value,
            SettingsField::ShowStatusBar => self.show_status_bar = value == "true",
            SettingsField::AccountUsageDisplay => {
                if let Some(display) = settings::AccountUsageDisplay::from_key(&value) {
                    self.account_usage_display = display;
                }
            }
            SettingsField::ShowWorktreeChangesIcons => {
                self.show_worktree_changes_icons = value == "true"
            }
            SettingsField::ShowMentionedPullRequests => {
                self.show_mentioned_pull_requests = value == "true"
            }
            SettingsField::InspectorOpenByDefault => {
                self.inspector_open_by_default = value == "true"
            }
            SettingsField::InspectorShowActivity => self.inspector_show_activity = value == "true",
            SettingsField::InspectorShowWork => self.inspector_show_work = value == "true",
            SettingsField::InspectorWidth => self.inspector_width = value,
            SettingsField::RelaunchAgentsOnUpgrade => {
                self.relaunch_agents_on_upgrade = value == "true"
            }
            SettingsField::CheckForUpdates => self.check_for_updates = value == "true",
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
            SettingsField::ShortcutToggleProjects => {
                self.keyboard_shortcuts.toggle_projects = value
            }
            SettingsField::ShortcutRenamePane => self.keyboard_shortcuts.rename_pane = value,
            SettingsField::ShortcutCommandPalette => {
                self.keyboard_shortcuts.command_palette = value
            }
            SettingsField::ShortcutNewThread => self.keyboard_shortcuts.new_native_thread = value,
            SettingsField::ShortcutChatPermissions => {
                self.keyboard_shortcuts.chat_permissions = value
            }
            SettingsField::ShortcutChatModel => self.keyboard_shortcuts.chat_model = value,
            SettingsField::ShortcutChatAccount => self.keyboard_shortcuts.chat_account = value,
            SettingsField::ShortcutToggleInspector => {
                self.keyboard_shortcuts.toggle_inspector = value
            }
            SettingsField::ShortcutOpenSettings => self.keyboard_shortcuts.open_settings = value,
            SettingsField::ShortcutMovePaneLeft => self.keyboard_shortcuts.move_pane_left = value,
            SettingsField::ShortcutMovePaneRight => self.keyboard_shortcuts.move_pane_right = value,
            SettingsField::ShortcutReopenPane => self.keyboard_shortcuts.reopen_pane = value,
            SettingsField::ShortcutFocusPane1 => self.keyboard_shortcuts.focus_pane_1 = value,
            SettingsField::ShortcutFocusPane2 => self.keyboard_shortcuts.focus_pane_2 = value,
            SettingsField::ShortcutFocusPane3 => self.keyboard_shortcuts.focus_pane_3 = value,
            SettingsField::ShortcutFocusPane4 => self.keyboard_shortcuts.focus_pane_4 = value,
            SettingsField::ShortcutFocusPane5 => self.keyboard_shortcuts.focus_pane_5 = value,
            SettingsField::ShortcutFocusPane6 => self.keyboard_shortcuts.focus_pane_6 = value,
            SettingsField::ShortcutFocusPane7 => self.keyboard_shortcuts.focus_pane_7 = value,
            SettingsField::ShortcutFocusPane8 => self.keyboard_shortcuts.focus_pane_8 = value,
            SettingsField::ShortcutFocusPane9 => self.keyboard_shortcuts.focus_pane_9 = value,
            SettingsField::ShortcutNewChannel => self.keyboard_shortcuts.new_channel = value,
            SettingsField::ShortcutClonePane => self.keyboard_shortcuts.clone_pane = value,
            SettingsField::ShortcutCloseFocusedColumn => {
                self.keyboard_shortcuts.close_focused_column = value;
            }
            SettingsField::ShortcutFindInPane => {
                self.keyboard_shortcuts.find_in_pane = value;
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

// A return value its caller consumes at once, so boxing would only add an allocation.
#[allow(clippy::large_enum_variant)]
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
        SettingsField::ShortcutToggleProjects => {
            next.keyboard_shortcuts.toggle_projects = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutRenamePane => {
            next.keyboard_shortcuts.rename_pane = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutCommandPalette => {
            next.keyboard_shortcuts.command_palette = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutChatAccount => {
            next.keyboard_shortcuts.chat_account = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutChatModel => {
            next.keyboard_shortcuts.chat_model = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutChatPermissions => {
            next.keyboard_shortcuts.chat_permissions = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutNewThread => {
            next.keyboard_shortcuts.new_native_thread = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutToggleInspector => {
            next.keyboard_shortcuts.toggle_inspector = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutOpenSettings => {
            next.keyboard_shortcuts.open_settings = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutMovePaneLeft => {
            next.keyboard_shortcuts.move_pane_left = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutMovePaneRight => {
            next.keyboard_shortcuts.move_pane_right = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutReopenPane => {
            next.keyboard_shortcuts.reopen_pane = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane1 => {
            next.keyboard_shortcuts.focus_pane_1 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane2 => {
            next.keyboard_shortcuts.focus_pane_2 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane3 => {
            next.keyboard_shortcuts.focus_pane_3 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane4 => {
            next.keyboard_shortcuts.focus_pane_4 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane5 => {
            next.keyboard_shortcuts.focus_pane_5 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane6 => {
            next.keyboard_shortcuts.focus_pane_6 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane7 => {
            next.keyboard_shortcuts.focus_pane_7 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane8 => {
            next.keyboard_shortcuts.focus_pane_8 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::ShortcutFocusPane9 => {
            next.keyboard_shortcuts.focus_pane_9 = settings::parse_chords(value);
            Ok(())
        }
        SettingsField::Notifications(field) => value
            .parse::<bool>()
            .map(|enabled| next.notifications.set(field, enabled))
            .map_err(|_| "Choose whether this notification is enabled.".to_owned()),
        SettingsField::FocusFollowsMouse => value
            .parse::<bool>()
            .map(|enabled| next.focus_follows_mouse = enabled)
            .map_err(|_| "Choose whether hovering activates panes.".to_owned()),
        SettingsField::Confirm(kind) => value
            .parse::<bool>()
            .map(|ask| next.set_asks(kind, ask))
            .map_err(|_| "Choose whether to ask first.".to_owned()),
        SettingsField::ConfirmMultilinePaste => value
            .parse::<bool>()
            .map(|value| next.confirm_multiline_paste = value)
            .map_err(|_| "Choose whether multiline pastes require confirmation.".to_owned()),
        SettingsField::BrowserSearchUrl => {
            next.browser_search_url = value.to_owned();
            Ok(())
        }
        SettingsField::ChatExpandToolGroups => value
            .parse::<bool>()
            .map(|enabled| next.chat_expand_tool_groups = enabled)
            .map_err(|_| "Choose whether tool groups start expanded.".to_owned()),
        SettingsField::ChatContentFraction => value
            .parse::<f32>()
            .map(|fraction| next.chat_content_fraction = fraction)
            .map_err(|_| "Chat width must be a number.".to_owned()),
        SettingsField::DefaultNativePaneFraction => value
            .parse::<f32>()
            .map(|fraction| next.default_native_pane_fraction = fraction)
            .map_err(|_| "Native pane width must be a number.".to_owned()),
        SettingsField::ChatEnterSends => value
            .parse::<bool>()
            .map(|enabled| next.chat_enter_sends = enabled)
            .map_err(|_| "Choose which key sends a chat message.".to_owned()),
        SettingsField::ShowStatusBar => value
            .parse::<bool>()
            .map(|enabled| next.show_status_bar = enabled)
            .map_err(|_| "Choose whether the status bar is shown.".to_owned()),
        SettingsField::AccountUsageDisplay => settings::AccountUsageDisplay::from_key(value)
            .map(|display| next.account_usage_display = display)
            .ok_or_else(|| "Choose Left or Used.".to_owned()),
        SettingsField::ShowWorktreeChangesIcons => value
            .parse::<bool>()
            .map(|enabled| next.show_worktree_changes_icons = enabled)
            .map_err(|_| "Choose whether worktree changes icons are shown.".to_owned()),
        SettingsField::ShowMentionedPullRequests => value
            .parse::<bool>()
            .map(|enabled| next.show_mentioned_pull_requests = enabled)
            .map_err(|_| "Choose whether mentioned pull requests are listed.".to_owned()),
        SettingsField::InspectorOpenByDefault => value
            .parse::<bool>()
            .map(|enabled| next.inspector_open_by_default = enabled)
            .map_err(|_| "Choose whether inspectors open with each pane.".to_owned()),
        SettingsField::InspectorShowActivity => value
            .parse::<bool>()
            .map(|enabled| next.inspector_show_activity = enabled)
            .map_err(|_| "Choose whether the inspector shows activity.".to_owned()),
        SettingsField::InspectorShowWork => value
            .parse::<bool>()
            .map(|enabled| next.inspector_show_work = enabled)
            .map_err(|_| "Choose whether the inspector lists work.".to_owned()),
        SettingsField::InspectorWidth => value
            .parse::<f32>()
            .map(|width| {
                next.inspector_width =
                    width.clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH)
            })
            .map_err(|_| "Inspector width must be a number.".to_owned()),
        SettingsField::AutoDeleteMergedWorktrees => value
            .parse::<bool>()
            .map(|enabled| next.auto_delete_merged_worktrees = enabled)
            .map_err(|_| "Choose whether merged worktrees are deleted automatically.".to_owned()),
        SettingsField::RelaunchAgentsOnUpgrade => value
            .parse::<bool>()
            .map(|enabled| next.relaunch_agents_on_upgrade = enabled)
            .map_err(|_| "Choose whether upgrades relaunch unresumable agents.".to_owned()),
        SettingsField::CheckForUpdates => value
            .parse::<bool>()
            .map(|enabled| next.check_for_updates = enabled)
            .map_err(|_| "Choose whether to check for updates automatically.".to_owned()),
        SettingsField::SidebarWorktrees => settings::SidebarWorktrees::from_key(value)
            .map(|mode| next.set_sidebar_worktrees(mode))
            .ok_or_else(|| "Choose Grouped, Expanded, or Hidden.".to_owned()),
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
        SettingsField::EditorFontFamily => {
            next.editor_font_family = (!value.trim().is_empty()).then(|| value.trim().to_owned());
            Ok(())
        }
        SettingsField::EditorFontSize => {
            if value.trim().is_empty() {
                next.editor_font_size = None;
                Ok(())
            } else {
                value.parse::<f32>().map(|size| next.editor_font_size = Some(size)).map_err(|_| "Enter an editor font size from 8 to 40, or leave it blank to follow the terminal.".to_owned())
            }
        }
        SettingsField::DiffHideUnchanged => value
            .parse::<bool>()
            .map(|enabled| next.diff_hide_unchanged = enabled)
            .map_err(|_| "Choose whether unchanged diff lines are folded.".to_owned()),
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
            .map_err(|_| "Maximum hot panes must be a whole number.".to_owned()),
        SettingsField::HibernationDelayMs => value
            .parse::<u64>()
            .map(|value| next.hibernation_delay_ms = value)
            .map_err(|_| "Hibernation delay must be milliseconds as a whole number.".to_owned()),
        SettingsField::WorkStaleAfterDays => value
            .parse::<u16>()
            .map(|value| next.work_stale_after_days = value)
            .map_err(|_| "Stale after must be a whole number of days.".to_owned()),
        SettingsField::TerminalFontSize => value
            .parse::<f32>()
            .map(|value| next.terminal_font_size = value)
            .map_err(|_| "Font size must be a number.".to_owned()),
        SettingsField::TerminalFontFamily => {
            next.terminal_font_family = value.to_owned();
            Ok(())
        }
        SettingsField::TerminalScrollbackMib => value
            .parse::<usize>()
            .map(|value| next.terminal_scrollback_mib = value)
            .map_err(|_| "Terminal scrollback must be a whole number.".to_owned()),
        SettingsField::ManagedWorktreeDirectory => {
            next.managed_worktree_directory = value.to_owned();
            Ok(())
        }
        SettingsField::DefaultColumnFraction => value
            .parse::<f32>()
            .map(|value| next.default_column_fraction = value)
            .map_err(|_| "Default pane width must be a number.".to_owned()),
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
            next.keyboard_shortcuts.command_palette =
                next.keyboard_shortcuts.command_palette.clone();
            next.keyboard_shortcuts.toggle_inspector =
                next.keyboard_shortcuts.toggle_inspector.clone();
            next.keyboard_shortcuts.move_pane_right =
                next.keyboard_shortcuts.move_pane_right.clone();
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
        SettingsField::ShortcutFindInPane => {
            next.keyboard_shortcuts.find_in_pane = settings::parse_chords(value);
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
        SettingsField::TerminalScrollbackMib => format!(
            "Scrollback must be 0–{} MiB.",
            grouped_digits(settings::MAX_TERMINAL_SCROLLBACK_MIB)
        ),
        SettingsField::MaxHotCards => format!("{label} must be a whole number, at least 1."),
        SettingsField::HibernationDelayMs => {
            "Hibernation delay must be a whole number of milliseconds.".to_owned()
        }
        SettingsField::WorkStaleAfterDays => format!(
            "{label} must be 1–{} days.",
            crate::work::MAX_STALE_AFTER_DAYS
        ),
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
        if fault.action != action {
            return None;
        }
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
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[derive(Debug, Clone)]
pub enum Message {
    Device(device_ui::Action),
    ReviewFilePanelToggle {
        card: CardId,
        collapsed: bool,
    },
    ReviewFilePanelResize {
        card: CardId,
        width: f32,
    },
    ReviewFilePanelResizeFinished,
    Palette(palette_ui::Action),
    ResumeConversation(resume_ui::Action),
    RunCommand(workspace_widget::GlobalShortcut),
    /// A worktree snapshot for the source-control panel.
    ///
    /// `key` is the binding the request was made against. The panel rebinds
    /// whenever the focused card names a different worktree, so a result whose
    /// key is no longer the bound one is a read of a tree nobody is looking at
    /// and is dropped rather than rendered.
    SourceControlLoaded {
        key: ActiveWorktreeKey,
        request: u64,
        result: Result<SourceControlRead, String>,
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
        operation: uuid::Uuid,
        report: Option<String>,
        /// A commit that succeeded clears the message input; one that did not
        /// must keep it, because the text is the user's work.
        committed: bool,
    },
    NewTerminal,
    TerminalFailure(terminal_failure::Action),
    TerminalSave(terminal_save::Action),
    SavedTerminal(saved_terminal::Action),
    TerminalStarted {
        card: CardId,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
    },
    /// The final screen of a restored ended terminal, or `None` when the
    /// daemon no longer holds one.
    RetainedTerminalAttached {
        card: CardId,
        terminal: TerminalId,
        result: Result<Option<TerminalSurface>, String>,
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
    DeviceProjectLoaded {
        project_id: ProjectId,
        generation: u64,
        result: Result<crate::device_service::Navigation, String>,
    },
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
    OperationRecovery(crate::operation_recovery::Action),
    ResolvedDiffBasis {
        card: CardId,
        basis: DiffBasis,
    },
    Sidebar(SidebarAction),
    /// Folds the Attention list's In progress section, once a probe of the
    /// drawn rows has said whether one of them had keyboard focus.
    FoldInProgress {
        refocus: bool,
    },
    FocusCard(CardId),
    AcknowledgeVisiblePane {
        card: CardId,
        epoch: u64,
    },
    MovePaneToChannel {
        card: CardId,
        channel: crate::model::ChannelId,
    },
    PaneDropToChannel {
        generation: u64,
        card: CardId,
        channel: crate::model::ChannelId,
    },
    TerminalReviewMode {
        card: CardId,
        terminal: crate::model::TerminalId,
        active: bool,
    },
    ResizeCard {
        card: CardId,
        width: f32,
    },
    ResizeCardFinished(CardId),
    CyclePaneWidth,
    CloseCard(CardId),
    CloseCardUnasked(CardId),
    WindowCloseRequested(window::Id),
    CancelTransientWorkClose,
    ReturnToDeviceBeforeClose,
    ReturnToAutomationBeforeClose,
    DiscardAutomationAndClose(u64),
    /// iced has removed a window, and winit has queued its destruction.
    WindowClosed(window::Id),
    NewWindow,
    NewWindowOpened {
        saved: Result<SettingsSaved, String>,
        result: Result<(), String>,
    },
    SharedPreferencesObserved {
        generation: u64,
        settings: AppSettings,
    },
    SettingsSaved {
        generation: u64,
        result: Result<SettingsSaved, String>,
    },
    StartupWindowOpened(window::Id),
    StartupReported(Result<(), String>),
    VerifyStartupPanes,
    StartupStateInspected(startup_gate::Inspection),
    StartupPanesSaved(Result<SettingsSaved, String>, Result<(), String>),
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
    ConfirmChatClose,
    CancelChatClose,
    /// Ticks or clears "Don't ask again" in the open question of this kind.
    DontAskAgain(settings::Confirmation, bool),
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
    /// The delay on saving a change of navigation alone has passed, for the
    /// save armed with this generation; a superseded one is discarded.
    NavigationSaveDue(u64),
    ReplicaCaptureDue(u64),
    /// The delay on saving runtime-reported card titles has passed.
    ReportedTitleSaveDue,
    /// Reconcile the repositories whose background loads finished since the
    /// last reconciliation; see [`Cagents::defer_repository_reconciliation`].
    RepositoryReconciliationDue,
    WorkspaceSwitcherSelected {
        since: Instant,
        index: usize,
    },
    WorkspaceSwitcherActivate {
        since: Instant,
        index: usize,
    },
    /// The modifier holding the workspace switcher open since this instant
    /// was released.
    WorkspaceSwitcherReleased(Instant),
    /// The scrim around the workspace switcher held open since this instant
    /// was clicked.
    WorkspaceSwitcherDismissed(Instant),
    /// The switcher's selection, held open since this instant, may have
    /// rested long enough to bring its pane up for the preview.
    WorkspaceSwitcherRested(Option<Instant>),
    /// The list of the switcher held open since `since` was scrolled to
    /// `offset`, by the wheel or its scrollbar.
    WorkspaceSwitcherScrolled {
        since: Instant,
        offset: f32,
    },
    #[cfg(feature = "integrated-browser")]
    InitializeBrowserFactory,
    #[cfg(feature = "integrated-browser")]
    BrowserFactoryReady {
        generation: uuid::Uuid,
        result: Result<BrowserFactoryBinding, crate::capability::BrowserUnavailable>,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserFactoryPoll(uuid::Uuid),
    #[cfg(feature = "integrated-browser")]
    BrowserFactoryProbed {
        generation: uuid::Uuid,
        operation: uuid::Uuid,
        result: crate::browser::CapabilityProbe,
    },
    WindowScaleFactorChanged(f32),
    WindowResized(f32, f32),
    WindowEdgeState {
        window: window::Id,
        filled: bool,
        right_edge_tiled: bool,
    },
    WindowFilled(window::Id, bool),
    ActivateUi {
        token: Option<String>,
    },
    ActivateSession {
        session: TerminalId,
        token: Option<String>,
    },
    OpenBrowser(Url),
    OpenBrowserInSystem(CardId),
    BrowserSystemOpened {
        card: CardId,
        browser: crate::model::BrowserId,
        opened: bool,
    },
    OpenLink(CardId, Url, LinkOpenGesture),
    OpenDocumentLink(CardId, String),
    PreviewLocalFile(CardId),
    OpenTerminalOutput(CardId),
    ExternalLinkOpened(Result<(), String>),
    FileRevealed(Result<file_manager::RevealOutcome, String>),
    NewBrowser,
    ExternalUrlOpened(Result<(), String>),
    /// A page meant for a browser pane went to the system browser instead,
    /// because the integrated browser wasn't available for `reason`.
    #[cfg(feature = "integrated-browser")]
    BrowserFallbackOpened {
        result: Result<(), String>,
        reason: crate::capability::BrowserUnavailable,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserBack(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserForward(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserReload(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserStop(CardId),
    #[cfg(feature = "integrated-browser")]
    FocusBrowserAddress(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserImport(browser_import_ui::Action),
    RetryCard(CardId),
    /// Enter pressed on an ended terminal's final screen.
    RelaunchFromEndedScreen(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserAddressChanged {
        card: CardId,
        value: String,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserNavigate(CardId),
    #[cfg(feature = "integrated-browser")]
    BrowserAddressFocus {
        card: CardId,
        focused: bool,
    },
    #[cfg(feature = "integrated-browser")]
    BrowserProgressTick,
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
    PrWorktreeCreated {
        origin: pr_checkout_ui::PrCheckoutOrigin,
        repository: PathBuf,
        outcome: WorktreeCreationOutcome,
    },
    WorktreeCreated {
        project_id: ProjectId,
        repository: PathBuf,
        result: Result<WorktreeCreationOutcome, String>,
    },
    PullRequestBaseChanged(String),
    PullRequestHeadChanged(String),
    PullRequestTitleChanged(String),
    PullRequestBodyChanged(iced::widget::text_editor::Action),
    PullRequestBodyCursor(iced::widget::text_editor::Cursor),
    PullRequestDraftChanged(bool),
    PullRequestRetryConfirmed(bool),
    SubmitPullRequest,
    PullRequestCreated {
        dialog: PullRequestDialog,
        generation: u64,
        result: Result<PullRequestCreationOutcome, String>,
    },
    RestorePullRequestDialog {
        dialog: PullRequestDialog,
        generation: u64,
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
    TerminalFind(terminal_ui::Action),
    ZoomFont {
        card: CardId,
        delta: i8,
        editor: bool,
    },
    /// Opens or closes the focused pane's inspector.
    ToggleContextInspector,
    /// Opens or closes this pane's inspector.
    TogglePaneInspector(CardId),
    ClosePaneInspector(CardId),
    ContextSection(context_sections::Action),
    /// The inspector sections were laid out at this height.
    ContextSectionsMeasured(f32),
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
    PullRequestCheckoutReady {
        card: CardId,
        project: ProjectId,
        generation: u64,
        target: pull_request_review::CheckoutTarget,
        result: Result<pull_request_review::checkout::PreparedCheckout, String>,
    },
    PullRequestReview {
        card: CardId,
        action: pull_request_review::Action,
    },
    NativeWindowAction(cosmic::app::Action),
    ChatUi(chat_ui::Action),
    UpdateUi(update_ui::Action),
    UpdateResume(update_resume::Action),
    ChromeUi(chrome_ui::Action),
    ChannelUi(channel_ui::Action),
    SidebarSelection(sidebar_selection_ui::Action),
    BranchUi(branch_ui::Action),
    CommitUi(history_ui::Action),
    RefreshUi(refresh_ui::Action),
    ReviewRequests(review_request_ui::Action),
    GitUi(git_ui::Action),
    NotificationUi(notification_ui::Action),
    DaemonHealth(crate::daemon_health::State),
    /// The desktop's text-cursor blink, or `None` once it states none.
    CursorBlink(Option<cosmic::iced::advanced::text::Blink>),
    DaemonHealthUi(daemon_health_ui::Action),
    AccountUi(account_ui::Action),
    AutomationUi(crate::automation_ui::Action),
    WorkUi(crate::work_ui::Action),
    AutomationApproval(automation_approval::Action),
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
    RetrySharedViewSeeds,
    SessionRecovery(session_recovery::Action),
    AcknowledgeSessionRecovery,
    DismissStatus,
    /// Dismisses exactly one logged notice; a stale id is a no-op.
    DismissNotice(u64),
    DismissAllNotices,
    /// The earliest confirmation in the notice log is due to retire.
    ExpireNotices,
    /// The retiring notices control has finished its conditional focus handoff.
    NoticeControlRetired,
}

/// What a source-control reload read, compared with the snapshot the panel
/// was built from.
#[derive(Debug, Clone)]
pub enum SourceControlRead {
    /// The worktree differs from what the panel shows; rebuild from this.
    Changed(Box<RepositorySnapshot>),
    /// The read equals the snapshot the panel shows, so its model stands.
    Unchanged,
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
    PaneGroup,
    Card(CardId),
    Application(window::Id),
    /// A confirmed channel deletion, which continues once its editors are
    /// saved or discarded.
    Channel(crate::model::ChannelId),
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
    ignored: bool,
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
}

#[cfg(feature = "integrated-browser")]
impl BrowserAddress {
    fn location(url: &Url) -> Self {
        Self::Location {
            authoritative: url.clone(),
            presented: SanitizedLocation::from(url),
        }
    }

    /// Page navigation updates the persisted location independently of an
    /// unsubmitted address draft, including after the field loses focus.
    fn report_location(&mut self, url: &Url) {
        if matches!(self, Self::Location { .. }) {
            *self = Self::location(url);
        }
    }

    fn user_input(value: String) -> Self {
        Self::UserInput(value)
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Location { presented, .. } => presented.as_str(),
            Self::UserInput(value) => value,
        }
    }

    fn editable_str(&self, focused: bool) -> &str {
        if let Self::Location { authoritative, .. } = self {
            let local = match authoritative.host() {
                Some(url::Host::Domain(host)) => {
                    host == "localhost" || host.ends_with(".localhost")
                }
                Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unique_local(),
                None => false,
            };
            if focused || local || authoritative.scheme() == "file" {
                return authoritative.as_str();
            }
        }
        self.as_str()
    }

    /// Resolves the value to navigate without deriving authority from its
    /// redacted presentation. A restored or reported location therefore keeps
    /// its complete URL when the user submits the unchanged address, while a
    /// submitted draft is immediately replaced by its safe presentation.
    #[cfg(test)]
    fn submit(&mut self) -> Result<Url, ()> {
        self.submit_with_search("https://duckduckgo.com/?q={query}")
    }
    fn submit_with_search(&mut self, search_url: &str) -> Result<Url, ()> {
        let result = match self {
            Self::Location { authoritative, .. } => Ok(authoritative.clone()),
            Self::UserInput(value) => {
                crate::location::navigation_url_with_search(value, search_url)
            }
        };

        match result {
            Ok(url) => {
                *self = Self::location(&url);
                Ok(url)
            }
            Err(_) => Err(()),
        }
    }
}

#[cfg(feature = "integrated-browser")]
struct BrowserRuntime {
    handle: BrowserHandle,
    ime: Option<InputMethodState>,
    lifecycle: BrowserLifecycle,
    address: BrowserAddress,
    address_focused: bool,
    address_error: bool,
    progress_phase: f32,
    load_state: Option<String>,
}

enum CardRuntime {
    Loading,
    Failed(terminal_failure::Failure),
    Completed,
    Terminal(TerminalSurface),
    Chat(Box<chat_ui::Runtime>),
    #[cfg(feature = "integrated-browser")]
    Browser(Box<BrowserRuntime>),
    Repository(Box<RepositoryRuntime>),
    PullRequests(Box<PullRequestPane>),
    /// The settings pane. Its drafts are the window's, not the card's, so
    /// the runtime only marks that the card is ready.
    Settings,
    Accounts,
    Automations,
    Work,
}

impl CardRuntime {
    /// The runtime for a utility pane kind.
    fn utility(kind: &CardKind) -> Self {
        match kind {
            CardKind::Settings => Self::Settings,
            CardKind::Accounts => Self::Accounts,
            CardKind::Automations => Self::Automations,
            CardKind::Work => Self::Work,
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
    file_panel_width: f32,
    file_panel_collapsed: Option<bool>,
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

/// Whether a pull request card `width` wide hides its side panel.
fn review_files_collapsed(pane: &PullRequestPane, width: f32) -> bool {
    pane.file_panel_collapsed.unwrap_or(width < 640.0)
}

/// Static chrome strings for a card kind.
struct CardLabels {
    type_label: &'static str,
    icon: Icon,
    starting: &'static str,
    unavailable: &'static str,
}

/// Inputs that can change a terminal's presentation. Ordinary runtime wakes
/// reuse the numbered title instead of allocating it again for every pane.
#[derive(Debug)]
struct TerminalTitleSource {
    reported: String,
    directory: PathBuf,
    program: String,
    ordinal: u32,
    fallback: bool,
}

impl TerminalTitleSource {
    fn fallback(title: &str) -> bool {
        // The host supplies a process/cwd fallback when OSC is absent. A
        // nonempty reported title is authoritative even if it is a basename.
        title.is_empty()
    }

    fn describes(&self, title: &str, directory: &Path, program: &str, ordinal: u32) -> bool {
        let fallback = Self::fallback(title);
        self.reported == title
            && self.ordinal == ordinal
            && self.fallback == fallback
            && (!fallback || (self.directory == directory && self.program == program))
    }
}

/// Widget ids and accessible names for one card.
///
/// Both embed the card id and title, so they are built when a card appears or
/// is renamed instead of once per rendered frame. Reusing an id also keeps the
/// card's accessibility node stable across frames.
#[derive(Debug)]
struct CardChrome {
    tooltip: String,
    focus_id: widget::Id,
    title_id: widget::Id,
    tab_id: widget::Id,
    container_id: widget::Id,
    close_id: widget::Id,
    resize_id: widget::Id,
    inspector_toggle_id: widget::Id,
    inspector_resize_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_back_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_forward_id: widget::Id,
    #[cfg(feature = "integrated-browser")]
    browser_reload_id: widget::Id,
    browser_address_id: widget::Id,
    type_label: &'static str,
    title: String,
    title_width: f32,
    compact_title: Option<String>,
    terminal_ordinal_suffix: Option<String>,
    terminal_title_source: Option<TerminalTitleSource>,
    focused_name: String,
    caption: String,
    /// What `caption` was formatted from; `None` for chrome built from a
    /// caption alone, which is never reused.
    caption_source: Option<CaptionSource<'static>>,
    unfocused_name: String,
    resize_name: String,
    close_name: String,
}

impl CardChrome {
    fn new(id: CardId, type_label: &'static str, title: &str, caption: &str) -> Self {
        Self {
            tooltip: if caption.is_empty() {
                title.to_owned()
            } else {
                format!("{title}\n{caption}")
            },
            focus_id: workspace_widget::card_focus_id(id),
            title_id: card_control_id(id, "title"),
            tab_id: card_control_id(id, "tab"),
            container_id: workspace_widget::card_widget_id(id),
            close_id: card_control_id(id, "close"),
            resize_id: card_control_id(id, "resize"),
            inspector_toggle_id: card_control_id(id, "inspector.toggle"),
            inspector_resize_id: card_control_id(id, "inspector.resize"),
            #[cfg(feature = "integrated-browser")]
            browser_back_id: card_control_id(id, "browser.back"),
            #[cfg(feature = "integrated-browser")]
            browser_forward_id: card_control_id(id, "browser.forward"),
            #[cfg(feature = "integrated-browser")]
            browser_reload_id: card_control_id(id, "browser.reload"),
            browser_address_id: card_control_id(id, "browser.address"),
            type_label,
            title: title.to_owned(),
            title_width: chrome_ui::measure_tab_title(title),
            compact_title: None,
            terminal_ordinal_suffix: None,
            terminal_title_source: None,
            caption: caption.to_owned(),
            caption_source: None,
            focused_name: card_accessible_name(type_label, title, caption, true),
            unfocused_name: card_accessible_name(type_label, title, caption, false),
            resize_name: format!("Resize {type_label} pane {title}"),
            close_name: card_close_name(type_label, title),
        }
    }

    fn with_caption_source(mut self, source: CaptionSource<'static>) -> Self {
        self.caption_source = Some(source);
        self
    }

    fn describes(&self, type_label: &str, title: &str, caption: &CaptionSource<'_>) -> bool {
        self.type_label == type_label
            && self.title == title
            && self.caption_source.as_ref() == Some(caption)
    }

    fn accessible_name(&self, focused: bool) -> &str {
        if focused {
            &self.focused_name
        } else {
            &self.unfocused_name
        }
    }
}

/// Which position-addressed request the editor asked for.
#[derive(Debug, Clone, Copy)]
enum LspRequestKind {
    Completion,
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
    /// configured server claims this file's extension, or another independent
    /// buffer already owns its language-server attachment.
    document: Option<LspDocumentId>,
    /// Independent editor buffers cannot share one language-server URI.
    /// Keep the refused buffer editable and let Restart retry after its owner
    /// closes, without registering its text over the other pane's snapshot.
    blocked_by_other_buffer: bool,
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
    completion_request: Option<(crate::lsp::LspRequestId, LspPosition)>,
    /// Last observed runtime intent; a request is captured against this value.
    completion_intent: u64,
    editor_hover_request: Option<(crate::lsp::LspRequestId, LspPosition)>,
    editor_navigation: Option<crate::lsp::LspRequestId>,
}

impl LspInspectorState {
    /// Drops every answer that describes text the buffer no longer holds.
    ///
    /// Diagnostics are kept and re-dated instead of dropped: the server
    /// replaces them wholesale on its own schedule, and blanking them on each
    /// keystroke would make the list flicker empty while the user typed. They
    /// are refused at the point of use by version rather than erased here.
    fn invalidate_answers(&mut self) {
        self.context.locations = None;
        self.context.completions.clear();
        self.completion_request = None;
        self.editor_hover_request = None;
        self.editor_navigation = None;
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

/// The workspace a sidebar action opens, if it opens one.
fn workspace_destination(action: &SidebarAction) -> Option<WorkspaceDestination> {
    match action {
        SidebarAction::OpenRepository { project_id, .. } => {
            Some(WorkspaceDestination::Folder(*project_id))
        }
        SidebarAction::OpenWorktree { worktree_id, .. } => {
            Some(WorkspaceDestination::Worktree(*worktree_id))
        }
        SidebarAction::OpenChannel { channel_id } => {
            Some(WorkspaceDestination::Channel(*channel_id))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct SourceControlMutation {
    id: uuid::Uuid,
    kind: MutationKind,
    commit_revision: Option<u64>,
}

pub struct Cagents {
    startup_gate: startup_gate::State,
    device_ui: device_ui::State,
    workspace: Workspace,
    durable: Arc<WorkspaceState>,
    cards: HashMap<CardId, CardRuntime>,
    terminal_working_directories: HashMap<CardId, PathBuf>,
    terminal_find: HashMap<CardId, terminal_ui::Find>,
    terminal_review_card: Option<CardId>,
    terminal_review_revision: u64,
    terminal_review_identity: Option<crate::model::TerminalId>,
    terminal_review_input: crate::terminal::ReviewInputBarrier,
    terminal_context: Option<terminal_ui::Context>,
    terminal_paste: Option<(CardId, String)>,
    card_chrome: HashMap<CardId, CardChrome>,
    card_indices: HashMap<CardId, usize>,
    /// The workspace `card_indices` was built from. Holding it weakly pins
    /// its allocation, so a changed workspace always has a different address.
    card_indices_source: std::sync::Weak<WorkspaceState>,
    /// The workspace `sync_chrome` last dropped departed cards' terminal
    /// numbers against, pinned as `card_indices_source` is.
    terminal_ordinals_source: std::sync::Weak<WorkspaceState>,
    navigation_cache: sidebar::NavigationCache,
    /// Where each folder Projects lists sits, for the inspectors' activity
    /// blocks to name a pane's place without scanning every project.
    place_index: inspector_activity::PlaceIndex,
    palette: palette_ui::State,
    recently_closed: Vec<(CardState, Option<WorkspaceDestination>)>,
    /// Where each open pane is on screen and what its runtime holds. Both
    /// describe this window's run only, so they live here rather than in the
    /// durable workspace: changing them neither copies the workspace nor
    /// saves it. A card with no entry is offscreen and cold.
    presence: HashMap<CardId, PanePresence>,
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
    /// A save of navigation alone is scheduled; see [`NAVIGATION_SAVE_DELAY`].
    navigation_save: Option<DeferredSave>,
    replica_capture: ReplicaCapture,
    /// Which timer is the scheduled save's: one armed for a burst that a save
    /// already wrote must not cut the next burst short.
    navigation_save_generation: u64,
    /// Repositories whose background load finished and whose shared model
    /// reconciliation waits for [`Message::RepositoryReconciliationDue`], in
    /// load order. Any message other than another background load reconciles
    /// them first.
    pending_repository_reconciliation: Vec<PathBuf>,
    /// Where editors were when a save last dropped them from durable: a
    /// closed editor, or one still loading. A shared view reopening the same
    /// pane, or a retried attach, puts the editor back there.
    released_repository_navigation: HashMap<CardId, crate::model::RepositoryNavigation>,
    /// A focused chat pane whose message input was not drawn when the pane
    /// was focused, which takes the keyboard once it is; see
    /// [`Cagents::focus_pane_keyboard`].
    keyboard_owed: Option<CardId>,
    /// See [`REPORTED_TITLE_SAVE_DELAY`].
    reported_title_save: ReportedTitleSave,
    /// Where the camera was heading when the last sweep found cards on screen
    /// that it was only passing, which that sweep left cold. The camera
    /// coming to rest or heading elsewhere is then a reason to sweep again.
    lifecycle_sweep_destination: Option<f32>,
    /// The main window's latest input, whichever widget or overlay took it;
    /// hover activation reads it. See [`crate::input_trace`].
    input_trace: crate::input_trace::InputTrace,
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
    terminal_failure: terminal_failure::State,
    terminal_save: terminal_save::State,
    saved_terminals: saved_terminal::State,
    /// Observations held back while a start was outstanding.
    pending_adoptions: Vec<TerminalSession>,
    /// Restored agent terminals that are not on screen, attached one at a
    /// time after startup so their rows show what the agent is doing.
    restored_agent_attaches: VecDeque<CardId>,
    relaunched_terminals: HashSet<TerminalId>,
    terminal_crash_notices: HashMap<CardId, crate::model::HostSessionProof>,
    /// The shared sidebar model as this window last saw it.
    shared_views: shared_views::State,
    workspace_model: crate::workspace_model::WorkspaceModel,
    /// Repositories this window has already been asked to open on the shared
    /// model's behalf. Asking twice would mint a second project on one checkout.
    workspace_model_opens: HashSet<PathBuf>,
    workspace_model_rx: async_channel::Receiver<workspace_model_sync::ModelUpdate>,
    workspace_model_tx: async_channel::Sender<workspace_model_sync::ModelRequest>,
    activation_rx: async_channel::Receiver<instance::UiActivation>,
    notification_ui: notification_ui::State,
    attention_focus_epoch: u64,
    daemon_health_ui: daemon_health_ui::State,
    session: Option<SessionRuntime>,
    save_health: Option<SaveHealthState>,
    session_recovery: session_recovery::State,
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
    browser_backend: browser_backend::State,
    #[cfg(feature = "integrated-browser")]
    browser_parent_lifetime: Option<Arc<ParentWindowLifetime>>,
    /// Hands URLs to the system browser; tests record them instead.
    system_url_opener: file_manager::SystemUrlOpener,
    /// Compositor scale, before the user's interface scale multiplier.
    window_scale_factor: f32,
    /// Starts at an estimate, because the pane density and the strip's viewport
    /// are both derived from it and the surface does not report a width until
    /// it has been mapped. The native Opened and Resized events replace it.
    window_width: f32,
    window_height: f32,
    /// The inspector sections' laid-out height, once they have been shown.
    context_sections_height: Option<f32>,
    /// Whether the main window is maximized or fullscreen, so the shell offers
    /// no resize border. Tracked here rather than read from `core`, which only
    /// learns it from Wayland window states.
    window_filled: bool,
    /// Whether a Wayland window state has arrived for the main window. Those
    /// events then report maximized and fullscreen, so only a backend without
    /// them, such as X11, needs `query_window_filled` on every resize.
    window_state_observed: bool,
    /// Whether the compositor reports the main window's right edge as tiled,
    /// so the shell offers no resize border there.
    right_edge_tiled: bool,
    core: Core,
    settings_path: Option<PathBuf>,
    settings_write_blocked: bool,
    /// Bounded notice history, fed from `status` at the end of every `update`.
    notices: NoticeLog,
    sidebar_model: SidebarModel,
    project_ui: project_ui::State,
    worktree_protections: crate::worktree_cleanup::WindowProtections,
    channel_ui: channel_ui::State,
    sidebar_selection_ui: sidebar_selection_ui::State,
    chat_ui: chat_ui::State,
    resume_ui: resume_ui::State,
    update_ui: update_ui::State,
    update_resume: update_resume::State,
    branch_ui: branch_ui::State,
    history_ui: history_ui::State,
    refresh_ui: refresh_ui::State,
    source_control_mutations: HashMap<PathBuf, SourceControlMutation>,
    operation_recovery: crate::operation_recovery::Shared,
    account_ui: account_ui::Accounts,
    /// The sign-ins running in open terminals, which the account views ask
    /// after every frame.
    sign_ins: account_ui::SignIns,
    automation_ui: crate::automation_ui::State,
    work_ui: crate::work_ui::State,
    automation_approval: automation_approval::State,
    chrome_ui: chrome_ui::State,
    pull_request_inspector: pull_request_ui::State,
    sidebar_state: SidebarState,
    active_workspace: Option<PathBuf>,
    worktree_focus: HashMap<PathBuf, CardId>,
    worktree_offsets: HashMap<PathBuf, f32>,
    channel_offsets: HashMap<crate::model::ChannelId, f32>,
    worktree_owners: HashMap<CardId, PathBuf>,
    /// The worktrees the last protection sync asked to protect, which the
    /// projection check compares.
    #[cfg(any(test, feature = "development"))]
    projected_protections: std::collections::BTreeSet<PathBuf>,
    known_worktree_roots: worktree_ui::KnownWorktreeRoots,
    worktree_paths: worktree_ui::WorktreePathIndex,
    /// What the projects list of pull requests, by repository and number,
    /// which every chat labels its pull request links from.
    pull_request_board: chat_ui::PullRequestBoard,
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
    /// Routine questions whose open dialog has "Don't ask again" ticked. The
    /// tick is kept in settings only once the action goes ahead.
    dont_ask_again: BTreeSet<settings::Confirmation>,
    settings_reset_keyboard_nav: Option<bool>,
    pull_request_dialog: Option<PullRequestDialog>,
    /// Opening or dismissing a newer draft supersedes in-flight draft recovery.
    pull_request_dialog_generation: u64,
    dirty_repository_close: Option<DirtyRepositoryClose>,
    chat_close_confirm: Option<chat_close::ChatCloseConfirm>,
    transient_work_close: Option<window::Id>,
    automation_close_generation: u64,
    terminal_close: Option<terminal_close::CloseIntent>,
    queued_card_closes: VecDeque<terminal_close::QueuedCardClose>,
    /// The ended pane whose terminal the daemon is being asked to release.
    completed_release: Option<CardId>,
    close_queue_advancing: bool,
    /// The pending removal whose commit has been delivered, so anything still
    /// holding it is one of the closes the commit started.
    close_commit_delivered: Option<CardId>,
    terminal_close_attempts: HashSet<TerminalId>,
    terminal_close_generation: u64,
    closed_terminal_cards: HashSet<CardId>,
    /// Closing panes whose terminal the daemon proved gone, which reopening
    /// must not start again.
    proven_gone_cards: HashSet<CardId>,
    uncertain_terminal_proofs: HashMap<CardId, HostSessionProof>,
    terminal_recovery_attempts: HashSet<TerminalId>,
    /// Restored ended terminals whose final screen the daemon was asked for.
    retained_outputs: HashMap<TerminalId, terminal_end_ui::RetainedOutput>,
    pending_terminal_handoffs: HashMap<TerminalId, TerminalSession>,
    /// The handoffs above that a user's return to terminal started, with their
    /// cards, until a flush has saved them. Others are ordinary terminal starts.
    unsaved_chat_returns: HashMap<TerminalId, CardId>,
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
    /// What the messages dispatched since the last projection owe it; see
    /// [`Cagents::dispatch`].
    owed_projection: Option<OwedProjection>,
    /// A message dispatched in this update, its own or one a handler
    /// dispatched within it, may have changed which projects and worktrees
    /// attention covers. An update run within another keeps the other's.
    attention_scope_changed: bool,
    /// How many updates are running their message's handler: a dispatch
    /// made while none is has no update to project what it owes.
    dispatching: u32,
    frame_clock: FrameClock,
    latency: crate::performance::InteractionLatency,
    latency_focus: Option<CardId>,
    #[cfg(feature = "latency-harness")]
    latency_fixture: Option<crate::latency_fixture::Fixture>,
    #[cfg(feature = "latency-harness")]
    latency_fixture_started: bool,
    pane_inspectors: pane_inspector::PaneInspectors,
    context_sections: context_sections::Sections,
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

impl Cagents {
    fn set_terminal_review_owner(&mut self, owner: Option<CardId>) {
        self.terminal_review_card = owner;
        self.terminal_review_identity = owner.and_then(|card| match self.cards.get(&card) {
            Some(CardRuntime::Terminal(handle)) => Some(handle.terminal_id()),
            _ => None,
        });
        self.terminal_review_revision = self.terminal_review_revision.wrapping_add(1);
    }

    fn reconcile_terminal_review_owner(&mut self) {
        if let Some(owner) = self.terminal_review_card {
            let current_identity = match self.cards.get(&owner) {
                Some(CardRuntime::Terminal(handle)) => Some(handle.terminal_id()),
                _ => None,
            };
            if self.workspace.focused() != Some(owner)
                || current_identity.is_none()
                || current_identity != self.terminal_review_identity
            {
                self.set_terminal_review_owner(None);
            }
        }
    }

    fn focus_workspace_card(&mut self, id: CardId) -> Task<Message> {
        match self.workspace.focus(id) {
            Ok(update) => self.apply_viewport(update, true, DurableChange::None),
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            }
        }
    }

    fn bootstrap(strict: bool) -> Result<Bootstrap, String> {
        // Publish the stopped-pane display index before shared views arrive.
        if let Err(error) = crate::upgrade::floor::load() {
            tracing::warn!(%error, "Stopped update panes could not be indexed at startup");
        }
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

    /// Restore this window's saved presentation around its saved scope.
    /// Strip offsets must be known before the scope binds its camera, and
    /// binding the scope's source control clears the panel's disclosures, so
    /// the sidebar view is applied after that first binding.
    /// `scope_project` is the project whose worktree record holds the saved
    /// scope. The record outlives the worktree's sidebar row, so a removed
    /// worktree can still fall back to its own project.
    fn restore_window_view(
        &mut self,
        view: Option<WindowView>,
        scope_project: Option<ProjectId>,
    ) -> Task<Message> {
        if let Some(view) = &view {
            self.worktree_offsets.clone_from(&view.worktree_offsets);
            self.channel_offsets.clone_from(&view.channel_offsets);
        }
        let saved_scope = (
            self.durable.active_worktree.clone(),
            self.durable.active_channel,
        );
        let scope = self.restore_workspace_scope(view.is_some(), scope_project);
        let Some(view) = view else {
            return scope;
        };
        // Binding the restored scope cleared the panel's worktree-local
        // state, which the view now puts back, but only when the window
        // came back to the worktree that state was saved for.
        let kept_scope = match saved_scope {
            (_, Some(channel)) => self.durable.active_channel == Some(channel),
            (worktree, None) => {
                self.durable.active_channel.is_none() && self.durable.active_worktree == worktree
            }
        };
        self.sidebar_state.restore_view(if kept_scope {
            view.sidebar
        } else {
            view.sidebar.without_worktree_panel()
        });
        if let Some(category) = view.settings_category {
            self.settings_category = category;
        }
        self.settings_search = view.settings_search;
        // The first read was built before the view restored its file filter
        // and open folders, so it would list neither.
        let reload = match self.sidebar_state.active_worktree().cloned() {
            Some(key)
                if !self.sidebar_state.files_query().is_empty()
                    || self
                        .sidebar_state
                        .expanded_directory_paths()
                        .next()
                        .is_some() =>
            {
                self.reload_source_control(&key)
            }
            _ => Task::none(),
        };
        Task::batch([scope, reload])
    }

    fn rebuild_workspace(state: &WorkspaceState) -> Workspace {
        // Durable state names each card once; a repeat keeps its first place,
        // as the strip always has.
        let mut seen = HashSet::new();
        let cards = state
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && seen.insert(card.id))
            .map(|card| {
                let width = if card.width_fraction.is_finite() && card.width_fraction > 0.0 {
                    CardWidth::Fraction(card.width_fraction.min(1.0))
                } else {
                    CardWidth::Fraction(0.5)
                };
                WorkspaceCard::new(
                    card.id,
                    if matches!(card.kind, CardKind::Browser(_)) {
                        browser_host::TITLE.to_owned()
                    } else {
                        card.title.clone()
                    },
                    width,
                )
                .unwrap_or_else(|_| WorkspaceCard {
                    id: card.id,
                    accessible_name: "Workspace card".into(),
                    width: CardWidth::Fraction(0.5),
                })
            });
        let mut workspace =
            Workspace::with_cards(WorkspaceConfig::default(), cards).unwrap_or_default();
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
        if crate::device::is_remote() {
            let project_id = project_id.unwrap_or_default();
            self.repository_load_sequence = self.repository_load_sequence.wrapping_add(1);
            let generation = self.repository_load_sequence;
            self.repository_generations.insert(project_id, generation);
            return self.device_project_read_task(path, project_id, generation, kind);
        }
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
            .map(|card| (card.id, self.exposure(card.id)))
            .collect::<Vec<_>>();
        let now = Instant::now();
        for (card, exposure) in pending {
            self.lifecycle
                .register(card, exposure, Residency::Hibernated, now);
            self.set_residency(card, Residency::Hibernated);
        }
    }

    fn initialize_runtimes(&mut self) -> Task<Message> {
        let initial_views = self.local_view_scopes();
        self.startup_gate.initial_panes.capture(
            instance::window_launch_restore_policy(),
            &self.durable.cards,
        );
        self.startup_gate.initial_panes.capture_views(initial_views);
        let mut repository_tasks = Vec::new();
        let open_terminals_exposed = |exposure| {
            self.durable
                .cards
                .iter()
                .filter(|card| {
                    card.existence.is_open()
                        && self.exposure(card.id) == exposure
                        && matches!(&card.kind, CardKind::Terminal(_))
                })
                .map(|card| card.id)
                .collect::<HashSet<_>>()
        };
        let initially_visible = open_terminals_exposed(Exposure::Visible);
        let initially_prefetch = open_terminals_exposed(Exposure::Prefetch);
        // Closed cards keep their history in the workspace but start nothing.
        let open = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .cloned()
            .collect::<Vec<_>>();
        for card in open {
            let exposure = self.exposure(card.id);
            let terminal_residency =
                restored_terminal_residency(&card.kind, exposure, self.residency(card.id));
            #[cfg(feature = "integrated-browser")]
            let runtime_residency =
                restored_runtime_residency(&card.kind, exposure, self.residency(card.id));
            match card.kind {
                CardKind::Terminal(session) => {
                    if self.prepare_saved_terminal(card.id, &session) {
                        repository_tasks.push(self.inspect_saved_terminal(card.id));
                        continue;
                    }
                    let terminal_residency =
                        terminal_residency.expect("terminal restore has lifecycle residency");
                    self.lifecycle.register(
                        card.id,
                        exposure,
                        terminal_residency.clone(),
                        Instant::now(),
                    );
                    self.set_residency(card.id, terminal_residency);
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
                    if (self.shared_views.attached.contains(&card.id)
                        && !self.durable.local_only_cards.contains(&card.id))
                        || self.durable.pending_chat_admissions.get(&card.id) == Some(&session)
                    {
                        // Show saved text without admitting provider effects
                        // from a cached shared descriptor.
                        self.await_shared_chat_admission(card.id, session);
                        continue;
                    }
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
                    self.set_residency(card.id, runtime_residency);
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
                    repository_tasks.push(self.attach_repository_pane(card.id, view));
                }
                CardKind::PullRequests(view) => {
                    repository_tasks.push(self.attach_pull_request_pane(card.id, view));
                }
                kind @ (CardKind::Settings
                | CardKind::Accounts
                | CardKind::Automations
                | CardKind::Work) => {
                    self.set_residency(card.id, Residency::Hot);
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
            self.reconcile_lifecycle(&initially_visible, &HashSet::new(), &initially_prefetch)
                .0,
            self.attach_next_restored_agent(),
            Task::batch(repository_tasks),
            // Saved panes waiting for their workspace say whether an update
            // restore is why.
            self.watch_update_recovery(),
        ])
    }

    fn start_terminal(
        &self,
        card: CardId,
        session: TerminalSession,
        visible: bool,
        explicit_retry: bool,
    ) -> Task<Message> {
        if self.terminal_start_held(card, explicit_retry) {
            return Task::none();
        }
        // A restored recipe is presentation until its exact attachment has
        // been resolved; visibility alone must never execute it.
        if !explicit_retry && self.saved_terminal_current(card).is_some() {
            return Task::none();
        }
        let existing_only = !explicit_retry
            && self
                .startup_gate
                .initial_panes
                .existing_only(card, session.id);
        // Recovery can replace the identity this start ends up with, so
        // adoption has to know a start is outstanding before the task runs.
        self.begin_terminal_start(card);
        tracing::debug!(target: "cagents::pane_open", stage = "start_dispatched", ?card);
        let config = self.terminal_config(visible);
        let sender = self.terminal_wake_tx.clone();
        let terminal = session.id;
        let shared = self.attaches_as_shared(card);
        let reservation = self.terminal_save_reservation(card);
        let reserved = reservation.is_some();
        if reservation.as_ref().is_some_and(
            |reservation| !matches!(&reservation.pane.kind, CardKind::Terminal(current) if current.id == session.id),
        ) {
            return cosmic::task::message(Message::TerminalStarted {
                card,
                terminal,
                result: Err(SHARED_TERMINAL_DISCONNECTED.into()),
            });
        }
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if existing_only {
                    return attach_initial_terminal(card, session, sender);
                }
                let mut created = false;
                let mut save_failure = None;
                let (recovered, mode) = if shared && (!explicit_retry || reserved) {
                    let crate::terminal::ReadyDaemon { socket, mut client } =
                        crate::terminal::connect_ready_daemon()
                            .map_err(|error| error.to_string())?;
                    let hosted = if let Some(reservation) = reservation {
                        let (hosted, failure) = terminal_save::complete(
                            &mut client,
                            reservation,
                            &session,
                            config.clone().into(),
                        )?;
                        save_failure = failure;
                        created = save_failure.is_none();
                        Some(hosted)
                    } else {
                        // Name what the daemon shows about a terminal it no
                        // longer lists, rather than a reconnect it can't make.
                        Some(daemon_recovery::hosted_proving_absence(
                            &session,
                            &mut client,
                        )?)
                    };
                    let hosted = hosted.ok_or(SHARED_TERMINAL_DISCONNECTED)?;
                    shared_terminal_attachment(hosted.session, client.proof().clone(), socket)
                } else {
                    recover_terminal(session, explicit_retry)?
                };
                tracing::debug!(target: "cagents::pane_open", stage = "recovered", ?card, shared);
                let mut started = open_terminal(card, recovered, mode, config, sender, created)?;
                started.save_failure = save_failure;
                tracing::debug!(target: "cagents::pane_open", stage = "attached", ?card);
                Ok(started)
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
    ///
    /// A shared pane's hosted session is restored by the registry's clients,
    /// so it is attached exactly as opening the card attaches it: by its exact
    /// ID under the daemon's current proof, never through this window's own
    /// recovery of a cached descriptor.
    fn attach_restored_agent(&self, card: CardId, session: TerminalSession) -> Task<Message> {
        self.begin_terminal_start(card);
        let config = self.terminal_config(false);
        let sender = self.terminal_wake_tx.clone();
        let terminal = session.id;
        let shared = self.attaches_as_shared(card);
        let existing_only = self
            .startup_gate
            .initial_panes
            .existing_only(card, session.id);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if existing_only {
                    return attach_initial_terminal(card, session, sender).map(Some);
                }
                let connect =
                    || crate::terminal::connect_ready_daemon().map_err(|error| error.to_string());
                let Some((recovered, mode)) = restored_attachment(shared, session, connect)? else {
                    return Ok(None);
                };
                open_terminal(card, recovered, mode, config, sender, false).map(Some)
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

    /// Whether a queued restored row attaches from a cold residency, or why
    /// its card is not attached from the queue at all.
    fn restored_attach_residency(&self, card: CardId) -> Result<bool, &'static str> {
        match self.lifecycle.state(card) {
            Some((_, Residency::Hibernated)) => Ok(false),
            Some((Exposure::Offscreen, Residency::Cold)) => Ok(true),
            Some(_) => Err("its pane did not finish opening"),
            None => Err("this window has not registered its pane"),
        }
    }

    /// Why the attach queue would hold `card`'s restored `session` back.
    pub(super) fn restored_attach_hold(
        &self,
        card: CardId,
        session: &TerminalSession,
        views_ready: &mut Option<bool>,
    ) -> Option<&'static str> {
        if let Err(hold) = self.restored_attach_residency(card) {
            return Some(hold);
        }
        if self
            .card_state(card)
            .is_some_and(|state| self.project_ui.deleting_card(state))
        {
            return Some("its worktree is being deleted");
        }
        self.shared_terminal_attach_hold(card, session, views_ready)
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
        // A shared pane waits for the registry to confirm its descriptor, as
        // opening it would. A daemon without one leaves its descriptor the
        // only record, which is attached the way it was before sharing.
        let mut held = Vec::new();
        let mut task = Task::none();
        // Only the attach that ends this loop changes any state.
        let mut views_ready = None;
        while let Some(card) = self.restored_agent_attaches.pop_front() {
            // A card being opened has a start of its own. One that is cold
            // has none coming while it is off screen, which is how a shared
            // view projects a pane it has not shown yet.
            let Ok(cold) = self.restored_attach_residency(card) else {
                continue;
            };
            if self
                .card_state(card)
                .is_some_and(|state| self.project_ui.deleting_card(state))
            {
                held.push(card);
                continue;
            }
            if let Some(session) = deferred_terminal_session(
                self.card_state(card),
                matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
            ) {
                if !self.shared_terminal_ready_for_restored_attach(card, &session, &mut views_ready)
                {
                    held.push(card);
                    continue;
                }
                // Attached for its row or for startup qualification only, so
                // its rendering surface stays asleep until the card is opened.
                if cold {
                    self.lifecycle.register(
                        card,
                        Exposure::Offscreen,
                        Residency::Hibernated,
                        Instant::now(),
                    );
                    self.set_residency(card, Residency::Hibernated);
                }
                task = self.attach_restored_agent(card, session);
                break;
            }
        }
        for card in held.into_iter().rev() {
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
        self.terminal_started_for_operation(card, terminal, result, true)
    }

    fn terminal_started_for_operation(
        &mut self,
        card: CardId,
        terminal: TerminalId,
        result: Result<StartedTerminal, String>,
        current_operation: bool,
    ) -> Task<Message> {
        tracing::debug!(target: "cagents::pane_open", stage = "started", ?card, ok = result.is_ok());
        let accept = current_operation && can_accept_terminal_start(
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
                let claimed = self.terminal_runtime_claims(started.session.id);
                if claimed {
                    // A stale operation may have started the same hosted PTY
                    // a newer operation now holds or is attaching. Disconnect
                    // its client without sending ShutdownTerminal to that PTY.
                    self.retire_terminal_surface(started.handle, false);
                } else {
                    self.begin_terminal_teardown(started.handle);
                }
            }
            // Nothing will claim this identity, so a held observation
            // is free to decide about it.
            return self.finish_terminal_start(card);
        }
        self.startup_gate.initial_panes.settled(card, terminal);
        // A card still hibernated when its start lands is a restored agent
        // attached only for its row.
        let hibernated = matches!(self.lifecycle.state(card), Some((_, Residency::Hibernated)));

        let mut tasks = Vec::new();
        match result {
            Ok(started) => {
                self.resolve_notice(NoticeKey::TerminalStart(card));
                let unsaved = started.save_failure.is_some();
                if let Some(failure) = started.save_failure {
                    self.record_terminal_save_failure(card, failure);
                }
                self.startup_terminal_attached(card);
                if let Some(proof) = started.session.continuity.live_proof().filter(|_| !unsaved) {
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
                    // Recovery can attach an already-live descendant. Establish
                    // its new identity through the ordered relaunch write first;
                    // the following snapshot saves the authenticated live proof.
                    let mut replacement = started.session.clone();
                    if replacement.continuity.live_proof().is_some() {
                        replacement.continuity = TerminalContinuity::RelaunchAvailable {
                            recipe: replacement.launch.clone(),
                            last_exit: None,
                        };
                    }
                    writer.replace_terminal_identity(card, terminal, replacement);
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
                if self.ended_terminal(card).is_some() {
                    // The daemon can report a session that already
                    // ended without a queued event to drain, and a card
                    // registered as hydrating has nothing else left to
                    // resolve it.
                    self.lifecycle.remove(card);
                    self.set_residency(card, Residency::Hibernated);
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
                if let Err(error) = handle.set_theme(self.effective_terminal_theme()) {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not apply terminal theme: {error}"
                    )));
                }
                let visible = self
                    .terminal_start_visibility(card)
                    .expect("accepted terminal start has an open terminal card");
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
                if self.card_holds_keyboard(card) {
                    tasks.push(self.focus_pane_keyboard(card));
                }
            }
            Err(error) => {
                self.fail_runtime(card, error.clone());
                let notice = self.missing_terminal_notice(card);
                self.status = Some(
                    TransientNotice::error(notice.unwrap_or(error))
                        .with_key(NoticeKey::TerminalStart(card)),
                );
                tasks.push(cosmic::task::message(Message::LifecycleSweep));
            }
        }
        // A terminal that started off screen holds a slot a pane on screen
        // may be waiting for. A hibernated card holds none to hand over.
        if !hibernated
            && self
                .card_state(card)
                .is_some_and(|_| self.exposure(card) == Exposure::Offscreen)
        {
            tasks.push(self.lifecycle_sweep());
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
            max_scrollback_bytes: self.settings.terminal_scrollback_mib * 1024 * 1024,
            theme: self.effective_terminal_theme(),
            ..TerminalConfig::default()
        }
    }

    /// The trailing inset that keeps the interface out of the window's resize
    /// border.
    ///
    /// While the right window edge is free, the shell starts a resize from any
    /// press within [`design::WINDOW_RESIZE_BORDER`] of it, before the press
    /// reaches a widget. Panes put their scrollbars at their right edge, so the
    /// rightmost pane's scrollbar would lie wholly in that border. Maximized,
    /// fullscreen and right-tiled windows have no border there and stay flush.
    fn window_edge_inset(&self) -> f32 {
        if self.window_filled || self.right_edge_tiled {
            0.0
        } else {
            design::WINDOW_RESIZE_BORDER / self.settings.ui_scale
        }
    }

    /// The trailing padding the rightmost surface gives its interactive edge
    /// content - its scrollbar rail, its trailing controls - so they stay out
    /// of the window's resize border.
    ///
    /// Surfaces, rules and backgrounds still run flush to the window edge;
    /// only what a press could land on is inset. The window controls' header,
    /// the status bar, the workspace strip while nothing is docked beside it
    /// and the docked inspector each apply it themselves.
    pub(super) fn trailing_edge_inset(&self) -> f32 {
        self.window_edge_inset()
    }

    /// The window width left to the interface beside the resize border.
    fn content_width(&self) -> f32 {
        (self.window_width - self.window_edge_inset()).max(0.0)
    }

    /// Reads whether the main window is maximized or fullscreen from the
    /// windowing backend, which is what the shell's resize border consults.
    /// Wayland reports this through window states, which make the query
    /// redundant; X11 has no such event, so there every resize asks again.
    fn query_window_filled(&self) -> Task<Message> {
        if self.window_state_observed {
            return Task::none();
        }
        let Some(window) = self.core.main_window_id() else {
            return Task::none();
        };
        iced::window::is_maximized(window)
            .then(move |maximized| {
                iced::window::mode(window)
                    .map(move |mode| maximized || mode == iced::window::Mode::Fullscreen)
            })
            .map(move |filled| cosmic::Action::App(Message::WindowFilled(window, filled)))
    }

    fn new_pane_fraction(&self, configured: f32) -> f32 {
        let width = if self.workspace.cards().is_empty() {
            // The welcome view has no WorkspaceSurface to replace the startup
            // width estimate. Use the window's current bounds until one mounts.
            let sidebar =
                sidebar::effective_width(self.projects_sidebar_mode(), self.settings.sidebar_width);
            (self.content_width() - sidebar).max(0.0)
        } else {
            self.workspace.viewport_width()
        };
        if width > 0.0 && width < design::NARROW_WORKSPACE_WIDTH {
            1.0
        } else {
            configured
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
        if let Some(shell) = crate::device::shell() {
            launch.program = shell.to_owned();
        }
        // A shell can start either provider's CLI. Give it the same default
        // homes as native conversations, and persist those choices in its
        // launch recipe so a restored terminal never silently changes login.
        for provider in Account::PROVIDERS {
            let account = self.default_account_for_directory(provider, &working_directory);
            if crate::device::is_remote() {
                launch
                    .environment
                    .extend(crate::device::environment(provider, true));
                if matches!(account.home, account::AccountHome::System) {
                    continue;
                }
            }
            launch.unset_environment.extend(
                account
                    .credential_overrides()
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
            // A home that is not a path the recipe can record stays unbound.
            let _ = account.bind_home(&mut launch);
        }
        self.add_terminal_recipe(launch)
    }

    fn add_terminal_recipe(&mut self, launch: LaunchRecipe) -> Task<Message> {
        if self.refuse_in_deleting_worktree(&launch.working_directory) {
            return Task::none();
        }
        let scope = if self.durable.active_channel.is_none()
            && self.active_workspace.as_deref() != Some(launch.working_directory.as_path())
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
        tracing::debug!(target: "cagents::pane_open", stage = "requested", card = ?card_id, kind = "terminal");
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
            width_fraction: self.new_pane_fraction(self.settings.default_column_fraction),
            existence: Existence::Open,
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
        self.register_presence(
            card_id,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
        );
        if self.shared_views.creating_default {
            self.shared_views.defaults.insert(card_id, session.clone());
        } else if session.continuity.live_proof().is_none() {
            self.reserve_terminal_creation(card_id);
        }
        self.persist();
        Task::batch([
            self.start_terminal(card_id, session, true, false),
            self.apply_viewport(update, true, DurableChange::None),
        ])
    }

    fn clone_focused_pane(&mut self) -> Task<Message> {
        if self.workspace_inputs_blocked() || self.focused_repository_is_saving() {
            return Task::none();
        }
        let Some(source) = self
            .workspace
            .focused()
            .and_then(|id| self.card_state(id))
            .cloned()
        else {
            return Task::none();
        };
        if self.project_ui.deleting_card(&source) {
            return Task::none();
        }
        // A window holds one of each utility pane; a copy would show the
        // same application state twice.
        if !source.existence.is_open() || source.kind.is_utility() {
            return Task::none();
        }
        self.duplicate_pane(source)
    }

    fn duplicate_pane(&mut self, source: CardState) -> Task<Message> {
        let owner = self.card_worktree(&source).map(Path::to_path_buf);
        // A repository browser keeps its current file in window-local
        // navigation, not in the shared pane recipe. Snapshot the live view
        // now, before the clone is attached; its cached snapshot can be older.
        let navigation = match self.cards.get(&source.id) {
            Some(CardRuntime::Repository(runtime)) => runtime.navigation(),
            _ => None,
        };
        self.duplicate_pane_with_navigation(source, owner, navigation, false)
    }

    fn duplicate_pane_in_scope(
        &mut self,
        source: CardState,
        owner: Option<PathBuf>,
    ) -> Task<Message> {
        // Reopening a saved pane has no live clone navigation to inherit.
        self.duplicate_pane_with_navigation(source, owner, None, true)
    }

    fn duplicate_pane_with_navigation(
        &mut self,
        source: CardState,
        owner: Option<PathBuf>,
        navigation: Option<crate::model::RepositoryNavigation>,
        reopening: bool,
    ) -> Task<Message> {
        #[cfg(not(feature = "integrated-browser"))]
        if let CardKind::Browser(session) = &source.kind {
            return self.open_external_url(session.current_url.clone());
        }
        let source_id = source.id;
        let user_title = self.chrome_ui.user_titles.get(&source_id).cloned();
        let save_title = user_title.is_some();
        let mut card = source;
        card.id = CardId::new();
        // A split starts with its inspector as the pane it came from had it.
        self.pane_inspectors.inherit(source_id, card.id);
        card.existence = Existence::Open;
        card.generation = self.durable.generation;
        let mut residency = Residency::Hydrating { epoch: 1 };
        match &mut card.kind {
            CardKind::Terminal(session) => {
                if let Some(path) = self.terminal_working_directories.get(&source_id) {
                    session.launch.working_directory = path.clone();
                }
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
                // A clone is a conversation of its own, not a successor.
                session.recovery_origin = None;
                session.predecessor = None;
                session.continuity = TerminalContinuity::RelaunchAvailable {
                    recipe: session.launch.clone(),
                    last_exit: None,
                };
                session.resume_hint = None;
                session.transcript_path = transcript_path_for(&session.launch, session.id);
            }
            CardKind::Chat(session) => {
                // A fresh conversation must not reconnect to the source's
                // native owner or reuse its pane-specific Codex endpoint.
                // Reset while the source identity still names that endpoint.
                session.reset_owned_codex_listener();
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
                residency = Residency::Hot;
            }
            CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
                unreachable!("utility panes are refused before cloning")
            }
        }
        if let Some(title) = &user_title {
            card.title = title.clone();
        }
        let (index, durable_index) = self.terminal_insertion_indices();
        let id = card.id;
        let update = match self.workspace.add_card(
            WorkspaceCard::new(
                id,
                user_title
                    .as_deref()
                    .unwrap_or_else(|| browser_host::presentation_title(&card.kind, &card.title)),
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
        // The descriptor's directory can be a nested editor directory rather
        // than its containing workspace. Keep the filing across projection.
        if worktree_ui::card_directory(&card).is_some()
            && let Some(owner) = owner
        {
            durable.card_scopes.insert(id, owner.clone());
            self.worktree_owners.insert(id, owner);
        }
        if let Some(title) = user_title {
            self.chrome_ui.user_titles.insert(id, title);
        }
        self.register_channel_card(id);
        self.register_presence(id, Exposure::Visible, residency);
        // A reopened chat takes back what it had not sent when it closed; a
        // clone of an open one starts with an empty composer.
        if reopening && matches!(card.kind, CardKind::Chat(_)) && self.reclaim_kept_draft(source_id)
        {
            self.move_chat_draft(source_id, id);
        }
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
                if let Some(navigation) = navigation {
                    Arc::make_mut(&mut self.durable)
                        .repository_navigation
                        .insert(id, navigation);
                }
                self.attach_repository_pane(id, view)
            }
            CardKind::PullRequests(view) => self.attach_pull_request_pane(id, view),
            CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
                unreachable!("utility panes are refused before cloning")
            }
        };
        self.persist();
        let viewport = self.apply_viewport(update, true, DurableChange::None);
        let title = if save_title {
            self.save_settings_task(false)
        } else {
            Task::none()
        };
        Task::batch([start, viewport, title])
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
        runtime.set_editor_font(
            crate::terminal::monospace_font(self.settings.editor_font_family()),
            self.settings.editor_font_size(),
        );
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        runtime.set_find_preferences(
            self.settings.editor_find_options,
            self.settings.editor_replace_preserve_case,
        );
        runtime.set_syntax_theme(self.settings.editor_syntax_theme);
        drop(runtime.set_diff_hide_unchanged(self.settings.diff_hide_unchanged));
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
            file_panel_width: view
                .file_panel_width
                .map_or(design::NAVIGATION_DEFAULT_WIDTH, f32::from),
            file_panel_collapsed: view.file_panel_collapsed,
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
                && self.workspace.card(card.id).is_some()
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
                && self.workspace.card(card.id).is_some()
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

    fn navigate_local_diff(
        &mut self,
        card: CardId,
        shortcut: workspace_widget::GlobalShortcut,
    ) -> Task<Message> {
        use workspace_widget::GlobalShortcut;
        match shortcut {
            GlobalShortcut::ReviewNextChange => {
                return cosmic::task::message(Message::Repository(RepositoryAction::NextChange {
                    card,
                }));
            }
            GlobalShortcut::ReviewPreviousChange => {
                return cosmic::task::message(Message::Repository(
                    RepositoryAction::PreviousChange { card },
                ));
            }
            GlobalShortcut::ReviewNextFile | GlobalShortcut::ReviewPreviousFile => {}
            _ => return Task::none(),
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
        let staged = matches!(runtime.selected_diff_basis(), DiffBasis::HeadToIndex);
        let paths: Vec<_> = source
            .groups()
            .iter()
            .filter(|group| (group.area == ChangeArea::Staged) == staged)
            .flat_map(|group| {
                group
                    .rows()
                    .iter()
                    .map(move |row| (group.area, &row.entry.path))
            })
            .collect();
        if paths.is_empty() {
            return Task::none();
        }
        let current = paths
            .iter()
            .position(|(_, path)| Some(path.as_path()) == runtime.selected_path());
        let index = match (shortcut, current) {
            (GlobalShortcut::ReviewPreviousFile, Some(index)) => index.saturating_sub(1),
            (_, Some(index)) => (index + 1).min(paths.len() - 1),
            _ => 0,
        };
        let (area, path) = paths[index];
        cosmic::task::message(Message::Sidebar(SidebarAction::OpenChange {
            key: source.key.clone(),
            path: path.clone(),
            area,
        }))
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
        let reason = pane
            .project(&self.sidebar_model)
            .and_then(|id| self.github_availability.get(&id))
            .map(GitHubAvailability::guidance);
        if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
            pane.review.set_unavailable_reason(reason);
        }
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
            file_panel_width: None,
            file_panel_collapsed: None,
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
            width_fraction: self.new_pane_fraction(self.settings.default_native_pane_fraction),
            existence: Existence::Open,
            kind: CardKind::PullRequests(view.clone()),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card);
        durable.focused = Some(card_id);
        // A review is work in the workspace where the user opened it. Unlike
        // a global browser, it must not reappear when another worktree's
        // metadata refresh rebuilds that worktree's strip.
        if durable.active_channel.is_none() {
            let scope = self.active_workspace.clone().unwrap_or(repository.clone());
            durable.card_scopes.insert(card_id, scope.clone());
            self.worktree_owners.insert(card_id, scope);
        }
        self.set_presence(card_id, Exposure::Visible, Residency::Hot);
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
                    CardWidth::Fraction(
                        self.new_pane_fraction(self.settings.default_native_pane_fraction),
                    ),
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
            Task::batch([
                self.apply_viewport(update, true, DurableChange::None),
                source,
            ]),
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
            return workspace_widget::FocusedPane::None;
        };
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(_))
                if self.ended_terminal(card).is_none() && !self.terminal_close_pending(card) =>
            {
                workspace_widget::FocusedPane::Terminal
            }
            Some(CardRuntime::Chat(_)) => workspace_widget::FocusedPane::Chat,
            Some(CardRuntime::PullRequests(_)) => workspace_widget::FocusedPane::Review,
            Some(CardRuntime::Repository(runtime)) if runtime.shows_diff() => {
                workspace_widget::FocusedPane::Review
            }
            _ if !self.pane_has_inspector(card) => workspace_widget::FocusedPane::NoInspector,
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
        // Explicit selection or dismissal supersedes deferred restoration.
        // Otherwise the next GitHub refresh can reopen a dismissed review.
        let dismissed_pending =
            matches!(&action, pull_request_review::Action::Close) && pane.pending_number.is_some();
        if matches!(
            action,
            pull_request_review::Action::Open(_)
                | pull_request_review::Action::OpenNumber(_)
                | pull_request_review::Action::Close
        ) {
            pane.pending_number = None;
        }
        let mut tasks = vec![
            pane.review
                .update(action)
                .map(move |action| Message::PullRequestReview { card, action })
                .map(cosmic::Action::App),
        ];
        let mut selection: Option<Option<u64>> = dismissed_pending.then_some(None);
        let mut links = Vec::new();
        let mut checkout = None;
        let mut checkout_open = None;
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
                pull_request_review::Effect::OpenCheckout(path) => checkout_open = Some(path),
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
            // Saved now, though it is browsing: a shared view holds the
            // pane's kind, and an update to that view arriving before a
            // deferred save would put the previous pull request back.
            self.persist();
        }
        for url in links {
            tasks.push(self.open_clicked_link(url));
        }
        if let Some(path) = checkout_open {
            tasks.push(self.navigate_to_worktree(path, false));
        }
        if let Some((project, target)) = checkout {
            tasks.push(self.prepare_pr_checkout(card, project, target));
        }
        self.cancel_stale_pr_creations();
        tasks.push(self.complete_closed_projects());
        Task::batch(tasks)
    }

    /// The pull request card: the project's open pull requests, or the
    /// review of the chosen one, beside the file the review selected.
    fn pull_request_card_view<'a>(
        &'a self,
        id: CardId,
        pane: &'a PullRequestPane,
    ) -> Element<'a, Message> {
        self.pull_request_card_layout(id, pane, self.pull_request_card_width(id))
    }

    /// The pull request card's width. The strip already tracks the rendered
    /// width, including animations; Responsive would hide the review and its
    /// controls from AccessKit.
    fn pull_request_card_width(&self, id: CardId) -> f32 {
        self.workspace
            .presentation_geometry()
            .card(id)
            .map_or(self.workspace.viewport_width(), |card| card.width())
    }

    /// Whether the pull request card shows its side panel, which holds the
    /// review's header, summary and files: the person's choice, or else
    /// whether the card is wide enough for it.
    fn pull_request_side_shown(&self, id: CardId, pane: &PullRequestPane) -> bool {
        !review_files_collapsed(pane, self.pull_request_card_width(id))
    }

    fn pull_request_card_layout<'a>(
        &'a self,
        id: CardId,
        pane: &'a PullRequestPane,
        width: f32,
    ) -> Element<'a, Message> {
        let collapsed = review_files_collapsed(pane, width);
        let max_width = (width - 320.0).clamp(120.0, 600.0);
        let panel_width = pane.file_panel_width.clamp(120.0, max_width);
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
                        Icon::PullRequest
                            .handle()
                            .icon()
                            .size(design::ROW_ICON_SIZE)
                            .into(),
                        crate::ui_controls::FadeText::heading(heading)
                            .width(Length::Fill)
                            .into(),
                    ])
                    .spacing(space::S)
                    .align_y(Alignment::Center),
                )
                .height(Length::Fixed(design::HEADER_CONTENT_HEIGHT))
                .padding([0.0, space::M])
                .align_y(Alignment::Center)
                .width(Length::Fill)
                .into(),
            );
            side.push(design::horizontal_rule());
            side.push(review.browser(modal_open));
        }
        let side: Element<'a, pull_request_review::Action> =
            widget::container(widget::column::with_children(side).height(Length::Fill))
                .width(Length::Fixed(panel_width))
                .height(Length::Fill)
                .into();
        let side = side.map(move |action| Message::PullRequestReview { card: id, action });

        let mut main: Vec<Element<'a, Message>> = Vec::new();
        let file_panel_toggle = || {
            crate::ui_controls::icon_button(
                Icon::Sidebar,
                if collapsed {
                    "Show review files"
                } else {
                    "Hide review files"
                },
            )
            .selected(!collapsed)
            .on_press_maybe((!modal_open).then(|| Message::ReviewFilePanelToggle {
                card: id,
                collapsed: !collapsed,
            }))
        };
        let mut review_controls = Vec::new();
        if let Some(comment) =
            review.file_comment_control(modal_open, pane.runtime.can_show_file_discussion())
        {
            review_controls.push(comment.map(move |action| {
                Message::Repository(RepositoryAction::Review { card: id, action })
            }));
        }
        review_controls.push(file_panel_toggle().into());
        if let Some(toolbar) = repository_widget::selected_file_toolbar_mapped_with(
            id,
            &pane.runtime,
            Some(crate::ui_controls::toolbar_actions(review_controls)),
            Message::Repository,
        ) {
            main.push(toolbar);
        } else {
            main.push(
                widget::container(
                    widget::row::with_children(vec![
                        design::heading("Changes").width(Length::Fill).into(),
                        file_panel_toggle().into(),
                    ])
                    .align_y(Alignment::Center),
                )
                .padding([0.0, design::TOOLBAR_INSET])
                .height(design::HEADER_CONTENT_HEIGHT)
                .into(),
            );
        }
        main.push(design::horizontal_rule());
        if review.selected().is_some() {
            main.push(
                repository_widget::view_with_review(id, &pane.runtime, Some(review))
                    .map(Message::Repository),
            );
        } else {
            let prompt = if review.is_open() {
                "Select a file"
            } else {
                "Select a pull request"
            };
            main.push(
                widget::container(
                    widget::column::with_children(vec![
                        Icon::PullRequest.handle().icon().size(24).into(),
                        design::caption(prompt)
                            .class(cosmic::theme::Text::Custom(design::muted_text))
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .align_x(Alignment::Center)
                            .width(Length::Fill)
                            .into(),
                    ])
                    .align_x(Alignment::Center)
                    .spacing(space::M)
                    .width(Length::Fill),
                )
                .center(Length::Fill)
                .padding(space::XL)
                .into(),
            );
        }
        let main = widget::column::with_children(main)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        let mut children = Vec::with_capacity(3);
        if !collapsed {
            children.push(side);
            children.push(
                crate::splitter::Splitter::new(
                    widget::Id::new(format!("review.files.resize.{id}")),
                    "Review file panel width",
                    panel_width,
                    120.0,
                    max_width,
                    crate::splitter::Edge::Trailing,
                    modal_open,
                    move |width| Message::ReviewFilePanelResize { card: id, width },
                    || Message::ReviewFilePanelResizeFinished,
                )
                .into(),
            );
        }
        children.push(main);
        widget::row::with_children(children)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    /// Repository panes have no engine surface to hydrate. Start their reads
    /// when attached, including when a shared descriptor replaces a local one.
    fn attach_repository_pane(&mut self, card: CardId, view: RepositoryView) -> Task<Message> {
        if crate::device::is_remote() {
            self.cards.insert(
                card,
                CardRuntime::Failed("File viewing is unavailable on remote devices.".into()),
            );
            return Task::none();
        }
        let mut runtime = if self
            .sidebar_model
            .project(view.project)
            .is_some_and(|project| project.kind == ProjectKind::Folder)
        {
            RepositoryRuntime::new_directory(view)
        } else {
            RepositoryRuntime::new(view)
        };
        runtime.set_auto_save(self.settings.editor_auto_save);
        runtime.set_editor_font(
            crate::terminal::monospace_font(self.settings.editor_font_family()),
            self.settings.editor_font_size(),
        );
        runtime.set_wrap_lines(self.settings.editor_wrap_lines);
        runtime.set_find_preferences(
            self.settings.editor_find_options,
            self.settings.editor_replace_preserve_case,
        );
        runtime.set_syntax_theme(self.settings.editor_syntax_theme);
        drop(runtime.set_diff_hide_unchanged(self.settings.diff_hide_unchanged));
        runtime.set_external_editors(self.sidebar_model.external_editors.clone());
        let released = self.released_repository_navigation.remove(&card);
        if let Some(navigation) = self
            .durable
            .repository_navigation
            .get(&card)
            .or(released.as_ref())
        {
            runtime.restore_navigation(navigation);
        }
        let load = repository_task(runtime.initial_load(card));
        self.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        load
    }

    fn attach_pull_request_pane(
        &mut self,
        card: CardId,
        view: crate::model::PullRequestView,
    ) -> Task<Message> {
        if crate::device::is_remote() {
            self.cards.insert(
                card,
                CardRuntime::Failed(
                    "Pull request viewing is unavailable on remote devices.".into(),
                ),
            );
            return Task::none();
        }
        let pane = self.pull_request_pane(&view);
        self.cards
            .insert(card, CardRuntime::PullRequests(Box::new(pane)));
        self.open_pending_pull_request(card)
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
    ) -> (CardId, Task<Message>) {
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
            width_fraction: self.new_pane_fraction(self.settings.default_native_pane_fraction),
            existence: Existence::Open,
            kind: CardKind::Repository(view.clone()),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card);
        durable.focused = Some(card_id);
        self.set_presence(card_id, Exposure::Visible, Residency::Hot);
        self.register_channel_card(card_id);
        let load = self.attach_repository_pane(card_id, view);
        let index = self.workspace.cards().len();
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    title,
                    CardWidth::Fraction(
                        self.new_pane_fraction(self.settings.default_native_pane_fraction),
                    ),
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
        let task = Task::batch([self.apply_viewport(update, true, DurableChange::None), load]);
        (card_id, task)
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
            LinkOpenDestination::SystemDefault => {
                let opened = self.system_url_opener.open(url);
                cosmic::task::future(async move { Message::ExternalLinkOpened(opened.await) })
            }
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
        // A pane that cannot show the page is a dead end, so a page the
        // system browser can show goes there, and the notice says why. A
        // check that might succeed on a second try starts again, so the next
        // page can open here.
        if let Some(reason) = self.browser_unavailable_reason()
            && matches!(url.scheme(), "http" | "https" | "file")
        {
            let opened = self.system_url_opener.open(url);
            return Task::batch([
                cosmic::task::future(async move {
                    Message::BrowserFallbackOpened {
                        result: opened.await,
                        reason,
                    }
                }),
                self.recheck_browser(),
            ]);
        }
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
        let mut startup_error = self
            .browser_unavailable_reason()
            .map(|reason| reason.to_string());
        if let Some(factory) = self.browser_factory.clone() {
            match spawn_browser(
                &session,
                true,
                effective_surface_scale(self.window_scale_factor, self.settings.ui_scale),
                Some(factory),
            ) {
                Ok(runtime) => {
                    self.cards
                        .insert(card_id, CardRuntime::Browser(Box::new(runtime)));
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
        let width_fraction = self.new_pane_fraction(self.settings.default_column_fraction);
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(CardState {
            id: card_id,
            generation: durable.generation,
            title: url
                .host_str()
                .and_then(normalize_card_name)
                .unwrap_or_else(|| "Browser".into()),
            width_fraction,
            existence: Existence::Open,
            kind: CardKind::Browser(session),
        });
        durable.focused = Some(card_id);
        self.register_channel_card(card_id);
        self.register_presence(
            card_id,
            Exposure::Visible,
            Residency::Hydrating {
                epoch: INITIAL_HYDRATION_EPOCH,
            },
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
                    CardWidth::Fraction(
                        self.new_pane_fraction(self.settings.default_column_fraction),
                    ),
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
        self.apply_viewport(update, true, DurableChange::None)
    }

    #[cfg(not(feature = "integrated-browser"))]
    fn add_browser(&mut self, url: Url) -> Task<Message> {
        self.open_external_url(url)
    }

    fn open_external_url(&self, url: Url) -> Task<Message> {
        let opened = self.system_url_opener.open(url);
        cosmic::task::future(async move { Message::ExternalUrlOpened(opened.await) })
    }

    /// Keeps the saved focus on a pane the window still holds open.
    ///
    /// The saved focus mirrors the strip's, and [`Self::apply_viewport`] is
    /// what carries one to the other. A pane can stop being open with no
    /// viewport update to carry the handoff - another client closes it, its
    /// project is removed - and storage refuses a workspace that focuses a
    /// pane it does not hold open, which sets the whole window aside. So every
    /// snapshot is settled here before it is taken. The strip is asked again
    /// rather than trusted: it can still show a pane whose close it could not
    /// take, and that pane is no more open for being on screen.
    fn settle_focus(&mut self) {
        if self.durable.focused.is_none() || self.durable.focused_index().is_some() {
            return;
        }
        let focused = self.workspace.focused().filter(|id| {
            self.durable
                .cards
                .iter()
                .any(|card| card.id == *id && card.existence.is_open())
        });
        Arc::make_mut(&mut self.durable).focused = focused;
    }

    /// Saves the workspace and publishes its views, reported titles included.
    fn persist(&mut self) {
        self.reported_title_save.cards.clear();
        self.save_workspace();
    }

    /// Hands the writer this window's snapshot without projecting shared
    /// views, for a save of state that none of them reads.
    ///
    /// The snapshot still takes the strip's order into the active channel,
    /// and a pane moved or closed in the strip since the last save changes
    /// what that channel's view holds. Such a save publishes as
    /// [`Self::persist`] does.
    fn save_snapshot(&mut self) {
        if self.snapshot_workspace_scope() {
            self.persist();
        } else {
            self.save_workspace_with_view_publication(|_| {});
        }
    }

    /// [`Self::persist`] that leaves reported titles waiting for their
    /// deferred save held back from shared views.
    fn save_workspace(&mut self) {
        self.save_workspace_with_view_publication(Self::publish_view_changes);
    }

    /// Snapshot the active strip before publishing its exact pane projection.
    /// Shared reconciliation supplies an already projected publication pass.
    fn save_workspace_with_view_publication(&mut self, publish: impl FnOnce(&mut Self)) {
        self.save_projected_workspace(false, publish);
    }

    fn save_projected_workspace(&mut self, replica: bool, publish: impl FnOnce(&mut Self)) {
        self.shared_views.prune_restoration_requests(&self.durable);
        if !self.durable.pending_terminal_creations.is_empty() {
            let open = self
                .durable
                .cards
                .iter()
                .filter(|card| {
                    card.existence.is_open() && matches!(card.kind, CardKind::Terminal(_))
                })
                .map(|card| card.id)
                .collect::<HashSet<_>>();
            Arc::make_mut(&mut self.durable)
                .pending_terminal_creations
                .retain(|id| open.contains(id));
        }
        #[cfg(test)]
        terminal_output_tests::record_persist();
        let mut phase = WorkspaceProjectionTiming::new(true);
        phase.mark("save_prepare");
        self.snapshot_workspace_scope();
        phase.mark("save_scope_snapshot");
        publish(self);
        phase.mark("save_publish");
        // Ordered identity work needs its exact complete snapshot now. Scope
        // projection alone is not authority to coalesce these checkpoints.
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_policy",
            replica, has_writer = self.session.is_some(),
            adoptions = self.shared_views.adoptions.len(), restorations = self.shared_views.restorations.len(),
            chat_adoptions = self.shared_views.chat_adoptions.len(), "Considered replica snapshot capture");
        if replica
            && self.session.is_some()
            && self.shared_views.adoptions.is_empty()
            && self.shared_views.restorations.is_empty()
            && self.shared_views.chat_adoptions.is_empty()
        {
            self.mark_replica_snapshot(Instant::now());
        } else {
            self.capture_prepared_workspace(true);
        }
    }

    fn mark_replica_snapshot(&mut self, now: Instant) {
        let Some(writer) = &self.session else {
            return;
        };
        if let Some(pending) = &mut self.replica_capture.pending {
            pending.changes = pending
                .changes
                .merge(crate::session::SnapshotChanges::at(now));
        } else {
            self.replica_capture.generation = self.replica_capture.generation.wrapping_add(1);
            self.replica_capture.pending = Some(ReplicaSnapshotDebt {
                changes: crate::session::SnapshotChanges::at(now),
                registry: self.workspace_model.identity,
                workspace: self.durable.id,
                writer: writer.writer_identity(),
            });
            self.replica_capture.rejected = false;
        }
    }

    fn snapshot_offer(
        &self,
        state: Arc<WorkspaceState>,
        ordinary: bool,
    ) -> crate::session::SnapshotOffer {
        let changes = self.replica_capture.pending.as_ref().map(|pending| {
            if ordinary {
                pending
                    .changes
                    .merge(crate::session::SnapshotChanges::at(Instant::now()))
            } else {
                pending.changes
            }
        });
        crate::session::SnapshotOffer { state, changes }
    }

    fn snapshot_offer_answered(&mut self, accepted: bool) {
        if accepted {
            #[cfg(test)]
            {
                self.replica_capture.offers += 1;
            }
            self.replica_capture.pending = None;
            self.replica_capture.generation = self.replica_capture.generation.wrapping_add(1);
            self.replica_capture.timer_armed = false;
            self.replica_capture.rejected = false;
            self.navigation_save = None;
        } else if self.replica_capture.pending.is_some() {
            // A poisoned or closed queue cannot recover by spinning timers.
            // Retain the debt for an explicit save/recovery or replacement.
            self.replica_capture.rejected = true;
            self.replica_capture.timer_armed = false;
        }
    }

    fn capture_prepared_workspace(&mut self, ordinary: bool) {
        let started =
            tracing::enabled!(target: "cagents::workspace_pipeline", tracing::Level::DEBUG)
                .then(Instant::now);
        let had_replica_debt = self.replica_capture.pending.is_some();
        let mut accepted = false;
        #[cfg(test)]
        {
            self.replica_capture.captures += 1;
        }
        let saved = self.persistent_workspace();
        self.record_shared_chat_identities();
        let offer = self.snapshot_offer(saved, ordinary);
        if let Some(writer) = &self.session {
            let result = if self.shared_views.adoptions.is_empty()
                && self.shared_views.restorations.is_empty()
            {
                writer.offer_snapshot(offer)
            } else {
                writer.offer_terminal_panes(
                    offer,
                    self.shared_views.adoptions.clone(),
                    self.shared_views.restorations.clone(),
                )
            };
            if result.is_ok() {
                self.shared_views.adoptions.clear();
                self.shared_views.restorations.clear();
            }
            accepted = result.is_ok();
            self.snapshot_offer_answered(accepted);
        } else if self.replica_capture.pending.is_none() {
            // Memory-only windows retain their existing navigation bookkeeping;
            // this is not an accepted offer and acknowledges no replica debt.
            self.navigation_save = None;
        }
        if let Some(started) = started {
            let parent = SNAPSHOT_CAPTURE_PARENT.with(std::cell::Cell::get);
            tracing::debug!(target: "cagents::workspace_pipeline", stage = "window_snapshot_capture",
                elapsed_ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                parent, had_replica_debt, accepted, "Prepared a local window snapshot");
        }
    }

    fn materialize_replica_snapshot(&mut self) {
        if self.replica_capture.pending.is_some() {
            self.capture_prepared_workspace(false);
        }
    }

    fn arm_replica_capture(&mut self) -> Task<Message> {
        if self.replica_capture.timer_armed || self.replica_capture.rejected {
            return Task::none();
        }
        let Some(pending) = &self.replica_capture.pending else {
            return Task::none();
        };
        let delay = pending
            .changes
            .deadline()
            .saturating_duration_since(Instant::now());
        let generation = self.replica_capture.generation;
        self.replica_capture.timer_armed = true;
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::ReplicaCaptureDue(generation)
        })
    }

    fn replica_capture_due(&mut self, generation: u64, now: Instant) -> Task<Message> {
        if generation != self.replica_capture.generation || !self.replica_capture.timer_armed {
            tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_wake", outcome = "stale", "Handled replica snapshot timer");
            return Task::none();
        }
        self.replica_capture.timer_armed = false;
        let Some(pending) = &self.replica_capture.pending else {
            tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_wake", outcome = "stale", "Handled replica snapshot timer");
            return Task::none();
        };
        if pending.registry != self.workspace_model.identity
            || pending.workspace != self.durable.id
            || !self
                .session
                .as_ref()
                .is_some_and(|writer| writer.owns_writer(&pending.writer))
        {
            tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_wake", outcome = "owner_changed", "Handled replica snapshot timer");
            self.replica_capture.rejected = true;
            return Task::none();
        }
        if now < pending.changes.deadline() {
            tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_wake", outcome = "early", "Handled replica snapshot timer");
            return self.arm_replica_capture();
        }
        tracing::debug!(target: "cagents::workspace_pipeline", stage = "replica_snapshot_wake", outcome = "capture", "Handled replica snapshot timer");
        self.materialize_replica_snapshot();
        Task::none()
    }

    /// The window to save, after publishing this window's own edits to the
    /// shared views it shows. An edit saved but not published is undone by
    /// the next shared projection, and storage refuses the snapshot that
    /// projection saves when it reverts what storage already holds.
    ///
    /// Conversations the shared views re-pointed chat cards to are recorded
    /// with it, ahead of whichever write the caller makes with it.
    fn shared_window_snapshot(&mut self) -> Arc<WorkspaceState> {
        self.shared_window_snapshot_with(Self::publish_view_changes)
    }

    fn shared_window_snapshot_with(
        &mut self,
        publish: impl FnOnce(&mut Self),
    ) -> Arc<WorkspaceState> {
        self.snapshot_workspace_scope();
        publish(self);
        let saved = self.persistent_workspace();
        self.record_shared_chat_identities();
        saved
    }

    /// Records the conversations shared views re-pointed chat cards to since
    /// the last save, for the write of the window's state that has to follow:
    /// storage refuses a write that re-points a chat card unless the change
    /// is recorded, and that write queues them ahead of itself.
    pub(super) fn record_shared_chat_identities(&mut self) {
        if self.shared_views.chat_adoptions.is_empty() {
            return;
        }
        let Some(writer) = &self.session else {
            return;
        };
        let adoptions = std::mem::take(&mut self.shared_views.chat_adoptions);
        writer.stage_chat_adoptions(adoptions.into_values());
    }

    fn commit_chat_session(&mut self, card: CardId, session: crate::chat::Session) {
        match self.install_chat_session(card, session) {
            Some(true) => {
                let saved = self.shared_window_snapshot();
                let offer = self.snapshot_offer(saved, true);
                if let Some(writer) = &self.session {
                    let accepted = writer.offer_conversion(offer).is_ok();
                    self.snapshot_offer_answered(accepted);
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
        self.set_residency(card, Residency::Hot);
        if unchanged {
            return None;
        }
        if self.card_state(card).is_some_and(
            |state| matches!(&state.kind, CardKind::Chat(current) if current.id != session.id),
        ) {
            // An accepted identity replacement retires attribution, not journal news.
            self.sidebar_state.forget_card_attention(card);
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
    /// identity. Like any other edit, the new conversation is published to
    /// the card's shared view.
    fn replace_chat_conversation(&mut self, card: CardId, session: crate::chat::Session) {
        let expected = match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(current)) if current != &session => current.id,
            _ => return self.commit_chat_session(card, session),
        };
        if self.install_chat_session(card, session.clone()).is_none() {
            return;
        }
        let saved = self.shared_window_snapshot();
        let offer = self.snapshot_offer(saved, true);
        if let Some(writer) = &self.session {
            let accepted = writer
                .offer_chat_identity(card, expected, session, offer)
                .is_ok();
            self.snapshot_offer_answered(accepted);
        }
    }

    fn finish_chat_terminal_handoff(
        &mut self,
        card: CardId,
        session: TerminalSession,
    ) -> Task<Message> {
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open())
        {
            return Task::none();
        }
        let residency = Residency::Hydrating {
            epoch: unix_millis().max(1) as u64,
        };
        let exposure = self.exposure(card);
        self.register_presence(card, exposure, residency);
        if let Some(state) = self.card_state_mut(card) {
            state.kind = CardKind::Terminal(session.clone());
        }
        self.closed_terminal_cards.remove(&card);
        self.proven_gone_cards.remove(&card);
        self.cards.insert(card, CardRuntime::Loading);
        self.card_chrome.remove(&card);
        let saved = self.shared_window_snapshot();
        let offer = self.snapshot_offer(saved, true);
        if let Some(writer) = &self.session {
            let accepted = writer.offer_conversion(offer).is_ok();
            self.snapshot_offer_answered(accepted);
        }
        self.pending_terminal_handoffs
            .insert(session.id, session.clone());
        self.unsaved_chat_returns.insert(session.id, card);
        let commit = self.commit_terminal_handoffs();
        let attach = self.start_terminal(card, session, exposure == Exposure::Visible, false);
        Task::batch([commit, attach])
    }

    /// The open card of a return to terminal that no flush has saved yet.
    fn unsaved_chat_return(&self) -> Option<CardId> {
        self.unsaved_chat_returns.values().copied().find(|&card| {
            self.card_state(card)
                .is_some_and(|state| state.existence.is_open())
        })
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
        self.materialize_replica_snapshot();
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
        #[cfg(test)]
        terminal_output_tests::record_owner_resolution();
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
        self.exit.is_none()
            && self.refresh_ui.focused
            && self.workspace.focused() == Some(card)
            && !self.modal_is_open()
            && !matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime)) if !runtime.parent_transcript_selected())
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
        // Only an agent that carries on was answered. One that disconnects,
        // fails, or closes while waiting reports no state at all.
        if previous == Some(crate::agent::ActivityState::Waiting)
            && matches!(
                next,
                Some(crate::agent::ActivityState::Working | crate::agent::ActivityState::Idle)
            )
        {
            let listed = self.attention_listed_rows();
            let placement = self.attention_placement(card, &listed);
            self.sidebar_state.settle_waiting_agent(card, &placement);
        }
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
                channels_holding(&self.durable.channels, card)
                    .next()
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

    fn focus_attention_card(&mut self, id: CardId) -> Task<Message> {
        if self.exit.is_some()
            || self.workspace_inputs_blocked()
            || self.focused_repository_is_saving()
            || self.update_prepared
        {
            return Task::none();
        }
        let Some(card) = self
            .card_state(id)
            .filter(|card| card.existence.is_open())
            .cloned()
        else {
            return Task::none();
        };
        let channel = channels_holding(&self.durable.channels, id)
            .next()
            .map(|channel| channel.id);
        // Going to a pane that is already open changes only where this
        // window is.
        let scope = if let Some(channel) = channel {
            self.navigate_to_channel(channel)
        } else {
            let directory = self.unchanneled_card_worktree(&card).map(Path::to_path_buf);
            directory.map_or_else(Task::none, |directory| {
                self.navigate_to_worktree(directory, false)
            })
        };
        let focus = self.focus_workspace_card(id);
        // Attention belongs to the native parent. Reveal its transcript before
        // acknowledging it, but only after navigation has actually succeeded.
        let (transcript, read) = if self.workspace.focused() == Some(id) {
            let parent = match self.cards.get(&id) {
                Some(CardRuntime::Chat(runtime)) => Some(runtime.target),
                _ => None,
            };
            let transcript = parent.map_or_else(Task::none, |target| {
                self.handle_chat_ui(chat_ui::Action::SelectSubagent(target, None))
            });
            (transcript, self.mark_cards_read(vec![id]))
        } else {
            (Task::none(), Task::none())
        };
        Task::batch([scope, focus, transcript, read])
    }

    /// The user has read `card`'s update.
    fn read_card_update(&mut self, card: CardId) {
        self.read_card_updates(&[card]);
    }

    /// The user has read these cards' updates. The rows the Attention list
    /// draws are gathered once, before any read changes them, and only when
    /// there is an update to read.
    fn read_card_updates(&mut self, cards: &[CardId]) {
        let mut listed = None;
        for &card in cards {
            if !self.sidebar_state.has_card_update(card) {
                continue;
            }
            let listed = listed.get_or_insert_with(|| self.attention_listed_rows());
            let placement = self.attention_placement(card, listed);
            self.sidebar_state
                .acknowledge_agent_update(card, &placement);
        }
    }

    /// The rows the Attention list draws now. Only that list keeps rows, so
    /// nothing is gathered under another filter.
    fn attention_listed_rows(&self) -> Vec<sidebar::SelectionRow> {
        if self.sidebar_state.filter() == sidebar::SidebarFilter::Attention {
            self.visible_sidebar_rows()
        } else {
            Vec::new()
        }
    }

    /// Where `card`'s attention shows among `listed`, so dealing with it
    /// keeps those rows in place.
    fn attention_placement<'a>(
        &self,
        card: CardId,
        listed: &'a [sidebar::SelectionRow],
    ) -> sidebar::AttentionPlacement<'a> {
        if listed.is_empty() {
            return sidebar::AttentionPlacement::default();
        }
        sidebar::AttentionPlacement {
            channels: channels_holding(&self.durable.channels, card)
                .map(|channel| channel.id)
                .collect(),
            listed,
        }
    }

    fn mark_cards_read(&mut self, cards: Vec<CardId>) -> Task<Message> {
        self.read_card_updates(&cards);
        let tasks: Vec<_> = cards
            .into_iter()
            .map(|card| {
                self.acknowledge_card_notices(card);
                if let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
                    && let Err(error) = handle.acknowledge_updates()
                {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not acknowledge terminal output: {error}"
                    )));
                }
                self.acknowledge_chat_updates(card)
            })
            .collect();
        self.refresh_sidebar_channels();
        Task::batch(tasks)
    }

    fn acknowledge_active_pane(&mut self) -> Task<Message> {
        self.acknowledge_active_pane_with_notices(true)
    }

    fn acknowledge_active_pane_with_notices(&mut self, acknowledge_notices: bool) -> Task<Message> {
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
        self.read_card_update(card);
        if let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
            && let Err(error) = handle.acknowledge_updates()
        {
            self.status = Some(TransientNotice::error(format!(
                "Could not acknowledge terminal output: {error}"
            )));
        }
        if acknowledge_notices {
            self.acknowledge_card_notices(card);
        }
        self.acknowledge_chat_updates(card)
    }

    fn card_state(&self, id: CardId) -> Option<&CardState> {
        self.card_index(id)
            .and_then(|index| self.durable.cards.get(index))
    }

    fn card_index(&self, id: CardId) -> Option<usize> {
        self.card_indices
            .get(&id)
            .copied()
            .filter(|index| {
                self.durable
                    .cards
                    .get(*index)
                    .is_some_and(|card| card.id == id)
            })
            .or_else(|| self.durable.cards.iter().position(|card| card.id == id))
    }

    /// Copy-on-write access; callers must confirm a change is needed first, as
    /// this clones the workspace while the writer thread holds the last snapshot.
    fn card_state_mut(&mut self, id: CardId) -> Option<&mut CardState> {
        let index = self.card_index(id)?;
        Arc::make_mut(&mut self.durable).cards.get_mut(index)
    }

    fn presence(&self, id: CardId) -> &PanePresence {
        self.presence.get(&id).unwrap_or(&PanePresence::ABSENT)
    }

    fn exposure(&self, id: CardId) -> Exposure {
        self.presence(id).exposure
    }

    fn residency(&self, id: CardId) -> &Residency {
        &self.presence(id).residency
    }

    /// Records where a card of this workspace is on screen. A card the
    /// workspace does not hold has no presence to record.
    fn set_exposure(&mut self, id: CardId, exposure: Exposure) {
        if let Some(presence) = self.presence_mut(id) {
            presence.exposure = exposure;
        }
    }

    /// Records what a card's runtime holds, as [`Self::set_exposure`] does.
    fn set_residency(&mut self, id: CardId, residency: Residency) {
        if let Some(presence) = self.presence_mut(id) {
            presence.residency = residency;
        }
    }

    fn set_presence(&mut self, id: CardId, exposure: Exposure, residency: Residency) {
        if let Some(presence) = self.presence_mut(id) {
            *presence = PanePresence {
                exposure,
                residency,
            };
        }
    }

    /// Places a card the workspace now holds and hands its runtime to the
    /// lifecycle, which decides from here on what it keeps.
    fn register_presence(&mut self, id: CardId, exposure: Exposure, residency: Residency) {
        self.set_presence(id, exposure, residency.clone());
        self.lifecycle
            .register(id, exposure, residency, Instant::now());
    }

    fn presence_mut(&mut self, id: CardId) -> Option<&mut PanePresence> {
        self.card_index(id)?;
        Some(self.presence.entry(id).or_default())
    }

    /// The presence of every card still in the workspace. Runs whenever the
    /// card index is rebuilt, which is after every change to the card list.
    fn prune_presence(&mut self) {
        self.presence
            .retain(|id, _| self.card_indices.contains_key(id));
    }

    /// The terminal a card shows, once the card is not in the middle of
    /// starting one.
    ///
    /// A relaunch carries the previous exit forward as metadata, so between
    /// pressing Restart and the daemon answering the session still reads as
    /// ended. Hydration is what tells the two apart.
    fn settled_terminal(&self, id: CardId) -> Option<&TerminalSession> {
        if matches!(self.residency(id), Residency::Hydrating { .. }) {
            return None;
        }
        match &self.card_state(id)?.kind {
            CardKind::Terminal(session) => Some(session),
            _ => None,
        }
    }

    /// Whether an open terminal card starts revealed, or `None` for any other
    /// card.
    fn terminal_start_visibility(&self, id: CardId) -> Option<bool> {
        self.card_state(id)
            .filter(|state| state.existence.is_open())
            .filter(|state| matches!(&state.kind, CardKind::Terminal(_)))
            .map(|_| self.exposure(id) == Exposure::Visible)
    }

    fn completed_terminal(&self, id: CardId) -> Option<&TerminalSession> {
        self.settled_terminal(id)
            .filter(|session| terminal_completed(session))
    }

    fn ended_terminal(&self, id: CardId) -> Option<&TerminalSession> {
        self.settled_terminal(id)
            .filter(|session| terminal_ended(session))
    }

    /// Renames a card to the title its own runtime reported, and answers whether
    /// the durable state changed.
    ///
    /// A shell that emits a bare `OSC 0` or a page with no `<title>` reports an
    /// empty string, and a title of nothing but formatting normalises to one.
    /// Storing it would blank the card header, the card's accessible name and
    /// the window title all at once, so the card keeps the name it already had.
    fn apply_reported_title(&mut self, id: CardId, title: String) -> bool {
        if self
            .card_state(id)
            .is_some_and(|card| self.managed_agent_title(card).is_some())
        {
            return false;
        }
        if self.chrome_ui.user_titles.contains_key(&id) {
            return false;
        }
        let Some(title) = normalize_card_name(&title) else {
            return false;
        };
        let presentation = if self
            .card_state(id)
            .is_some_and(|card| matches!(card.kind, CardKind::Browser(_)))
        {
            browser_host::TITLE
        } else {
            &title
        };
        let _ = self.workspace.rename_card(id, presentation.to_owned());
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

    /// Rebuilds `card_indices` when the durable workspace is not the one it was
    /// built from. Copy-on-write gives every changed workspace a new address,
    /// and the weak reference keeps the old address from being reused.
    fn index_cards(&mut self) {
        if self.card_indices_source.as_ptr() == Arc::as_ptr(&self.durable) {
            return;
        }
        #[cfg(test)]
        terminal_output_tests::record_card_index();
        self.card_indices_source = Arc::downgrade(&self.durable);
        self.card_indices.clear();
        self.card_indices.extend(
            self.durable
                .cards
                .iter()
                .enumerate()
                .map(|(index, card)| (card.id, index)),
        );
        self.prune_presence();
    }

    /// Rebuilds everything the workspace derives for the sidebar, the panes'
    /// chrome and inspectors, source control and worktree protection, from
    /// the state as it now stands. This is the `Full` projection, which every
    /// narrower one must agree with.
    fn project_workspace(&mut self, reconcile_row_selection: bool) -> Task<Message> {
        projection_stage!("worktree_protections", self.sync_worktree_protections());
        projection_stage!("sidebar_selection", self.synchronize_sidebar_selection());
        projection_stage!("sidebar_channels", self.refresh_sidebar_channels());
        projection_stage!(
            "retained_attention",
            self.sidebar_state
                .prune_retained_attention(&self.sidebar_model)
        );
        if reconcile_row_selection {
            projection_stage!("row_selection", self.prune_sidebar_row_selection());
        }
        projection_stage!("worktree_panes", self.refresh_sidebar_worktree_panes());
        // Ahead of the rebind, which pins what the inspector drawing source
        // control chose: a pane that has just arrived, closed or taken focus
        // must not show the last binding for a frame.
        projection_stage!("pane_inspectors", self.sync_pane_inspectors());
        projection_stage!("pull_request_inspector", self.sync_pull_request_inspector());
        let binding = projection_stage!("source_control", self.rebind_source_control());
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
        // `view` cannot rebuild the chrome cache itself, so refresh it here.
        let chrome = projection_stage!("chrome", self.sync_chrome());
        let terminal_probes =
            projection_stage!("transcript_probes", self.terminal_transcript_probes());
        // Another project shown, or a review or the workspace switcher closed.
        let approval =
            projection_stage!("automation_requests", self.read_owed_automation_requests());
        Task::batch([binding, chrome, terminal_probes, approval])
    }

    /// Builds chrome for cards that appeared or were renamed, drops entries for
    /// cards that closed or the workspace no longer holds, and renames the
    /// window. It visits only cards that wear chrome (`wears_chrome`), so it
    /// costs what the window can show rather than every card it ever held.
    fn sync_chrome(&mut self) -> Task<Message> {
        #[cfg(test)]
        terminal_output_tests::record_chrome();
        self.terminal_working_directories
            .retain(|card, _| self.cards.contains_key(card));
        self.terminal_find
            .retain(|card, _| self.cards.contains_key(card));
        self.index_cards();
        // A moved worktree keeps its pane numbers. Reconcile its new scope's
        // counter before minting a number, including when insertion precedes
        // an existing pane in the saved order.
        if self.durable.cards.iter().any(|card| {
            matches!(card.kind, CardKind::Terminal(_))
                && !self.chrome_ui.terminal_ordinals.contains_key(&card.id)
        }) {
            for card in &self.durable.cards {
                if let Some(ordinal) = self.chrome_ui.terminal_ordinals.get(&card.id).copied() {
                    let scope = self
                        .unchanneled_card_worktree(card)
                        .unwrap_or_else(|| Path::new(""))
                        .to_owned();
                    let next = self
                        .chrome_ui
                        .terminal_next_ordinals
                        .entry(scope)
                        .or_default();
                    *next = (*next).max(ordinal);
                }
            }
        }
        for card in &self.durable.cards {
            if matches!(card.kind, CardKind::Terminal(_))
                && !self.chrome_ui.terminal_ordinals.contains_key(&card.id)
            {
                let scope = self
                    .unchanneled_card_worktree(card)
                    .unwrap_or_else(|| Path::new(""))
                    .to_owned();
                let next = self
                    .chrome_ui
                    .terminal_next_ordinals
                    .entry(scope)
                    .or_default();
                *next += 1;
                self.chrome_ui.terminal_ordinals.insert(card.id, *next);
            }
        }
        // The first titled channel holding each card, as `card_display_title`
        // finds it, in one pass rather than one channel scan per chat.
        let mut titled_channels = HashMap::new();
        for channel in self
            .durable
            .channels
            .iter()
            .filter(|channel| !channel.title.is_empty())
        {
            for card in &channel.cards {
                titled_channels.entry(*card).or_insert(channel);
            }
        }
        let held_closed = self.closed_cards_on_the_strip();
        let mut presentation_updates = Vec::new();
        let mut dressed = 0;
        for card in &self.durable.cards {
            if !Self::wears_chrome(card, &held_closed) {
                continue;
            }
            dressed += 1;
            let (chrome, title) =
                self.project_card_chrome(card, titled_channels.get(&card.id).copied());
            if let Some(chrome) = chrome {
                self.card_chrome.insert(card.id, chrome);
            }
            if let Some(title) = title {
                presentation_updates.push((card.id, title));
            }
        }
        for (id, title) in presentation_updates {
            let _ = self.workspace.rename_card(id, title);
        }
        // Every card that wears chrome now has an entry, so any other entry is
        // for a card that closed or left the workspace.
        if self.card_chrome.len() != dressed {
            let durable = &self.durable;
            let indices = &self.card_indices;
            self.card_chrome.retain(|id, _| {
                indices
                    .get(id)
                    .and_then(|index| durable.cards.get(*index))
                    .is_some_and(|card| Self::wears_chrome(card, &held_closed))
            });
        }
        // A closed terminal keeps its number, so the ones opened after it
        // keep theirs; only a card gone from the durable workspace gives its
        // number up, and that changes the workspace's address.
        if self.terminal_ordinals_source.as_ptr() != Arc::as_ptr(&self.durable) {
            self.terminal_ordinals_source = Arc::downgrade(&self.durable);
            self.chrome_ui
                .terminal_ordinals
                .retain(|id, _| self.card_indices.contains_key(id));
        }
        self.sync_window_title()
    }

    /// Whether `card` gets chrome, the tab, title and accessible names drawn
    /// for a pane. A closed card is drawn nowhere, and a long-lived workspace
    /// holds far more closed cards than open ones, so they get none unless
    /// the workspace strip still holds one (`held_closed`). A scoped update
    /// that drops such a card from the strip without naming it leaves its
    /// chrome until the next full projection, which the projection oracle
    /// would report; none does, as the strip lets a pane go before its card
    /// closes.
    fn wears_chrome(card: &CardState, held_closed: &[CardId]) -> bool {
        !matches!(card.existence, Existence::Closed { .. }) || held_closed.contains(&card.id)
    }

    /// Closed durable cards the workspace strip still holds. The strip is
    /// rebuilt from open cards and a close leaves it before the card closes,
    /// so this is normally empty; it keeps chrome for anything that does not.
    fn closed_cards_on_the_strip(&self) -> Vec<CardId> {
        self.workspace
            .order()
            .filter(|id| {
                self.card_state(*id)
                    .is_some_and(|card| matches!(card.existence, Existence::Closed { .. }))
            })
            .collect()
    }

    /// Both projection paths share title precedence, caching, accessibility and
    /// terminal presentation. Only the set of cards to visit differs.
    fn project_card_chrome(
        &self,
        card: &CardState,
        channel: Option<&crate::model::ProjectChannel>,
    ) -> (Option<CardChrome>, Option<String>) {
        #[cfg(test)]
        if !projection_oracle::verifying() {
            self.shared_views
                .chrome_cards_visited
                .set(self.shared_views.chrome_cards_visited.get() + 1);
        }
        let type_label = card_kind_labels(Some(&card.kind)).type_label;
        let title = self.repository_chrome_with(card, || {
            self.card_display_title_with_chat_title(card, || self.chat_channel_title(card, channel))
        });
        let presentation = if matches!(card.kind, CardKind::Browser(_))
            && self
                .workspace
                .card(card.id)
                .is_some_and(|pane| pane.accessible_name != title)
        {
            Some(title.to_string())
        } else {
            None
        };
        let caption_source = self.card_caption_source(card);
        let chrome = if self
            .card_chrome
            .get(&card.id)
            .is_none_or(|chrome| !chrome.describes(type_label, &title, &caption_source))
        {
            let caption = caption_source.caption();
            let mut chrome = CardChrome::new(card.id, type_label, &title, &caption)
                .with_caption_source(caption_source.into_owned());
            if matches!(card.kind, CardKind::Terminal(_))
                && !self.chrome_ui.user_titles.contains_key(&card.id)
                && let Some(ordinal) = self.chrome_ui.terminal_ordinals.get(&card.id)
            {
                let suffix = format!(" ({ordinal})");
                let base = title.strip_suffix(&suffix).unwrap_or(&title);
                chrome.compact_title = Some(format!("{ordinal} {base}"));
                chrome.terminal_ordinal_suffix = Some(suffix);
                if let CardKind::Terminal(session) = &card.kind {
                    let reported = self.card_display_title(card);
                    let directory = self
                        .terminal_working_directories
                        .get(&card.id)
                        .unwrap_or(&session.launch.working_directory);
                    chrome.terminal_title_source = Some(TerminalTitleSource {
                        reported: reported.to_owned(),
                        directory: directory.clone(),
                        program: session.launch.program.clone(),
                        ordinal: *ordinal,
                        fallback: TerminalTitleSource::fallback(reported),
                    });
                }
            }
            Some(chrome)
        } else {
            None
        };
        (chrome, presentation)
    }

    fn install_card_chrome(
        &mut self,
        updates: Vec<(CardId, (Option<CardChrome>, Option<String>))>,
    ) {
        for (id, (chrome, title)) in updates {
            if let Some(chrome) = chrome {
                self.card_chrome.insert(id, chrome);
            }
            if let Some(title) = title {
                let _ = self.workspace.rename_card(id, title);
            }
        }
    }

    fn sync_scoped_chrome(&mut self, cards: &[CardId]) -> Task<Message> {
        self.index_cards();
        let held_closed = self.closed_cards_on_the_strip();
        // Ordinals require a global counter reconciliation; missing chrome can
        // also mean this is the first projection after restoration.
        if cards.iter().any(|id| {
            self.card_state(*id).is_none_or(|card| {
                (Self::wears_chrome(card, &held_closed) && !self.card_chrome.contains_key(id))
                    || (matches!(card.kind, CardKind::Terminal(_))
                        && !self.chrome_ui.terminal_ordinals.contains_key(id))
            })
        }) {
            return self.sync_chrome();
        }
        #[cfg(test)]
        terminal_output_tests::record_chrome();
        self.terminal_working_directories
            .retain(|card, _| self.cards.contains_key(card));
        self.terminal_find
            .retain(|card, _| self.cards.contains_key(card));
        for id in cards {
            if self
                .card_state(*id)
                .is_some_and(|card| !Self::wears_chrome(card, &held_closed))
            {
                self.card_chrome.remove(id);
            }
        }
        let updates = cards
            .iter()
            .filter_map(|id| {
                let card = self
                    .card_state(*id)
                    .filter(|card| Self::wears_chrome(card, &held_closed))?;
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| !channel.title.is_empty() && channel.cards.contains(id));
                Some((*id, self.project_card_chrome(card, channel)))
            })
            .collect::<Vec<_>>();
        self.install_card_chrome(updates);
        self.sync_window_title()
    }

    /// Present the current editor file without overwriting a saved pane name.
    /// The full path in the caption distinguishes files sharing a basename.
    fn repository_chrome<'a>(&'a self, card: &'a CardState) -> Cow<'a, str> {
        self.repository_chrome_with(card, || self.card_display_title(card))
    }

    /// `repository_chrome` with the card's display title supplied, so a pass
    /// over every card can resolve channel titles once.
    fn repository_chrome_with<'a>(
        &'a self,
        card: &'a CardState,
        display_title: impl FnOnce() -> &'a str,
    ) -> Cow<'a, str> {
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card.id)
            && let Some(path) = runtime.selected_path()
            && let Some(name) = path.file_name()
        {
            return self.chrome_ui.user_titles.get(&card.id).map_or_else(
                || name.to_string_lossy(),
                |title| Cow::Borrowed(title.as_str()),
            );
        }
        let title = display_title();
        if let CardKind::Terminal(session) = &card.kind
            && !self.chrome_ui.user_titles.contains_key(&card.id)
        {
            let directory = self
                .terminal_working_directories
                .get(&card.id)
                .unwrap_or(&session.launch.working_directory);
            if let Some(ordinal) = self.chrome_ui.terminal_ordinals.get(&card.id)
                && let Some(chrome) = self.card_chrome.get(&card.id)
                && chrome.terminal_title_source.as_ref().is_some_and(|source| {
                    source.describes(title, directory, &session.launch.program, *ordinal)
                })
            {
                return Cow::Borrowed(chrome.title.as_str());
            }
            let title = if TerminalTitleSource::fallback(title) {
                let process = Path::new(&session.launch.program)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                Cow::Owned(crate::names::process_title(&process, directory))
            } else {
                Cow::Borrowed(title)
            };
            if let Some(ordinal) = self.chrome_ui.terminal_ordinals.get(&card.id) {
                return Cow::Owned(format!("{title} ({ordinal})"));
            }
            return title;
        }
        Cow::Borrowed(title)
    }

    /// What `card`'s caption is formatted from: the editor's file, a
    /// terminal's reported directory, or what the card's kind names.
    fn card_caption_source<'a>(&'a self, card: &'a CardState) -> CaptionSource<'a> {
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card.id)
            && let Some(path) = runtime.selected_path()
            && path.file_name().is_some()
        {
            return CaptionSource::File {
                worktree: Cow::Borrowed(&runtime.descriptor.worktree),
                path: Cow::Borrowed(path),
            };
        }
        if matches!(card.kind, CardKind::Terminal(_))
            && let Some(directory) = self.terminal_working_directories.get(&card.id)
        {
            return CaptionSource::Directory(Cow::Borrowed(directory));
        }
        match &card.kind {
            CardKind::Terminal(session) if self.relaunched_terminals.contains(&session.id) => {
                CaptionSource::Relaunched(Cow::Borrowed(&session.launch.working_directory))
            }
            kind => CaptionSource::of_kind(kind),
        }
    }

    /// Automatic spawn identity applies only while this card still presents
    /// the exact managed process. A replacement conversation owns its title.
    fn managed_agent_title<'a>(&'a self, card: &CardState) -> Option<&'a str> {
        use crate::agent_execution::HostedSession;
        let source = self.chrome_ui.agent_title_sources.get(&card.id)?;
        let matches = match (source, &card.kind) {
            (HostedSession::Chat(source), CardKind::Chat(current)) => {
                crate::agent_execution::same_chat_process(source, current)
            }
            (HostedSession::Terminal(source), CardKind::Terminal(current)) => {
                crate::agent_execution::same_terminal_process(source, current)
            }
            _ => false,
        };
        matches
            .then(|| {
                self.chrome_ui
                    .agent_labels
                    .get(&crate::model::AgentId::from_uuid(card.id.as_uuid()))
                    .map(String::as_str)
            })
            .flatten()
    }

    /// Provider activity belongs in the status icon, including restored titles.
    fn card_display_title<'a>(&'a self, card: &'a CardState) -> &'a str {
        self.card_display_title_with_chat_title(card, || {
            self.chat_channel_title(
                card,
                self.durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card.id) && !channel.title.is_empty()),
            )
        })
    }

    /// `title`, as a row that names the pane's checkout `place` beside it
    /// shows it. A shell that sets no title worth showing is titled
    /// "command · directory"; while the shell is in that checkout the place
    /// already says where it is, so the row keeps the command. A shell
    /// elsewhere, a channel's place, and a title the user gave keep it whole.
    pub(super) fn titled_beside_place<'a>(
        &self,
        card: &CardState,
        title: &'a str,
        place: Option<&Path>,
    ) -> &'a str {
        let CardKind::Terminal(session) = &card.kind else {
            return title;
        };
        if self.chrome_ui.user_titles.contains_key(&card.id) {
            return title;
        }
        let directory = self
            .terminal_working_directories
            .get(&card.id)
            .unwrap_or(&session.launch.working_directory);
        if !place.is_some_and(|place| directory.starts_with(place)) {
            return title;
        }
        crate::names::process_title_command(title, directory).unwrap_or(title)
    }

    /// The title of a pane's sidebar row, which names the checkout the
    /// workspace files the pane under.
    pub(super) fn pane_row_title<'a>(&'a self, card: &'a CardState) -> &'a str {
        self.titled_beside_place(
            card,
            self.card_display_title(card),
            self.card_worktree(card),
        )
    }

    /// The title a chat card shows from `channel`, the first titled channel
    /// holding it: a provider-default chat, or the primary chat of a channel
    /// the user named, is presented by its channel's name.
    fn chat_channel_title<'a>(
        &self,
        card: &CardState,
        channel: Option<&'a crate::model::ProjectChannel>,
    ) -> Option<&'a str> {
        let CardKind::Chat(session) = &card.kind else {
            return None;
        };
        let channel = channel?;
        ((card.title == chat_ui::chat_card_title(session.launch.provider)
            && self.managed_agent_title(card).is_none())
            || (channel.title_source == crate::model::ChannelTitleSource::Manual
                && self.channel_primary_chat(channel) == Some(card.id)))
        .then_some(channel.title.as_str())
    }

    /// Batch projections can resolve channel membership once while sharing the
    /// same user-title, browser-title and provider-title precedence.
    fn card_display_title_with_chat_title<'a>(
        &'a self,
        card: &'a CardState,
        chat_title: impl FnOnce() -> Option<&'a str>,
    ) -> &'a str {
        if let Some(title) = self.chrome_ui.user_titles.get(&card.id) {
            return title;
        }
        if matches!(card.kind, CardKind::Browser(_)) {
            return browser_host::TITLE;
        }
        if matches!(card.kind, CardKind::Chat(_))
            && let Some(title) = chat_title()
        {
            return title;
        }
        if let Some(title) = self.managed_agent_title(card) {
            return title;
        }
        let title = crate::agent::codex_title_without_status(&card.title);
        // Provider recognition can only change an empty or status-prefixed
        // title. Most saved cards already have plain names; avoid decoding
        // their recovery recipes on every update and redraw.
        if !card.title.is_empty() && title == card.title {
            return &card.title;
        }
        if let CardKind::Terminal(session) = &card.kind
            && (self
                .sidebar_state
                .card_activity(card.id)
                .is_some_and(|activity| activity.provider == crate::agent::Provider::Codex)
                || crate::agent::Provider::recognize_launch(&session.launch)
                    == Some(crate::agent::Provider::Codex)
                || session
                    .agent_resume_recipe()
                    .as_ref()
                    .and_then(crate::agent::Provider::recognize_launch)
                    == Some(crate::agent::Provider::Codex))
        {
            return if title.is_empty() {
                crate::agent::Provider::Codex.label()
            } else {
                title
            };
        }
        &card.title
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
                .map(|card| self.repository_chrome(card))
                .unwrap_or(Cow::Borrowed(APPLICATION_NAME));
            let showing = showing.trim();
            let showing = if showing.is_empty() {
                APPLICATION_NAME
            } else {
                showing
            };
            if self.core.window.header_title == showing {
                return Task::none();
            }
            showing.to_owned()
        };
        let mut window_title = if header == APPLICATION_NAME {
            header.clone()
        } else {
            format!("{header}{WINDOW_TITLE_SEPARATOR}{APPLICATION_NAME}")
        };
        if let Some(device) = crate::device::selected() {
            window_title = format!("{} · {}", device.name, window_title);
        }
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
            code: terminal_failure::runtime_error_code(&message, "runtime_hydration_failed").into(),
            message,
        });
        if !self.lifecycle.hydration_finished(card, epoch, result) {
            return false;
        }
        if let Some(residency) = self
            .lifecycle
            .state(card)
            .map(|(_, residency)| residency.clone())
        {
            self.set_residency(card, residency);
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
                code: terminal_failure::runtime_error_code(&message, "runtime_failed").into(),
                message,
            },
            retryable: true,
        };
        self.lifecycle
            .register(card, exposure, residency.clone(), Instant::now());
        self.set_residency(card, residency);
    }
    fn fail_runtime(&mut self, card: CardId, message: String) {
        let mut failure = self.terminal_failure_payload(card, message.clone());
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
                | CardRuntime::Automations
                | CardRuntime::Work,
            )
            | None => {}
        }
        self.mark_runtime_failed(card, message);
        failure.enrich_assigned_failure(self.card_state(card).map(|_| self.residency(card)));
        self.cards.insert(card, CardRuntime::Failed(failure));
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
        if self
            .terminal_paste
            .as_ref()
            .is_some_and(|(target, _)| *target == card)
        {
            self.terminal_paste = None;
        }
        // An end is one fact, recorded by whichever report of it arrives
        // first. A surface attached to a terminal that had already ended
        // replays that report; recording it again would move the exit to now
        // and announce it a second time.
        if let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
            && let Some((instance, owner)) = handle.execution_instance().zip(handle.host_proof())
            && let Some(CardKind::Terminal(source)) = self.card_state(card).map(|s| &s.kind)
        {
            self.shared_views.terminal_source_workers.insert(
                card,
                (
                    source.id,
                    crate::workspace_model::views::TerminalRemovalBasis::Worker { owner, instance },
                ),
            );
        }
        let already_ended = self.ended_terminal(card).is_some();
        if !already_ended
            && let Some(state) = self.card_state_mut(card)
            && let CardKind::Terminal(session) = &mut state.kind
        {
            transition_terminal_to_relaunchable(session, terminal_exit(exit_code, signal));
        }
        self.lifecycle.remove(card);
        self.sidebar_state.clear_agent_activity(card);
        self.set_residency(card, Residency::Hibernated);
        if !already_ended && !intentional && (exit_code != 0 || signal.is_some()) {
            self.status = Some(TransientNotice::error(match signal {
                Some(signal) => format!("terminal exited with {signal}"),
                None => format!("terminal exited with status {exit_code}"),
            }));
        }
    }

    fn completed_terminal_has_native_owner(&self, card: CardId) -> bool {
        // Only a clean finish hands the card to a native conversation. A
        // crashed agent has nothing to open; its card offers a restart.
        self.completed_terminal(card).is_some_and(|session| {
            self.chat_has_parked_runtime(card)
                || chat_ui::conversion(session).is_ok_and(|chat| chat.owns_codex_listener())
        })
    }

    fn retry_card(&mut self, card: CardId) -> Task<Message> {
        self.retry_card_as(card, None)
    }

    /// Retries `card`; a chat starts with `chat_intent` when given, rather
    /// than the intent a retry would choose for it.
    fn retry_card_as(
        &mut self,
        card: CardId,
        chat_intent: Option<crate::chat::Intent>,
    ) -> Task<Message> {
        // A card whose close is already accepted never starts anything again.
        if self
            .card_state(card)
            .is_none_or(|state| !state.existence.is_open())
            || self.pane_close_owned(card)
            || self.chat_start_pending(card)
            || self.chat_retry_blocked(card)
            // One relaunch at a time: a second would replace the identity the
            // first is starting and tear down whatever it launched.
            || self.terminal_start_in_flight(card)
            || self.refuse_for_deleting_card(card)
            // A fenced pane's saved session is not the one its agent runs, so
            // no route may reattach or relaunch it.
            || self.shared_views.quarantined.contains(&card)
        {
            return Task::none();
        }
        // Nothing has started in a pane whose reopen failed: it asks the
        // service to prepare its conversation again.
        if self.reopen_failed(card) {
            return self.retry_reopening(card);
        }
        #[cfg(feature = "integrated-browser")]
        if matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Browser(_))
        ) && let Some(reason) = self.browser_unavailable_reason()
        {
            // Only a check that may succeed on a second try is worth
            // retrying; the pane waits for it and loads if it finds the
            // browser.
            if !reason.is_recoverable() {
                return Task::none();
            }
            self.cards.insert(card, CardRuntime::Loading);
            return self.recheck_browser();
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
            return self.open_external_url(session.current_url.clone());
        }
        // A utility pane starts nothing, so a retry only reinstalls its view.
        if state.kind.is_utility() {
            self.cards.insert(card, CardRuntime::utility(&state.kind));
            return Task::none();
        }
        if self.saved_terminal_current(card).is_some() {
            return self.inspect_saved_terminal(card);
        }
        self.saved_terminals.entries.remove(&card);
        let retry_details = self.terminal_details(card);
        let terminal_replacement = if self.pending_terminal_reservation(card).is_none()
            && let CardKind::Terminal(session) = &mut state.kind
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
        let exposure = self.exposure(card);
        self.register_presence(card, exposure, Residency::Hydrating { epoch });
        if self
            .card_state(card)
            .is_some_and(|durable_state| durable_state.kind != state.kind)
            && let Some(durable_state) = self.card_state_mut(card)
        {
            durable_state.kind = state.kind.clone();
        }
        self.status = None;
        if let Some((expected_terminal_id, _)) = &terminal_replacement {
            // A fresh actor must not inherit its predecessor's sidebar news.
            // Retiring that attribution does not acknowledge the old journal.
            self.sidebar_state.forget_card_attention(card);
            // The ended identity's final-screen outcome is spent.
            self.retained_outputs.remove(expected_terminal_id);
        }
        if let Some((expected_terminal_id, replacement)) = terminal_replacement
            && let Some(session) = &self.session
        {
            session.replace_terminal_identity(card, expected_terminal_id, replacement);
        }
        self.persist();

        match state.kind {
            CardKind::Terminal(session) => {
                self.startup_gate.initial_panes.explicit_action(card);
                self.terminal_recovery_attempts.remove(&session.id);
                self.cards.insert(card, CardRuntime::Loading);
                let task = self.start_terminal(card, session, exposure == Exposure::Visible, true);
                if let Some(details) = retry_details {
                    self.track_terminal_retry(card, details, task)
                } else {
                    task
                }
            }
            #[cfg(feature = "integrated-browser")]
            CardKind::Browser(session) => {
                self.cards.insert(card, CardRuntime::Loading);
                let Some(factory) = self.browser_factory.clone() else {
                    return cosmic::task::message(Message::InitializeBrowserFactory);
                };
                match spawn_browser(
                    &session,
                    exposure == Exposure::Visible,
                    effective_surface_scale(self.window_scale_factor, self.settings.ui_scale),
                    Some(factory),
                ) {
                    Ok(runtime) => {
                        self.cards
                            .insert(card, CardRuntime::Browser(Box::new(runtime)));
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
            CardKind::Repository(view) => self.attach_repository_pane(card, view),
            CardKind::PullRequests(view) => self.attach_pull_request_pane(card, view),
            CardKind::Chat(session) => {
                let intent = chat_intent.unwrap_or_else(|| self.chat_retry_intent(card, &session));
                self.open_chat(card, session, intent)
            }
            CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
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

    /// Whether `card` is where the keyboard belongs: the focused pane, in the
    /// widget tree, with nothing over the workspace taking input instead.
    ///
    /// Work that finishes behind the user's back - a terminal starting, a
    /// chat attaching - asks this before focusing its own control. A focus
    /// operation unfocuses every widget that is not its target, so one issued
    /// for a pane that is not in use takes the keyboard from the one that is.
    fn card_holds_keyboard(&self, card: CardId) -> bool {
        self.workspace.focused() == Some(card)
            && self
                .workspace
                .visibility(card)
                .is_some_and(VisibilityClass::is_mounted)
            && !self.pane_content_inputs_blocked(card)
    }

    fn restore_repository_editor_focus(&mut self) -> Task<Message> {
        let Some(card) = self
            .workspace
            .focused()
            .filter(|card| self.card_holds_keyboard(*card))
        else {
            return Task::none();
        };
        let placeholder = self.card_header_widget_id(card);
        let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        runtime
            .take_restored_editor_focus(card)
            .map_or_else(Task::none, |target| {
                workspace_widget::focus_after_loading(placeholder, target).map(cosmic::Action::App)
            })
    }

    /// Whether `card` is a chat pane that owes its message input the keyboard
    /// once that input is drawn.
    fn owes_chat_input(&self, card: CardId) -> bool {
        self.is_chat_card(card) && !self.chat_input_drawn(card)
    }

    fn is_chat_card(&self, card: CardId) -> bool {
        matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(_))
        )
    }

    /// Gives the keyboard to the focused pane's own control: a chat's message
    /// input, a terminal, an editor. Every path that focuses a pane hands the
    /// keyboard over through here or through the pointer and window variants
    /// below, so what happens while the input is not drawn yet is decided in
    /// one place. Closing a dialog only gives the keyboard back to the pane
    /// that already had it, and asks [`Self::card_keyboard_focus`] directly.
    ///
    /// A chat that is still starting or changing session has no input yet.
    /// Its title holds the keyboard meanwhile and the debt is recorded;
    /// [`Self::settle_owed_keyboard`] pays it when the input appears, unless
    /// the person chose another control in between.
    fn focus_pane_keyboard(&mut self, card: CardId) -> Task<Message> {
        self.keyboard_owed = self.owes_chat_input(card).then_some(card);
        iced::widget::operation::focus(self.card_keyboard_focus(card)).map(cosmic::Action::App)
    }

    /// The keyboard after a press inside the focused pane `card`. A press on
    /// a control that takes focus - a find field, transcript text being
    /// selected, a terminal - keeps it; a press anywhere else in a chat,
    /// terminal or editor puts the keyboard in that pane's own control,
    /// with the draft and caret as they were.
    fn focus_pane_keyboard_after_press(&mut self, card: CardId) -> Task<Message> {
        if !self.card_holds_keyboard(card)
            || !matches!(
                self.cards.get(&card),
                Some(CardRuntime::Chat(_) | CardRuntime::Terminal(_) | CardRuntime::Repository(_))
            )
        {
            return Task::none();
        }
        self.keyboard_owed = self.owes_chat_input(card).then_some(card);
        workspace_widget::focus_card_if_outside(
            self.card_container_id(card),
            self.card_keyboard_focus(card),
        )
        .map(cosmic::Action::App)
    }

    /// The keyboard after the pointer moved into `card` and focused it. It
    /// comes from another pane or from nowhere; a field outside the strip,
    /// such as sidebar search, keeps what is being typed into it.
    fn focus_pane_keyboard_after_entry(&mut self, card: CardId) -> Task<Message> {
        if !self.card_holds_keyboard(card) {
            return Task::none();
        }
        self.keyboard_owed = self.owes_chat_input(card).then_some(card);
        workspace_widget::focus_unless_held_outside(
            self.workspace_scroll_id.clone(),
            self.card_keyboard_focus(card),
        )
        .map(cosmic::Action::App)
    }

    /// The keyboard after the window is activated: the focused pane's own
    /// control, unless some control kept focus while the window was away.
    fn focus_pane_keyboard_on_activation(&mut self) -> Task<Message> {
        let Some(card) = self.workspace.focused() else {
            return Task::none();
        };
        if !self.card_holds_keyboard(card) {
            return Task::none();
        }
        self.keyboard_owed = self.owes_chat_input(card).then_some(card);
        workspace_widget::focus_if_unheld(self.card_keyboard_focus(card)).map(cosmic::Action::App)
    }

    /// Pays a debt [`Self::focus_pane_keyboard`] recorded once the chat's
    /// input is drawn. The pane's title held the keyboard in its place, so a
    /// control the person chose meanwhile keeps it. A debt for a pane that is
    /// no longer focused lapses.
    fn settle_owed_keyboard(&mut self) -> Task<Message> {
        let Some(card) = self.keyboard_owed else {
            return Task::none();
        };
        if self.workspace.focused() != Some(card) || !self.is_chat_card(card) {
            self.keyboard_owed = None;
            return Task::none();
        }
        if !self.chat_input_drawn(card) || !self.card_holds_keyboard(card) {
            return Task::none();
        }
        self.keyboard_owed = None;
        workspace_widget::focus_after_loading(
            self.card_header_widget_id(card),
            self.card_keyboard_focus(card),
        )
        .map(cosmic::Action::App)
    }

    /// Answers a key, with the projection its answer needs. A key that
    /// does something projects everything, as the rest of `update` does:
    /// what it did may move the focus, close a surface or change what any
    /// part of the workspace shows, and nothing after it is relied on to
    /// project that. Only an answer that changed nothing a projection
    /// reads projects nothing: a key nothing took, a key a dialog's or the
    /// palette's own field answers, a step with nowhere to go. Typing
    /// reaches those at input rate.
    fn key_event(&mut self, event: keyboard::Event, captured: bool) -> (Task<Message>, Projection) {
        // A held chord copies once, so its repeats must not cancel the
        // copy its first press started.
        if let keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(key),
            modifiers,
            repeat: false,
            ..
        } = &event
            && modifiers.command()
            && (key.eq_ignore_ascii_case("c") || key.eq_ignore_ascii_case("x"))
        {
            chat_ui::invalidate_copy_intent();
        }
        if let Some(task) = self.automation_approval_key(&event, captured) {
            return (task, Projection::Full);
        }
        #[cfg(feature = "integrated-browser")]
        if is_escape_press(&event) && self.browser_import_on_top() {
            return (
                self.update_browser_import(self.browser_import.escape_action()),
                Projection::Full,
            );
        }
        if let Some(task) = self.session_recovery_key_event(&event, captured) {
            return (task, Projection::Full);
        }
        if let Some(task) = self.daemon_health_key_event(&event, captured) {
            return (task, Projection::Full);
        }
        if let keyboard::Event::ModifiersChanged(modifiers) = &event {
            self.keyboard_modifiers = *modifiers;
        }
        // The viewer dialog, a modal, takes every key. Its own Escape and an
        // open context menu's are answered in `Gallery::dialog_view`.
        if self.chat_ui.image_dialog.is_some() {
            return self
                .image_key(chat_ui::images::ViewerHost::Dialog, &event)
                .unwrap_or((Task::none(), Projection::Unchanged));
        }
        if !captured
            && !self.workspace_inputs_blocked()
            && workspace_widget::global_shortcut(
                &event,
                &self.settings.keyboard_shortcuts,
                self.focused_pane(),
            )
            .is_none()
            && let keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(key),
                modifiers,
                ..
            } = &event
            && modifiers.control()
            && modifiers.shift()
            && !modifiers.alt()
            && !modifiers.logo()
            && key.eq_ignore_ascii_case("s")
            && let Some(card) = self.workspace.focused()
            && !self.pane_content_inputs_blocked(card)
            && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
        {
            let stage = runtime.selected_staging_selection(true).is_some();
            if stage || runtime.selected_staging_selection(false).is_some() {
                return (
                    self.dispatch(Message::Repository(RepositoryAction::StageSelected {
                        card,
                        stage,
                    })),
                    Projection::Full,
                );
            }
        }
        if crate::tab_history::released(&event) {
            self.tab_history.finish(self.workspace.focused());
        }
        if crate::tab_history::modifiers_released(&event, self.workspace_cycle_modifiers)
            && let Some(since) = self.chrome_ui.workspace_preview_since
        {
            // A window losing focus is told its modifiers are released
            // just before it is told about the focus. Deciding one
            // message later lets that cancel instead of commit.
            self.workspace_cycle_modifiers = keyboard::Modifiers::empty();
            return (
                cosmic::task::message(Message::WorkspaceSwitcherReleased(since)),
                Projection::Full,
            );
        }
        // Modal key capture must not swallow the modifier release
        // that commits an already active navigation cycle.
        if let Some(answer) = self.palette_key_event(&event) {
            return answer;
        }
        if self.chrome_ui.workspace_preview.is_some() {
            return (self.workspace_switcher_key(&event), Projection::Full);
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
            if let Some(id) = self
                .tab_history
                .step(&available, self.workspace.focused(), backwards)
                && let Ok(update) = self.workspace.focus(id)
            {
                return (
                    self.apply_viewport(update, true, DurableChange::None),
                    Projection::Full,
                );
            }
            // No other pane to step to: only the history's cycle changed.
            return (Task::none(), Projection::Unchanged);
        }
        if let Some(answer) = self.chrome_key_event(&event) {
            return answer;
        }
        if is_escape_press(&event)
            && let Some(dismiss) = self.account_question_escape(captured)
        {
            return (self.dispatch(dismiss), Projection::Full);
        }
        if let Some(task) = self.account_key_event(&event) {
            return (task, Projection::Full);
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
            return (self.cancel_form_dialog(), Projection::Full);
        }
        // A close question opens over the composer or terminal it asks
        // about, which keeps its focus and captures Escape under the
        // dialog. The question answers it all the same.
        if is_escape_press(&event) && self.update_ui.confirmation.is_none() {
            if self.terminal_close_dialog_visible() {
                return (self.cancel_terminal_close(), Projection::Full);
            }
            if self.chat_close_dialog_visible() {
                return (self.dispatch(Message::CancelChatClose), Projection::Full);
            }
        }
        if self.form_dialog_on_top()
            && let Some(task) = self.resume_key_event(&event)
        {
            return (task, Projection::Full);
        }
        if is_escape_press(&event) && self.device_ui.menu_open && !self.modal_is_open() {
            return (
                self.update_device(device_ui::Action::Menu(false)),
                Projection::Full,
            );
        }
        // The model search field captures Escape to blur itself. Close
        // its picker in the same press, while dialogs retain priority.
        if is_escape_press(&event)
            && self
                .workspace
                .focused()
                .is_some_and(|card| !self.pane_content_inputs_blocked(card))
            && self.dismiss_chat_picker()
        {
            return (
                self.workspace.focused().map_or_else(Task::none, |card| {
                    iced::widget::operation::focus(card_control_id(card, "chat-composer"))
                        .map(cosmic::Action::App)
                }),
                Projection::Full,
            );
        }
        if self.settings_reset_on_top() && is_escape_press(&event) {
            return (self.close_settings_reset(), Projection::Full);
        }
        if is_escape_press(&event) && !captured {
            if self.transient_work_close.is_some() {
                return (
                    self.dispatch(Message::CancelTransientWorkClose),
                    Projection::Full,
                );
            }
            if let Some(action) = self.automation_ui.keep_draft_action() {
                return (self.update_automation_ui(action), Projection::Full);
            }
            if let Some((card, _)) = &self.terminal_paste {
                return (
                    self.handle_terminal_find(terminal_ui::Action::ConfirmPaste(*card, false)),
                    Projection::Full,
                );
            }
            if self.update_ui.confirmation.is_some() {
                return (
                    self.update_update_ui(update_ui::Action::CancelApply),
                    Projection::Full,
                );
            }
            if self.terminal_close_dialog_visible() {
                return (self.cancel_terminal_close(), Projection::Full);
            }
            if self.chat_close_dialog_visible() {
                return (self.dispatch(Message::CancelChatClose), Projection::Full);
            }
            if self.sidebar_state.pending_discard().is_some() {
                return (
                    self.dispatch(Message::Sidebar(SidebarAction::CancelDiscard)),
                    Projection::Full,
                );
            }
            if self.project_ui.modal_open() {
                return (
                    self.handle_project_action(project_ui::Action::Dismiss),
                    Projection::Full,
                );
            }
            if self.channel_ui.modal_open() {
                return (
                    self.handle_channel_ui(channel_ui::Action::Dismiss),
                    Projection::Full,
                );
            }
            if self.sidebar_selection_ui.dialog.is_some() {
                return (
                    self.update_sidebar_selection(sidebar_selection_ui::Action::Dismiss),
                    Projection::Full,
                );
            }
            if self.dirty_repository_close.is_some() {
                if self.dirty_repository_close.as_ref().is_some_and(|close| {
                    matches!(self.cards.get(&close.card), Some(CardRuntime::Repository(runtime)) if runtime.is_saving())
                }) {
                    return (Task::none(), Projection::Unchanged);
                }
                return (
                    self.dispatch(Message::CancelRepositoryClose),
                    Projection::Full,
                );
            }
            // Ahead of the inspector and the sidebar because the find
            // bar is the innermost thing the user is looking at, and
            // behind the confirmations above because those are modal.
            if let Some(card) = self.focused_repository_card_with_find() {
                return (
                    self.dispatch(Message::Repository(RepositoryAction::CloseFind { card })),
                    Projection::Full,
                );
            }
            if self.worktree_dialog.is_some() || self.pull_request_dialog.is_some() {
                return (self.cancel_form_dialog(), Projection::Full);
            }
            if self.sidebar_state.controls_expanded() {
                return (
                    self.dispatch(Message::Sidebar(SidebarAction::ToggleControls)),
                    Projection::Full,
                );
            }
            // An inspector covering a narrow pane's content closes
            // like the other surfaces over it.
            if let Some(card) = self.workspace.focused()
                && self.pane_inspectors.is_open(card)
                && self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay)
            {
                return (self.close_pane_inspector_overlay(card), Projection::Full);
            }
            // Last, behind everything drawn in front of the pane.
            if let Some(dismiss) = self.pane_escape() {
                return (self.dispatch(dismiss), Projection::Full);
            }
        }
        if let Some(backwards) = is_tab_press(&event)
            && self.dirty_repository_close.is_some()
        {
            return (self.cycle_dirty_close_focus(backwards), Projection::Full);
        }
        // A chord typed at a dialog is not the workspace's to act on:
        // the close chord must not close the card behind the dialog.
        // The dialog is an overlay, so it has already seen the
        // keystroke by the time this runs, and what it does with one
        // is its own affair.
        if self.workspace_inputs_blocked() {
            // The dialog's own fields answer what is typed at it, as their own
            // messages: this changed nothing.
            return (Task::none(), Projection::Unchanged);
        }
        if let Some(task) = self.chat_picker_key(&event) {
            return (task, Projection::Full);
        }
        // Behind every popup, question and overlay in front of the pane.
        if let Some(answer) = self.pane_image_key(&event, captured) {
            return answer;
        }
        if let Some(card) = self.workspace.focused()
            && !self.pane_content_inputs_blocked(card)
            && self.terminal_find.get(&card).is_some_and(|find| find.open)
            && let keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                ..
            } = &event
            && *key == keyboard::key::Named::Escape
            && !captured
        {
            return (
                self.handle_terminal_find(terminal_ui::Action::Close(card)),
                Projection::Full,
            );
        }
        if let keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::F3),
            modifiers,
            ..
        } = &event
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
            && let Some(card) = self.focused_repository_card_with_find()
        {
            return (
                self.dispatch(Message::Repository(if modifiers.shift() {
                    RepositoryAction::FindPrevious { card }
                } else {
                    RepositoryAction::FindNext { card }
                })),
                Projection::Full,
            );
        }
        #[cfg(feature = "integrated-browser")]
        if !captured
            && let Some(card) = self.workspace.focused()
            && !self.pane_content_inputs_blocked(card)
            && matches!(self.cards.get(&card), Some(CardRuntime::Browser(_)))
            && let Some(message) = browser_shortcut(card, &event)
        {
            return (self.dispatch(message), Projection::Full);
        }
        if !captured && let Some(task) = self.chat_scroll_key(&event) {
            return (task, Projection::Full);
        }
        if is_save_press(&event)
            && let Some(card) = self.workspace.focused()
            && !self.pane_content_inputs_blocked(card)
            && matches!(self.cards.get(&card), Some(CardRuntime::Repository(_)))
        {
            return (
                self.dispatch(Message::Repository(RepositoryAction::Save {
                    card,
                    overwrite: false,
                })),
                Projection::Full,
            );
        }
        match workspace_widget::global_shortcut(
            &event,
            &self.settings.keyboard_shortcuts,
            self.focused_pane(),
        ) {
            Some(command) => {
                let modifiers = match event {
                    keyboard::Event::KeyPressed { modifiers, .. } => modifiers,
                    _ => keyboard::Modifiers::empty(),
                };
                (
                    self.run_workspace_command(command, modifiers),
                    Projection::Full,
                )
            }
            None => match (!captured)
                .then(|| self.type_into_chat_input(&event))
                .flatten()
            {
                Some(typed) => (typed, Projection::Full),
                // Nothing answered the key: a key a text field took,
                // a release or a modifier change. What it changed on
                // the way here, the held modifiers, the tab history's
                // cycle and a copy's intent, is read by no projection.
                // Typing comes this way at input rate.
                None => (Task::none(), Projection::Unchanged),
            },
        }
    }

    /// Text typed while a chat pane is focused but nothing took the key -
    /// its transcript, a button or nothing at all holding the keyboard -
    /// goes into its message input at the caret, which then keeps the
    /// keyboard. Chords stay with whatever they belong to, such as copying a
    /// transcript selection. `None` when the key typed nothing, and so
    /// changed nothing.
    fn type_into_chat_input(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        let keyboard::Event::KeyPressed {
            text: Some(text),
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        if modifiers.control()
            || modifiers.alt()
            || modifiers.logo()
            || text.is_empty()
            || text.chars().any(char::is_control)
        {
            return None;
        }
        let card = self.workspace.focused()?;
        if !self.chat_input_drawn(card) || !self.card_holds_keyboard(card) {
            return None;
        }
        // An open picker's search field stands in for the input, and is
        // where the keyboard already belongs.
        let target = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) if runtime.focus_control() == "chat-composer" => {
                runtime.target
            }
            _ => return None,
        };
        let mut tasks: Vec<Task<Message>> = text
            .chars()
            .map(|character| {
                self.handle_chat_ui(chat_ui::Action::Composer(
                    target,
                    iced::widget::text_editor::Action::Edit(
                        iced::widget::text_editor::Edit::Insert(character),
                    ),
                ))
            })
            .collect();
        self.keyboard_owed = None;
        tasks.push(
            iced::widget::operation::focus(card_control_id(card, "chat-composer"))
                .map(cosmic::Action::App),
        );
        Some(Task::batch(tasks))
    }

    fn card_container_id(&self, card: CardId) -> widget::Id {
        self.card_chrome.get(&card).map_or_else(
            || workspace_widget::card_widget_id(card),
            |chrome| chrome.container_id.clone(),
        )
    }

    /// Where keyboard focus for `card` lands.
    fn card_keyboard_focus(&self, card: CardId) -> widget::Id {
        if self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay) {
            return card_control_id(card, "inspector.toggle");
        }
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(_)) => self
                .card_chrome
                .get(&card)
                .map(|chrome| chrome.focus_id.clone())
                .unwrap_or_else(|| workspace_widget::card_focus_id(card)),
            // Until its input is drawn the pane's title holds the keyboard
            // in its place: a focus aimed at a control that is not in the
            // tree blurs every widget instead. See `focus_pane_keyboard`.
            // An image viewer over the transcript holds the pane's keyboard
            // in the composer's place, so a press on the image or a return
            // to the pane leaves the arrows browsing. What is typed still
            // reaches the composer (`type_into_chat_input`).
            Some(CardRuntime::Chat(runtime))
                if self.chat_input_drawn(card)
                    && runtime.image_viewer.is_some()
                    && runtime.focus_control() == "chat-composer" =>
            {
                chat_ui::images::viewer_focus(card)
            }
            Some(CardRuntime::Chat(runtime)) if self.chat_input_drawn(card) => {
                card_control_id(card, runtime.focus_control())
            }
            Some(CardRuntime::Repository(runtime)) => match runtime.editor_focus_target(card) {
                Some(_) if runtime.find_is_open() => repository_widget::find_focus_id(card),
                Some(target) => target,
                None => self.card_header_widget_id(card),
            },
            // Arriving at settings means looking for one, so the keyboard
            // lands in its search field.
            Some(CardRuntime::Settings) => crate::settings_widgets::search_id(),
            // An ended card without its final screen answers the keyboard
            // with its primary action, which Enter then presses.
            Some(CardRuntime::Completed) if self.ended_terminal(card).is_some() => {
                card_control_id(card, "ended.relaunch")
            }
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
        let viewport = self.apply_viewport(update, true, DurableChange::Content);
        // Focus did not change, so nothing above follows the active tab to
        // wherever the new order put it. Dragging some other tab leaves the
        // strip where the drag has it.
        if self.workspace.focused() == Some(card) {
            Task::batch([viewport, self.reveal_active_tab()])
        } else {
            viewport
        }
    }

    fn apply_viewport(
        &mut self,
        update: ViewportUpdate,
        synchronize_scroll: bool,
        durable_change: DurableChange,
    ) -> Task<Message> {
        self.apply_viewport_reading(update, synchronize_scroll, durable_change, FocusRead::Now)
    }

    fn apply_viewport_reading(
        &mut self,
        update: ViewportUpdate,
        synchronize_scroll: bool,
        durable_change: DurableChange,
        read: FocusRead,
    ) -> Task<Message> {
        // A pane resized too narrow for its inspector closes it before it is
        // drawn squeezed.
        self.sync_pane_inspectors();
        let focused = self.workspace.focused();
        // Every focus move converges here, including tab cycling and closing a
        // pane. Also revoke ownership when the terminal runtime is replaced.
        self.reconcile_terminal_review_owner();
        self.cancel_stale_pr_creations();
        // Focus is the only part of the viewport the workspace keeps; the
        // camera and what it exposes belong to this run.
        let focus_changed = self.durable.focused != focused;
        if focus_changed {
            Arc::make_mut(&mut self.durable).focused = focused;
        }
        let mut tasks = Vec::new();
        if focus_changed && read == FocusRead::Now {
            tasks.push(self.acknowledge_active_pane());
        }
        if self.chrome_ui.tabs_focus != focused {
            self.chrome_ui.tabs_focus = focused;
            tasks.push(self.reveal_active_tab());
        }
        // Every focus move lands here, including those whose handlers return
        // early and so skip the check at the end of `update`.
        tasks.push(self.flush_settings_on_blur());
        let mut schedule_delayed_sweep = false;
        let mut reconcile_now = false;
        for transition in update.transitions {
            tracing::debug!(target: "cagents::pane_open", stage = "visibility", card = ?transition.id, to = ?transition.to);
            self.set_exposure(transition.id, transition.to.into());
            // Whether an unreachable agent still runs can change while its
            // card is out of sight, so it is asked again whenever it is shown.
            if transition.to == VisibilityClass::Visible
                && let Some(source) = self
                    .unnamed_agent(transition.id)
                    .map(|blocked| blocked.source.clone())
            {
                tasks.push(self.ask_unnamed_agent_running(transition.id, source));
            }
            let browser_card = self
                .card_state(transition.id)
                .is_some_and(|card| matches!(&card.kind, CardKind::Browser(_)));
            match self.cards.get_mut(&transition.id) {
                Some(CardRuntime::Terminal(handle)) => {
                    if transition.to == VisibilityClass::Visible {
                        let _ = handle.reveal();
                    } else {
                        let _ = handle.hide();
                    }
                }
                Some(CardRuntime::Chat(runtime)) => {
                    let _ = runtime.set_visible(transition.to == VisibilityClass::Visible);
                }
                #[cfg(feature = "integrated-browser")]
                Some(CardRuntime::Browser(browser)) => {
                    let _ = browser
                        .handle
                        .set_visible(transition.to == VisibilityClass::Visible);
                }
                _ => {}
            }
            match transition.to {
                VisibilityClass::Offscreen => {
                    schedule_delayed_sweep = true;
                    // A card's grace runs from the sweep that sees it leave
                    // the screen, so one that left without passing through
                    // the overscan is shown to a sweep now.
                    reconcile_now |= transition.from == VisibilityClass::Visible;
                }
                VisibilityClass::Overscan if browser_card => {
                    reconcile_now = true;
                    schedule_delayed_sweep = true;
                }
                VisibilityClass::Visible | VisibilityClass::Overscan => reconcile_now = true,
            }
        }
        self.trim_warm_chats();
        if synchronize_scroll && update.offset_changed {
            tasks.push(self.synchronize_workspace_scroll());
        }
        if let Some(card) = self.workspace.take_widget_focus_request() {
            tasks.push(self.focus_pane_keyboard(card));
        }
        tasks.push(self.restore_repository_editor_focus());
        reconcile_now |= self.take_passing_sweep();
        if reconcile_now {
            tasks.push(cosmic::task::message(Message::LifecycleSweep));
        }
        if schedule_delayed_sweep {
            let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
            tasks.push(self.request_lifecycle_sweep(delay));
        }
        match durable_change {
            DurableChange::Content => self.persist(),
            DurableChange::Navigation => {
                // The durable state stays current; only handing it to the
                // writer waits, and that is the expensive part.
                self.snapshot_workspace_scope();
                tasks.push(self.save_navigation_soon());
            }
            DurableChange::None if focus_changed => {
                self.snapshot_workspace_scope();
                tasks.push(self.save_navigation_soon());
            }
            DurableChange::None => {}
        }
        Task::batch(tasks)
    }

    /// Saves a change of navigation alone, such as focus or the open channel
    /// or worktree, within [`NAVIGATION_SAVE_DELAY`], together with whatever
    /// else changes by then.
    fn save_navigation_soon(&mut self) -> Task<Message> {
        self.defer_save(DeferredSave::Views)
    }

    /// Saves a change no shared view reads within [`NAVIGATION_SAVE_DELAY`],
    /// as [`Self::save_navigation_soon`] does, but without projecting views
    /// unless navigation joins it before then.
    fn save_snapshot_soon(&mut self) -> Task<Message> {
        self.defer_save(DeferredSave::Snapshot)
    }

    fn defer_save(&mut self, save: DeferredSave) -> Task<Message> {
        if let Some(pending) = &mut self.navigation_save {
            // The burst's timer is already armed, and now saves both.
            *pending = (*pending).max(save);
            return Task::none();
        }
        self.navigation_save = Some(save);
        self.navigation_save_generation = self.navigation_save_generation.wrapping_add(1);
        let generation = self.navigation_save_generation;
        cosmic::task::future(async move {
            tokio::time::sleep(NAVIGATION_SAVE_DELAY).await;
            Message::NavigationSaveDue(generation)
        })
    }

    /// What the navigation save timer armed for `generation` still has to
    /// save, if anything: another save may have carried it already, or a
    /// later burst armed its own timer.
    fn navigation_save_due(&self, generation: u64) -> Option<DeferredSave> {
        self.navigation_save
            .filter(|_| generation == self.navigation_save_generation)
    }

    /// Whether the work every update does after its handler, committing
    /// settings on blur and settling queued or pending closes, may change
    /// what a projection reads. A handler's narrower projection describes
    /// only what the handler changed.
    fn update_tail_may_close_or_commit(&self) -> bool {
        self.settings_blur_pending()
            || !self.queued_card_closes.is_empty()
            || self.workspace.pending_close().is_some()
    }

    /// What a lifecycle sweep's update projects: what the sweep changed,
    /// unless the update's tail may still close or commit something.
    fn sweep_projection(&self, sweep: Projection) -> Projection {
        if self.update_tail_may_close_or_commit() {
            Projection::Full
        } else {
            sweep
        }
    }

    /// A navigation save timer with nothing left to save changes nothing a
    /// projection reads, so it skips the sidebar and chrome rebuild.
    fn navigation_save_is_stale(&self, message: &Message) -> bool {
        matches!(message, Message::NavigationSaveDue(generation)
            if self.navigation_save_due(*generation).is_none())
    }

    /// Saves runtime-reported titles within [`REPORTED_TITLE_SAVE_DELAY`],
    /// together with whatever else changes by then.
    fn save_reported_titles_soon(&mut self, card: CardId) -> Task<Message> {
        self.reported_title_save.cards.insert(card);
        if std::mem::replace(&mut self.reported_title_save.scheduled, true) {
            return Task::none();
        }
        cosmic::task::future(async {
            tokio::time::sleep(REPORTED_TITLE_SAVE_DELAY).await;
            Message::ReportedTitleSaveDue
        })
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

    fn lifecycle_sweep_elapsed(&mut self, generation: u64) -> (Task<Message>, Projection) {
        if generation != self.lifecycle_sweep_generation {
            // Superseded by a later arming, whose own delivery is still to come.
            return (Task::none(), Projection::Unchanged);
        }
        // A matching generation carries the deadline it was armed with, because
        // `arm_lifecycle_sweep` writes both together, so neither branch below
        // is reachable through the normal path. They are guards, not flow: a
        // sweep that ran without a deadline has nothing to do, and a timer that
        // woke marginally early must wait out the grace rather than cut it.
        // Neither changes anything a projection reads.
        let Some(deadline) = self.lifecycle_sweep_deadline else {
            return (Task::none(), Projection::Unchanged);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            return (self.arm_lifecycle_sweep(deadline), Projection::Unchanged);
        }
        self.lifecycle_sweep_deadline = None;
        let (sweep, projection) = self.lifecycle_sweep_update();
        // That reconcile made the manager's exposure current, so the next
        // wakeup can be placed exactly when the earliest card still holding
        // its grace exhausts it.
        (
            Task::batch([sweep, self.reschedule_offscreen_lifecycle_sweep()]),
            projection,
        )
    }

    fn lifecycle_sweep(&mut self) -> Task<Message> {
        self.lifecycle_sweep_update().0
    }

    /// Brings up the cards on screen and lets go of those that left it, and
    /// answers how much of the workspace projection that changed: see
    /// [`Self::reconcile_lifecycle`].
    fn lifecycle_sweep_update(&mut self) -> (Task<Message>, Projection) {
        let mut visible = HashSet::new();
        // On screen, but only until the camera has gone by.
        let mut passing = HashSet::new();
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
                if self.workspace.is_passing(id) {
                    passing.insert(id);
                }
            } else if visibility == VisibilityClass::Overscan {
                let browser = match self.cards.get(&id) {
                    Some(CardRuntime::Terminal(_))
                    | Some(CardRuntime::Repository(_))
                    | Some(CardRuntime::PullRequests(_))
                    | Some(CardRuntime::Settings)
                    | Some(CardRuntime::Accounts)
                    | Some(CardRuntime::Automations)
                    | Some(CardRuntime::Work) => false,
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
        // The pane the workspace switcher previews is on screen there,
        // whichever workspace holds it.
        if let Some(card) = self.chrome_ui.workspace_switcher_preview {
            visible.insert(card);
        }
        // An ended card holds no lifecycle slot, so the reconcile below never
        // brings it up; its final screen is asked for once it is on screen.
        let retained = self.attach_retained_outputs(visible.iter().copied().collect::<Vec<_>>());
        self.lifecycle_sweep_destination =
            (!passing.is_empty()).then(|| self.workspace.camera().target());
        let (reconciled, projection) = self.reconcile_lifecycle(&visible, &passing, &prefetch);
        (Task::batch([reconciled, retained]), projection)
    }

    /// Whether the cards the last sweep left cold as passing are owed
    /// another: the camera is at rest, so whichever is still on screen is
    /// there to stay, or it has a new destination, which may be one of them.
    fn take_passing_sweep(&mut self) -> bool {
        let camera = self.workspace.camera();
        let due = self
            .lifecycle_sweep_destination
            .is_some_and(|destination| !camera.is_animating() || camera.target() != destination);
        if due {
            self.lifecycle_sweep_destination = None;
        }
        due
    }

    /// Applies what the lifecycle decides for the cards on screen, beside it
    /// and passing, and answers how much of the workspace projection that
    /// changed. A card's exposure is read as runtimes start, attach or wake,
    /// never by a projection, so a reconcile that only moves exposures, as
    /// switching between panes that are all running does, changes nothing a
    /// projection reads. Bringing a card up or letting one go changes its
    /// runtime and residency, which projections read: the transcript
    /// probes, for one.
    fn reconcile_lifecycle(
        &mut self,
        visible: &HashSet<CardId>,
        passing: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
    ) -> (Task<Message>, Projection) {
        let now = Instant::now();
        let actions = self
            .lifecycle
            .reconcile_in_flight(visible, passing, prefetch, now);
        let projection = if actions
            .iter()
            .all(|action| matches!(action, LifecycleAction::SetExposure { .. }))
        {
            Projection::Unchanged
        } else {
            Projection::Full
        };
        // Exposure and residency are this run's presence, not the workspace's,
        // so a sweep saves nothing of its own.
        let mut tasks = Vec::new();
        let mut schedule_sweep = false;
        for action in actions {
            match action {
                LifecycleAction::SetExposure { card, exposure } => {
                    self.set_exposure(card, exposure);
                }
                LifecycleAction::Hydrate { card, epoch } => {
                    tracing::debug!(target: "cagents::lifecycle", action = "hydrate", ?card);
                    self.set_residency(card, Residency::Hydrating { epoch });
                    let deferred_session = deferred_terminal_session(
                        self.card_state(card),
                        matches!(self.cards.get(&card), Some(CardRuntime::Loading)),
                    );
                    if let Some(session) = deferred_session {
                        tasks.push(self.start_terminal(
                            card,
                            session,
                            visible.contains(&card),
                            false,
                        ));
                        continue;
                    }
                    #[cfg(feature = "integrated-browser")]
                    let mut deferred_browser_started = false;
                    #[cfg(not(feature = "integrated-browser"))]
                    let deferred_browser_started = false;
                    #[cfg(feature = "integrated-browser")]
                    let deferred_browser =
                        self.card_state(card).and_then(|state| match &state.kind {
                            CardKind::Browser(session)
                                if matches!(self.cards.get(&card), Some(CardRuntime::Loading)) =>
                            {
                                Some(session.clone())
                            }
                            _ => None,
                        });
                    #[cfg(feature = "integrated-browser")]
                    if let Some(session) = deferred_browser {
                        if let Some(reason) = self.browser_unavailable_reason() {
                            self.settle_unavailable_browser(card, reason);
                            continue;
                        }
                        let Some(factory) = self.browser_factory.clone() else {
                            tasks.push(cosmic::task::message(Message::InitializeBrowserFactory));
                            continue;
                        };
                        match spawn_browser(
                            &session,
                            visible.contains(&card),
                            effective_surface_scale(
                                self.window_scale_factor,
                                self.settings.ui_scale,
                            ),
                            Some(factory),
                        ) {
                            Ok(runtime) => {
                                self.cards
                                    .insert(card, CardRuntime::Browser(Box::new(runtime)));
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
                                    .restore(visible.contains(&card))
                                    .map_err(|error| error.to_string()),
                            ),
                            #[cfg(feature = "integrated-browser")]
                            Some(CardRuntime::Browser(browser)) => {
                                match browser.handle.restore().and_then(|()| {
                                    browser.handle.set_visible(visible.contains(&card))
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
                            self.set_residency(card, Residency::Hot);
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
                    tracing::debug!(target: "cagents::lifecycle", action = "hibernate", ?card);
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
                            self.set_residency(card, Residency::Hibernated);
                        }
                        Err(message) => {
                            if let Some(residency) =
                                rollback_failed_hibernation(&mut self.lifecycle, card, epoch, now)
                            {
                                self.set_residency(card, residency);
                                schedule_sweep = true;
                            }
                            self.status = Some(TransientNotice::error(message));
                        }
                    }
                }
            }
        }
        if schedule_sweep {
            let delay = Duration::from_millis(self.settings.hibernation_delay_ms);
            tasks.push(self.request_lifecycle_sweep(delay));
        }
        (Task::batch(tasks), projection)
    }

    fn drain_terminal(&mut self, card: CardId, generation: u64) -> Task<Message> {
        self.drain_terminal_update(card, generation).0
    }

    /// Drain controls and lifecycle events on every wake, independently of grid
    /// repaint. Answers how much of the workspace projection the wake changed.
    fn drain_terminal_update(
        &mut self,
        card: CardId,
        _generation: u64,
    ) -> (Task<Message>, Projection) {
        if self.chat_start_pending(card) {
            return (Task::none(), Projection::Unchanged);
        }
        let (proof, worker, collected) = {
            let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
                return (Task::none(), Projection::Unchanged);
            };
            let events = handle.events();
            (
                handle.host_proof(),
                handle.execution_instance(),
                events.try_iter().collect::<Vec<_>>(),
            )
        };
        if let Some((instance, owner)) = worker.zip(proof.clone())
            && let Some(CardKind::Terminal(source)) = self.card_state(card).map(|s| &s.kind)
        {
            self.shared_views.terminal_source_workers.insert(
                card,
                (
                    source.id,
                    crate::workspace_model::views::TerminalRemovalBasis::Worker { owner, instance },
                ),
            );
        }
        let mut durable_changed = false;
        let mut metadata_changed = false;
        // A reported title is durable, but only chrome reads it and its save
        // can wait; see `REPORTED_TITLE_SAVE_DELAY`.
        let mut title_changed = false;
        let mut recovery = None;
        let mut clipboard_task = Task::none();
        if let Some(proof) = proof
            && self.ended_terminal(card).is_none()
        {
            metadata_changed |= self.uncertain_terminal_proofs.remove(&card).is_some();
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
                // An ended terminal runs no agent. A surface attached after
                // the end replays the activity it once reported, which must
                // not bring a finished agent's row back to life.
                TerminalEvent::AgentActivityChanged(activity) => {
                    if self.ended_terminal(card).is_none()
                        && self.sidebar_state.card_activity(card) != activity
                    {
                        metadata_changed = true;
                        self.observe_agent_activity_for_card(card, activity);
                    }
                }
                TerminalEvent::TitleChanged(title) => {
                    title_changed |= self.apply_reported_title(card, title);
                }
                TerminalEvent::WorkingDirectoryChanged(path) => {
                    if self.terminal_working_directories.get(&card) != Some(&path) {
                        self.terminal_working_directories.insert(card, path);
                        metadata_changed = true;
                    }
                }
                TerminalEvent::ClipboardText { request, text } => {
                    if let Some(find) = self.terminal_find.get_mut(&card)
                        && find.pending_copy == Some(request)
                    {
                        find.pending_copy = None;
                        if !text.trim().is_empty() {
                            chat_ui::invalidate_copy_intent();
                            clipboard_task = iced::clipboard::write(text).map(cosmic::Action::App);
                        }
                    }
                }
                TerminalEvent::SearchResults {
                    request,
                    query,
                    matches,
                    truncated,
                    ..
                } => {
                    self.terminal_search_results(card, request, query, matches, truncated);
                }
                TerminalEvent::ConversationTitleChanged(title) => {
                    if let Some(title) = crate::workspace_model::channel_title(&title) {
                        metadata_changed |= self.suggest_channel_title(
                            card,
                            title,
                            crate::model::ChannelTitleSource::Provider,
                        );
                    }
                }
                TerminalEvent::ResumeHintChanged(hint) => {
                    if self.card_state(card).is_some_and(|state| {
                        matches!(&state.kind, CardKind::Terminal(session) if session.resume_hint != hint)
                    })
                        && let Some(state) = self.card_state_mut(card)
                        && let CardKind::Terminal(session) = &mut state.kind
                    {
                        session.resume_hint = hint;
                        durable_changed = true;
                    }
                }
                TerminalEvent::Bell => {
                    self.observe_agent_update_for_card(card);
                    metadata_changed = true;
                }
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
                } => {
                    self.retire_ended_terminal(card, exit_code, signal.as_deref(), intentional);
                    durable_changed = true;
                }
                TerminalEvent::Completed {
                    exit_code,
                    signal,
                    intentional,
                } => {
                    self.retire_ended_terminal(card, exit_code, signal.as_deref(), intentional);
                    self.refresh_completed_terminal_find(card);
                    durable_changed = true;
                }
                TerminalEvent::InputRejected { operation, message } => {
                    // The hosted adapter proved this individual command was
                    // refused before effects. Its live terminal remains owned;
                    // retry is the reader's choice, never an automatic replay.
                    if matches!(self.cards.get(&card), Some(CardRuntime::Terminal(_)))
                        && self.ended_terminal(card).is_none()
                        && !self.terminal_close_pending(card)
                    {
                        self.status = Some(TransientNotice::warning(format!(
                            "Terminal input rejected ({operation}): {message}. Wait briefly, then try again."
                        )));
                    }
                }
                TerminalEvent::InputNotice { message, .. } => {
                    // This typed worker notice may follow partial delivery.
                    // Preserve the live attachment without replaying input or
                    // treating an acknowledged nonfatal condition as failure.
                    if matches!(self.cards.get(&card), Some(CardRuntime::Terminal(_)))
                        && self.ended_terminal(card).is_none()
                        && !self.terminal_close_pending(card)
                    {
                        self.status = Some(TransientNotice::warning(message));
                    }
                }
                TerminalEvent::Error { operation, message } => {
                    metadata_changed = true;
                    if matches!(
                        operation,
                        "retain completed terminal history"
                            | "retain completed terminal screen"
                            | "review completed terminal"
                            | "copy completed terminal selection"
                            | "copy terminal selection"
                            | "search terminal"
                    ) {
                        if let Some(find) = self.terminal_find.get_mut(&card) {
                            find.pending_copy = None;
                        }
                        self.status = Some(TransientNotice::error(message));
                        continue;
                    }
                    // Commands already in flight can lose the race with the
                    // process ending. They cannot turn an end into a failure.
                    if self.ended_terminal(card).is_some() {
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
                        let visible = self.exposure(card) == Exposure::Visible;
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
        if self.ended_terminal(card).is_none() {
            let activity = self.cards.get(&card).and_then(|runtime| match runtime {
                CardRuntime::Terminal(handle) => handle.agent_activity(),
                _ => None,
            });
            if self.sidebar_state.card_activity(card) != activity {
                metadata_changed = true;
                self.observe_agent_activity_for_card(card, activity);
            }
        }
        // Attention follows semantic agent transitions, not ordinary terminal output.
        // A save now carries any reported title with it.
        let title_save = if durable_changed {
            self.persist();
            Task::none()
        } else if title_changed {
            self.save_reported_titles_soon(card)
        } else {
            Task::none()
        };
        if let Some((session, visible, hibernated)) = recovery {
            // A hibernated agent kept attached for its row goes back to
            // waiting for its card: it may be attached again, but launching it
            // again is the user's decision, made by opening the card.
            if let Some(exposure) =
                hibernated.filter(|_| !visible && agent_ui::observes_terminal_agent(&session))
            {
                self.lifecycle
                    .register(card, exposure, Residency::Hibernated, Instant::now());
                self.set_residency(card, Residency::Hibernated);
                self.cards.insert(card, CardRuntime::Loading);
                self.restored_agent_attaches.push_back(card);
                return (
                    Task::batch([title_save, self.attach_next_restored_agent()]),
                    Projection::Full,
                );
            }
            let residency = Residency::Hydrating {
                epoch: unix_millis().max(1) as u64,
            };
            self.set_residency(card, residency.clone());
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
            return (
                Task::batch([
                    title_save,
                    self.start_terminal(card, session, visible, false),
                ]),
                Projection::Full,
            );
        }
        let projection = if metadata_changed || durable_changed {
            Projection::Full
        } else if title_changed {
            Projection::Chrome
        } else {
            Projection::Unchanged
        };
        (
            Task::batch([
                clipboard_task,
                title_save,
                self.refresh_live_terminal_find(card),
            ]),
            projection,
        )
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
                    browser.address.report_location(&url);
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
                    chat_ui::invalidate_copy_intent();
                    tasks.push(iced::clipboard::write(text).map(cosmic::Action::App));
                }
                DelegateRequest::ClipboardClear => {
                    chat_ui::invalidate_copy_intent();
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
            BrowserEvent::Error(_) => {
                self.status = Some(TransientNotice::error(format!(
                    "Browser card {id}: a browser operation failed"
                )));
            }
            BrowserEvent::Crashed { reason, .. } => {
                self.fail_runtime(id, reason.clone());
                durable_changed = true;
                self.status = Some(TransientNotice::error(format!(
                    "Browser card {id}: the browser stopped unexpectedly"
                )));
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

    fn workspace_card_view(
        &self,
        id: CardId,
        focused: bool,
        splitter: bool,
    ) -> Element<'_, Message> {
        self.card_view(id, focused, splitter)
    }

    fn card_view<'a>(
        &'a self,
        id: CardId,
        focused: bool,
        show_resize_splitter: bool,
    ) -> Element<'a, Message> {
        let card = self.card_state(id);
        let attention = self.sidebar_state.has_card_update(id) && !focused;
        let labels = card_kind_labels(card.map(|card| &card.kind));
        // `sync_chrome` keeps an entry for every durable card the strip can
        // show; a miss can only be a card that is not durable, which renders
        // degenerate chrome.
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

        // A pane whose content starts with a toolbar keeps the closed
        // inspector's toggle at the toolbar's end rather than over its
        // controls.
        // A pane with no inspector here, such as an application page, has
        // no toggle either.
        let inspector = card.is_some() && self.pane_has_inspector(id);
        let inspector_closed = inspector && self.pane_inspector_layout(id).is_none();
        let toolbar_toggle = || inspector_closed.then(|| self.pane_inspector_toggle(id, false));
        let toolbar_hosts_toggle = browser_session.is_some()
            || matches!(
                self.cards.get(&id),
                Some(CardRuntime::Repository(runtime)) if runtime.selected_path().is_some()
            );
        #[cfg(feature = "integrated-browser")]
        let browser_toolbar: Option<Element<'a, Message>> = browser_session.map(|session| {
            let browser_loading = browser_runtime.is_some_and(|browser| {
                browser.lifecycle == BrowserLifecycle::Restoring || browser.load_state.is_some()
            });
            let address = browser_runtime
                .map(|browser| browser.address.editable_str(browser.address_focused))
                .or_else(|| chrome.map(|chrome| chrome.caption.as_str()))
                .unwrap_or(SanitizedLocation::INVALID);
            let active = browser_runtime.is_some();
            let can_go_back = active && session.history_index > 0;
            let can_go_forward =
                active && session.history_index.saturating_add(1) < session.history.len();
            let mut address_input = crate::ui_controls::field::text("URL", address)
                .private()
                .id(chrome.map_or_else(
                    || card_control_id(id, "browser.address"),
                    |chrome| chrome.browser_address_id.clone(),
                ))
                .width(Length::Fill);
            if active {
                address_input = address_input
                    .on_focus(Message::BrowserAddressFocus {
                        card: id,
                        focused: true,
                    })
                    .on_unfocus(Message::BrowserAddressFocus {
                        card: id,
                        focused: false,
                    })
                    .on_input(move |value| Message::BrowserAddressChanged { card: id, value })
                    .on_submit(move |_| Message::BrowserNavigate(id));
            }
            let toolbar = widget::container(
                widget::row::with_children(
                    vec![
                        crate::interaction::tooltip_above(
                            widget::button::icon(Icon::ChevronLeft)
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
                        crate::interaction::tooltip_above(
                            widget::button::icon(Icon::ChevronRight)
                                .id(chrome.map_or_else(
                                    || card_control_id(id, "browser.forward"),
                                    |chrome| chrome.browser_forward_id.clone(),
                                ))
                                .name("Forward")
                                .width(ICON_CONTROL_SIZE)
                                .height(ICON_CONTROL_SIZE)
                                .on_press_maybe(
                                    can_go_forward.then_some(Message::BrowserForward(id)),
                                ),
                            "Forward",
                        ),
                        crate::interaction::tooltip_above(
                            widget::button::icon(if browser_loading {
                                Icon::Cancel
                            } else {
                                Icon::Refresh
                            })
                            .id(chrome.map_or_else(
                                || card_control_id(id, "browser.reload"),
                                |chrome| chrome.browser_reload_id.clone(),
                            ))
                            .name(if browser_loading { "Stop" } else { "Reload" })
                            .width(ICON_CONTROL_SIZE)
                            .height(ICON_CONTROL_SIZE)
                            .on_press_maybe(active.then_some(if browser_loading {
                                Message::BrowserStop(id)
                            } else {
                                Message::BrowserReload(id)
                            })),
                            if browser_loading { "Stop" } else { "Reload" },
                        ),
                        address_input.into(),
                        crate::ui_controls::button::secondary("Open in system browser")
                            .id(card_control_id(id, "browser.system"))
                            .on_press(Message::OpenBrowserInSystem(id))
                            .into(),
                        crate::interaction::tooltip_above(
                            widget::button::icon(Icon::Import)
                                .id(card_control_id(id, "browser.import-logins"))
                                .name("Import browser logins")
                                .width(ICON_CONTROL_SIZE)
                                .height(ICON_CONTROL_SIZE)
                                .on_press(Message::BrowserImport(browser_import_ui::Action::Open)),
                            "Import browser logins",
                        ),
                    ]
                    .into_iter()
                    .chain(toolbar_toggle())
                    .collect::<Vec<_>>(),
                )
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .width(Length::Fill),
            )
            .height(Length::Fixed(BROWSER_TOOLBAR_HEIGHT))
            .padding([0.0, space::S])
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
            if browser_runtime.is_some_and(|browser| browser.address_error) {
                rows.push(
                    crate::design::caption("Invalid URL — edit the address and try again.").into(),
                );
            }
            {
                let phase = browser_runtime.map_or(0.0, |browser| browser.progress_phase);
                rows.push(
                    widget::container(crate::ui_controls::responsive(move |size| {
                        let width = size.width * 0.25;
                        widget::row::with_children(vec![
                            iced::widget::Space::new()
                                .width((size.width - width) * phase)
                                .into(),
                            widget::container(iced::widget::Space::new())
                                .width(width)
                                .height(BROWSER_PROGRESS_HEIGHT)
                                .class(cosmic::theme::Container::custom(move |theme| {
                                    cosmic::iced::widget::container::Style {
                                        background: browser_loading.then_some(
                                            iced::Background::Color(
                                                (*theme.cosmic().accent_color()).into(),
                                            ),
                                        ),
                                        ..Default::default()
                                    }
                                }))
                                .into(),
                        ])
                        .into()
                    }))
                    .height(Length::Fixed(BROWSER_PROGRESS_HEIGHT))
                    .width(Length::Fill)
                    .into(),
                );
            }
            widget::column::with_children(rows).into()
        });

        // A card that kept model focus under an open dialog would take the
        // typing aimed at the dialog and write it into a shell, which runs
        // what it is given. An overlay inspector additionally hides the workspace, so
        // that shell would not even be on screen.
        let card_focused =
            focused && self.refresh_ui.focused && !self.pane_content_inputs_blocked(id);
        #[cfg(not(feature = "integrated-browser"))]
        let browser_toolbar: Option<Element<'a, Message>> = browser_session.map(|_| {
            let address = chrome
                .map(|chrome| chrome.caption.as_str())
                .unwrap_or(SanitizedLocation::INVALID);
            let address = crate::ui_controls::field::text("Saved URL", address)
                .private()
                .id(chrome.map_or_else(
                    || card_control_id(id, "browser.address"),
                    |chrome| chrome.browser_address_id.clone(),
                ))
                .width(Length::Fill);
            let toolbar = widget::container(
                widget::row::with_children(
                    vec![
                        address.into(),
                        crate::ui_controls::button::primary("Open in system browser")
                            .id(card_control_id(id, "browser.system"))
                            .on_press(Message::OpenBrowserInSystem(id))
                            .into(),
                    ]
                    .into_iter()
                    .chain(toolbar_toggle())
                    .collect::<Vec<_>>(),
                )
                .align_y(Alignment::Center)
                .spacing(space::XS)
                .width(Length::Fill),
            )
            .height(Length::Fixed(BROWSER_TOOLBAR_HEIGHT))
            .padding([0.0, space::S])
            .class(cosmic::theme::Container::Background);
            widget::column::with_children(vec![toolbar.into(), crate::design::horizontal_rule()])
                .into()
        });

        let retry_label = if cfg!(not(feature = "integrated-browser")) && browser_session.is_some()
        {
            "Open in system browser"
        } else {
            "Retry"
        };
        let content: Element<'a, Message> = if let Some(view) = self.saved_terminal_view(id) {
            view
        } else {
            match self.cards.get(&id) {
            Some(CardRuntime::Chat(_)) => self.chat_view(id).map(move |message| match message {
                Message::ChatUi(chat_ui::Action::OpenUrl(uri)) => {
                    Message::OpenDocumentLink(id, uri)
                }
                message => message,
            }),
            Some(CardRuntime::Terminal(handle)) => {
                let terminal = handle.terminal_id();
                let find = self.terminal_find.get(&id).filter(|find| find.open);
                let terminal = crate::file_drop::target(
                    TerminalWidget::new(handle.clone())
                        .review_boundary(
                            self.terminal_review_input.clone(),
                            self.terminal_review_card.is_some(),
                            self.terminal_review_card == Some(id),
                            self.terminal_review_revision,
                            move |active| Message::TerminalReviewMode {
                                card: id,
                                terminal,
                                active,
                            },
                        )
                        .accessible_name(format!(
                            "Terminal {}, card {id}",
                            card.map_or_else(
                                || "Terminal".to_owned(),
                                |card| card.title.chars().take(80).collect::<String>()
                            )
                        ))
                        .selection_reset(
                            self.terminal_find
                                .get(&id)
                                .map_or(0, |find| find.selection_reset),
                        )
                        .select_all_request(
                            self.terminal_find
                                .get(&id)
                                .map_or(0, |find| find.select_all_request),
                        )
                        .width_settled(self.workspace.width_is_settled(id))
                        .on_context(move |point, selection, link| {
                            Message::TerminalFind(terminal_ui::Action::Context(
                                id, point, selection, link,
                            ))
                        })
                        .on_paste(move |text| {
                            Message::TerminalFind(terminal_ui::Action::Paste(id, text))
                        })
                        .on_copy(move |range| {
                            Message::TerminalFind(terminal_ui::Action::CopySelection(id, range))
                        })
                        .find_matches(find.map(|find| (find.matches.clone(), find.selected)))
                        .working_directory(
                            self.terminal_working_directories
                                .get(&id)
                                .cloned()
                                .or_else(|| {
                                    card.and_then(|card| match &card.kind {
                                        CardKind::Terminal(session) => {
                                            Some(session.launch.working_directory.clone())
                                        }
                                        _ => None,
                                    })
                                })
                                .unwrap_or_else(|| STARTING_DIRECTORY.clone()),
                        )
                        .read_only(
                            self.ended_terminal(id).is_some() || self.terminal_close_pending(id),
                        )
                        .on_read_only_submit(
                            (self.ended_terminal(id).is_some() && !self.terminal_close_pending(id))
                                .then_some(Message::RelaunchFromEndedScreen(id)),
                        )
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
                        .model_focused(card_focused && !find.is_some_and(|find| find.focused))
                        .on_shortcut(move |event| {
                            let keyboard::Event::KeyPressed {
                                key,
                                physical_key,
                                modifiers,
                                ..
                            } = event
                            else {
                                return None;
                            };
                            crate::interaction::font_zoom_chord(&key, physical_key, modifiers).map(
                                |delta| Message::ZoomFont {
                                    card: id,
                                    delta,
                                    editor: false,
                                },
                            )
                        })
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
                    self.ended_terminal(id).is_none()
                        && !self.terminal_close_pending(id)
                        && !self.pane_content_inputs_blocked(id),
                    move |payload| Message::TerminalDropped { card: id, payload },
                );
                let terminal = crate::ui_controls::popover(
                    terminal,
                    self.terminal_menu(id),
                    widget::popover::Position::Point(
                        self.terminal_context
                            .as_ref()
                            .filter(|context| context.card == id)
                            .map_or(iced::Point::ORIGIN, |context| context.point),
                    ),
                    Message::TerminalFind(terminal_ui::Action::DismissContext(id)),
                );
                let mut content = Vec::new();
                if let Some(notice) = self.terminal_save_notice(id, false) {
                    content.push(notice);
                }
                if let Some(strip) = self.terminal_find_view(id) {
                    content.push(strip);
                }
                content.push(terminal);
                widget::column::with_children(content).height(Length::Fill).into()
            }
            #[cfg(feature = "integrated-browser")]
            // The page's native surface draws over the application, so an
            // inspector covering a narrow pane's content hides it.
            Some(CardRuntime::Browser(browser)) => BrowserWidget::new(&browser.handle)
                .obscured(self.pane_content_inputs_blocked(id))
                .on_shortcut(move |event| browser_shortcut(id, &event))
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
                settings_ui::SettingsContext {
                    feedback: self.settings_feedback.as_ref(),
                    invalid: &self.settings_invalid,
                    pressed_key: self.palette.pressed_key_filter(),
                    accounts: self.default_account_choices(),
                    updates: self.update_summary(),
                    testing_notification: self.notification_ui.testing,
                },
            ),
            Some(CardRuntime::Accounts) => self.accounts_pane(),
            Some(CardRuntime::Automations) => self.automation_ui.view().map(Message::AutomationUi),
            Some(CardRuntime::Work) => self.work_ui.view().map(Message::WorkUi),
            Some(CardRuntime::Completed) => {
                self.ended_terminal_placeholder(id, self.ended_terminal(id))
            }
            // A fenced pane never attaches, so it must not look like it is
            // still starting, nor offer a retry that would use its stale
            // session, including after an earlier attachment failed.
            Some(CardRuntime::Loading | CardRuntime::Failed(_)) | None
                if self.shared_views.quarantined.contains(&id) =>
            {
                widget::container(
                    widget::container(
                        widget::column::with_children(vec![
                            widget::row::with_children(vec![
                                Icon::Warning
                                    .handle()
                                    .icon()
                                    .size(design::STATUS_ICON_SIZE)
                                    .into(),
                                design::title(labels.unavailable).into(),
                            ])
                            .align_y(Alignment::Center)
                            .spacing(space::S)
                            .into(),
                            widget::selectable_text(shared_views::FENCED_PANE_EXPLANATION)
                                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                                .width(Length::Fill)
                                .into(),
                            // Closing is the way out: the pane's saved
                            // session can't be confirmed, so nothing retries it.
                            crate::ui_controls::button::primary("Close")
                                .id(card_control_id(id, "failed.close"))
                                .name(format!("Close {}", labels.type_label))
                                .on_press(Message::CloseCard(id))
                                .into(),
                        ])
                        .spacing(space::L),
                    )
                    .class(cosmic::theme::Container::Secondary)
                    .max_width(520)
                    .padding(space::XL),
                )
                .center(Length::Fill)
                .into()
            }
            Some(CardRuntime::Failed(_)) if self.unnamed_agent(id).is_some() => self
                .unnamed_agent_view(id)
                .expect("an unreachable agent's card has its own state"),
            Some(CardRuntime::Failed(message)) => widget::container(
                widget::container(
                    widget::column::with_children(vec![
                        widget::row::with_children(vec![
                            Icon::Warning
                                .handle()
                                .icon()
                                .size(design::STATUS_ICON_SIZE)
                                .into(),
                            design::title(if self.reopen_failed(id) {
                                "Couldn't reopen this conversation"
                            } else {
                                labels.unavailable
                            })
                            .into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(space::S)
                        .into(),
                        widget::selectable_text(if browser_session.is_some() {
                            if let Some(reason) = self.browser_unavailable_reason() {
                                std::borrow::Cow::Owned(if reason.is_recoverable() {
                                    "The integrated browser didn't start in time. Open this page in your system browser, or try the integrated browser again.".to_owned()
                                } else {
                                    format!("{reason}. Open this page in your system browser instead.")
                                })
                            } else if cfg!(feature = "integrated-browser") {
                                std::borrow::Cow::Borrowed("The browser could not load. Try again or open it in your system browser.")
                            } else {
                                std::borrow::Cow::Borrowed("This build does not include the integrated browser. Open the saved page in your system browser.")
                            }
                        } else if let Some(blocked) = self.blocked_failure(id) {
                            std::borrow::Cow::Borrowed(blocked.reopen.headline())
                        } else { std::borrow::Cow::Borrowed(message.message()) })
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                            .width(Length::Fill)
                            .into(),
                        if self.blocked_failure_source(id).is_some() {
                            self.blocked_failure_diagnostics(id)
                        } else if matches!(message, terminal_failure::Failure::Terminal(_)) {
                            widget::column::with_children(vec![
                                self.terminal_diagnostic_summary(id),
                                self.terminal_diagnostic_controls(id),
                            ])
                            .spacing(space::M)
                            .into()
                        } else {
                            iced::widget::Space::new().into()
                        },
                        self.account_recovery_controls(id)
                            .unwrap_or_else(|| iced::widget::Space::new().into()),
                        self.blocked_failure_guidance(id)
                            .or_else(|| self.chat_capacity_guidance(id).map(str::to_owned))
                            .map(|guidance| design::caption(guidance).into())
                            .unwrap_or_else(|| iced::widget::Space::new().into()),
                        widget::row::with_children({
                            let mut controls = if self.chat_retry_blocked(id) {
                                // A permanent refusal keeps no Retry; the
                                // recovery guidance above names the next step.
                                Vec::new()
                            } else if let Some(reason) = browser_session
                                .and(self.browser_unavailable_reason())
                            {
                                // The page can still be read: the system
                                // browser is the way to it, and a check that
                                // may succeed again can be retried.
                                let mut controls = vec![
                                    crate::ui_controls::button::primary("Open in system browser")
                                        .id(card_control_id(id, "retry"))
                                        .name("Open this page in your system browser")
                                        .on_press(Message::OpenBrowserInSystem(id))
                                        .into(),
                                ];
                                if reason.is_recoverable() {
                                    controls.push(
                                        crate::ui_controls::button::secondary("Retry")
                                            .id(card_control_id(id, "browser.recheck"))
                                            .name("Retry the integrated browser")
                                            .on_press_maybe(
                                                (!self.terminal_close_in_progress(id))
                                                    .then_some(Message::RetryCard(id)),
                                            )
                                            .into(),
                                    );
                                }
                                controls
                            } else { vec![
                                (if self.terminal_retry_is_blocked(id) {
                                    crate::ui_controls::button::secondary(retry_label)
                                } else {
                                    crate::ui_controls::button::primary(retry_label)
                                })
                                    .id(card_control_id(id, "retry"))
                                    .name(format!("{retry_label} {}", labels.type_label))
                                    // Retry does nothing once Close owns the pane.
                                    .on_press_maybe(
                                        (!self.terminal_close_in_progress(id)).then_some(
                                            if cfg!(not(feature = "integrated-browser"))
                                                && browser_session.is_some()
                                            {
                                                Message::OpenBrowserInSystem(id)
                                            } else {
                                                Message::RetryCard(id)
                                            },
                                        ),
                                    )
                                    .into(),
                            ] };
                            // A conversation no retry can reach still has one
                            // thing left to do with it, and the reader has to
                            // be the one who asks for it.
                            // Without Retry, the first remaining action is
                            // the card's primary one, as on a stopped pane.
                            let closing = self.terminal_close_in_progress(id);
                            // While a restart is the way back, no button is
                            // the next step: each one gives that way up.
                            let waits = self
                                .blocked_failure(id)
                                .is_some_and(|blocked| blocked.reopen.waits());
                            let button = |controls: &Vec<Element<'_, Message>>, label| {
                                if controls.is_empty() && !waits {
                                    crate::ui_controls::button::primary(label)
                                } else {
                                    crate::ui_controls::button::secondary(label)
                                }
                            };
                            if self
                                .blocked_failure(id)
                                .is_some_and(|blocked| blocked.reopen.reopens())
                            {
                                controls.push(
                                    button(&controls, chat_ui::blocked_recovery::REOPEN_LABEL)
                                        .id(card_control_id(id, "chat.reopen"))
                                        .name(chat_ui::blocked_recovery::REOPEN_LABEL)
                                        // Close owns the card until it settles.
                                        .on_press_maybe((!closing).then_some(Message::ChatUi(
                                            chat_ui::Action::ReopenStopped(id),
                                        )))
                                        .into(),
                                );
                            }
                            if self.chat_offers_fresh_conversation(id) {
                                controls.push(
                                    button(&controls, "Start a new conversation")
                                        .id(card_control_id(id, "chat.fresh"))
                                        .name("Start a new conversation")
                                        // Close owns the card until it settles.
                                        .on_press_maybe((!closing).then_some(Message::ChatUi(
                                            chat_ui::Action::StartFresh(id),
                                        )))
                                        .into(),
                                );
                            }
                            controls.push(
                                {
                                    button(&controls, "Close")
                                        .instead("Checking…", closing)
                                        .id(card_control_id(id, "failed.close"))
                                        .name(format!("Close {}", labels.type_label))
                                        .on_press_maybe((!closing).then_some(Message::CloseCard(id)))
                                        .into()
                                },
                            );
                            controls
                        })
                        .spacing(space::S)
                        .into(),
                    ])
                    .spacing(space::L),
                )
                .class(cosmic::theme::Container::Secondary)
                .max_width(520)
                .padding(space::XL),
            )
            .center(Length::Fill)
            .into(),
            Some(CardRuntime::Loading) if self.terminal_retry_pending(id) => self
                .terminal_retry_view(id)
                .expect("retry presentation exists"),
            Some(CardRuntime::Loading) if self.startup_terminal_gave_up(id) => {
                widget::container(
                    widget::container(
                        widget::column::with_children(vec![
                            design::title(self.startup_gave_up_title(id)).into(),
                            design::caption(self.startup_gave_up_caption(id))
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                            .into(),
                        ])
                        .spacing(space::M),
                    )
                    .class(cosmic::theme::Container::Card)
                    .max_width(STATUS_SURFACE_MAX_WIDTH)
                    .padding(space::XL),
                )
                .center(Length::Fill)
                .padding(space::XL)
                .into()
            }
            Some(CardRuntime::Loading) | None => widget::container(
                widget::container(
                    widget::column::with_children(vec![
                        crate::ui_controls::activity_spinner(24.0, design::accent_color),
                        design::title(labels.starting).into(),
                        design::caption("Preparing this workspace pane…")
                            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                            .into(),
                    ])
                    .align_x(Alignment::Center)
                    .spacing(space::M),
                )
                .class(cosmic::theme::Container::Card)
                .max_width(STATUS_SURFACE_MAX_WIDTH)
                .padding(space::XL),
            )
            .center(Length::Fill)
            .padding(space::XL)
            .into(),
        }
        };

        let mut rows: Vec<Element<'a, Message>> = Vec::with_capacity(3);
        // The PTY may report its final exit before the close receipt comes
        // back. Keep the closing card visually stable until the owner either
        // confirms removal or reports that it must stay open.
        let closing_terminal =
            self.closed_terminal_cards.contains(&id) || self.terminal_close_pending(id);
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get(&id) {
            let html = runtime
                .selected_path()
                .and_then(Path::extension)
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
                });
            let preview = html.then(|| {
                crate::ui_controls::icon_button(Icon::Browser, "Preview HTML")
                    .on_press_maybe(
                        (!self.modal_is_open()).then_some(Message::PreviewLocalFile(id)),
                    )
                    .into()
            });
            let trailing: Vec<Element<'a, Message>> =
                preview.into_iter().chain(toolbar_toggle()).collect();
            let trailing = (!trailing.is_empty()).then(|| {
                widget::row::with_children(trailing)
                    .spacing(space::XXS)
                    .align_y(Alignment::Center)
                    .into()
            });
            if let Some(toolbar) = repository_widget::selected_file_toolbar_mapped_with(
                id,
                runtime,
                trailing,
                Message::Repository,
            ) {
                rows.push(toolbar);
                rows.push(design::horizontal_rule());
            }
        }
        if let Some(toolbar) = browser_toolbar {
            rows.push(toolbar);
        }
        rows.push(content);
        if let Some(session) = self.ended_terminal(id).filter(|_| !closing_terminal) {
            // Under the final screen, where the reader's eye already is when
            // the output stops: the last lines it printed stay in view.
            rows.push(self.ended_terminal_footer(id, session));
        }
        let surface: Element<'a, Message> =
            widget::container(widget::column::with_children(rows).height(Length::Fill))
                .class(cosmic::theme::Container::custom(move |theme| {
                    iced::widget::container::Style {
                        background: Some(iced::Background::Color(theme.cosmic().bg_color().into())),
                        ..Default::default()
                    }
                }))
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        let surface = if let Some(session) = browser_session {
            browser_host::surface(
                id,
                &SanitizedLocation::from(&session.current_url),
                crate::capability::BrowserCapability::current(),
                self.browser_host_load(id),
                card_focused,
                self.browser_unavailable_reason(),
                surface,
            )
        } else {
            surface
        };
        // Beside the browser's surface rather than inside it, so a native
        // page never draws over the inspector.
        let surface = if inspector {
            self.with_pane_inspector(id, surface, toolbar_hosts_toggle)
        } else {
            surface
        };
        // The attention border goes around the whole pane, its inspector
        // included, and over both, so the inspector's surface cannot hide
        // it. It is always mounted, so attention coming and going never
        // remounts the pane.
        let surface = crate::interaction::layer(
            surface,
            widget::container(iced::widget::Space::new())
                .class(cosmic::theme::Container::custom(move |theme| {
                    iced::widget::container::Style {
                        border: iced::Border {
                            color: design::warning_color(theme),
                            width: if attention { design::BORDER_WIDTH } else { 0.0 },
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                }))
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
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
                        || {
                            card.map_or("Resize terminal pane", |state| {
                                self.card_display_title(state)
                            })
                        },
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
                .default_width(
                    self.settings.default_column_fraction * self.workspace.viewport_width(),
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
            CardKind::Settings | CardKind::Accounts | CardKind::Automations | CardKind::Work => {
                None
            }
        };
        if let Some(action) = action {
            if let SidebarAction::OpenWorktree {
                project_id,
                worktree_id,
                ..
            } = &action
                && let Some(project) = self.sidebar_model.project(*project_id)
            {
                self.sidebar_state
                    .reveal_worktree(project, *worktree_id, false);
            }
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

    /// The focused card, reduced to what binding the source-control panel
    /// needs. A scoped workspace binds its worktree whatever is focused. A
    /// card that names no worktree directory - a browser, an application
    /// page - is [`SidebarFocus::None`], which leaves the panel where it is
    /// rather than tearing it down.
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
            // The repository whose pull requests it lists, so its inspector
            // shows that repository's work rather than another's.
            CardKind::PullRequests(view) => SidebarFocus::Repository {
                worktree: view.repository.clone(),
            },
            CardKind::Browser(_)
            | CardKind::Settings
            | CardKind::Accounts
            | CardKind::Automations
            | CardKind::Work => SidebarFocus::None,
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
        #[cfg(test)]
        terminal_output_tests::record_source_control_rebind();
        // Work chosen in the inspector binds the panel without moving the
        // list's selection, which stays with the focused card.
        self.sidebar_state
            .pin_panel_worktree(self.inspector_panel_pin());
        // The Attention list draws the focused pane's row as current.
        self.sidebar_state.follow_card(self.workspace.focused());
        let focus = self.sidebar_focus();
        let changed = self
            .sidebar_state
            .sync_active_worktree(&self.sidebar_model, &focus);
        if crate::device::is_remote() {
            if changed
                || self.sidebar_model.source_control.is_some()
                || self.refresh_ui.source_cancel.is_some()
            {
                self.cancel_source_read();
                self.cancel_branch_changes();
                self.history_ui.cancel();
                self.history_ui.next_page_request();
            }
            self.sidebar_model.source_control = None;
            return Task::none();
        }
        if !changed {
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
        let mut source = SourceControlModel::loading(key.clone());
        source.set_mutation(
            self.source_control_mutations
                .get(&key.canonical_path)
                .map(|mutation| mutation.kind),
        );
        self.sidebar_model.source_control = Some(source);
        self.source_control_snapshot_task(key)
    }

    /// Asks for the first page of history once, and only once the `History`
    /// tab is showing: a page is a `git log` walk, and a view nobody opened is
    /// not worth one.
    fn load_pending_history(&mut self) -> Task<Message> {
        if crate::device::is_remote() {
            return Task::none();
        }
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

    /// The open repository browsers for this worktree in the active channel,
    /// in pane order. A browser shows the whole worktree rather than one
    /// fixed file.
    fn repository_browsers<'a>(&'a self, worktree: &'a Path) -> impl Iterator<Item = CardId> + 'a {
        self.active_repository_panes()
            .filter_map(move |(card, view)| {
                (view.selected_file.is_none() && view.worktree == worktree).then_some(card)
            })
    }

    /// The open repository panes of the active channel, in pane order.
    fn active_repository_panes(&self) -> impl Iterator<Item = (CardId, &RepositoryView)> {
        let channel = self.durable.active_channel;
        self.durable.cards.iter().filter_map(move |card| {
            let CardKind::Repository(view) = &card.kind else {
                return None;
            };
            (card.existence.is_open()
                && self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card.id))
                    .map(|channel| channel.id)
                    == channel)
                .then_some((card.id, view))
        })
    }

    /// The repository browser that sidebar navigation into this worktree
    /// should drive: the focused browser if it can navigate, otherwise the
    /// first that can.
    ///
    /// A browser holding unsaved edits, a save, editor recovery or a
    /// durability warning refuses navigation, so routing a click to it would
    /// only focus it and drop the request. `None` means the caller opens a
    /// new browser, as a file link does when every file pane is busy.
    fn repository_panel_destination(&self, worktree: &Path) -> Option<CardId> {
        self.repository_browser_where(worktree, |card| {
            !matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        })
    }

    /// The focused browser for this worktree if it satisfies `accept`,
    /// otherwise the first that does.
    fn repository_browser_where(
        &self,
        worktree: &Path,
        accept: impl Fn(CardId) -> bool,
    ) -> Option<CardId> {
        self.workspace
            .focused()
            .filter(|focused| {
                accept(*focused)
                    && self
                        .repository_browsers(worktree)
                        .any(|card| card == *focused)
            })
            .or_else(|| {
                self.repository_browsers(worktree)
                    .find(|card| accept(*card))
            })
    }

    /// The repository pane whose unsaved work on `worktree`'s `path` keeps
    /// it from navigating, preferring the focused one. A file pane counts as
    /// well as a browser: either holds edits a second copy would lack.
    fn repository_editing(&self, worktree: &Path, path: &Path) -> Option<CardId> {
        let target = worktree.join(path);
        let editing = |card: CardId| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime))
            if runtime.blocks_navigation()
                && runtime
                    .selected_path()
                    .is_some_and(|selected| {
                        same_file(&runtime.descriptor.worktree.join(selected), &target)
                    }))
        };
        let mut panes = self.active_repository_panes().map(|(card, _)| card);
        self.workspace
            .focused()
            .filter(|focused| {
                editing(*focused)
                    && self
                        .active_repository_panes()
                        .any(|(card, _)| card == *focused)
            })
            .or_else(|| panes.find(|card| editing(*card)))
    }

    fn focus_pane(&mut self, card: CardId) -> Task<Message> {
        self.workspace
            .focus(card)
            .map(|update| self.apply_viewport(update, true, DurableChange::None))
            .unwrap_or_else(|_| Task::none())
    }

    /// Routes one panel row to the worktree's repository card.
    ///
    /// A browser already editing that file is focused as it is. Otherwise the
    /// row goes to [`Self::repository_panel_destination`], and a new browser
    /// opens if no existing one can navigate. Either way the navigation
    /// is parked in [`Self::pending_panel_open`] and performed when that card's
    /// file list lands, because the card resolves a path against the list it
    /// holds and the sidebar's rows come from a snapshot the list may predate.
    fn route_panel_open(&mut self, key: &ActiveWorktreeKey, path: &RepoPath) -> Task<Message> {
        if self.sidebar_state.active_worktree() != Some(key) {
            return Task::none();
        }
        let ignored = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| source.key == *key)
            .is_some_and(|source| {
                source
                    .files
                    .iter()
                    .any(|row| row.path == *path && row.ignored)
            });
        // A browser that refuses navigation because of this very file's
        // unsaved edits already shows what was asked for. Focus it rather than
        // opening a second copy that would lack those edits.
        if let Some(card) = self.repository_editing(&key.canonical_path, path.as_path()) {
            self.pending_panel_open = None;
            return self.focus_pane(card);
        }
        let path = path.as_path().to_path_buf();
        match self.repository_panel_destination(&key.canonical_path) {
            Some(card) => {
                self.pending_panel_open = Some(PendingPanelOpen {
                    card,
                    path,
                    ignored,
                });
                let focus = self.focus_pane(card);
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
                let (card, task) = self.add_repository_card(
                    key.project_id,
                    key.canonical_path.clone(),
                    key.canonical_path.clone(),
                );
                self.pending_panel_open = Some(PendingPanelOpen {
                    card,
                    path,
                    ignored,
                });
                task
            }
        }
    }

    fn reconcile_completion_intent(&mut self, card: CardId) {
        let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card) else {
            return;
        };
        let intent = runtime.editor_completion_intent();
        let pending = self.lsp_contexts.get_mut(&card).and_then(|state| {
            if state.completion_intent == intent {
                return None;
            }
            state.completion_intent = intent;
            state.context.completions.clear();
            state.completion_request.take()
        });
        if let Some((request, _)) = pending {
            self.lsp.cancel(request);
        }
    }

    /// Hands the card's buffer lifecycle to the language-server manager.
    ///
    /// The widget records what happened to the buffer; nothing in it can talk
    /// to a process. Draining here is what turns those records into protocol
    /// traffic, and it must happen after *every* repository action, because an
    /// `Open` that is never drained leaves a document the server has not been
    /// told about while the application shows results for it.
    fn drain_lsp_intents(&mut self, card: CardId) -> Task<Message> {
        self.reconcile_completion_intent(card);
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
                    let previous_version = self.lsp.document_version(doc);
                    match text {
                        repository_widget::LspChangeText::Text(text) => {
                            self.lsp.change(doc, version, &edits, &text)
                        }
                        repository_widget::LspChangeText::Snapshot(source) => {
                            self.lsp.change_snapshot(doc, version, &edits, source)
                        }
                    }
                    let synchronized_version = self.lsp.document_version(doc);
                    if let Some(state) = self.lsp_contexts.get_mut(&card) {
                        state.invalidate_answers();
                        if !edits.is_empty()
                            && diagnostics_are_current(state.diagnostics_version, previous_version)
                        {
                            crate::lsp::shift_diagnostics(&mut state.context.diagnostics, &edits);
                            state.diagnostics_version = synchronized_version;
                        } else {
                            // A full-text transition carries no safe coordinate mapping.
                            // Clear native marks too, since F8 navigates those directly.
                            state.context.diagnostics.clear();
                            state.diagnostics_version = None;
                        }
                    }
                    if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                        if let Some(state) = self.lsp_contexts.get(&card) {
                            runtime.set_editor_diagnostics(state.context.diagnostics.clone());
                        }
                        runtime.dismiss_editor_results();
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
        text: &repository_widget::LspChangeText,
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
            return self.flush_pending_lsp_jump_with_source(card, path, Some(text));
        };
        if self.lsp_contexts.iter().any(|(other, state)| {
            *other != card && state.document.is_some() && state.path.as_deref() == Some(path)
        }) {
            let state = self.lsp_contexts.entry(card).or_default();
            state.blocked_by_other_buffer = true;
            state.context.status = Some(LspStatus::Unavailable {
                reason: "This file is already open for language tools in another pane. Close that file there, then restart the language server here.".into(),
            });
            return self.flush_pending_lsp_jump_with_source(card, path, Some(text));
        }
        let doc = LspDocumentId(self.next_lsp_document);
        self.next_lsp_document += 1;
        let status = match text {
            repository_widget::LspChangeText::Text(text) => {
                self.lsp.open(doc, path, worktree, language, version, text)
            }
            repository_widget::LspChangeText::Snapshot(source) => {
                self.lsp
                    .open_snapshot(doc, path, worktree, language, version, source.clone())
            }
        };
        if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
            runtime.set_lsp_document(Some(doc));
        }
        self.lsp_documents.insert(doc, card);
        let state = self.lsp_contexts.entry(card).or_default();
        state.document = Some(doc);
        state.context.status = Some(status);
        self.flush_pending_lsp_jump(card, path)
    }

    /// Apply only exact original IDs the manager actually closed. A retained
    /// buffer or replacement attachment at the same path is never recaptured.
    fn clear_removed_lsp_documents(&mut self, closed: &[LspDocumentId]) {
        for &doc in closed {
            let Some(card) = self.lsp_documents.remove(&doc) else {
                continue;
            };
            if let Some(state) = self.lsp_contexts.get_mut(&card)
                && state.document == Some(doc)
            {
                *state = LspInspectorState::default();
                self.lsp_pending_jumps.remove(&card);
            }
            if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                && runtime.lsp_document() == Some(doc)
            {
                runtime.set_lsp_document(None);
            }
        }
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
            state.blocked_by_other_buffer = false;
        }
    }

    /// Applies a parked jump now that `path` is the card's open buffer.
    fn flush_pending_lsp_jump(&mut self, card: CardId, path: &Path) -> Task<Message> {
        self.flush_pending_lsp_jump_with_source(card, path, None)
    }

    fn flush_pending_lsp_jump_with_source(
        &mut self,
        card: CardId,
        path: &Path,
        source: Option<&repository_widget::LspChangeText>,
    ) -> Task<Message> {
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
            // loaded buffer can place it even when another independent pane
            // owns this path's language tools. Never borrow that pane's text.
            LspCoordinates::Utf16 => {
                let converted = if let Some(source) = source {
                    Self::loaded_lsp_scalar_position(source, jump.at)
                } else {
                    let Some(doc) = self.lsp_document(card) else {
                        return Task::none();
                    };
                    self.lsp
                        .scalar_position(doc, jump.at)
                        .map_err(|error| error.to_string())
                };
                match converted {
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

    /// Convert only the destination buffer's text, without acquiring an LSP
    /// attachment or serializing a normal native snapshot's other lines.
    fn loaded_lsp_scalar_position(
        source: &repository_widget::LspChangeText,
        at: LspPosition,
    ) -> Result<LspPosition, String> {
        let outside = || {
            format!(
                "line {} UTF-16 character {} is outside this buffer",
                at.line, at.character
            )
        };
        let convert = |line: &str| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut units = 0;
            let mut scalars = 0;
            for character in line.chars() {
                if units == at.character {
                    return Ok(LspPosition::new(at.line, scalars));
                }
                let width = character.len_utf16() as u32;
                if at.character > units && at.character < units + width {
                    return Err(format!(
                        "UTF-16 offset {} on line {} splits a surrogate pair",
                        at.character, at.line
                    ));
                }
                units += width;
                scalars += 1;
            }
            if units == at.character {
                Ok(LspPosition::new(at.line, scalars))
            } else {
                Err(outside())
            }
        };
        let from_text = |text: &str| {
            text.split('\n')
                .nth(at.line as usize)
                .ok_or_else(outside)
                .and_then(convert)
        };
        match source {
            repository_widget::LspChangeText::Text(text) => from_text(text),
            repository_widget::LspChangeText::Snapshot(source)
                if source.protocol_compatible_lines() =>
            {
                source
                    .line(at.line as usize)
                    .ok_or_else(outside)
                    .and_then(|line| convert(&line.text))
            }
            repository_widget::LspChangeText::Snapshot(source) => from_text(&source.text()),
        }
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
        self.reconcile_completion_intent(card);
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
        if !self.lsp.event_is_current(&event) {
            return;
        }
        if let LspEvent::RootDiscovered { doc, ticket, root } = event {
            if let Some(status) = self.lsp.finish_discovery(doc, ticket, root)
                && let Some(card) = self.lsp_documents.get(&doc)
                && let Some(state) = self.lsp_contexts.get_mut(card)
            {
                state.context.status = Some(status);
            }
            return;
        }
        if matches!(event, LspEvent::Resources { .. } | LspEvent::Status { .. }) {
            for state in self.lsp_contexts.values_mut() {
                if matches!(event, LspEvent::Resources { .. })
                    && let Some(doc) = state.document
                {
                    // A shared session can retire after its last routing document
                    // closed. Refresh every attachment before resolving that identity.
                    let status = self.lsp.status(doc);
                    // Admission can retain a document without a backend (for
                    // example at the session cap). Resource telemetry cannot
                    // replace that actionable refusal with "not configured".
                    let retained_refusal = matches!(status, LspStatus::Disabled)
                        && matches!(
                            state.context.status.as_ref(),
                            Some(LspStatus::Unavailable { .. })
                        )
                        && self.lsp.document_version(doc).is_some();
                    if !retained_refusal {
                        state.context.status = Some(status);
                    }
                }
            }
        }
        let doc = match &event {
            LspEvent::Resources { doc }
            | LspEvent::RootDiscovered { doc, .. }
            | LspEvent::Diagnostics { doc, .. }
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
        self.reconcile_completion_intent(card);
        let Some(state) = self.lsp_contexts.get_mut(&card) else {
            return;
        };
        match event {
            LspEvent::RootDiscovered { .. } => unreachable!("handled above"),
            LspEvent::Resources { .. } => {}
            LspEvent::Diagnostics { version, items, .. } => {
                // A delayed publication must not erase the shifted standing
                // set for a newer buffer revision.
                if self.lsp.document_version(doc) != Some(version) {
                    return;
                }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                    runtime.set_editor_diagnostics(items.clone());
                }
                state.context.diagnostics = items;
                state.diagnostics_version = Some(version);
            }
            LspEvent::Completion { request, items, .. } => {
                if let Some((expected, at)) = state.completion_request
                    && request == expected
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                    && runtime.cursor_position() == Some(at)
                {
                    state.completion_request = None;
                    runtime.set_editor_completions(items);
                    state.context.completions = runtime.editor_completion_targets(card);
                }
            }
            LspEvent::Hover {
                request, markdown, ..
            } => {
                if let Some((expected, at)) = state.editor_hover_request
                    && request == expected
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                {
                    runtime.set_editor_hover(markdown.as_deref(), Some(at));
                }
            }
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
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                    runtime.dismiss_editor_results();
                }
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
            InspectorAction::RestartLanguageServer(card) => {
                let Some(doc) = self.lsp_document(card) else {
                    if self
                        .lsp_contexts
                        .get(&card)
                        .is_some_and(|state| state.blocked_by_other_buffer)
                        && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                    {
                        runtime.resync_lsp();
                        return self.drain_lsp_intents(card);
                    }
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
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                    runtime.dismiss_editor_results();
                    runtime.set_editor_diagnostics(Vec::new());
                }
                Task::none()
            }
            InspectorAction::DismissLspResults(card) => {
                if let Some(state) = self.lsp_contexts.get_mut(&card) {
                    state.invalidate_answers();
                    state.failure = None;
                    state.refresh_notice();
                }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                    runtime.dismiss_editor_results();
                }
                Task::none()
            }
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

    fn apply_lsp_completion(
        &mut self,
        card: CardId,
        index: usize,
        target: &repository_widget::CompletionTarget,
    ) -> Task<Message> {
        let Some(state) = self.lsp_contexts.get(&card) else {
            return Task::none();
        };
        let Some(current) = state.context.completions.get(index) else {
            return Task::none();
        };
        // A queued action names the exact publication, document revision and
        // edits the user chose. A label can be reused with different edits.
        if current != target {
            self.status = Some(TransientNotice::warning(
                "the completion list changed; nothing was applied",
            ));
            return Task::none();
        }
        let applied = match self.cards.get_mut(&card) {
            Some(CardRuntime::Repository(runtime)) => {
                let applied = runtime.apply_completion_target(card, target);
                if applied {
                    runtime.dismiss_editor_results();
                }
                applied
            }
            _ => false,
        };
        if !applied {
            self.status = Some(TransientNotice::warning(format!(
                "`{}` could not be applied to this buffer",
                target.item().label
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
        self.reconcile_completion_intent(card);
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
            LspRequestKind::Completion => {
                let request = self.lsp.request_completion(doc, at);
                self.lsp_contexts
                    .entry(card)
                    .or_default()
                    .completion_request = Some((request, at));
                request
            }
            LspRequestKind::References => self.lsp.request_references(doc, at),
        };
        Task::none()
    }

    fn update_editor_lsp(
        &mut self,
        card: CardId,
        action: repository_widget::EditorLspAction,
    ) -> Task<Message> {
        use repository_widget::EditorLspAction as Action;
        match action {
            Action::Complete => self.request_lsp(card, LspRequestKind::Completion),
            Action::CompleteAt { doc, version, at }
                if self.lsp_document(card) == Some(doc)
                    && self.lsp.document_version(doc) == Some(version)
                    && matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.cursor_position() == Some(at)) =>
            {
                self.request_lsp(card, LspRequestKind::Completion)
            }
            Action::Hover(at) => {
                if let Some(state) = self.lsp_contexts.get_mut(&card)
                    && let Some((request, _)) = state.editor_hover_request.take()
                {
                    self.lsp.cancel(request);
                }
                if let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card) {
                    runtime.set_editor_hover(None, at);
                }
                if let Some(at) = at
                    && let Some(doc) = self.lsp_document(card)
                {
                    let request = self.lsp.request_hover(doc, at);
                    self.lsp_contexts
                        .entry(card)
                        .or_default()
                        .editor_hover_request = Some((request, at));
                }
                Task::none()
            }
            Action::Definition(at) => {
                let Some(doc) = self.lsp_document(card) else {
                    return Task::none();
                };
                let at = at.or_else(|| match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) => runtime.cursor_position(),
                    _ => None,
                });
                if let Some(at) = at {
                    let request = self.lsp.request_definition(doc, at);
                    self.lsp_contexts.entry(card).or_default().editor_navigation = Some(request);
                }
                Task::none()
            }
            Action::Diagnostics => {
                self.reveal_context_section(card, context_sections::Section::Problems)
            }
            Action::References => {
                let reveal =
                    self.reveal_context_section(card, context_sections::Section::Locations);
                Task::batch([reveal, self.request_lsp(card, LspRequestKind::References)])
            }
            Action::Choose { index, target } => self.apply_lsp_completion(card, index, &target),
            Action::Next(_) | Action::Diagnostic(_) | Action::Dismiss => {
                if matches!(action, Action::Dismiss)
                    && let Some(state) = self.lsp_contexts.get_mut(&card)
                {
                    state.invalidate_answers();
                }
                match self.cards.get_mut(&card) {
                    Some(CardRuntime::Repository(runtime)) => {
                        repository_task(runtime.update(RepositoryAction::Language { card, action }))
                    }
                    _ => Task::none(),
                }
            }
            Action::CompleteAt { .. } => Task::none(),
        }
    }

    fn flush_pending_panel_open(&mut self, card: CardId) -> Task<Message> {
        if self.pending_panel_open.as_ref().map(|open| open.card) != Some(card) {
            return Task::none();
        }
        let open = self
            .pending_panel_open
            .take()
            .expect("the pending open was just matched");
        let action = if open.ignored {
            let Ok(path) = RepoPath::parse(&open.path) else {
                return Task::none();
            };
            RepositoryAction::OpenFileFromBrowser { card, path }
        } else {
            RepositoryAction::OpenFile {
                card,
                path: open.path,
            }
        };
        cosmic::task::message(Message::Repository(action))
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

    fn reset_files_scroll(key: &ActiveWorktreeKey) -> Task<Message> {
        iced::widget::scrollable::scroll_to(
            sidebar::files_scroll_id(key),
            iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(0.0),
            },
        )
        .map(cosmic::Action::App)
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
        if crate::device::is_remote() {
            return Task::none();
        }
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
            untracked_files: untracked_changes
                || self.workspace_view_shown() == Some(context_sections::WorkspaceView::Files),
        };
        let browse_files =
            self.workspace_view_shown() == Some(context_sections::WorkspaceView::Files);
        let bound = self
            .sidebar_model
            .source_control
            .as_ref()
            .filter(|source| source.key == key);
        let previous_errors = bound
            .map(|source| source.ignored_directory_errors().clone())
            .unwrap_or_default();
        let previous = bound.and_then(|source| source.source_snapshot().cloned());
        let expanded = self
            .sidebar_state
            .expanded_directory_paths()
            .cloned()
            .collect::<Vec<_>>();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let service = RepositoryService::system("git");
                let mut snapshot = service
                    .snapshot_with_options(&worktree, options, &cancellation)
                    .map_err(|error| error.to_string())?;
                if browse_files {
                    let ignored = service
                        .ignored_files(&worktree, &snapshot.files.paths, &cancellation)
                        .map_err(|error| error.to_string())?;
                    snapshot.files.ignored = ignored.ignored;
                    snapshot.files.ignored_directories = ignored.ignored_directories;
                    repository_widget::expand_ignored_directories(
                        &worktree,
                        &mut snapshot.files,
                        &expanded,
                        &previous_errors,
                        &cancellation,
                    )?;
                }
                Ok(source_control_ui::source_control_read(
                    previous.as_deref(),
                    snapshot,
                ))
            })
            .await
            .map_err(|error| format!("source-control reader join failed: {error}"))
            .and_then(|result| result);
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
        if crate::device::is_remote() {
            return Task::none();
        }
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
    fn begin_mutation(
        &mut self,
        key: &ActiveWorktreeKey,
        kind: MutationKind,
        commit_revision: Option<u64>,
    ) -> Option<uuid::Uuid> {
        if !self.source_control_is_bound(key)
            || self.folder_scope(key)
            || self
                .source_control_mutations
                .contains_key(&key.canonical_path)
        {
            return None;
        }
        let source = self.sidebar_model.source_control.as_mut()?;
        if source.busy() {
            return None;
        }
        let id = uuid::Uuid::new_v4();
        self.source_control_mutations.insert(
            key.canonical_path.clone(),
            SourceControlMutation {
                id,
                kind,
                commit_revision,
            },
        );
        source.set_error(None);
        source.set_mutation(Some(kind));
        Some(id)
    }

    /// The task one finished mutation turns into: report what Git did, then
    /// re-read the worktree, because the index is the only authority on what
    /// the mutation left behind.
    fn mutation_task(
        key: ActiveWorktreeKey,
        operation: uuid::Uuid,
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
                operation,
                report,
                committed,
            }
        })
    }

    /// How a notice names a pane: quoted, by the title its tab shows, so a
    /// terminal reads as its numbered tab rather than a name several tabs
    /// share. The notice log is shared by every pane, so "this conversation"
    /// would not say which.
    fn notice_pane_name(&self, card: CardId) -> Option<String> {
        self.card_state(card)
            .map(|state| format!("“{}”", self.repository_chrome(state)))
    }

    /// Withdraws reports about cards that are gone. A card leaves by many
    /// paths - a close, a deleted worktree or channel, a sign-in's terminal
    /// retiring, another client removing a shared pane - so this holds for
    /// all of them in one place, after every update, rather than in each.
    /// Likewise a card that could not be restored at startup may still open
    /// here, from a shared view that holds it, and its report goes then.
    fn resolve_card_notices(&mut self) {
        self.prune_terminal_save_notices();
        let closed = self
            .notices
            .entries
            .iter()
            .filter_map(|entry| match entry.notice.key.as_ref() {
                Some(
                    key @ (NoticeKey::ChatAttach(card)
                    | NoticeKey::ChatReturn(card)
                    | NoticeKey::ChatClose(card)
                    | NoticeKey::TerminalStart(card)
                    | NoticeKey::TerminalSave(card, _)),
                ) if self
                    .card_state(*card)
                    .is_none_or(|state| !state.existence.is_open()) =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in closed {
            self.notices.resolve(key);
        }
        let restored = self
            .notices
            .entries
            .iter()
            .filter_map(|entry| match entry.notice.key.as_ref() {
                Some(key @ NoticeKey::CardRestore(card))
                    if self
                        .card_state(*card)
                        .is_some_and(|state| state.existence.is_open()) =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in restored {
            self.notices.resolve(key);
        }
    }

    /// Whether `key` has a report standing, including one raised earlier in
    /// this same update that the log has not drained yet.
    fn notice_stands(&self, key: NoticeKey) -> bool {
        self.status
            .as_ref()
            .is_some_and(|notice| notice.key.as_ref() == Some(&key))
            || self.notices.has(key)
    }

    /// Withdraws whatever `key` reported, because its condition no longer
    /// holds, including a report raised earlier in this same update that the
    /// log has not drained yet.
    fn resolve_notice(&mut self, key: NoticeKey) {
        if self
            .status
            .as_ref()
            .is_some_and(|notice| notice.key.as_ref() == Some(&key))
        {
            self.status = None;
        }
        self.notices.resolve(key);
    }

    fn set_workspace_surfaces_visible(&mut self, workspace_visible: bool) {
        self.apply_workspace_surface_visibility(workspace_visible);
        self.trim_warm_chats();
    }

    /// [`Self::set_workspace_surfaces_visible`] without applying the warm
    /// chat budget, for a caller that shows the next panes before the budget
    /// is applied: hiding everything first must not cool the chats about to
    /// be shown again.
    fn apply_workspace_surface_visibility(&mut self, workspace_visible: bool) {
        let visible_cards = self
            .presence
            .iter()
            .filter(|(_, presence)| presence.exposure == Exposure::Visible)
            .map(|(id, _)| *id)
            .filter(|id| {
                self.card_state(*id)
                    .is_some_and(|card| card.existence.is_open())
            })
            .collect::<HashSet<_>>();
        let mut failures = Vec::new();
        for (id, runtime) in &mut self.cards {
            let visible = workspace_visible && visible_cards.contains(id);
            let result = match runtime {
                // A terminal whose daemon connection has ended for good
                // already says so in its pane, and the next attachment
                // applies visibility as it starts, so there is nothing for
                // this pass to report; that matches a disconnected chat.
                CardRuntime::Terminal(surface) => match if visible {
                    surface.reveal()
                } else {
                    surface.hide()
                } {
                    Ok(()) | Err(crate::terminal::TerminalSendError::Closed) => Ok(()),
                    Err(error) => Err(error.to_string()),
                },
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
                | CardRuntime::Automations
                | CardRuntime::Work => Ok(()),
            };
            if let Err(error) = result {
                failures.push((*id, visible, error));
            }
        }
        self.report_surface_visibility(failures);
    }

    /// Reports the panes one visibility pass could not reach, each as
    /// `(card, whether it was being shown, why)`. Visibility is reapplied on
    /// every pass, so a pass that reaches every pane is what resolves an
    /// earlier failure.
    fn report_surface_visibility(&mut self, mut failures: Vec<(CardId, bool, String)>) {
        if failures.is_empty() {
            self.resolve_notice(NoticeKey::SurfaceVisibility);
            return;
        }
        // A pane left showing stale content is the failure the reader can
        // see, so it is the one reported when both kinds happen together.
        let showing = failures.iter().any(|(_, visible, _)| *visible);
        failures.retain(|(_, visible, _)| *visible == showing);
        // Named in saved card order, so which pane a notice leads with does
        // not depend on how a hash map iterates; panes no longer saved come
        // last, and ids break the tie.
        failures.sort_by_cached_key(|(id, _, _)| (self.card_index(*id).unwrap_or(usize::MAX), *id));
        let others = failures.len() - 1;
        let (card, _, error) = failures.swap_remove(0);
        let pane = self.notice_pane_name(card);
        let subject = match (pane, others) {
            (Some(pane), 0) => pane,
            (None, 0) => "A pane".to_owned(),
            (Some(pane), 1) => format!("{pane} and 1 other pane"),
            (Some(pane), others) => format!("{pane} and {others} other panes"),
            (None, others) => format!("{} panes", others + 1),
        };
        let message = match (showing, others) {
            (true, 0) => format!("{subject} may not show its latest content."),
            (true, _) => format!("{subject} may not show their latest content."),
            (false, _) => format!("{subject} may keep updating while hidden."),
        };
        self.status = Some(
            TransientNotice::warning(message)
                .with_details(error)
                .with_key(NoticeKey::SurfaceVisibility),
        );
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
        if self.pane_content_inputs_blocked(card) {
            return None;
        }
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
        self.device_ui.open
            || self.worktree_dialog.is_some()
            || self.pull_request_dialog.is_some()
            || self.resume_ui.open
    }

    /// Closes the topmost form dialog, stopping whatever it was waiting on.
    fn cancel_form_dialog(&mut self) -> Task<Message> {
        if self.automation_approval_open() {
            return self.cancel_automation_approval();
        }
        if self.session_recovery.open() {
            return self.update_session_recovery(session_recovery::Action::Cancel);
        }
        if self.daemon_health_ui.confirmation {
            return self.update_daemon_health_ui(daemon_health_ui::Action::Cancel);
        }
        if self.transient_work_close.is_some() {
            return self.dispatch(Message::CancelTransientWorkClose);
        }
        if let Some(action) = self.automation_ui.keep_draft_action() {
            return self.update_automation_ui(action);
        }
        if self.device_ui.open {
            return self.update_device(device_ui::Action::Close);
        }
        if self.resume_ui.open {
            return self.update_resume(resume_ui::Action::Close);
        }
        if let Some(dialog) = self.worktree_dialog.take() {
            dialog.cancel();
        } else if self.pull_request_dialog.take().is_some() {
            self.pull_request_dialog_generation =
                self.pull_request_dialog_generation.wrapping_add(1);
        }
        if let Some(target) = self.nested_modal_focus_target() {
            return iced::widget::operation::focus(target).map(cosmic::Action::App);
        }
        Task::none()
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
        self.session_recovery.open()
            || self.daemon_health_ui.confirmation
            || self.automation_approval_open()
            || self.dirty_repository_close.is_some()
            || self.transient_work_close.is_some()
            || self.automation_ui.draft_dialog_open()
            || self.chat_ui.image_dialog.is_some()
            || self.terminal_paste.is_some()
            || self.terminal_close_dialog_visible()
            || self.chat_close_dialog_visible()
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

    fn open_sidebar_navigation_row(&mut self, row: sidebar::SelectionRow) -> Task<Message> {
        let action = self.sidebar_navigation_action(row);
        action.map_or_else(Task::none, |action| self.dispatch(Message::Sidebar(action)))
    }

    fn sidebar_navigation_action(&self, row: sidebar::SelectionRow) -> Option<SidebarAction> {
        match row {
            sidebar::SelectionRow::Project(id) => {
                self.sidebar_model.project(id).and_then(|project| {
                    sidebar::project_primary_action(project, self.sidebar_state.filter())
                })
            }
            sidebar::SelectionRow::Channel(channel_id) => {
                Some(SidebarAction::OpenChannel { channel_id })
            }
            sidebar::SelectionRow::Card(card) => Some(SidebarAction::OpenCard { card }),
            sidebar::SelectionRow::Worktree(project_id, worktree_id) => self
                .sidebar_model
                .project(project_id)
                .and_then(|project| {
                    project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.id == worktree_id)
                })
                .map(|worktree| SidebarAction::OpenWorktree {
                    project_id,
                    worktree_id,
                    path: worktree.path.clone(),
                }),
        }
    }

    fn projects_sidebar_mode(&self) -> SidebarMode {
        if self.chrome_ui.projects_collapsed {
            SidebarMode::Compact
        } else {
            SidebarMode::Expanded
        }
    }

    fn workspace_inputs_blocked(&self) -> bool {
        self.modal_is_open() || self.chrome_ui.is_open() || self.palette.is_open()
    }

    /// Chrome popups close when the form returns; these layers keep their
    /// place above it instead.
    fn pull_request_restore_covered(&self) -> bool {
        self.modal_is_open() || self.palette.is_open()
    }

    /// A pane's overlay takes input from its covered content, while the other
    /// panes and the inspector's own controls remain usable.
    fn pane_content_inputs_blocked(&self, card: CardId) -> bool {
        self.workspace_inputs_blocked()
            || self.pane_inspector_layout(card) == Some(InspectorLayout::Overlay)
    }

    fn handle_context_inspector_action(&mut self, action: InspectorAction) -> Task<Message> {
        match action {
            InspectorAction::Resize { width } => {
                // Whole pixels, as Settings shows and steps them.
                self.settings.inspector_width = width
                    .round()
                    .clamp(design::INSPECTOR_MIN_WIDTH, design::INSPECTOR_MAX_WIDTH);
                Task::none()
            }
            InspectorAction::ResizeFinished => {
                // Settings, if open, steps from the dragged width.
                self.settings_draft.inspector_width = self.settings.inspector_width.to_string();
                self.save_settings_task(false)
            }
            InspectorAction::CopyLspDiagnostic(diagnostic) => {
                chat_ui::invalidate_copy_intent();
                iced::clipboard::write(diagnostic.copy_text()).map(cosmic::Action::App)
            }
            action @ (InspectorAction::RestartLanguageServer(_)
            | InspectorAction::DismissLspResults(_)
            | InspectorAction::JumpToLspDiagnostic { .. }
            | InspectorAction::JumpToLspLocation { .. }) => self.update_lsp(action),
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
        let navigation_blocked = self.modal_is_open() || repository_blocks;
        let covered = self.pull_request_restore_covered();
        // This runs after every update: leave a queue nothing can leave alone.
        if self.deferred_navigation.is_empty() || (navigation_blocked && covered) {
            return Task::none();
        }
        let mut ready = Vec::new();
        for message in std::mem::take(&mut self.deferred_navigation) {
            let blocked = if matches!(&message, Message::RestorePullRequestDialog { .. }) {
                covered
            } else {
                navigation_blocked
            };
            if blocked {
                self.deferred_navigation.push_back(message);
            } else {
                ready.push(cosmic::task::message(message));
            }
        }
        Task::batch(ready)
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
        if self.operation_recovery_pending() {
            self.status = Some(TransientNotice::error(
                "Finish or resolve retained file operations before closing this window.",
            ));
            return Task::none();
        }

        if self.exit.is_some() {
            return Task::none();
        }
        if let Err(error) = crate::performance::chat_stage::flush() {
            tracing::warn!(target: "cagents::chat_stage", error_code = ?error.raw_os_error(),
                "Could not flush opt-in chat stage measurements");
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
    /// 3. retire the capability generation and explicitly clear every factory
    ///    queue, even when a late UI callback retains its factory Arc;
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
        self.browser_backend.retire();
        if let Some(factory) = self.browser_factory.take() {
            factory.release_parent();
        }
        if let Some(parent_lifetime) = self.browser_parent_lifetime.take() {
            parent_lifetime.deactivate();
        }
    }

    fn activate_main_window(&self, token: Option<String>) -> Task<Message> {
        self.activate_main_window_on_backend(cosmic::app::cosmic::windowing_system(), token)
    }

    fn activate_main_window_on_backend(
        &self,
        backend: Option<cosmic::app::cosmic::WindowingSystem>,
        token: Option<String>,
    ) -> Task<Message> {
        let Some(window) = self.core.main_window_id() else {
            return Task::none();
        };
        if backend == Some(cosmic::app::cosmic::WindowingSystem::Wayland) {
            // Preserve the compositor's token decision on an actual Wayland
            // surface. Compiling Wayland support does not make an X11 window
            // capable of receiving that platform-specific activation command.
            iced::Task::done(cosmic::Action::Cosmic(cosmic::app::Action::Activate(
                token.unwrap_or_default(),
            )))
        } else {
            iced::window::minimize(window, false)
                .chain(iced::window::gain_focus(window))
                .map(cosmic::Action::App)
        }
    }

    fn window_close_transient_blocker(&self) -> Option<&'static str> {
        if self.operation_recovery_pending() {
            return Some("Finish or resolve retained file operations before closing this window.");
        }

        if let Some(blocker) = self.automation_ui.draft_close_blocker() {
            return Some(blocker);
        }
        if self.device_ui.dirty() {
            return Some(
                "The device file has unsaved edits. Return to the file to save or discard them, then close the window again.",
            );
        }
        if self.device_ui.busy() {
            return Some("Wait for the device operation to finish before closing the window.");
        }
        if self.chat_ui.transition_pending() {
            return Some(
                "Wait for the chat to finish starting or changing views before closing the window.",
            );
        }
        self.cards
            .values()
            .find_map(|runtime| match runtime {
                CardRuntime::Chat(runtime) => runtime.restart_blocker(),
                CardRuntime::PullRequests(runtime) => runtime.review.restart_blocker(),
                _ => None,
            })
            .or_else(|| self.parked_chat_restart_blocker())
    }

    fn begin_dirty_close(
        &mut self,
        intent: DirtyCloseIntent,
        mut cards: VecDeque<CardId>,
    ) -> Task<Message> {
        let Some(card) = cards.pop_front() else {
            return match intent {
                DirtyCloseIntent::PaneGroup => self.close_pane_group(),
                DirtyCloseIntent::Card(card) => self.dispatch(Message::CloseCard(card)),
                DirtyCloseIntent::Application(_) => self.exit_after_language_servers(),
                DirtyCloseIntent::Channel(channel) => self.resume_channel_deletion(channel),
            };
        };
        // Keep a stable workspace fallback even when another modal is
        // currently underneath. That modal may finish its own close animation
        // while dirty-close is topmost; Cancel re-checks the live modal tree
        // and uses this only when none remains.
        let restore_card = self.workspace.focused();
        let focused = DirtyCloseControl::Cancel;
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

    fn commit_requested_close(&mut self, id: CardId) -> Task<Message> {
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
        match self.commit_card_removal(id, None) {
            Some(task) => task,
            None => {
                // Refused after all: the pane stays, and the closes queued
                // behind it move on.
                self.abandon_removal(id);
                self.next_queued_card_close()
            }
        }
    }

    fn report_notice(&mut self, notice: TransientNotice) {
        if crate::daemon_health::replaces_notice(&notice.message)
            || notice
                .details
                .as_deref()
                .is_some_and(crate::daemon_health::replaces_notice)
        {
            // The banner speaks for this outcome now, so an earlier report
            // under the same key would only be giving an outdated reason.
            if let Some(key) = notice.key {
                self.notices.resolve(key);
            }
        } else {
            let details = notice.details.as_deref();
            match notice.severity {
                NoticeSeverity::Error => {
                    tracing::error!(message = %notice.message, details, "application notice")
                }
                NoticeSeverity::Warning => {
                    tracing::warn!(message = %notice.message, details, "application notice")
                }
                NoticeSeverity::Success => {}
            }
            self.notices.record(notice);
        }
    }

    fn close_pane_group(&mut self) -> Task<Message> {
        let cards = std::mem::take(&mut self.chrome_ui.closing_group);
        let tasks: Vec<_> = cards
            .into_iter()
            .map(|card| self.begin_card_close(card))
            .collect();
        Task::batch(tasks)
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
            let focused = DirtyCloseControl::Cancel;
            close.focused = focused;
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(card, focused))
                .map(cosmic::Action::App);
        }
        if let Some(card) = close.next_unresolved(|card| {
            matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())
        }) {
            let focused = DirtyCloseControl::Cancel;
            close.focused = focused;
            self.dirty_repository_close = Some(close);
            return iced::widget::operation::focus(dirty_close_control_id(
                card,
                focused,
            ))
            .map(cosmic::Action::App);
        }
        match close.intent {
            DirtyCloseIntent::PaneGroup => self.close_pane_group(),
            // Called rather than sent: a queued pane could start meanwhile and
            // leave this close, the one the reader just asked for, turned away.
            DirtyCloseIntent::Card(card) => self.dispatch(Message::CloseCard(card)),
            DirtyCloseIntent::Application(_) => self.exit_after_language_servers(),
            DirtyCloseIntent::Channel(channel) => self.resume_channel_deletion(channel),
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
        match close.intent {
            DirtyCloseIntent::PaneGroup => self.chrome_ui.closing_group.clear(),
            DirtyCloseIntent::Channel(channel) => self.cancel_channel_deletion(channel),
            DirtyCloseIntent::Card(_) | DirtyCloseIntent::Application(_) => {}
        }
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
        if self.automation_ui.draft_dialog_open() {
            return Some(crate::automation_ui::draft_dialog_focus_id());
        }
        if self.resume_ui.open {
            return Some(resume_ui::focus_id());
        }
        if let Some(dialog) = &self.sidebar_selection_ui.dialog {
            Some(dialog.focus_target())
        } else if self.sidebar_state.pending_discard().is_some() {
            Some(
                dialog_ui::Question::DiscardChanges
                    .initial_focus(sidebar::discard_cancel_id(), sidebar::discard_confirm_id()),
            )
        } else if self.project_ui.modal_open() {
            Some(self.project_ui.focus_target())
        } else if self.channel_ui.modal_open() {
            Some(channel_ui::State::focus_target())
        } else if self.worktree_dialog.is_some() {
            Some(worktree_dialog_focus_id())
        } else if self.pull_request_dialog.is_some() {
            Some(pull_request_dialog_focus_id())
        } else {
            None
        }
    }

    fn pull_request_submission_allowed(&self) -> bool {
        self.pull_request_dialog.as_ref().is_some_and(|dialog| {
            dialog.can_submit()
                && self
                    .github_availability
                    .get(&dialog.project_id)
                    .and_then(GitHubAvailability::repository_key)
                    == Some(&dialog.repository)
        })
    }

    fn restore_pull_request_dialog(
        &mut self,
        dialog: PullRequestDialog,
        generation: u64,
    ) -> Task<Message> {
        if generation != self.pull_request_dialog_generation || self.pull_request_dialog.is_some() {
            return Task::none();
        }
        if self.palette.is_open() {
            // Use the existing deferred completion queue: the palette stays
            // visible, and a command that opens a modal keeps the draft waiting.
            self.deferred_navigation
                .push_back(Message::RestorePullRequestDialog { dialog, generation });
            return Task::none();
        }
        let covered = self.pull_request_restore_covered();
        let close_chrome = self.update_chrome(chrome_ui::Action::Close);
        self.pull_request_dialog = Some(dialog);
        if covered {
            return close_chrome;
        }
        close_chrome.chain(
            iced::widget::operation::focus(pull_request_dialog_focus_id()).map(cosmic::Action::App),
        )
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
            Ok(update) => self.apply_viewport(update, true, DurableChange::None),
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
            width_fraction: self.new_pane_fraction(self.settings.default_native_pane_fraction),
            existence: Existence::Open,
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
        self.set_presence(id, Exposure::Visible, Residency::Hot);
        self.register_channel_card(id);
        self.cards.insert(id, runtime);
        // No shared view holds a utility pane, so showing one is saved like
        // any other move of where this window is.
        self.apply_viewport(update, true, DurableChange::Navigation)
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
        let (title, width) = (
            browser_host::presentation_title(&card.kind, &card.title).to_owned(),
            card.width_fraction,
        );
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
        // Leaving a channel takes nothing out of its view, which never held
        // the pane: this moves only where the window shows it.
        self.apply_viewport(update, true, DurableChange::Navigation)
    }

    fn prune_pane_title_overrides(&mut self) {
        if self.chrome_ui.user_titles.is_empty() {
            return;
        }
        let retained: HashSet<_> = self
            .durable
            .cards
            .iter()
            .map(|card| card.id)
            .chain(self.recently_closed.iter().map(|(card, _)| card.id))
            .collect();
        self.chrome_ui
            .user_titles
            .retain(|id, _| retained.contains(id));
    }

    fn capture_settings_save(&mut self) -> Result<SettingsSave, String> {
        #[cfg(test)]
        terminal_output_tests::record_settings_capture();
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
        self.prune_pane_title_overrides();
        let mut view = WindowView {
            user_titles: self.chrome_ui.user_titles.clone(),
            projects_sidebar_collapsed: self.chrome_ui.projects_collapsed,
            sidebar_width: self.settings.sidebar_width,
            inspector_width: self.settings.inspector_width,
            context_inspector_open: self
                .workspace
                .focused()
                .is_some_and(|card| self.pane_inspectors.is_open(card)),
            open_inspectors: Some(self.saved_open_inspectors()),
            inspector_choices: Some(self.saved_inspector_choices()),
            context_sections: self.context_sections,
            sidebar: self.sidebar_state.saved_view(),
            worktree_offsets: self.worktree_offsets.clone(),
            channel_offsets: self.channel_offsets.clone(),
            settings_category: Some(self.settings_category),
            settings_search: self.settings_search.clone(),
        };
        if let Some(channel) = self.durable.active_channel {
            view.channel_offsets
                .insert(channel, self.workspace.camera().offset());
        } else if let Some(path) = &self.active_workspace {
            view.worktree_offsets
                .insert(path.clone(), self.workspace.camera().offset());
        }
        #[cfg(test)]
        terminal_output_tests::record_captured_view(&view.sidebar);
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
        self.materialize_replica_snapshot();
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
        let theme = self.effective_terminal_theme();
        for runtime in self.cards.values() {
            if let CardRuntime::Terminal(surface) = runtime
                && let Err(error) = surface.set_theme(theme)
            {
                self.status = Some(TransientNotice::error(format!(
                    "Could not apply terminal theme: {error}"
                )));
            }
        }
    }

    fn effective_terminal_theme(&self) -> crate::terminal::TerminalTheme {
        use crate::terminal::TerminalTheme;
        match self.settings.terminal_theme {
            TerminalTheme::Auto
                if self
                    .settings
                    .app_theme
                    .resolve(self.core.system_theme())
                    .theme_type
                    .is_dark() =>
            {
                TerminalTheme::Dark
            }
            TerminalTheme::Auto => TerminalTheme::Light,
            theme => theme,
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

    fn settings_blur_pending(&self) -> bool {
        self.settings_pane_focused
            && !self.settings_card_focused()
            && !self.settings_pending.is_empty()
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

    /// Retry refused shortcut drafts against the newly saved table. A different
    /// row may have released the chord they were waiting for.
    fn retry_shortcut_drafts(&mut self) {
        loop {
            let mut progressed = false;
            let fields: Vec<_> = SettingsField::SHORTCUTS
                .into_iter()
                .filter(|field| self.settings_invalid.contains_key(field))
                .collect();
            for field in fields {
                let action = field.shortcut_action().expect("shortcut field");
                let value = self
                    .settings_draft
                    .keyboard_shortcuts
                    .shortcut_values()
                    .into_iter()
                    .find(|(candidate, _)| *candidate == action)
                    .expect("registered shortcut")
                    .1
                    .to_owned();
                match settings_update(&self.settings, field, &value) {
                    SettingsDecision::Persist(settings) => {
                        self.settings = settings;
                        self.settings_invalid.remove(&field);
                        progressed = true;
                    }
                    SettingsDecision::Unchanged => {
                        self.settings_invalid.remove(&field);
                        progressed = true;
                    }
                    SettingsDecision::Invalid(reason) => {
                        self.settings_invalid.insert(field, reason);
                    }
                }
            }
            if !progressed {
                break;
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
                let previous = self.settings.clone();
                self.retry_shortcut_drafts();
                let changed = previous != self.settings;
                let save = if changed || self.settings_save_error.is_some() {
                    self.save_settings_task(false)
                } else {
                    Task::none()
                };
                self.refresh_settings_feedback();
                let effect = if changed {
                    self.synchronize_settings(&previous)
                } else {
                    Task::none()
                };
                Task::batch([save, effect])
            }
            SettingsDecision::Persist(settings) => {
                let previous = std::mem::replace(&mut self.settings, settings);
                self.settings_invalid.remove(&field);
                self.retry_shortcut_drafts();
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
        for runtime in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime) = runtime {
                runtime.tool_groups_expanded = self.settings.chat_expand_tool_groups;
            }
        }
        let mut tasks = Vec::new();
        if previous.inspector_open_by_default != self.settings.inspector_open_by_default {
            // Panes whose inspector the user never opened or closed follow
            // the default; the rest keep what was chosen for them.
            self.set_inspectors_open_by_default(self.settings.inspector_open_by_default);
            self.sync_pane_inspectors();
            tasks.push(self.rebind_source_control());
        }
        if previous.editor_font_family() != self.settings.editor_font_family()
            || previous.editor_font_size() != self.settings.editor_font_size()
        {
            let font = crate::terminal::monospace_font(self.settings.editor_font_family());
            let size = self.settings.editor_font_size();
            for (&card, runtime) in self.cards.iter_mut() {
                let runtime = match runtime {
                    CardRuntime::Repository(runtime) => runtime,
                    CardRuntime::PullRequests(pane) => &mut pane.runtime,
                    _ => continue,
                };
                runtime.set_editor_font(font, size);
                tasks.push(repository_task(runtime.refresh_diff_presentation(card)));
            }
        }
        if previous.editor_find_options != self.settings.editor_find_options
            || previous.editor_replace_preserve_case != self.settings.editor_replace_preserve_case
        {
            for (card, runtime) in &mut self.cards {
                let task = match runtime {
                    CardRuntime::Repository(runtime) => runtime.update_find_preferences(
                        *card,
                        self.settings.editor_find_options,
                        self.settings.editor_replace_preserve_case,
                    ),
                    CardRuntime::PullRequests(pane) => pane.runtime.update_find_preferences(
                        *card,
                        self.settings.editor_find_options,
                        self.settings.editor_replace_preserve_case,
                    ),
                    _ => iced::Task::none(),
                };
                tasks.push(repository_task(task));
            }
        }
        if previous.keyboard_shortcuts != self.settings.keyboard_shortcuts {
            self.card_chrome.clear();
        }
        if previous.worktree_sort != self.settings.worktree_sort {
            self.sidebar_state
                .set_worktree_sort(self.settings.worktree_sort);
        }
        self.sidebar_state
            .set_sidebar_worktrees(self.settings.sidebar_worktrees);
        self.sidebar_state
            .set_worktree_overrides(self.settings.sidebar_worktree_overrides.clone());
        self.sidebar_state
            .set_show_worktree_changes_icons(self.settings.show_worktree_changes_icons);
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
            for (&card, runtime) in self.cards.iter_mut() {
                let runtime = match runtime {
                    CardRuntime::Repository(runtime) => runtime,
                    CardRuntime::PullRequests(pane) => &mut pane.runtime,
                    CardRuntime::Chat(runtime) => {
                        runtime.set_syntax_theme(syntax_theme);
                        continue;
                    }
                    _ => continue,
                };
                runtime.set_wrap_lines(wrap_lines);
                runtime.set_syntax_theme(syntax_theme);
                // A comparison recoloured in the new palette is requested
                // again rather than left empty until the next refresh.
                tasks.push(repository_task(runtime.refresh_diff_presentation(card)));
            }
            self.chat_ui.set_parked_syntax_theme(syntax_theme);
        }
        if previous.diff_hide_unchanged != self.settings.diff_hide_unchanged {
            for runtime in self.cards.values_mut() {
                let runtime = match runtime {
                    CardRuntime::Repository(runtime) => runtime,
                    CardRuntime::PullRequests(pane) => &mut pane.runtime,
                    _ => continue,
                };
                tasks.push(repository_task(
                    runtime.set_diff_hide_unchanged(self.settings.diff_hide_unchanged),
                ));
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
            self.device_ui.menu_open = false;
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
        if self.lsp.servers() == self.settings.language_servers.as_slice()
            && self.lsp.lifecycle()
                == (
                    self.settings.lsp_idle_timeout_seconds.max(1),
                    self.settings.lsp_max_sessions.clamp(1, 64),
                )
        {
            return Task::none();
        }
        // `shutdown` alone moves the sessions into the outgoing manager's own
        // retiring list and kills nothing, and dropping that manager drops
        // them unreaped - after which no exit path can reach them, because
        // every exit path addresses the manager that replaced it.
        // `begin_shutdown` spawns the reap on the runtime the sessions run on,
        // so it survives if no new document is opened. New launches wait
        // for the same reap instead of overlapping a second admission cap.
        let predecessor = self.lsp.begin_shutdown();
        self.lsp = LspManager::new(self.settings.language_servers.clone())
            .with_lifecycle(
                self.settings.lsp_idle_timeout_seconds,
                self.settings.lsp_max_sessions,
            )
            .after_shutdown(predecessor);
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

    fn settings_reset_on_top(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return false;
        }
        self.settings_reset_confirm
            && !self.automation_approval_open()
            && self.transient_work_close.is_none()
            && !self.automation_ui.draft_dialog_open()
            && !self.device_ui.open
            && self.chat_ui.image_dialog.is_none()
            && self.terminal_paste.is_none()
            && !self.session_recovery.open()
            && !self.daemon_health_ui.confirmation
            && self.update_ui.confirmation.is_none()
            && !self.terminal_close_dialog_visible()
            && !self.chat_close_dialog_visible()
    }

    fn close_settings_reset(&mut self) -> Task<Message> {
        if !std::mem::take(&mut self.settings_reset_confirm) {
            return Task::none();
        }
        if let Some(restore) = self.settings_reset_keyboard_nav.take() {
            self.core.set_keyboard_nav(restore);
        }
        let target = if self.settings_card_focused() {
            widget::Id::new("settings.reset")
        } else {
            self.workspace.focused().map_or_else(
                || widget::Id::new(TOOLBAR_FOCUS_ID),
                |card| self.card_keyboard_focus(card),
            )
        };
        iced::widget::operation::focus(target).map(cosmic::Action::App)
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
        if let Err(error) = crate::performance::chat_stage::initialize_from_environment() {
            tracing::warn!(target: "cagents::chat_stage", error_code = ?error.raw_os_error(),
                "Could not enable opt-in chat stage measurements");
        }
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
        let retired = worktree_ui::retire_missing_worktrees(&mut worktrees);
        let scope_project = durable.active_worktree.as_ref().and_then(|scope| {
            worktrees
                .iter()
                .find(|worktree| worktree.path == *scope)
                .map(|worktree| worktree.project_id)
        });
        if let Some(session) = &session {
            session.save_worktrees(retired);
        }
        let window_view = startup.window_view;
        let inspector_open_by_default = settings.inspector_open_by_default;
        if let Some(view) = &window_view {
            settings.sidebar_width = sidebar::clamp_expanded_width(view.sidebar_width);
            // Whole pixels, as a drag leaves it, so Settings steps from it.
            settings.inspector_width = if view.inspector_width.is_finite() {
                view.inspector_width
                    .round()
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
                .with_kind(project.kind)
            })
            .collect();
        let workspace = Self::rebuild_workspace(&durable);
        let chat_ui = chat_ui::State::for_workspace(&durable);
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
        let shared_views = shared_views::State::for_workspace(&durable);
        let workspace_history = crate::tab_history::TabHistory::from_recent(
            durable
                .workspace_history
                .iter()
                .copied()
                .take(WORKSPACE_HISTORY_LIMIT),
        );
        // A window that keeps no state records nothing for the others.
        let update_resume = if session.is_some() {
            update_resume::State::shared()
        } else {
            update_resume::State::default()
        };
        let mut app = Self {
            startup_gate: startup_gate::State::default(),
            core,
            workspace,
            durable: Arc::new(durable),
            cards: HashMap::new(),
            terminal_working_directories: HashMap::new(),
            terminal_find: HashMap::new(),
            terminal_review_card: None,
            terminal_review_revision: 0,
            terminal_review_identity: None,
            terminal_review_input: crate::terminal::ReviewInputBarrier::default(),
            terminal_context: None,
            terminal_paste: None,
            card_chrome: HashMap::new(),
            card_indices: HashMap::new(),
            card_indices_source: std::sync::Weak::new(),
            terminal_ordinals_source: std::sync::Weak::new(),
            navigation_cache: sidebar::NavigationCache::default(),
            place_index: inspector_activity::PlaceIndex::default(),
            palette: palette_ui::State::default(),
            recently_closed: Vec::new(),
            presence: HashMap::new(),
            lifecycle: LifecycleManager::new(LifecycleBudget {
                max_hot_cards: settings.max_hot_cards,
                offscreen_grace: Duration::from_millis(settings.hibernation_delay_ms),
            }),
            lsp: LspManager::new(settings.language_servers.clone())
                .with_lifecycle(settings.lsp_idle_timeout_seconds, settings.lsp_max_sessions),
            lsp_documents: HashMap::new(),
            lsp_contexts: HashMap::new(),
            lsp_pending_jumps: HashMap::new(),
            next_lsp_document: 0,
            terminal_wake_tx,
            terminal_wake_rx,
            hosted_terminals_rx,
            terminal_starts: std::sync::Mutex::new(HashMap::new()),
            terminal_failure: terminal_failure::State::default(),
            terminal_save: terminal_save::State::default(),
            saved_terminals: saved_terminal::State::default(),
            pending_adoptions: Vec::new(),
            restored_agent_attaches: VecDeque::new(),
            relaunched_terminals: HashSet::new(),
            terminal_crash_notices: HashMap::new(),
            shared_views,
            workspace_model: crate::workspace_model::WorkspaceModel::default(),
            workspace_model_opens: HashSet::new(),
            workspace_model_rx,
            workspace_model_tx,
            activation_rx,
            notification_ui: notification_ui::State::default(),
            attention_focus_epoch: 0,
            daemon_health_ui: daemon_health_ui::State::default(),
            session,
            save_health,
            session_recovery: session_recovery::State::default(),
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
            browser_backend: browser_backend::State::default(),
            #[cfg(feature = "integrated-browser")]
            browser_parent_lifetime: None,
            system_url_opener: file_manager::SystemUrlOpener::default(),
            window_scale_factor,
            window_width: 1_600.0,
            window_height: 900.0,
            context_sections_height: None,
            window_filled: false,
            window_state_observed: false,
            right_edge_tiled: false,
            settings_path,
            settings_write_blocked,
            notices: NoticeLog::default(),
            sidebar_model: SidebarModel::ready(SidebarMode::Expanded, sidebar_projects),
            sidebar_state: SidebarState::default(),
            project_ui: project_ui::State::default(),
            worktree_protections: Default::default(),
            channel_ui: channel_ui::State::default(),
            sidebar_selection_ui: sidebar_selection_ui::State::default(),
            chat_ui,
            resume_ui: resume_ui::State::default(),
            update_ui: update_ui::State::new(),
            update_resume,
            branch_ui: branch_ui::State::default(),
            history_ui: history_ui::State::default(),
            refresh_ui: refresh_ui::State::default(),
            source_control_mutations: HashMap::new(),
            operation_recovery: Default::default(),
            device_ui: device_ui::State::default(),
            account_ui: account_ui::State::installation().into(),
            sign_ins: account_ui::SignIns::default(),
            automation_ui: crate::automation_ui::State::default(),
            work_ui: crate::work_ui::State::default(),
            automation_approval: automation_approval::State::default(),
            chrome_ui: chrome_ui::State::default()
                .with_user_titles(
                    window_view
                        .as_ref()
                        .map(|view| view.user_titles.clone())
                        .unwrap_or_default(),
                )
                .with_projects_collapsed(
                    window_view
                        .as_ref()
                        .is_some_and(|view| view.projects_sidebar_collapsed),
                ),
            pull_request_inspector: pull_request_ui::State::default(),
            active_workspace: None,
            worktree_focus: HashMap::new(),
            worktree_offsets: HashMap::new(),
            channel_offsets: HashMap::new(),
            worktree_owners: HashMap::new(),
            #[cfg(any(test, feature = "development"))]
            projected_protections: Default::default(),
            known_worktree_roots: worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect(),
            worktree_paths: worktree_ui::WorktreePathIndex::default(),
            pull_request_board: chat_ui::PullRequestBoard::default(),
            workspace_view_generation: 0,
            workspace_scroll_id: widget::Id::new("workspace-strip-0"),
            github_availability: HashMap::new(),
            review_repositories: HashMap::new(),
            repository_generations: HashMap::new(),
            repository_pr_cancellations: HashMap::new(),
            repository_load_sequence: 0,
            worktree_dialog: None,
            settings_reset_confirm: false,
            dont_ask_again: BTreeSet::new(),
            settings_reset_keyboard_nav: None,
            pull_request_dialog: None,
            pull_request_dialog_generation: 0,
            dirty_repository_close: None,
            chat_close_confirm: None,
            transient_work_close: None,
            automation_close_generation: 0,
            terminal_close: None,
            queued_card_closes: VecDeque::new(),
            close_queue_advancing: false,
            close_commit_delivered: None,
            terminal_close_attempts: HashSet::new(),
            completed_release: None,
            terminal_close_generation: 0,
            closed_terminal_cards: HashSet::new(),
            proven_gone_cards: HashSet::new(),
            uncertain_terminal_proofs: HashMap::new(),
            terminal_recovery_attempts: HashSet::new(),
            retained_outputs: HashMap::new(),
            pending_terminal_handoffs: HashMap::new(),
            unsaved_chat_returns: HashMap::new(),
            terminal_handoff_commit_inflight: false,
            worktree_operations: HashMap::new(),
            pending_panel_open: None,
            deferred_navigation: VecDeque::new(),
            owed_projection: None,
            attention_scope_changed: false,
            dispatching: 0,
            frame_clock: FrameClock::default(),
            latency: crate::performance::InteractionLatency::default(),
            latency_focus: None,
            #[cfg(feature = "latency-harness")]
            latency_fixture,
            #[cfg(feature = "latency-harness")]
            latency_fixture_started: false,
            // A window saved before each pane had its own inspector opens the
            // focused pane's as the window's was. Every pane nobody opened or
            // closed one in, as in a window never saved, follows the default.
            pane_inspectors: window_view
                .as_ref()
                .map_or_else(pane_inspector::PaneInspectors::default, |view| match &view
                    .inspector_choices
                {
                    Some(choices) => {
                        pane_inspector::PaneInspectors::restore_choices(choices.clone())
                    }
                    None => pane_inspector::PaneInspectors::restore(
                        view.open_inspectors.clone(),
                        view.context_inspector_open,
                    ),
                })
                .with_default(inspector_open_by_default),
            context_sections: window_view
                .as_ref()
                .map_or_else(context_sections::Sections::default, |view| {
                    view.context_sections
                }),
            settings_category: SettingsCategory::Appearance,
            settings_search: String::new(),
            tab_history: crate::tab_history::TabHistory::default(),
            workspace_history,
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
            navigation_save: None,
            replica_capture: ReplicaCapture::default(),
            navigation_save_generation: 0,
            pending_repository_reconciliation: Vec::new(),
            released_repository_navigation: HashMap::new(),
            keyboard_owed: None,
            reported_title_save: ReportedTitleSave::default(),
            lifecycle_sweep_destination: None,
            input_trace: crate::input_trace::InputTrace::default(),
        };
        // One notice per lost card, not a single joined line, so distinct
        // failures stay distinct. The alternative this replaces was losing
        // cards with no diagnostic anywhere in the UI.
        for warning in restore_warnings {
            app.notices.record(card_restore_notice(&warning));
        }
        for warning in settings_warnings {
            app.notices.record(TransientNotice::warning(warning));
        }
        app.sidebar_state
            .restore_unread_updates(app.durable.agent_updates.clone());
        app.sidebar_model
            .apply_project_order(&app.durable.project_order);
        app.sidebar_state
            .set_worktree_sort(app.settings.worktree_sort);
        app.sync_worktree_protections();
        app.sidebar_state
            .set_sidebar_worktrees(app.settings.sidebar_worktrees);
        app.sidebar_state
            .set_worktree_overrides(app.settings.sidebar_worktree_overrides.clone());
        app.sidebar_state
            .set_show_worktree_changes_icons(app.settings.show_worktree_changes_icons);
        // `view` lays the strip out against the width `Workspace` holds, which
        // starts at zero, and `PanSurface` only publishes its real bounds once
        // it has been mounted and redrawn. A strip projected against zero
        // mounts nothing but its overscan band, so without a seed the frame in
        // which the strip first appears - at startup, and again when the first
        // card arrives - flashes empty and rebuilds every card's widget state.
        // The navigation and the strip split the window, so subtracting it from
        // the starting width is the closest estimate available before the
        // surface reports its own. The strip then replaces the estimate itself,
        // from its own measured bounds, through `WorkspaceViewportEvent`; the
        // window's own resize only re-derives the densities. Card widths are
        // fractions re-derived per frame, so an estimate costs nothing but is
        // enough to mount them.
        let sidebar_width =
            sidebar::effective_width(app.projects_sidebar_mode(), app.settings.sidebar_width);
        let _ = app
            .workspace
            .resize_viewport((app.content_width() - sidebar_width).max(0.0));
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
            app.pane_inspectors = pane_inspector::PaneInspectors::default();
            let chrome = app.sync_chrome();
            return (app, chrome);
        }
        let scope = app.restore_window_view(window_view, scope_project);
        // Closed/exited replacements can still owe a lost durable acknowledgement.
        for card in &app.durable.cards {
            let terminal = match &card.kind {
                CardKind::Terminal(terminal) => Some(terminal),
                CardKind::Chat(chat) => chat.terminal.as_deref(),
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
            app.update_update_ui(update_ui::Action::Poll),
            app.update_update_ui(update_ui::Action::InspectInstallation),
            app.load_accounts(),
        ];
        // Unused default chats are never saved. Recreate the active scope's
        // placeholder only after restored runtimes have been initialized.
        tasks.push(app.ensure_default_pane());
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

    fn subscription(&self) -> Subscription<Message> {
        let _trace =
            crate::performance::ui_trace::scope(crate::performance::ui_trace::Phase::Subscription);
        let operation_subscriptions = self.worktree_progress_subscriptions();
        let mut subscriptions = vec![
            self.chat_subscription(),
            self.notification_subscription(),
            self.notice_expiry_subscription(),
            Subscription::run(crate::daemon_health::stream).map(Message::DaemonHealth),
            Subscription::run(crate::desktop_cursor_blink::stream).map(Message::CursorBlink),
            self.refresh_subscription(),
            self.review_request_subscription(),
            self.git_subscription(),
            self.account_subscription(),
            self.automation_subscription(),
            self.work_subscription(),
            self.conversation_pull_request_subscription(),
            self.automation_approval_subscription(),
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
                None => match activation.session {
                    Some(session) => Message::ActivateSession {
                        session,
                        token: activation.token,
                    },
                    None => Message::ActivateUi {
                        token: activation.token,
                    },
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
                iced::Event::Window(event) => window_geometry_message(&event),
                event @ iced::Event::PlatformSpecific(_) => {
                    window_edge_message(&event, event_window)
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
        subscriptions.extend(
            self.cards
                .iter()
                .filter_map(|(&card, runtime)| match runtime {
                    CardRuntime::Repository(runtime) => {
                        Some(runtime.document_subscription(card).map(Message::Repository))
                    }
                    _ => None,
                }),
        );
        subscriptions.push(Subscription::run_with(
            (self.settings_path.clone(), self.settings_ack_generation),
            |(path, generation)| {
                use cosmic::iced::futures::StreamExt;
                let generation = *generation;
                settings_ui::shared_preferences_stream(path.clone()).map(move |settings| {
                    Message::SharedPreferencesObserved {
                        generation,
                        settings,
                    }
                })
            },
        ));
        subscriptions.extend(operation_subscriptions);
        if let Some(window) = self.core.main_window_id()
            && (self.workspace.is_animating() || self.pane_inspectors.is_animating())
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
        #[cfg(feature = "integrated-browser")]
        if self.cards.iter().any(|(card, runtime)| matches!(runtime, CardRuntime::Browser(browser) if browser.load_state.is_some() || browser.lifecycle == BrowserLifecycle::Restoring)
            && self.workspace.visibility(*card) == Some(VisibilityClass::Visible))
        {
            subscriptions.push(iced::time::every(Duration::from_millis(50)).map(|_| Message::BrowserProgressTick));
        }
        Subscription::batch(subscriptions)
    }

    fn dialog(&self) -> Option<Element<'_, Message>> {
        if let Some(dialog) = self.automation_approval_dialog() {
            return Some(dialog);
        }
        if self.transient_work_close.is_some() {
            let mut controls: Vec<Element<'_, Message>> = vec![
                crate::ui_controls::button::secondary("Cancel close")
                    .id(widget::Id::new(TRANSIENT_CLOSE_CANCEL))
                    .on_press(Message::CancelTransientWorkClose)
                    .into(),
            ];
            if self.device_ui.dirty() {
                controls.push(
                    crate::ui_controls::button::primary("Return to file")
                        .on_press(Message::ReturnToDeviceBeforeClose)
                        .into(),
                );
            }
            if self.automation_ui.draft_close_blocker().is_some() {
                controls.push(
                    crate::ui_controls::button::primary("Keep editing automation")
                        .on_press(Message::ReturnToAutomationBeforeClose)
                        .into(),
                );
                if self.automation_ui.draft_dirty() && self.automation_ui.can_discard_draft() {
                    controls.push(
                        crate::ui_controls::button::destructive("Discard automation changes")
                            .id(widget::Id::new(TRANSIENT_CLOSE_DISCARD))
                            .on_press(Message::DiscardAutomationAndClose(
                                self.automation_close_generation,
                            ))
                            .into(),
                    );
                }
            }
            return Some(dialog_ui::overlay(
                dialog_ui::confirmation_surface(
                    dialog_ui::Question::CloseWithUnsavedWork,
                    widget::column::with_children(vec![
                        design::title("Keep unsaved work before closing").into(),
                        notice_detail(self.window_close_transient_blocker().unwrap_or(
                            "The pending work is complete. Cancel this dialog and close again.",
                        )),
                    ])
                    .spacing(space::M),
                    widget::row::with_children(controls)
                        .spacing(space::S)
                        .wrap(),
                    480.0,
                ),
                Some(Message::CancelTransientWorkClose),
                1.0,
            ));
        }
        if let Some(dialog) = self.automation_ui.draft_dialog() {
            return Some(dialog_ui::overlay(
                dialog.map(Message::AutomationUi),
                self.automation_ui
                    .keep_draft_action()
                    .map(Message::AutomationUi),
                1.0,
            ));
        }
        if self.device_ui.open {
            return Some(dialog_ui::overlay(
                self.device_dialog(),
                self.device_ui
                    .can_close()
                    .then_some(Message::Device(device_ui::Action::Close)),
                1.0,
            ));
        }
        if let Some(gallery) = &self.chat_ui.image_dialog {
            return Some(gallery.dialog_view());
        }
        if let Some(dialog) = self.terminal_paste_dialog() {
            return Some(dialog);
        }
        #[cfg(feature = "integrated-browser")]
        if let Some(dialog) = self.browser_import_dialog() {
            return Some(dialog);
        }
        if let Some(dialog) = self.session_recovery_dialog() {
            return Some(dialog);
        }
        if let Some(dialog) = self.daemon_health_dialog() {
            return Some(dialog);
        }
        if let Some(dialog) = self.update_dialog() {
            return Some(dialog);
        }
        if self.chat_close_dialog_visible() {
            return Some(self.chat_close_dialog());
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
        // Above the settings surface it is raised from, because it is about to
        // discard every shortcut, font, theme and language-server entry the
        // user configured, with no undo.
        if self.settings_reset_confirm {
            return Some(dialog_ui::overlay(
                dialog_ui::confirmation_surface(
                    dialog_ui::Question::ResetSettings,
                    widget::column::with_children(vec![
                        design::title("Reset all settings?").into(),
                        notice_body(
                            "Every customization returns to its default: keyboard shortcuts, \
                             terminal font and theme, editor syntax theme, interface scale, \
                             pane widths, and the configured language servers.",
                        ),
                        notice_detail("This cannot be undone."),
                    ])
                    .spacing(space::M),
                    dialog_ui::confirmation_actions(
                        crate::ui_controls::button::secondary("Cancel")
                            .id(widget::Id::new("dialog.reset-settings.cancel"))
                            .on_press(Message::CancelResetSettings),
                        crate::ui_controls::button::destructive("Reset settings")
                            .id(widget::Id::new("dialog.reset-settings.confirm"))
                            .on_press(Message::ConfirmResetSettings),
                    ),
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
            // The file at stake, not the repository the pane is rooted at.
            let title = runtime
                .selected_path()
                .map(|path| path.display().to_string())
                .or_else(|| self.card_state(card).map(|state| state.title.clone()))
                .unwrap_or_else(|| "Repository".into());
            let mut rows: Vec<Element<'_, Message>> = vec![
                design::title(if runtime.has_durability_warning() {
                    format!("Confirm the save to {title} before closing")
                } else {
                    format!("Save changes to {title}?")
                })
                .into(),
                design::body(if runtime.has_durability_warning() {
                    "The file was replaced on disk, but its durability is still unconfirmed. Retry the save, or cancel and reload from disk before closing; discarding editor changes cannot resolve this warning."
                } else if matches!(close.intent, DirtyCloseIntent::Channel(_)) {
                    "This editor has unsaved changes. Save them before the channel is deleted, discard them, or cancel to keep the channel."
                } else {
                    "This editor has unsaved changes. Save them before closing, discard them, or keep the repository open."
                })
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            ];
            if let Some(error) = runtime.save_error() {
                rows.push(
                    design::body(error.to_owned())
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
                crate::ui_controls::button::secondary("Cancel")
                    .id(dirty_close_control_id(card, DirtyCloseControl::Cancel))
                    .on_press_maybe(
                        (!runtime.is_saving()).then_some(Message::CancelRepositoryClose),
                    )
                    .into(),
            ];
            if runtime.can_abandon_unsaved_changes() {
                controls.push(
                    crate::ui_controls::button::destructive("Discard")
                        .id(dirty_close_control_id(card, DirtyCloseControl::Discard))
                        .on_press(Message::DiscardAndCloseRepository(card))
                        .into(),
                );
            }
            controls.push(
                crate::ui_controls::button::primary("Save and close")
                    .instead("Overwrite and close", runtime.has_save_conflict())
                    .instead("Retry save and close", runtime.has_durability_warning())
                    .id(dirty_close_control_id(card, DirtyCloseControl::Save))
                    .on_press_maybe(save_message)
                    .into(),
            );
            let dismiss = controls.remove(0);
            let actions = dialog_ui::confirmation_actions(
                dismiss,
                widget::row::with_children(controls).spacing(space::S),
            );
            // A long path or save error scrolls; the answers stay in reach.
            return Some(dialog_ui::overlay(
                dialog_ui::scrolling_surface_with_actions(
                    widget::container(widget::column::with_children(rows).spacing(space::M))
                        .id(widget::Id::new(format!("dialog.dirty-close.{card}"))),
                    actions,
                    560.0,
                    dialog_ui::MAX_HEIGHT,
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

        if let Some(dialog) = self.resume_dialog() {
            return Some(dialog);
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
            design::title("Create pull request").into(),
            design::caption(format!("{} · {context}", dialog.repository.gh_selector())).into(),
            widget::column::with_children(vec![
                design::caption("Title").into(),
                crate::ui_controls::field::text("Title", &dialog.title)
                    .id(pull_request_dialog_focus_id())
                    .on_input(Message::PullRequestTitleChanged)
                    .on_submit_maybe(
                        self.pull_request_submission_allowed()
                            .then_some(|_| Message::SubmitPullRequest),
                    )
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
            widget::column::with_children(vec![
                design::caption("Head branch").into(),
                crate::ui_controls::field::text("Head branch", &dialog.head)
                    .id(widget::Id::new("dialog.pull-request.head"))
                    .on_input(Message::PullRequestHeadChanged)
                    .on_submit_maybe(
                        self.pull_request_submission_allowed()
                            .then_some(|_| Message::SubmitPullRequest),
                    )
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
            widget::column::with_children(vec![
                design::caption("Base branch").into(),
                crate::ui_controls::field::text("Base branch", &dialog.base)
                    .id(widget::Id::new("dialog.pull-request.base"))
                    .on_input(Message::PullRequestBaseChanged)
                    .on_submit_maybe(
                        self.pull_request_submission_allowed()
                            .then_some(|_| Message::SubmitPullRequest),
                    )
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
            widget::column::with_children(vec![
                design::caption("Description").into(),
                crate::ui_controls::field::editor("Pull request description", &dialog.body.0)
                    .placeholder("Describe the change (Markdown supported)")
                    .height(160)
                    .on_action(Message::PullRequestBodyChanged)
                    .on_cursor(Message::PullRequestBodyCursor)
                    .into(),
            ])
            .spacing(space::XS)
            .into(),
            crate::ui_controls::field::checkbox(dialog.draft)
                .label("Create as draft")
                .on_toggle(Message::PullRequestDraftChanged)
                .into(),
        ];
        if let Some(confirmed) = dialog.retry_confirmation {
            rows.push(
                design::body(
                    "GitHub may have created this pull request. Check GitHub before retrying to avoid creating a duplicate.",
                )
                .into(),
            );
            rows.push(
                crate::ui_controls::field::checkbox(confirmed)
                    .label("I checked GitHub; no pull request was created")
                    .on_toggle(Message::PullRequestRetryConfirmed)
                    .into(),
            );
        }
        let actions = dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel").on_press(Message::CancelDialog),
            crate::ui_controls::button::primary("Create").on_press_maybe(
                self.pull_request_submission_allowed()
                    .then_some(Message::SubmitPullRequest),
            ),
        );
        Some(dialog_ui::overlay(
            dialog_ui::scrolling_surface_with_actions(
                widget::column::with_children(rows).spacing(space::M),
                actions,
                620.0,
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
        let _trace = crate::performance::ui_trace::scope(crate::performance::ui_trace::Phase::View);
        let sidebar_mode = self.projects_sidebar_mode();
        let sidebar = sidebar::view_cached(
            &self.sidebar_model,
            &self.sidebar_state,
            sidebar_mode,
            self.settings.sidebar_width,
            self.modal_is_open(),
            &self.navigation_cache,
            &self.settings.keyboard_shortcuts,
            !self.core.window.sharp_corners,
        )
        .map(Message::Sidebar);
        let sidebar = crate::performance::ui_trace::region("sidebar", sidebar);

        // The focused card's own name and path are the card header's job, and
        // the window's header bar already carries the name; see
        // `sync_window_title`.
        let has_multiple_panes = self.workspace.cards().len() > 1;

        let workspace: Element<'_, Message> = if self.workspace.cards().is_empty() {
            let directory = self
                .terminal_directory()
                .unwrap_or_else(|| STARTING_DIRECTORY.clone());
            let scoped = self.active_workspace.is_some() || self.durable.active_channel.is_some();
            let scope_name = self
                .durable
                .active_channel
                .and_then(|id| {
                    self.durable
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                        .map(|channel| channel.title.clone())
                })
                .or_else(|| {
                    directory
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "Workspace".into());
            let quick = |glyph, label, id, message| {
                Element::from(
                    crate::ui_controls::icon_button(glyph, label)
                        .on_press(message)
                        .id(widget::Id::new(id)),
                )
            };
            let mut actions: Vec<Element<'_, Message>> = vec![
                crate::interaction::tooltip(
                    crate::ui_controls::button::primary("New conversation")
                        .icon(Icon::Chat)
                        .on_press(Message::ChromeUi(chrome_ui::Action::Launch(
                            chrome_ui::NewTab::Thread,
                        ))),
                    workspace_widget::shortcut_tooltip(
                        workspace_widget::GlobalShortcut::NewThread,
                        &self.settings.keyboard_shortcuts,
                    ),
                ),
                quick(
                    Icon::History,
                    "Resume conversation".to_owned(),
                    "empty.resume-conversation",
                    Message::ResumeConversation(resume_ui::Action::Open),
                ),
                quick(
                    Icon::Terminal,
                    workspace_widget::shortcut_tooltip(
                        workspace_widget::GlobalShortcut::NewTerminal,
                        &self.settings.keyboard_shortcuts,
                    ),
                    "empty.new-terminal",
                    Message::NewTerminal,
                ),
                quick(
                    Icon::Browser,
                    if cfg!(feature = "integrated-browser") {
                        "New browser"
                    } else {
                        "Open in system browser"
                    }
                    .to_owned(),
                    "empty.new-browser",
                    Message::NewBrowser,
                ),
                quick(
                    Icon::Avatar,
                    "Agent accounts".to_owned(),
                    "empty.accounts",
                    Message::AccountUi(account_ui::Action::OpenPane),
                ),
            ];
            if !scoped {
                actions.pop();
            }
            let primary = actions.remove(0);
            let actions = widget::column::with_children(vec![
                primary,
                widget::row::with_children(actions)
                    .align_y(Alignment::Center)
                    .spacing(space::S)
                    .into(),
            ])
            .spacing(space::M);
            let mut content: Vec<Element<'_, Message>> = vec![
                crate::ui_controls::passive_icon(Icon::Chat, 32),
                design::display(if scoped {
                    scope_name
                } else {
                    "Workspace".into()
                })
                .into(),
                crate::interaction::tooltip(
                    crate::ui_controls::FadeText::caption(display_path(&directory).into_owned())
                        .edge(crate::ui_controls::FadeEdge::Start)
                        .width(Length::Fill),
                    directory.display().to_string(),
                ),
                actions.into(),
            ];
            if !scoped {
                content.push(
                    // The providers whose accounts the Accounts pane shows.
                    widget::row::with_children(
                        crate::agent::Provider::ALL
                            .into_iter()
                            .filter(|provider| crate::account::Account::REPORTED.contains(provider))
                            .map(|provider| {
                                let status = self.account_ui.provider_status(provider);
                                let control = crate::ui_controls::icon_button(
                                    crate::ui_controls::provider_icon(provider),
                                    format!("{} · {status} · Manage accounts", provider.label()),
                                )
                                .on_press(Message::AccountUi(account_ui::Action::OpenPane));
                                Element::from(if status == "Account available" {
                                    control.tone(design::Tone::Themed(design::success_color))
                                } else {
                                    control
                                })
                            })
                            .collect::<Vec<_>>(),
                    )
                    .spacing(space::M)
                    .into(),
                );
            }
            let surface =
                widget::container(widget::column::with_children(content).spacing(space::L))
                    .padding(space::XL)
                    .width(Length::Fill)
                    .max_width(440);
            widget::container(surface)
                .center(Length::Fill)
                .padding(space::XL)
                .into()
        } else {
            workspace_widget::view(
                &self.workspace,
                &self.settings.keyboard_shortcuts,
                self.focused_pane(),
                self.settings.focus_follows_mouse,
                &self.input_trace,
                self.workspace.viewport_width(),
                self.workspace_scroll_id.clone(),
                move |context| {
                    self.workspace_card_view(context.id, context.focused, has_multiple_panes)
                },
                // `sync_chrome` keeps an entry for every durable card the
                // strip holds, so the fallback is only reachable for a card
                // the workspace holds but the durable model does not.
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

        // The strip keeps its scrollbars and trailing controls out of the
        // window's resize border, as the rightmost surface.
        let workspace: Element<'_, Message> = widget::container(workspace)
            .padding(Padding::ZERO.right(self.trailing_edge_inset()))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        let workspace = crate::performance::ui_trace::region("workspace", workspace);
        let main_content = self.workspace_chrome(workspace);
        let surface: Element<'_, Message> = widget::row::with_children(vec![sidebar, main_content])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        // The bar comes and goes, but the tree around the workspace must not:
        // iced matches widget state by position, so wrapping the surface only
        // while the bar shows would reset every scroll offset and focus in it.
        let bar = self
            .status_bar()
            .unwrap_or_else(|| iced::widget::Space::new().height(Length::Shrink).into());
        let surface: Element<'_, Message> = widget::column::with_children(vec![
            surface,
            self.operation_recovery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .view()
                .map(Message::OperationRecovery),
            self.daemon_health_banner(),
            self.update_resume_banner(),
            self.automation_approval_banner(),
            bar,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        // The workspace switcher places itself, over a scrim of its own.
        let switcher = self
            .palette_surface()
            .or_else(|| self.pane_rename_surface())
            .map(|surface| {
                widget::container(surface)
                    .align_x(Alignment::Center)
                    .padding(2.0 * design::HEADER_HEIGHT)
                    .width(Length::Fill)
                    .into()
            })
            .or_else(|| self.workspace_switcher())
            .unwrap_or_else(|| iced::widget::Space::new().into());
        let surface = crate::interaction::layer(surface, switcher);
        let surface = crate::interaction::drive_animation(
            surface,
            self.workspace.is_animating() || self.pane_inspectors.is_animating(),
        );
        let surface = crate::input_trace::observe(surface, &self.input_trace);
        let surface = crate::file_drop::surface(surface, self.core.scale_factor());
        let surface = crate::reorder::channel_scope(
            surface,
            widget::Id::new("workspace.tabs"),
            self.workspace_view_generation,
            !self.workspace_inputs_blocked(),
            |card, channel| {
                self.workspace.order().any(|id| id == card)
                    && self.can_move_pane_to_channel(card, channel)
            },
            |card, channel| Message::PaneDropToChannel {
                generation: self.workspace_view_generation,
                card,
                channel,
            },
        );
        let surface = crate::performance::ui_trace::root(surface);
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

    fn update(&mut self, message: Message) -> Task<Message> {
        let _trace = crate::performance::ui_trace::update(&message);
        // Include dispatch and the shared projection tail. The trace is opt-in;
        // an ordinary update does not read the clock or allocate.
        let _capture_parent = SnapshotCaptureParent::enter(&message);
        let timer_update = matches!(&message, Message::ReplicaCaptureDue(_));
        let _workspace_timing = ((matches!(&message, Message::WorkspaceModelObserved(_))
            || timer_update)
            && tracing::enabled!(target: "cagents::workspace_pipeline", tracing::Level::DEBUG))
        .then(|| {
            WorkspaceUpdateTiming(
                std::time::Instant::now(),
                if timer_update {
                    "replica_snapshot_timer"
                } else {
                    "ui_update"
                },
            )
        });
        #[cfg(any(test, feature = "development"))]
        let (_running, settled_projection) = self.settle_projection(&message);
        #[cfg(any(test, feature = "development"))]
        let oracle_message =
            projection_oracle::enabled().then(|| crate::performance::ui_trace::label(&message));
        #[cfg(not(any(test, feature = "development")))]
        let settled_projection = Task::none();
        let notice_control = self.notice_control_severity();
        self.reconcile_terminal_review_owner();
        self.cancel_stale_pr_creations();
        if let Some(notice) = crate::diagnostics::take_notice() {
            self.notices.record(TransientNotice::warning(notice));
        }
        // An update can run inside another's admission, before that one's
        // handler: its postlude answers only for its own messages.
        let enclosing_scope_changed = std::mem::take(&mut self.attention_scope_changed);
        self.dispatching += 1;
        let task = self.dispatch(message);
        self.dispatching -= 1;
        let task = self.finish_update(
            task,
            UpdateEntry {
                notice_control,
                timed: _workspace_timing.is_some(),
                settled_projection,
                #[cfg(any(test, feature = "development"))]
                oracle_message,
            },
        );
        self.attention_scope_changed = enclosing_scope_changed;
        task
    }
}

impl Cagents {
    /// Settles what the window owes before any message is handled, whether
    /// it is an update's own or one a handler dispatches within it, then
    /// holds the message to the gates every message passes ([`Self::hold`]).
    /// `Continue` carries the admitted message with what settling started;
    /// `Break` carries everything a refused message comes to.
    fn admit(&mut self, message: Message) -> ControlFlow<Task<Message>, (Message, Task<Message>)> {
        let mut settled = Vec::new();
        if let Some(task) = self.settle_chat_close_confirm() {
            // The question has gone and the close queue has moved on, which
            // the chrome shows.
            self.owe_projection(OwedProjection::full());
            // An answer to the question that just ended must not answer the
            // next one, which the reader has not seen.
            if matches!(
                message,
                Message::ConfirmChatClose | Message::CancelChatClose
            ) {
                return ControlFlow::Break(task);
            }
            settled.push(task);
        }
        if !self.pending_repository_reconciliation.is_empty()
            && !defers_repository_reconciliation(&message)
        {
            // Only a run of background repository loads waits for one
            // reconciliation; every other message sees it done, in order.
            // A whole update rather than a dispatch, so the message finds
            // the reconciled workspace already projected, as it did when
            // the reconciliation's own message came first.
            settled.push(self.update(Message::RepositoryReconciliationDue));
        }
        match self.hold(message) {
            ControlFlow::Continue(message) => {
                ControlFlow::Continue((message, Task::batch(settled)))
            }
            ControlFlow::Break(refused) => {
                settled.push(refused);
                ControlFlow::Break(Task::batch(settled))
            }
        }
    }

    /// Holds `message` to the gates every message passes before its handler
    /// runs: a remote device refusing local work, an update being prepared,
    /// a dialog or the tab chrome holding the workspace, a save in flight,
    /// and navigation a dialog defers. `Break` carries what a refused
    /// message comes to. On the way it notes the focus in the tab history
    /// and ends a workspace preview the message takes the keyboard from.
    fn hold(&mut self, message: Message) -> ControlFlow<Task<Message>, Message> {
        if crate::device::is_remote() {
            match &message {
                Message::GitUi(_)
                | Message::ProjectUi(
                    project_ui::Action::EditorsLoaded(_) | project_ui::Action::AvatarLoaded { .. },
                ) => return ControlFlow::Break(Task::none()),
                Message::ChatUi(
                    chat_ui::Action::Attach(_)
                    | chat_ui::Action::Attached(..)
                    | chat_ui::Action::Dropped(..),
                )
                | Message::TerminalDropped { .. } => {
                    // The refusal answers the `+` menu's choice as a chooser would.
                    if let Message::ChatUi(chat_ui::Action::Attach(target)) = &message {
                        self.close_composer_actions(*target);
                    }
                    self.status = Some(TransientNotice::warning(
                        "File attachments require transfer to the selected device; use files already on that device",
                    ));
                    return ControlFlow::Break(Task::none());
                }
                Message::OpenRepository => {
                    return ControlFlow::Break(self.update_device(device_ui::Action::Open));
                }
                Message::PreviewLocalFile(_) => {
                    return ControlFlow::Break(self.update_device(device_ui::Action::Open));
                }
                Message::ResumeConversation(_)
                | Message::Repository(_)
                | Message::ProjectUi(_)
                | Message::SubmitWorktree => {
                    self.status = Some(TransientNotice::warning(
                        "Use Device files for remote files and Git operations",
                    ));
                    return ControlFlow::Break(Task::none());
                }
                Message::Sidebar(action) if !device_ui::supports_sidebar_action(action) => {
                    return ControlFlow::Break(self.update_device(device_ui::Action::Open));
                }
                _ => {}
            }
        }
        self.tab_history.observe(self.workspace.focused());
        // A workspace opened some other way while the switcher is held, by a
        // click on one of its rows or in the sidebar, is the choice. Anything
        // that takes the keyboard from the switcher ends it too: the release
        // that would have closed it may never be delivered.
        if self.chrome_ui.workspace_preview.is_some()
            && (matches!(&message, Message::Sidebar(action) if workspace_destination(action).is_some())
                || self.update_prepared
                || self.workspace_inputs_blocked()
                || self.chat_ui.image_dialog.is_some())
        {
            self.cancel_workspace_preview();
        }
        // Freeze user input, not lifecycle completions. Dropping a completed
        // save/Git/spawn result would strand that operation if another window vetoed.
        if self.update_prepared && user_message_blocked_during_update(&message) {
            // Nothing else would ever deliver this removal.
            if let Message::CommitClose(card) = message {
                self.abandon_removal(card);
                self.status = Some(TransientNotice::warning(
                    "An update is being prepared, so the pane was kept. Close it again if the update doesn't go ahead.",
                ));
            }
            return ControlFlow::Break(Task::none());
        }
        if self.modal_is_open() {
            self.chrome_ui.close();
        } else if self.chrome_ui.is_open()
            && !matches!(&message, Message::ChromeUi(_))
            && user_workspace_message_blocked_while_modal(&message)
        {
            return ControlFlow::Break(Task::none());
        }
        if self.modal_is_open() && user_workspace_message_blocked_while_modal(&message) {
            return ControlFlow::Break(Task::none());
        }
        if self.focused_repository_is_saving() && workspace_navigation_blocked_during_save(&message)
        {
            return ControlFlow::Break(Task::none());
        }
        if (self.focused_repository_is_saving() || self.modal_is_open())
            && completion_navigates(&message)
        {
            self.deferred_navigation.push_back(message);
            return ControlFlow::Break(Task::none());
        }
        ControlFlow::Continue(message)
    }

    /// Runs `message`'s handler once [`Self::admit`] lets it through, and
    /// adds the projection the handler declares to `owed_projection` rather
    /// than projecting. `update` dispatches its own message this way and then
    /// projects what is owed, once. A handler that acts by way of another
    /// message dispatches that message the same way, not through an `update`
    /// of its own, so one input projects once however many messages it comes
    /// to: a shortcut and the focus move it stands for share one projection.
    ///
    /// Outside a handler, where no update would project what it owes, as
    /// when a test drives a helper directly, it runs a whole update instead.
    fn dispatch(&mut self, message: Message) -> Task<Message> {
        if self.dispatching == 0 {
            return self.update(message);
        }
        // Noted before admission, as a message held back still arrived.
        self.attention_scope_changed |= changes_attention_scope(&message);
        let (message, settled) = match self.admit(message) {
            ControlFlow::Continue(admitted) => admitted,
            ControlFlow::Break(refused) => return refused,
        };
        if !self.workspace.is_animating() && !self.pane_inspectors.is_animating() {
            // Whatever this message starts is measured from now. Its first
            // frame then moves it by the time since the input that began it,
            // not by the time spent idle while the window-frame subscription
            // was disconnected, and not by nothing at all.
            self.frame_clock.start(Instant::now());
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
        let reconcile_row_selection = !matches!(
            &message,
            Message::Sidebar(SidebarAction::ScrollProjects(_) | SidebarAction::NavigateRow { .. })
        ) && matches!(
            &message,
            Message::RepositoryLoaded { .. }
                | Message::AttachedWorktreePrsResolved { .. }
                | Message::GitUi(_)
                | Message::WorkspaceModelObserved(_)
                | Message::ProjectUi(_)
                | Message::ChannelUi(_)
                | Message::Sidebar(_)
                | Message::FoldInProgress { .. }
                | Message::FocusCard(_)
                | Message::TerminalReviewMode { .. }
                | Message::FocusAdjacent(_)
                | Message::WindowResized(..)
        );
        let model_update = matches!(&message, Message::WorkspaceModelObserved(_));
        let mut projection = if repaint_tick(&message)
            || background_poll(&message)
            || matches!(message, Message::ReplicaCaptureDue(_))
            || self.navigation_save_is_stale(&message)
            || (matches!(message, Message::RepositoryReconciliationDue)
                && self.pending_repository_reconciliation.is_empty())
        {
            Projection::Unchanged
        } else {
            Projection::Full
        };
        // A handler that returns early, as nothing derived changed, owes no
        // projection.
        #[rustfmt::skip]
        let task = (|| {
        let task = match message {
            Message::TerminalFailure(action) => {
                if action.only_touches_probes() && !self.update_tail_may_close_or_commit() {
                    projection = Projection::TranscriptProbes;
                }
                self.update_terminal_failure(action)
            }
            Message::TerminalSave(action) => self.update_terminal_save(action),
            Message::SavedTerminal(action) => self.update_saved_terminal(action),
            Message::Device(action) => self.update_device(action),
            Message::Palette(action) => self.update_palette(action),
            Message::RunCommand(command) => {
                self.run_workspace_command(command, keyboard::Modifiers::empty())
            }
            Message::NewTerminal => self.add_terminal(
                self.terminal_directory()
                    .unwrap_or_else(|| STARTING_DIRECTORY.clone()),
            ),
            Message::TerminalHandoffsCommitted { result } => {
                self.terminal_handoff_commit_inflight = false;
                let notice = match result {
                    Ok(results) => {
                        let mut failures = Vec::new();
                        for (terminal, result) in results {
                            // The flush came first, so these are saved even
                            // when the daemon has yet to take the receipt.
                            if let Some(card) = self.unsaved_chat_returns.remove(&terminal.id) {
                                self.resolve_notice(NoticeKey::ChatReturn(card));
                            }
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
                            terminal_handoff_pending_notice(format!(
                                "{} terminal(s) remain pending; terminal {id}: {error}",
                                failures.len()
                            ))
                        })
                    }
                    Err(error) => Some(match self.unsaved_chat_return() {
                        Some(card) => {
                            tracing::warn!(%error, "could not save a return to terminal");
                            chat_return_unsaved_notice(card)
                        }
                        None => terminal_handoff_pending_notice(error),
                    }),
                };
                if let Some(notice) = notice {
                    self.status = Some(notice);
                    Task::none()
                } else {
                    self.commit_terminal_handoffs()
                }
            }
            Message::HostedTerminalsObserved(terminals) => {
                self.observe_hosted_descriptors(terminals)
            }
            Message::WorkspaceModelObserved(update) => {
                let (task, model_projection) = self.apply_workspace_model_update_projection(update);
                projection = model_projection;
                task
            }
            Message::TerminalStarted {
                card,
                terminal,
                result,
            } => self.terminal_started(card, terminal, result),
            Message::RetainedTerminalAttached {
                card,
                terminal,
                result,
            } => self.retained_terminal_attached(card, terminal, result),
            Message::RestoredAgentAttached {
                card,
                terminal,
                result,
            } => {
                match result {
                    Ok(Some(started)) => self.terminal_started(card, terminal, Ok(started)),
                    // Nobody is looking at this card, so its failure is not
                    // shown here. Opening it starts the terminal again and
                    // reports whatever still stands in the way; a startup gate
                    // reports it rather than waiting for that.
                    result => {
                        if self.card_state(card).is_some_and(|state| matches!(&state.kind, CardKind::Terminal(session) if session.id == terminal)) {
                            self.startup_gate.initial_panes.settled(card, terminal);
                        }
                        let reason = result.err().unwrap_or_else(|| {
                            "the terminal service no longer hosts it; open it to relaunch".into()
                        });
                        self.startup_attach_failed(card, terminal, reason);
                        self.finish_terminal_start(card)
                    }
                }
            }
            Message::TerminalWake { card, generation } => {
                tracing::trace!(target: "cagents::pane_open", stage = "wake", ?card, generation);
                let started =
                    tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::DEBUG)
                        .then(Instant::now);
                let (task, wake_projection) = self.drain_terminal_update(card, generation);
                projection = wake_projection;
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
                if self.ended_terminal(card).is_some()
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
            Message::BrowserError { card, message: _ } => {
                self.status = Some(TransientNotice::error(format!(
                    "Browser card {card}: a browser operation failed"
                )));
                Task::none()
            }
            Message::ExternalUrlOpened(result) => {
                self.status = Some(match result {
                    Ok(()) => TransientNotice::success("Opened in the system browser"),
                    Err(error) => TransientNotice::error(error),
                });
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFallbackOpened { result, reason } => {
                self.status = Some(match result {
                    Ok(()) => TransientNotice::warning(
                        "Opened in your system browser because the integrated browser wasn't available",
                    )
                    .with_details(reason.to_string()),
                    Err(error) => TransientNotice::error(error).with_details(reason.to_string()),
                });
                Task::none()
            }
            Message::FileRevealed(result) => {
                self.status = Some(match result {
                    Ok(file_manager::RevealOutcome::Selected) => {
                        TransientNotice::success("Revealed in file manager")
                    }
                    Ok(file_manager::RevealOutcome::OpenedContainingFolder) => {
                        TransientNotice::success("Opened containing folder in file manager")
                    }
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
            Message::DeviceProjectLoaded {
                project_id,
                generation,
                result,
            } => self.device_project_loaded(project_id, generation, result),
            Message::RepositoryLoaded {
                open_cards,
                project_id,
                generation,
                result,
            } => handled!(self.repository_loaded(open_cards, project_id, generation, result)),
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
                    let merged = recent.into_iter().chain(archived).collect::<Vec<_>>();
                    return self.cleanup_merged_worktree_task(project_id, &worktrees, &merged);
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
            Message::Repository(RepositoryAction::Language { card, action }) => {
                self.update_editor_lsp(card, action)
            }
            Message::Repository(RepositoryAction::OpenExternal { card, editor_id }) => {
                let target = match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) if runtime.can_open_external() => {
                        runtime.selected_path().map(|path| {
                            (
                                runtime.descriptor.worktree.join(path),
                                runtime.external_caret(),
                            )
                        })
                    }
                    _ => None,
                };
                target.map_or_else(Task::none, |(path, caret)| {
                    self.open_file_in_editor(path, caret, editor_id)
                })
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
                self.dispatch(Message::Sidebar(if stage {
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
            Message::Repository(RepositoryAction::ZoomFont { card, delta }) => {
                self.dispatch(Message::ZoomFont {
                    card,
                    delta,
                    editor: true,
                })
            }
            Message::ZoomFont {
                card,
                delta,
                editor,
            } => {
                if self.workspace.focused() != Some(card) {
                    return Task::none();
                }
                let current = if editor {
                    self.settings.editor_font_size()
                } else {
                    self.settings.terminal_font_size
                };
                let value = if delta == 0 {
                    if editor {
                        String::new()
                    } else {
                        AppSettings::default().terminal_font_size.to_string()
                    }
                } else {
                    (current + f32::from(delta)).clamp(8.0, 40.0).to_string()
                };
                self.dispatch(Message::SettingsChanged {
                    field: if editor {
                        SettingsField::EditorFontSize
                    } else {
                        SettingsField::TerminalFontSize
                    },
                    value,
                })
            }
            Message::OperationRecovery(action) => {
                let task = self
                    .operation_recovery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .update(action);

                let mut tasks = vec![
                    task.map(Message::OperationRecovery)
                        .map(cosmic::Action::App),
                ];
                for (card, runtime) in &mut self.cards {
                    if let CardRuntime::Repository(runtime) = runtime {
                        tasks.push(repository_task(runtime.reconcile_operation_recovery(*card)));
                    }
                }
                // Recovery can release a clean editor's last close barrier
                // without producing another repository event.
                if !self.project_ui.closing.is_empty() {
                    tasks.push(self.complete_closed_projects());
                }
                Task::batch(tasks)
            }
            Message::Repository(action) => handled!(self.update_repository(action)),
            Message::Sidebar(action) => handled!(self.update_sidebar(action)),
            Message::FoldInProgress { refocus } => {
                if self.sidebar_state.fold_in_progress() && refocus {
                    sidebar::focus_and_reveal(sidebar::in_progress_focus_id())
                        .map(cosmic::Action::App)
                } else {
                    Task::none()
                }
            }
            Message::AcknowledgeVisiblePane { card, epoch } => {
                let due = epoch == self.attention_focus_epoch && self.pane_is_active(card);
                // A dwell that marks nothing read records only that it ended,
                // which only the notification presence reads.
                if (!due || self.visible_dwell_reads_nothing())
                    && !self.update_tail_may_close_or_commit()
                {
                    projection = Projection::Unchanged;
                }
                if due { self.acknowledge_visible_pane() } else { Task::none() }
            }
            Message::FocusCard(id) => {
                // Explicit activation also cancels a pending entry on this same
                // card, even if no view has observed its acknowledgment yet.
                self.set_terminal_review_owner(None);
                self.focus_workspace_card(id)
            }
            Message::MovePaneToChannel { card, channel } => {
                self.move_pane_to_channel(card, channel)
            }
            Message::PaneDropToChannel {
                generation,
                card,
                channel,
            } => {
                if generation == self.workspace_view_generation {
                    self.move_pane_to_channel(card, channel)
                } else {
                    Task::none()
                }
            }
            Message::TerminalReviewMode {
                card,
                terminal,
                active,
            } => {
                // Queued requests belong to their originating terminal, not a
                // successor mounted in the same card while the task was pending.
                if !matches!(self.cards.get(&card), Some(CardRuntime::Terminal(handle)) if handle.terminal_id() == terminal)
                {
                    return Task::none();
                }
                if active {
                    self.set_terminal_review_owner(Some(card));
                    self.focus_workspace_card(card)
                } else {
                    if self.terminal_review_card == Some(card) {
                        self.set_terminal_review_owner(None);
                    }
                    Task::none()
                }
            }
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
                        self.apply_viewport(update, true, DurableChange::None)
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
                let Some(width) = self.workspace.next_width_preset(card) else {
                    return Task::none();
                };
                match self.workspace.animate_card_width(card, width) {
                    Ok(update) => {
                        if let Some(state) = self.card_state_mut(card)
                            && let CardWidth::Fraction(fraction) = width
                        {
                            state.width_fraction = fraction;
                        }
                        self.apply_viewport(update, true, DurableChange::Content)
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.to_string()));
                        Task::none()
                    }
                }
            }
            Message::ResizeCardFinished(card) => {
                let update = self.workspace.finish_card_resize(card);
                if self.card_state(card).is_some() {
                    self.persist();
                }
                self.apply_viewport(update, true, DurableChange::None)
            }
            Message::CloseCard(_) if self.dirty_repository_close.is_some() => Task::none(),
            Message::CloseCard(id) if matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation()) => {
                self.begin_dirty_close(DirtyCloseIntent::Card(id), VecDeque::from([id]))
            }
            Message::UpdateUi(action) => self.update_update_ui(action),
            Message::UpdateResume(action) => self.update_update_resume(action),
            Message::StartupWindowOpened(window) => {
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                // The workspace is on screen: a daemon startup notice has
                // nothing left to explain.
                instance::dismiss_startup_wait_notice();
                let report = self.begin_startup_gate();
                // window::run silently drops callbacks for windows that have not
                // opened yet. Initialize only after iced acknowledges this one.
                #[cfg(feature = "integrated-browser")]
                let report = Task::batch([report, self.browser_factory_task()]);
                report
            }
            Message::VerifyStartupPanes => self.verify_startup_gate(),
            Message::StartupStateInspected(state) => self.startup_state_inspected(state),
            Message::StartupPanesSaved(settings, saved) => {
                self.startup_panes_saved(settings, saved)
            }
            Message::StartupReported(result) => match result {
                Ok(()) => Task::none(),
                Err(error) => {
                    tracing::error!(%error, "could not acknowledge UI startup");
                    if self.startup_gate.window_opened() {
                        // The updater may have timed out and retained this
                        // usable recovery window while closing its pipe.
                        self.startup_report_failed();
                        self.status = Some(TransientNotice::warning(format!(
                            "The updater could not verify this window's recovery: {error}. Your chats and drafts remain available; verify them and save before updating again."
                        )));
                        return Task::none();
                    }
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
            Message::SharedPreferencesObserved { generation, settings } => {
                // A local edit owns these fields until its save is acknowledged.
                // A read queued by an earlier subscription can arrive after that
                // acknowledgment; only the current save generation may apply.
                if generation == self.settings_ack_generation
                    && self.settings_save_generation == self.settings_ack_generation
                {
                    // Never wait for a disk save on the event loop. An in-flight
                    // save's acknowledgment restarts this observation.
                    let Ok(mut lane) = self.settings_save_lane.try_lock() else { return Task::none(); };
                    let adopt = |into: &mut AppSettings| {
                        into.sidebar_worktrees = settings.sidebar_worktrees;
                        into.sidebar_worktree_overrides = settings.sidebar_worktree_overrides.clone();
                        into.account_usage_display = settings.account_usage_display;
                        into.confirm_terminal_close = settings.confirm_terminal_close;
                        into.confirmations = settings.confirmations.clone();
                        into.sidebar_worktrees_when_shown = settings.sidebar_worktrees_when_shown;
                    };
                    if let Some(saved) = lane.as_mut() {
                        adopt(&mut saved.settings);
                    }
                    drop(lane);
                    adopt(&mut self.settings);
                    adopt(&mut self.settings_base);
                    self.settings_draft.sidebar_worktrees = settings.sidebar_worktrees;
                    self.settings_draft.account_usage_display = settings.account_usage_display;
                    for kind in settings::Confirmation::ALL {
                        self.settings_draft.confirmations.insert(kind, settings.asks(kind));
                    }
                    self.sidebar_state.set_sidebar_worktrees(settings.sidebar_worktrees);
                    self.sidebar_state.set_worktree_overrides(settings.sidebar_worktree_overrides);
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
                // libcosmic also reports auxiliary surface teardown here.
                // Closing a popup must not begin main-window shutdown.
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                // libcosmic reports the closed window through this same
                // request, and a second close must not reopen the exit.
                if self.exit.is_some() || self.dirty_repository_close.is_some() {
                    return Task::none();
                }
                if self.window_close_transient_blocker().is_some() {
                    self.automation_close_generation =
                        self.automation_close_generation.wrapping_add(1);
                    self.transient_work_close = Some(window);
                    return dialog_ui::Question::CloseWithUnsavedWork.focus(
                        widget::Id::new(TRANSIENT_CLOSE_CANCEL),
                        widget::Id::new(TRANSIENT_CLOSE_DISCARD),
                    );
                }
                self.transient_work_close = None;
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
            Message::CancelTransientWorkClose => {
                self.transient_work_close = None;
                let target = if self.automation_ui.draft_dialog_open() {
                    Some(crate::automation_ui::draft_dialog_focus_id())
                } else if self.automation_ui.draft_close_blocker().is_some() {
                    Some(crate::automation_ui::draft_editor_focus_id())
                } else if self.device_ui.open && self.device_ui.dirty() {
                    Some(device_ui::file_editor_id())
                } else {
                    self.nested_modal_focus_target().or_else(|| {
                        self.workspace
                            .focused()
                            .map(|card| self.card_keyboard_focus(card))
                    })
                };
                target.map_or_else(Task::none, |target| {
                    iced::widget::operation::focus(target).map(cosmic::Action::App)
                })
            }
            Message::ReturnToAutomationBeforeClose => {
                if self.transient_work_close.is_none()
                    || self.automation_ui.draft_close_blocker().is_none()
                {
                    return Task::none();
                }
                self.transient_work_close = None;
                if let Some(action) = self.automation_ui.keep_draft_action() {
                    self.automation_ui.update(action);
                }
                Task::batch([
                    self.open_automations(),
                    iced::widget::operation::focus(crate::automation_ui::draft_editor_focus_id())
                        .map(cosmic::Action::App),
                ])
            }
            Message::DiscardAutomationAndClose(generation) => {
                if generation != self.automation_close_generation {
                    return Task::none();
                }
                let Some(window) = self.transient_work_close else {
                    return Task::none();
                };
                if self.automation_ui.draft_close_blocker().is_none()
                    || !self.automation_ui.discard_draft_before_close()
                {
                    return Task::none();
                }
                self.transient_work_close = None;
                cosmic::task::message(Message::WindowCloseRequested(window))
            }
            Message::ReturnToDeviceBeforeClose => {
                if self.transient_work_close.is_none() || !self.device_ui.dirty() {
                    return Task::none();
                }
                self.transient_work_close = None;
                if let Some(action) = self.automation_ui.keep_draft_action() {
                    self.automation_ui.update(action);
                }
                self.device_ui.open = true;
                Task::batch([
                    self.update_device(device_ui::Action::BackToFile),
                    iced::widget::operation::focus(device_ui::file_editor_id())
                        .map(cosmic::Action::App),
                ])
            }
            Message::CloseCard(id)
                if self
                    .card_state(id)
                    .is_some_and(|state| matches!(state.kind, CardKind::Chat(_)))
                    || self.chat_has_parked_runtime(id) =>
            {
                self.request_chat_close(id)
            }
            Message::CloseCard(id) => self.begin_card_close(id),
            Message::CloseCardUnasked(id) => self.close_card_unasked(id),
            Message::ConfirmTerminalClose => self.confirm_terminal_close(),
            Message::CancelTerminalClose => self.cancel_terminal_close(),
            Message::ConfirmChatClose => self.confirm_chat_close(),
            Message::CancelChatClose => self.cancel_chat_close(),
            Message::DontAskAgain(confirmation, ticked) => {
                if ticked {
                    self.dont_ask_again.insert(confirmation);
                } else {
                    self.dont_ask_again.remove(&confirmation);
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
            action @ (Message::SaveAndCloseRepository(id) | Message::OverwriteAndCloseRepository(id)) => {
                if self.dirty_repository_close.as_ref().is_none_or(|close| close.card != id) {
                    return Task::none();
                }
                if !self.prepare_editor_save(id) {
                    return Task::none();
                }
                match self.cards.get_mut(&id) {
                    Some(CardRuntime::Repository(runtime)) => repository_task(runtime.request_save(id, matches!(action, Message::OverwriteAndCloseRepository(_)))),
                    _ => Task::none(),
                }
            }
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
                // A fenced pane's close already asked nothing of its session;
                // routing it back through a close would request it again.
                if self.shared_views.quarantined.contains(&id) {
                    if !self
                        .card_state(id)
                        .is_some_and(|card| card.existence.is_open())
                    {
                        return Task::none();
                    }
                    // Recorded first: committing the close publishes it.
                    self.shared_views.fenced_closes.insert(id);
                    let closed = self.commit_card_removal(id, None);
                    if self
                        .card_state(id)
                        .is_none_or(|card| card.existence.is_open())
                    {
                        self.shared_views.fenced_closes.remove(&id);
                    }
                    // Projected after this update, as any closed pane is.
                    closed.unwrap_or_else(Task::none)
                } else {
                    if self
                        .workspace
                        .pending_close()
                        .is_some_and(|pending| pending.unmounting == id)
                    {
                        self.close_commit_delivered = Some(id);
                    }
                    self.commit_requested_close(id)
                }
            }
            // Moving a pane renumbers the tabs and the cards' order, which
            // the projection after it derives from.
            Message::Viewport {
                generation,
                event: WorkspaceViewportEvent::Reordered { card, before },
            } if generation == self.workspace_view_generation => self.reorder_pane(card, before),
            Message::Viewport { generation, event } => {
                // Native callbacks already queued by an unmounted worktree
                // must never mutate the newly selected strip.
                if generation != self.workspace_view_generation {
                    return Task::none();
                }
                if matches!(event, WorkspaceViewportEvent::DragScrolled(_))
                    && self.workspace_inputs_blocked()
                {
                    return Task::none();
                }
                if matches!(
                    event,
                    WorkspaceViewportEvent::PointerFocused { .. }
                        | WorkspaceViewportEvent::PointerEntered(_)
                ) && self.workspace_inputs_blocked()
                {
                    return Task::none();
                }
                if matches!(event, WorkspaceViewportEvent::PointerEntered(_))
                    && (!self.settings.focus_follows_mouse
                        || self.chat_menu_open()
                        || self.device_ui.menu_open)
                {
                    return Task::none();
                }
                let previous_width = self.workspace.viewport_width();
                let (update, synchronize_scroll) =
                    update_workspace_viewport(&mut self.workspace, event);
                // The pointer passing over a pane on its way elsewhere focuses
                // it, but has not read it: its update is read once it stays
                // visible for the attention dwell, like any other pane.
                let read = if matches!(event, WorkspaceViewportEvent::PointerEntered(_)) {
                    FocusRead::AfterDwell
                } else {
                    FocusRead::Now
                };
                let task =
                    self.apply_viewport_reading(update, false, DurableChange::None, read);
                let keyboard = match event {
                    WorkspaceViewportEvent::PointerEntered(card)
                        if self.workspace.focused() == Some(card) =>
                    {
                        self.focus_pane_keyboard_after_entry(card)
                    }
                    // Another button's press, such as one opening a context
                    // menu over selected text, leaves the keyboard alone.
                    WorkspaceViewportEvent::PointerFocused {
                        card,
                        primary: true,
                    } if self.workspace.focused() == Some(card) =>
                    {
                        self.focus_pane_keyboard_after_press(card)
                    }
                    _ => Task::none(),
                };
                let task = Task::batch([task, keyboard]);
                // Both native Scrolled feedback and Resized can measure the
                // narrower strip first. Reveal its active tab independently
                // of focus activation and attention acknowledgement.
                let tabs = if previous_width != self.workspace.viewport_width() {
                    self.reveal_active_tab()
                } else {
                    Task::none()
                };
                if synchronize_scroll {
                    Task::batch([task, tabs, self.synchronize_workspace_scroll()])
                } else {
                    Task::batch([task, tabs])
                }
            }
            Message::Frame(frame) => {
                let inspector_task = self.advance_pane_inspector_transitions(frame);
                let Some(elapsed) = self.frame_clock.elapsed(frame) else {
                    let deferred = self.release_deferred_navigation_if_unblocked();
                    return Task::batch([inspector_task, deferred]);
                };
                if self.focused_repository_is_saving() {
                    return inspector_task;
                }
                let update = self.workspace.tick(elapsed);
                Task::batch([inspector_task, self.apply_viewport(update, true, DurableChange::None)])
            }
            Message::LifecycleSweep => {
                let (task, sweep_projection) = self.lifecycle_sweep_update();
                projection = self.sweep_projection(sweep_projection);
                task
            }
            Message::LifecycleSweepElapsed(generation) => {
                let (task, sweep_projection) = self.lifecycle_sweep_elapsed(generation);
                projection = self.sweep_projection(sweep_projection);
                task
            }
            Message::WorkspaceSwitcherSelected { since, index } => {
                if self.chrome_ui.workspace_preview_since == Some(since) {
                    self.select_workspace_preview(index)
                } else {
                    Task::none()
                }
            }
            Message::WorkspaceSwitcherActivate { since, index } => {
                if self.chrome_ui.workspace_preview_since != Some(since) {
                    return Task::none();
                }
                self.open_workspace_row(index)
            }
            Message::WorkspaceSwitcherReleased(since) => {
                if self.chrome_ui.workspace_preview_since == Some(since) {
                    self.commit_workspace_preview()
                } else {
                    Task::none()
                }
            }
            Message::ReplicaCaptureDue(generation) => {
                self.replica_capture_due(generation, Instant::now())
            }
            Message::WorkspaceSwitcherDismissed(since) => {
                if self.chrome_ui.workspace_preview_since == Some(since) {
                    self.cancel_workspace_preview();
                }
                Task::none()
            }
            Message::WorkspaceSwitcherRested(since) => self.workspace_switcher_rested(since),
            Message::WorkspaceSwitcherScrolled { since, offset } => {
                if self.chrome_ui.workspace_preview_since == Some(since) {
                    self.chrome_ui.workspace_switcher_scroll = offset;
                }
                Task::none()
            }
            Message::NavigationSaveDue(generation) => {
                // A stale timer already projects nothing.
                if let Some(save) = self.navigation_save_due(generation) {
                    let before = SavedProjectionInputs::capture(self);
                    match save {
                        DeferredSave::Views => self.persist(),
                        DeferredSave::Snapshot => self.save_snapshot(),
                    }
                    if before.unchanged(self) && !self.update_tail_may_close_or_commit() {
                        projection = Projection::Unchanged;
                    }
                }
                Task::none()
            }
            Message::RepositoryReconciliationDue => self.reconcile_pending_repositories(),
            Message::ReportedTitleSaveDue => {
                self.reported_title_save.scheduled = false;
                if !self.reported_title_save.cards.is_empty() {
                    self.persist();
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::InitializeBrowserFactory => self.browser_factory_task(),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFactoryReady { generation, result } => {
                self.browser_factory_ready(generation, result)
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFactoryPoll(generation) => self.browser_factory_poll(generation),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserFactoryProbed {
                generation,
                operation,
                result,
            } => self.browser_factory_probed(generation, operation, result),
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
            Message::WindowEdgeState {
                window,
                filled,
                right_edge_tiled,
            } => {
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                self.window_state_observed = true;
                self.window_filled = filled;
                self.right_edge_tiled = right_edge_tiled;
                Task::none()
            }
            Message::WindowFilled(window, filled) => {
                if self.core.main_window_id() != Some(window) {
                    return Task::none();
                }
                self.window_filled = filled;
                Task::none()
            }
            Message::WindowResized(width, height) => {
                if height.is_finite() && height > 0.0 {
                    self.window_height = height;
                }
                if width.is_finite() && width > 0.0 {
                    self.window_width = width;
                }
                self.query_window_filled()
            }
            Message::ActivateUi { token } => self.activate_main_window(token),
            Message::ActivateSession { session, token } => {
                let activation = match token {
                    Some(token) => self.activate_main_window(Some(token)),
                    None => self
                        .core
                        .main_window_id()
                        .map_or_else(Task::none, |window| {
                            iced::window::minimize(window, false)
                                .chain(iced::window::gain_focus(window))
                                .map(cosmic::Action::App)
                        }),
                };
                Task::batch([
                    self.handle_chat_ui(chat_ui::Action::OpenSession(session.to_string())),
                    activation,
                ])
            }
            Message::OpenBrowser(url) => self.add_browser(url),
            Message::OpenBrowserInSystem(card) => self.open_browser_in_system(card),
            Message::BrowserSystemOpened {
                card,
                browser,
                opened,
            } => {
                self.browser_system_opened(card, browser, opened);
                Task::none()
            }
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
                let url = self.ended_terminal(card)
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
            #[cfg(feature = "integrated-browser")]
            Message::BrowserStop(card) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.load_state = None;
                }
                browser_command(&self.cards, card, |handle| handle.stop(), &mut self.status)
            }
            #[cfg(feature = "integrated-browser")]
            Message::FocusBrowserAddress(card) => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.address_focused = true;
                }
                iced::widget::operation::focus(card_control_id(card, "browser.address")).chain(
                    iced::widget::operation::select_all(card_control_id(card, "browser.address")),
                )
            }
            Message::RetryCard(card) => self.retry_card(card),
            Message::RelaunchFromEndedScreen(card) => self.relaunch_from_ended_screen(card),
            #[cfg(feature = "integrated-browser")]
            Message::BrowserAddressChanged { card, value } => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.address = BrowserAddress::user_input(value);
                    browser.address_error = false;
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserAddressFocus { card, focused } => {
                if let Some(CardRuntime::Browser(browser)) = self.cards.get_mut(&card) {
                    browser.address_focused = focused;
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserProgressTick => {
                for runtime in self.cards.values_mut() {
                    if let CardRuntime::Browser(browser) = runtime {
                        browser.progress_phase = (browser.progress_phase + 0.035) % 1.0;
                    }
                }
                Task::none()
            }
            #[cfg(feature = "integrated-browser")]
            Message::BrowserNavigate(card) => {
                let url = match self.cards.get_mut(&card) {
                    Some(CardRuntime::Browser(browser)) => {
                        let result = browser
                            .address
                            .submit_with_search(&self.settings.browser_search_url);
                        browser.address_error = result.is_err();
                        result
                    }
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
            Message::PrWorktreeCreated {
                origin,
                repository,
                outcome,
            } => {
                if !self.pr_creation_origin_is_current(&origin) {
                    self.status = Some(TransientNotice::warning(format!(
                        "The PR checkout was created after its owning review changed. Its journal result is retained; inspect {} before opening.",
                        match &outcome {
                            WorktreeCreationOutcome::Created { worktree, .. } =>
                                worktree.path.display().to_string(),
                            _ => repository.display().to_string(),
                        }
                    )));
                    return Task::none();
                }
                self.dispatch(Message::WorktreeCreated {
                    project_id: origin.project,
                    repository,
                    result: Ok(outcome),
                })
            }
            Message::WorktreeCreated {
                project_id,
                repository,
                result,
            } => match result {
                Ok(WorktreeCreationOutcome::Created { worktree, warnings }) => {
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
                    self.status = Some(worktree_operations::created_worktree_notice(
                        &worktree.path,
                        &warnings,
                    ));
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
                    // The new row, the selection moving to it and its
                    // terminal are projected after this update.
                    let terminal = self.select_worktree(worktree.path, true);
                    Task::batch([
                        terminal,
                        self.repository_load_task(repository, Some(project_id), false),
                    ])
                }
                Ok(WorktreeCreationOutcome::NotCreated { failure }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "Could not create the worktree: {failure}"
                    )));
                    Task::none()
                }
                Ok(WorktreeCreationOutcome::OutcomeUnknown { failure, .. }) => {
                    self.status = Some(TransientNotice::warning(format!(
                        "The worktree creation outcome is unknown. Refresh the project before trying again. {failure}"
                    )));
                    Task::none()
                }
                Err(error) => {
                    self.status = Some(TransientNotice::error(error));
                    Task::none()
                }
            },
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
            Message::PullRequestBodyCursor(cursor) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.body.0.move_to(cursor);
                }
                Task::none()
            }
            Message::PullRequestBodyChanged(action) => {
                if let Some(dialog) = &mut self.pull_request_dialog {
                    dialog.body.0.perform(action);
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
                    .is_some_and(|dialog| !dialog.can_submit())
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
                    body: dialog.body.0.text(),
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
                cosmic::task::future(async move {
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
                })
            }
            Message::PullRequestCreated {
                mut dialog,
                generation,
                result,
            } => match result {
                Ok(PullRequestCreationOutcome::Created { url, .. }) => {
                    self.status = Some(TransientNotice::success(format!(
                        "Created pull request {url}"
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
            Message::RestorePullRequestDialog { dialog, generation } => {
                self.restore_pull_request_dialog(dialog, generation)
            }
            Message::CancelDialog => self.cancel_form_dialog(),
            Message::KeyEvent { event, captured } => {
                let (task, key_projection) = self.key_event(event, captured);
                projection = key_projection;
                task
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
                        let task = self.apply_viewport(update, true, DurableChange::None);
                        if previous != self.workspace.focused() {
                            self.latency.commit(crate::performance::Action::CardFocus);
                            self.latency_focus = self.workspace.focused();
                        }
                        if previous == self.workspace.focused()
                            && let Some(card) = previous
                        {
                            self.keyboard_owed = self.owes_chat_input(card).then_some(card);
                            Task::batch([
                                task,
                                workspace_widget::focus_card_if_outside(
                                    workspace_widget::card_widget_id(card),
                                    self.card_keyboard_focus(card),
                                )
                                .map(cosmic::Action::App),
                            ])
                        } else {
                            task
                        }
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
                if !sidebar::search_is_reachable(&self.sidebar_model, self.projects_sidebar_mode())
                {
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
            Message::TerminalFind(action) => self.handle_terminal_find(action),
            Message::ToggleContextInspector => match self.focused_inspector_pane() {
                Some(card) => self.toggle_pane_inspector(card),
                None => Task::none(),
            },
            Message::TogglePaneInspector(_) | Message::ClosePaneInspector(_) if self.modal_is_open() => Task::none(),
            Message::TogglePaneInspector(card) => self.toggle_pane_inspector(card),
            Message::ClosePaneInspector(card) => self.close_pane_inspector_overlay(card),
            Message::ContextSection(_) if self.modal_is_open() => Task::none(),
            Message::ContextSection(action) => self.handle_context_section(action),
            // Reported on the first layout and whenever the size changes,
            // including the width while the inspector slides open.
            Message::ContextSectionsMeasured(height) => {
                self.context_sections_height = Some(height);
                Task::none()
            }
            Message::ContextInspector(_) if self.modal_is_open() => Task::none(),
            Message::ContextInspector(action) => self.handle_context_inspector_action(action),
            Message::SelectSettingsCategory(category) => {
                self.palette.clear_pressed_key_filter();
                self.settings_search.clear();
                self.settings_category = category;
                crate::settings_widgets::reveal_content_start()
            }
            Message::SettingsSearchChanged(query) => {
                self.palette.clear_pressed_key_filter();
                self.settings_search = query;
                crate::settings_widgets::reveal_content_start()
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
            Message::PullRequestCheckoutReady {
                card,
                project,
                generation,
                target,
                result,
            } => self.pr_checkout_ready(card, project, generation, target, result),
            Message::PullRequestReview { card, action } => {
                self.handle_pull_request_review(card, action)
            }
            Message::ReviewFilePanelToggle { card, collapsed } => {
                if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                    pane.file_panel_collapsed = Some(collapsed);
                }
                if let Some(state) = self.card_state_mut(card)
                    && let CardKind::PullRequests(view) = &mut state.kind
                {
                    view.file_panel_collapsed = Some(collapsed);
                }
                self.persist();
                Task::none()
            }
            Message::ReviewFilePanelResize { card, width } => {
                if !width.is_finite() {
                    return Task::none();
                }
                let width = width.clamp(120.0, 600.0);
                if let Some(CardRuntime::PullRequests(pane)) = self.cards.get_mut(&card) {
                    pane.file_panel_width = width;
                }
                if let Some(state) = self.card_state_mut(card)
                    && let CardKind::PullRequests(view) = &mut state.kind
                {
                    view.file_panel_width = Some(width.round() as u16);
                }
                Task::none()
            }
            Message::ReviewFilePanelResizeFinished => {
                self.persist();
                Task::none()
            }
            Message::CommitUi(action) => self.update_commit_ui(action),
            Message::ChromeUi(action) => {
                let (task, chrome_projection) = self.update_chrome_ui(action);
                if let Some(chrome_projection) = chrome_projection {
                    projection = chrome_projection;
                }
                task
            }
            Message::ChannelUi(action) => self.handle_channel_ui(action),
            Message::SidebarSelection(action) => self.update_sidebar_selection(action),
            Message::BranchUi(action) => self.update_branch_ui(action),
            Message::AccountUi(action) => self.update_account_ui(action),
            Message::ResumeConversation(action) => self.update_resume(action),
            Message::AutomationUi(action) => self.update_automation_ui(action),
            Message::WorkUi(action) => self.update_work_ui(action),
            Message::GitUi(action) => self.update_git(action),
            Message::NotificationUi(action) => self.update_notifications(action),
            Message::ReviewRequests(action) => self.update_review_requests(action),
            Message::RefreshUi(action) => {
                // A colour-only theme change reaches the hook but not the mode
                // watch, so the periodic refresh is where that one is caught.
                let refresh = self.update_refresh(action);
                let shared = if self.shared_views.blocked.is_empty() {
                    Task::none()
                } else {
                    self.reconcile_shared_views()
                };
                Task::batch([refresh, self.flush_document_tone(), shared])
            }
            Message::DocumentToneStale => {
                if self.settings.terminal_theme == crate::terminal::TerminalTheme::Auto {
                    self.synchronize_terminal_theme();
                }
                self.document_tone_stale = self.settings.app_theme == AppTheme::System;
                self.flush_document_tone()
            }
            Message::NativeWindowAction(action) => Task::done(cosmic::Action::Cosmic(action)),
            Message::ChatUi(action) => {
                let (task, chat_projection) = self.update_chat_ui(action);
                if let Some(chat_projection) = chat_projection {
                    projection = chat_projection;
                }
                task
            }
            Message::ResetSettings => {
                if self.settings_reset_confirm {
                    return Task::none();
                }
                self.settings_reset_confirm = true;
                self.settings_reset_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                dialog_ui::Question::ResetSettings.focus(
                    widget::Id::new("dialog.reset-settings.cancel"),
                    widget::Id::new("dialog.reset-settings.confirm"),
                )
            }
            Message::CancelResetSettings => self.close_settings_reset(),
            Message::ConfirmResetSettings => {
                if !self.settings_reset_confirm {
                    return Task::none();
                }
                let close = self.close_settings_reset();
                close.chain(self.reset_settings())
            }
            Message::Lsp(event) => {
                let navigation = if let LspEvent::Locations {
                    doc,
                    kind: crate::lsp::LocationKind::Definition,
                    items,
                    request,
                    ..
                } = &event
                {
                    self.lsp_documents.get(doc).copied().and_then(|card| {
                        let state = self.lsp_contexts.get_mut(&card)?;
                        if state.editor_navigation != Some(*request) {
                            return None;
                        }
                        state.editor_navigation = None;
                        Some((card, items.clone()))
                    })
                } else {
                    None
                };
                self.apply_lsp_event(event);
                if let Some((card, locations)) = navigation {
                    if locations.len() == 1
                        && let crate::lsp::LspTarget::Workspace { path } = &locations[0].target
                    {
                        return self.jump_to_lsp_location(
                            card,
                            path.clone(),
                            locations[0].range.start,
                            locations[0].coordinates,
                        );
                    }
                    return self
                        .reveal_context_section(card, context_sections::Section::Locations);
                }
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
            Message::RetrySharedViewSeeds => {
                self.retry_view_seeds();
                Task::none()
            }
            Message::RetrySessionSave => {
                self.materialize_replica_snapshot();
                if let (Some(session), Some(health)) = (&self.session, &mut self.save_health) {
                    health.request_retry(session);
                }
                Task::none()
            }
            Message::SessionRecovery(action) => self.update_session_recovery(action),
            Message::DaemonHealthUi(action) => self.update_daemon_health_ui(action),
            Message::AutomationApproval(action) => self.update_automation_approval(action),
            Message::CursorBlink(blink) => {
                // Every text cursor reads this on its next frame; the redraw
                // that follows this message shows one turned steady mid-blink.
                cosmic::iced::advanced::text::Blink::set_preferred(blink);
                Task::none()
            }
            Message::DaemonHealth(health) => {
                // A daemon that went away or speaks another protocol may have
                // been replaced by an install, which is when a stale window
                // starts losing its terminals: look now, not at the next tick.
                let inspection = if self.daemon_health_ui.health == health {
                    Task::none()
                } else {
                    self.update_update_ui(update_ui::Action::InspectInstallation)
                };
                self.daemon_health_ui.health = health;
                let confirmation = if !self.daemon_health_ui.health.unavailable()
                    && self.daemon_health_ui.confirmation
                {
                    self.update_daemon_health_ui(daemon_health_ui::Action::Cancel)
                } else {
                    Task::none()
                };
                Task::batch([inspection, confirmation])
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
            Message::ExpireNotices => {
                // Nothing is due while the countdown is paused, which also
                // makes a timer that fired as the notices opened a no-op.
                self.notices.expire(Instant::now());
                Task::none()
            }
            Message::NoticeControlRetired => self.notice_control_retired(),
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
            } => handled!(self.source_control_loaded(key, request, result)),
            Message::SourceControlHistory {
                generation,
                key,
                page,
                head,
                result,
            } => handled!(self.source_control_history(generation, key, page, head, result)),
            Message::SourceControlMutated {
                key,
                operation,
                report,
                committed,
            } => handled!(self.source_control_mutated(key, operation, report, committed)),
        };
        self.owe_projection(OwedProjection { projection, camera_only, reconcile_row_selection, model_update, save_view: false });
        task
            })();
        Task::batch([settled, task])
    }

    fn owe_projection(&mut self, owed: OwedProjection) {
        self.owed_projection = Some(match self.owed_projection.take() {
            Some(earlier) => earlier.merge(owed),
            None => owed,
        });
    }

    /// Projects, once, what this update's message and those its handlers
    /// dispatched within it owe, then settles what every update leaves
    /// behind: notices, the close queue, attention and keyboard focus.
    fn finish_update(&mut self, task: Task<Message>, entry: UpdateEntry) -> Task<Message> {
        let UpdateEntry {
            notice_control,
            timed,
            settled_projection,
            #[cfg(any(test, feature = "development"))]
            oracle_message,
        } = entry;
        #[rustfmt::skip]
        let task = (|| {
        let Some(OwedProjection { mut projection, camera_only, reconcile_row_selection, model_update, save_view }) = self.owed_projection.take() else {
            return task;
        };
        // Scoped proof was established during dispatch. Settings commit and
        // close processing can invalidate it before chrome is projected.
        if matches!(projection, Projection::ScopedChrome(_))
            && self.update_tail_may_close_or_commit()
        {
            projection = Projection::Full;
        }
        let scoped_source = matches!(projection, Projection::ScopedChrome(_))
            .then(|| Arc::downgrade(&self.durable));
        // `status` is the shell's write channel: ninety-odd call sites raise a
        // notice by assigning it. Draining it here is what makes those notices
        // survive the next assignment instead of silently replacing each other,
        // and it is the one place that can bound the history.
        // Before the notices drain, so one raised by a committed draft is
        // recorded with this update rather than the next.
        self.reconcile_terminal_review_owner();
        let task = Task::batch([
            task,
            self.flush_settings_on_blur(),
            self.settle_close_queue(),
        ]);
        if scoped_source.is_some_and(|source| source.as_ptr() != Arc::as_ptr(&self.durable)) {
            projection = Projection::Full;
        }
        let _projection_trace = crate::performance::ui_trace::projection(
            if camera_only && projection == Projection::Full {
                "Camera"
            } else {
                projection.name()
            },
        );
        if model_update && matches!(projection, Projection::Full | Projection::Chrome) {
            projection_stage!("replica_snapshot", self.materialize_replica_snapshot());
        }
        self.sync_popup_keyboard_nav();
        // The transport wording the health banner replaces may sit in either
        // part of a notice: a reworded one keeps the raw reason as detail.
        if let Some(notice) = self.status.take() {
            self.report_notice(notice);
        }
        self.resolve_card_notices();
        // A canonical save can arrive before the retry reply. Restore input
        // after either path retires the focused Retry save control.
        let task = Task::batch([task, self.refocus_saved_terminal()]);
        self.sync_notice_countdown();
        // Before `reconcile_notice_popup`, which owns the case of the notices
        // being open when their last entry goes.
        let task = Task::batch([task, self.retire_notice_control(notice_control)]);
        if self.latency_focus != self.workspace.focused() {
            self.latency.abandon_pending();
        }
        let projected = match &projection {
            Projection::Unchanged => self.reconcile_notice_popup(),
            // A reported title reaches the tabs, the window title and the
            // Attention list's pane rows; nothing else in the sidebar, nor
            // worktree ownership or protections, reads it.
            Projection::Chrome | Projection::ScopedChrome(_) => {
                #[cfg(test)]
                if model_update {
                    self.shared_views.chrome_tail_count += 1;
                }
                projection_stage!("attention_card_rows", self.refresh_attention_card_rows());
                let notices = self.reconcile_notice_popup();
                let mut phase = WorkspaceProjectionTiming::new(timed);
                let chrome = match &projection {
                    Projection::ScopedChrome(cards) => {
                        projection_stage!("scoped_chrome", self.sync_scoped_chrome(cards))
                    }
                    _ => projection_stage!("chrome", self.sync_chrome()),
                };
                phase.mark("selected_chrome");
                let snapshot = if matches!(projection, Projection::ScopedChrome(_)) {
                    self.arm_replica_capture()
                } else {
                    Task::none()
                };
                Task::batch([chrome, notices, snapshot])
            }
            Projection::TranscriptProbes => Task::batch([
                self.reconcile_notice_popup(),
                projection_stage!("transcript_probes", self.terminal_transcript_probes()),
            ]),
            // Panning and resizing the workspace move no label, selected
            // worktree or repository binding. A frame can end an animation
            // that releases a pane, so its worktree protection is kept.
            Projection::Full if camera_only => {
                projection_stage!("worktree_protections", self.sync_worktree_protections());
                Task::none()
            }
            Projection::Full => {
                #[cfg(test)]
                terminal_output_tests::record_projection();
                #[cfg(test)]
                if model_update {
                    self.shared_views.structural_tail_count += 1;
                }
                let notices = projection_stage!("notice_popup", self.reconcile_notice_popup());
                Task::batch([notices, self.project_workspace(reconcile_row_selection)])
            }
        };
        #[cfg(any(test, feature = "development"))]
        let projected = Task::batch([
            projected,
            self.verify_projection(
                oracle_message.as_deref(),
                &projection,
                camera_only,
                reconcile_row_selection,
            ),
        ]);
        let saved = if save_view {
            self.save_settings_task(false)
        } else {
            Task::none()
        };
        Task::batch([
            task,
            projected,
            saved,
            self.release_deferred_navigation_if_unblocked(),
        ])
            })();
        // A handler that returned before the projection above skipped it.
        #[cfg(any(test, feature = "development"))]
        let task = Task::batch([
            task,
            self.check_skipped_projection(oracle_message.as_deref()),
        ]);
        let _postlude_trace = crate::performance::ui_trace::projection("Postlude");
        projection_stage!("chat_failures", self.reconcile_chat_failure_presentations());
        if std::mem::take(&mut self.attention_scope_changed) {
            self.notification_ui.scope_changed();
        }
        projection_stage!("pull_request_labels", self.sync_pull_request_labels());
        let activation = self.retry_notification_activation();
        let visibility = projection_stage!("visible_attention", self.sync_visible_attention());
        projection_stage!("notification_presence", self.sync_notification_presence());
        let keyboard = self.settle_owed_keyboard();
        let preview = self.settle_workspace_switcher_preview();
        // Showing a cold chat can classify a current rejection synchronously,
        // including disconnected metadata with no future client wake. Only
        // mounted visible chats are inspected by this bounded handoff.
        let limit_usage =
            projection_stage!("chat_limit_accounts", self.refresh_chat_limit_accounts());
        // After everything above that may open, end or close a terminal or
        // change the accounts, so the frame looks the sign-ins up.
        projection_stage!("sign_ins", self.sync_sign_ins());
        // Last, after everything above that may change the accounts, so the
        // frame this update ends in reads its conversations' account choices.
        projection_stage!(
            "chat_account_choices",
            self.account_ui
                .settle_choices(self.settings.account_usage_display)
        );
        Task::batch([
            settled_projection,
            task,
            activation,
            visibility,
            keyboard,
            preview,
            limit_usage,
        ])
    }
}

impl Drop for Cagents {
    fn drop(&mut self) {
        self.latency.abandon_pending();
        self.persist();
        self.fold_settings_drafts();
        if let Err(error) = self.save_settings_now() {
            crate::diagnostics::record(crate::diagnostics::Event::SettingsFailed {
                category: crate::diagnostics::Failure::Unknown,
            });
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

/// The window-close question's dismissal, and its discard when it offers one.
const TRANSIENT_CLOSE_CANCEL: &str = "dialog.transient-close.cancel";
const TRANSIENT_CLOSE_DISCARD: &str = "dialog.transient-close.discard";

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
            progress_phase: 0.0,
            address_focused: false,
            address_error: false,
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
        && command(&browser.handle).is_err()
    {
        *status = Some(TransientNotice::error(format!(
            "Browser card {card}: a browser operation failed"
        )));
    }
    Task::none()
}

fn load_project(
    path: &Path,
    project_id: Option<ProjectId>,
    cancellation: CancellationToken,
    kind: Option<ProjectKind>,
) -> Result<LoadedRepository, String> {
    if crate::device::is_remote() {
        return Err("Use Device files for remote browsing and Git operations".into());
    }
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
    if crate::device::is_remote() {
        return Err("Use Device files for remote browsing and Git operations".into());
    }
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
    let github_availability = load_repository_github(&repository, false, &cancellation);
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

/// Reads a repository's GitHub state. A `revalidate` read, which a refresh
/// tick makes, reuses what the last read resolved, and keeps its listing when
/// GitHub says, for free, that nothing changed; opening or retrying a project
/// reads both.
fn load_repository_github(
    repository: &RepositoryInfo,
    revalidate: bool,
    cancellation: &CancellationToken,
) -> GitHubAvailability {
    let service = RepositoryService::system("git");
    let github = GitHubService::system("gh");
    (|| {
        let remotes = service
            .remotes(&repository.selected_path, cancellation)
            .map_err(|error| GitHubAvailability::Failed {
                detail: error.to_string(),
            })?;
        if !github
            .has_associated_remote(&remotes, cancellation)
            .map_err(GitHubAvailability::from_error)?
        {
            return Err(GitHubAvailability::NotAssociated);
        }
        let key = github
            .resolve_repository_cached(
                &repository.selected_path,
                &remotes,
                revalidate,
                cancellation,
            )
            .map_err(GitHubAvailability::from_error)?;
        let validator = match github.sidebar_probe(&key, revalidate, cancellation) {
            // The probe was answered as this account, so it is signed in.
            crate::github::SidebarProbe::Unchanged(pull_requests) => {
                return Ok(github_ready(key, pull_requests));
            }
            crate::github::SidebarProbe::Read(validator) => validator,
        };
        let auth = github
            .auth_status(&key, cancellation)
            .map_err(GitHubAvailability::from_error)?;
        if auth.state != crate::github::GhAuthState::Authenticated {
            return Err(GitHubAvailability::Unauthenticated);
        }
        let pull_requests = github
            .sidebar_listing(
                &key,
                validator,
                sidebar::PULL_REQUEST_LIST_LIMIT,
                cancellation,
            )
            .map_err(GitHubAvailability::from_error)?;
        Ok(github_ready(key, pull_requests))
    })()
    .unwrap_or_else(|unavailable| unavailable)
}

fn github_ready(
    repository_key: GitHubRepoKey,
    pull_requests: Vec<crate::github::PullRequest>,
) -> GitHubAvailability {
    GitHubAvailability::Ready {
        repository_key,
        pull_requests: pull_requests
            .into_iter()
            .map(|pull_request| {
                let checks = pull_request.check_summaries();
                (pull_request, checks)
            })
            .collect(),
        refreshed_at: unix_millis(),
    }
}

const SHARED_TERMINAL_DISCONNECTED: &str =
    "This terminal isn't connected. Use Retry or Close on its pane.";

/// An initial recovered pane can only subscribe to an exact issued owner.
/// Its captured endpoint remains read-only after the startup gate finishes.
fn attach_initial_terminal(
    card: CardId,
    source: TerminalSession,
    sender: async_channel::Sender<TerminalWakeup>,
) -> Result<StartedTerminal, String> {
    let socket =
        crate::terminal::existing_daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    let proof = client.proof().clone();
    let session = initial_terminal_from_daemon(&source, &mut client)?;
    saved_terminal::attach_existing(
        card,
        daemon_recovery::RecoveredTerminal {
            session,
            expected_host: Some(proof.clone()),
            attach_completed: false,
            recovery_lease: None,
            socket: Some(socket.clone()),
        },
        socket,
        proof,
        sender,
    )
}

/// [`initial_terminal_proving_absence`], asked of a connected daemon.
fn initial_terminal_from_daemon(
    source: &TerminalSession,
    client: &mut crate::control::ControlClient,
) -> Result<TerminalSession, String> {
    let proof = client.proof().clone();
    initial_terminal_proving_absence(
        source,
        &proof,
        client,
        daemon_recovery::list_terminals,
        daemon_recovery::ensure_admitted,
    )
}

/// The terminal an initial pane attaches to. A listing taken while the daemon
/// is still restoring can be partial, so it can attach to what it lists but
/// can't show that a terminal is gone: absence is read again after admission
/// readiness, as a shared pane's open does.
fn initial_terminal_proving_absence<C>(
    source: &TerminalSession,
    proof: &HostSessionProof,
    client: &mut C,
    list: impl Fn(&mut C) -> Result<Vec<crate::terminal::ipc::TerminalDescriptor>, String>,
    ready: impl Fn(&mut C) -> Result<(), String>,
) -> Result<TerminalSession, String> {
    if let Some(session) = initial_terminal_attachment(source, &list(client)?, proof)? {
        return Ok(session);
    }
    ready(client)?;
    initial_terminal_or_missing(source, &list(client)?, proof)
}

/// When its daemon no longer lists an initial pane's terminal, the pane is
/// kept and the error says what Retry will find.
fn initial_terminal_or_missing(
    source: &TerminalSession,
    terminals: &[crate::terminal::ipc::TerminalDescriptor],
    proof: &HostSessionProof,
) -> Result<TerminalSession, String> {
    initial_terminal_attachment(source, terminals, proof)?.ok_or_else(|| {
        terminal_failure::missing_terminal_message(source, proof, terminals).to_owned()
    })
}

fn initial_terminal_attachment(
    source: &TerminalSession,
    terminals: &[crate::terminal::ipc::TerminalDescriptor],
    proof: &HostSessionProof,
) -> Result<Option<TerminalSession>, String> {
    if terminals
        .iter()
        .any(|terminal| terminal.session.id == source.id)
    {
        return saved_terminal::attachment(source, terminals, proof);
    }
    let mut issued = terminals.iter().filter(|terminal| {
        terminal.upgrade_sources.contains(source)
            || terminal
                .upgrade_relaunches
                .iter()
                .any(|relaunch| relaunch.source == *source)
    });
    let Some(terminal) = issued.next() else {
        return Ok(None);
    };
    if issued.next().is_some()
        || !proof.is_complete()
        || !terminal.session.continuity.can_resume_live(proof)
    {
        return Err("The initial terminal has ambiguous or unconfirmed issued ownership".into());
    }
    Ok(Some(terminal.session.clone()))
}

/// Another client may own a shared terminal's recovery, so a shared pane only
/// attaches the exact session its daemon hosts, under that daemon's proof.
/// `socket` is that daemon's, already proven ready.
fn shared_terminal_attachment(
    hosted: TerminalSession,
    proof: HostSessionProof,
    socket: std::path::PathBuf,
) -> (daemon_recovery::RecoveredTerminal, TerminalStartMode) {
    (
        daemon_recovery::RecoveredTerminal {
            session: hosted,
            expected_host: Some(proof.clone()),
            attach_completed: false,
            recovery_lease: None,
            socket: Some(socket),
        },
        TerminalStartMode::Attach(proof),
    )
}

/// What a restored agent row attaches, or `None` when only a launch could
/// bring it back. `connect` reaches the daemon a shared pane is hosted by.
fn restored_attachment(
    shared: bool,
    session: TerminalSession,
    connect: impl FnOnce() -> Result<crate::terminal::ReadyDaemon, String>,
) -> Result<Option<(daemon_recovery::RecoveredTerminal, TerminalStartMode)>, String> {
    let (recovered, mode) = if shared {
        hosted_shared_terminal(connect()?, session.id)?
    } else {
        recover_terminal(session, false)?
    };
    Ok(matches!(mode, TerminalStartMode::Attach(_)).then_some((recovered, mode)))
}

/// The exact shared terminal the ready `daemon` hosts, ready to attach.
fn hosted_shared_terminal(
    daemon: crate::terminal::ReadyDaemon,
    terminal: TerminalId,
) -> Result<(daemon_recovery::RecoveredTerminal, TerminalStartMode), String> {
    let crate::terminal::ReadyDaemon { socket, mut client } = daemon;
    let hosted = client
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|hosted| hosted.session.id == terminal)
        .ok_or(SHARED_TERMINAL_DISCONNECTED)?;
    Ok(shared_terminal_attachment(
        hosted.session,
        client.proof().clone(),
        socket,
    ))
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
    created: bool,
) -> Result<StartedTerminal, String> {
    let recovery_lease = recovered.recovery_lease;
    let session = recovered.session;
    // A daemon the recovery already proved ready isn't proven again; the
    // worker's own connection is the attachment's. One that died since is
    // therefore not started again here: its attach fails, and only a
    // launch with no proof to keep would have gone ahead on a new daemon,
    // which the pane's Retry still does.
    let socket = recovered.socket;
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
    let socket = match socket {
        Some(socket) => socket,
        None => crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?,
    };
    let handle = match mode {
        TerminalStartMode::Attach(proof) => {
            HostedTerminalHandle::connect_at(socket, session.id, Some(proof), wake)
        }
        TerminalStartMode::Spawn(recipe) => {
            let mut session = session.clone();
            session.launch = recipe;
            HostedTerminalHandle::attach_or_spawn_at(
                socket,
                session,
                config,
                recovered.expected_host,
                wake,
            )
        }
    }
    .map_err(|error| error.to_string())?;
    // The daemon may wait up to its holder startup timeout before it can send
    // Spawned. Keep the handle alive through that reply and a reconnect grace.
    handle.wait_until_attached(crate::terminal::SPAWN_START_TIMEOUT + Duration::from_secs(10))?;
    drop(recovery_lease);
    let mut surface = TerminalSurface::from(handle);
    if spawned || created {
        surface = surface.spawned();
    }
    Ok(StartedTerminal {
        handle: surface,
        session,
        spawned,
        save_failure: None,
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

/// Whether two absolute paths name the same file. A repository descriptor
/// keeps the path its pane was opened with, and a file link's need not be
/// canonical. Only a candidate already sharing the file name reaches the
/// filesystem, so routing a click stays cheap.
fn same_file(candidate: &Path, target: &Path) -> bool {
    candidate == target
        || (candidate.file_name() == target.file_name()
            && candidate
                .canonicalize()
                .ok()
                .zip(target.canonicalize().ok())
                .is_some_and(|(candidate, target)| candidate == target))
}

#[cfg(test)]
mod tests;
