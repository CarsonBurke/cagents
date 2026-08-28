//! Project, worktree, and pull-request navigation.
//!
//! This module deliberately keeps the sidebar's presentation state separate
//! from repository discovery. Slow Git and GitHub work belongs outside the UI
//! thread; the view consumes small, immutable snapshots assembled from those
//! services.
//!
//! `view` runs on every terminal wake rather than on every interaction, so it
//! derives nothing that an entity already determines: folded filter keys,
//! widget identities, and accessible names are built once per entity.

use std::{
    borrow::Cow,
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use cosmic::iced::advanced::Renderer as _;
use cosmic::{
    Element, Renderer, Theme,
    iced::{
        Alignment, Border, Event, Length, Point, Rectangle, Shadow, Size,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
            text::EllipsizeHeightLimit,
            widget::{Operation, Tree, operation, tree},
        },
        id::Id,
        keyboard, window,
    },
    theme,
    widget::{self, button, icon},
};
use url::Url;

use crate::{
    github::{PullRequest, PullRequestCheck},
    model::{ProjectId, ProjectState, WorktreeId},
    repository::{
        DirtyState, RepositoryId, RepositoryInfo, StatusSnapshot, TrackingState, WorktreeRecord,
    },
};

/// The rendered bounds of the expanded pane, and the only bounds it has:
/// `crate::settings` validates persisted widths against these, so a width can
/// no longer pass validation and then silently render clamped.
pub const EXPANDED_WIDTH_MIN: f32 = 240.0;
pub const EXPANDED_WIDTH_MAX: f32 = 400.0;
/// Rendered when a persisted width is not a finite number. Mirrors
/// `settings::AppSettings::default`, so a corrupt file looks like a fresh one.
const DEFAULT_EXPANDED_WIDTH: f32 = 280.0;
const COMPACT_WIDTH: f32 = 52.0;
const COMPACT_BREAKPOINT: f32 = 1_200.0;
const HEADER_HEIGHT: f32 = 44.0;
const PROJECT_HEADER_HEIGHT: f32 = 36.0;
const WORKTREE_ROW_HEIGHT: f32 = 40.0;
const PULL_REQUEST_ROW_HEIGHT: f32 = 44.0;
const ICON_CONTROL_SIZE: f32 = 30.0;
const ROW_ICON_WIDTH: f32 = 20.0;
/// Icons that sit inside a row, beside its text.
const ROW_ICON_SIZE: u16 = 16;
/// Icons that stand alone as the illustration of a whole pane state.
const STATUS_ICON_SIZE: u16 = 20;
/// Pointer target of the resize handle. Wider than the rule it draws, because
/// a one-pixel grab target cannot be hit; the extra pixels come out of the
/// pane, so it stays as narrow as a comfortable target allows.
const RESIZE_HANDLE_WIDTH: f32 = 4.0;
/// One keyboard resize step, and the step assistive technologies report.
const RESIZE_STEP: f32 = 16.0;
const RESIZE_HANDLE_NAME: &str = "Resize sidebar";

/// The responsive density selected by the application from its available
/// width. Compact mode is the `COMPACT_WIDTH` rail: it keeps every command it
/// renders as a named, focusable icon, and applies no state it cannot render a
/// control for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarMode {
    Compact,
    #[default]
    Expanded,
}

impl SidebarMode {
    /// Selects the compact rail for narrow application windows.
    pub fn for_width(window_width: f32) -> Self {
        if window_width < COMPACT_BREAKPOINT {
            Self::Compact
        } else {
            Self::Expanded
        }
    }
}

/// Coarse filters which can be combined with the free-text query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarFilter {
    #[default]
    All,
    Attention,
    PullRequests,
}

/// Loading is explicit so an empty repository list is never mistaken for an
/// in-flight refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SidebarDataState {
    #[default]
    Loading,
    Ready,
    Error(String),
}

/// Immutable data presented by the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarModel {
    pub mode: SidebarMode,
    pub data_state: SidebarDataState,
    pub projects: Vec<SidebarProject>,
}

impl SidebarModel {
    pub fn ready(mode: SidebarMode, projects: Vec<SidebarProject>) -> Self {
        Self {
            mode,
            data_state: SidebarDataState::Ready,
            projects,
        }
    }

    pub fn loading(mode: SidebarMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub fn error(mode: SidebarMode, message: impl Into<String>) -> Self {
        Self {
            mode,
            data_state: SidebarDataState::Error(message.into()),
            projects: Vec::new(),
        }
    }
}

/// A project row derived from durable project state and repository discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarProject {
    pub id: ProjectId,
    pub repository_id: RepositoryId,
    pub name: String,
    pub repository: PathBuf,
    pub worktrees: Vec<SidebarWorktree>,
    pub pull_requests: Vec<SidebarPullRequest>,
    chrome: ProjectChrome,
}

impl SidebarProject {
    pub fn new(
        id: ProjectId,
        repository_id: RepositoryId,
        name: String,
        repository: PathBuf,
        worktrees: Vec<SidebarWorktree>,
        pull_requests: Vec<SidebarPullRequest>,
    ) -> Self {
        let chrome = ProjectChrome::new(id, &name, &repository);
        Self {
            id,
            repository_id,
            name,
            repository,
            worktrees,
            pull_requests,
            chrome,
        }
    }

    pub fn from_repository(
        project: &ProjectState,
        repository: &RepositoryInfo,
        worktrees: Vec<SidebarWorktree>,
        pull_requests: Vec<SidebarPullRequest>,
    ) -> Self {
        Self::new(
            project.id,
            repository.id,
            project.name.clone(),
            project.repository.clone(),
            worktrees,
            pull_requests,
        )
    }

    fn matches(&self, folded_query: &str) -> bool {
        self.chrome.folded_name.contains(folded_query)
            || self.chrome.folded_repository.contains(folded_query)
    }
}

/// Presentation strings and widget identities a project determines. Building
/// them per frame would allocate once per project per terminal wake, so they
/// are derived exactly once, in `SidebarProject::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectChrome {
    folded_name: Box<str>,
    folded_repository: Box<str>,
    open_id: Id,
    toggle_id: Id,
    new_worktree_id: Id,
    /// The header's command for the selected project is a second control, so
    /// it needs a second identity: two widgets sharing one custom id leave the
    /// state tree with a node the renderer cannot downcast.
    header_new_worktree_id: Id,
    create_pull_request_id: Id,
    toggle_pull_requests_id: Id,
    open_name: Box<str>,
    compact_open_name: Box<str>,
    collapse_name: Box<str>,
    expand_name: Box<str>,
    new_worktree_name: Box<str>,
    create_pull_request_name: Box<str>,
    collapse_pull_requests_name: Box<str>,
    expand_pull_requests_name: Box<str>,
}

impl ProjectChrome {
    fn new(id: ProjectId, name: &str, repository: &Path) -> Self {
        Self {
            folded_name: fold(name),
            folded_repository: fold(&repository.to_string_lossy()),
            open_id: entity_id("project.open", id),
            toggle_id: entity_id("project.toggle", id),
            new_worktree_id: entity_id("project.new_worktree", id),
            header_new_worktree_id: entity_id("header.new_worktree", id),
            create_pull_request_id: entity_id("project.create_pr", id),
            toggle_pull_requests_id: entity_id("project.toggle_prs", id),
            open_name: format!("Open project {name}").into_boxed_str(),
            compact_open_name: format!("Open {name}").into_boxed_str(),
            collapse_name: format!("Collapse {name} project").into_boxed_str(),
            expand_name: format!("Expand {name} project").into_boxed_str(),
            new_worktree_name: format!("New worktree for {name}").into_boxed_str(),
            create_pull_request_name: format!("Create pull request for {name}").into_boxed_str(),
            collapse_pull_requests_name: format!("Collapse {name} pull requests").into_boxed_str(),
            expand_pull_requests_name: format!("Expand {name} pull requests").into_boxed_str(),
        }
    }
}

/// Worktree presentation data derived from a worktree record and its optional
/// status snapshot. A missing status is represented rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarWorktree {
    pub id: WorktreeId,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub detached: bool,
    pub locked: bool,
    pub dirty: Option<DirtyState>,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    chrome: WorktreeChrome,
}

impl SidebarWorktree {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: WorktreeId,
        path: PathBuf,
        branch: Option<String>,
        detached: bool,
        locked: bool,
        dirty: Option<DirtyState>,
        ahead: Option<u64>,
        behind: Option<u64>,
    ) -> Self {
        let label = branch.clone().unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        });
        let path_label = path.display().to_string();
        let status = status_summary(detached, locked, dirty.as_ref(), ahead, behind);
        let chrome = WorktreeChrome {
            folded_branch: branch.as_deref().map(fold),
            folded_path: fold(&path.to_string_lossy()),
            open_id: entity_id("worktree.open", id),
            accessible_name: format!("Open worktree {label}, {status}").into_boxed_str(),
            tooltip: format!("{label} — {path_label}").into_boxed_str(),
            dirty_badge: dirty
                .as_ref()
                .filter(|dirty| dirty.is_dirty())
                .map(|dirty| format!("{} changes", dirty_count(dirty)).into_boxed_str()),
            ahead_badge: ahead
                .filter(|ahead| *ahead > 0)
                .map(|ahead| format!("↑{ahead}").into_boxed_str()),
            behind_badge: behind
                .filter(|behind| *behind > 0)
                .map(|behind| format!("↓{behind}").into_boxed_str()),
            label: label.into_boxed_str(),
        };

        Self {
            id,
            path,
            branch,
            detached,
            locked,
            dirty,
            ahead,
            behind,
            chrome,
        }
    }

    /// Builds the row for a worktree known only from durable session state,
    /// before repository discovery has produced a status snapshot.
    pub fn from_durable(id: WorktreeId, path: PathBuf, branch: Option<String>) -> Self {
        let detached = branch.is_none();
        Self::new(id, path, branch, detached, false, None, None, None)
    }

    pub fn from_repository(record: &WorktreeRecord, status: Option<&StatusSnapshot>) -> Self {
        let (ahead, behind) = match status.map(|status| &status.tracking) {
            Some(TrackingState::Compared { ahead, behind, .. }) => (Some(*ahead), Some(*behind)),
            _ => (None, None),
        };

        Self::new(
            record.id,
            record.path.clone(),
            record.branch.as_deref().map(|branch| {
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_owned()
            }),
            record.detached,
            record.lock.locked,
            status.map(|status| status.dirty.clone()),
            ahead,
            behind,
        )
    }

    fn matches(&self, folded_query: &str) -> bool {
        self.chrome
            .folded_branch
            .as_deref()
            .is_some_and(|branch| branch.contains(folded_query))
            || self.chrome.folded_path.contains(folded_query)
    }

    fn needs_attention(&self) -> bool {
        self.dirty.as_ref().is_some_and(DirtyState::is_dirty)
            || self.ahead.is_some_and(|ahead| ahead > 0)
            || self.behind.is_some_and(|behind| behind > 0)
    }
}

/// Presentation strings and widget identities a worktree determines, built
/// once in `SidebarWorktree::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeChrome {
    folded_branch: Option<Box<str>>,
    folded_path: Box<str>,
    open_id: Id,
    label: Box<str>,
    accessible_name: Box<str>,
    tooltip: Box<str>,
    dirty_badge: Option<Box<str>>,
    ahead_badge: Option<Box<str>>,
    behind_badge: Option<Box<str>>,
}

/// Aggregate check state for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Passing,
    Pending,
    Failing,
    Unknown,
}

impl CheckStatus {
    pub fn from_checks(checks: &[PullRequestCheck]) -> Self {
        if checks.is_empty() {
            return Self::Unknown;
        }

        let mut pending = false;
        let mut unknown = false;
        for check in checks {
            let bucket = check.bucket.0.to_ascii_lowercase();
            let state = check.state.0.to_ascii_lowercase();
            if is_check_failure(&bucket) || is_check_failure(&state) {
                return Self::Failing;
            }
            if is_check_pending(&bucket) || is_check_pending(&state) {
                pending = true;
            } else if !is_check_success(&bucket) && !is_check_success(&state) {
                unknown = true;
            }
        }

        if pending {
            Self::Pending
        } else if unknown {
            Self::Unknown
        } else {
            Self::Passing
        }
    }
}

/// Pull-request presentation data derived from `gh` JSON DTOs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarPullRequest {
    pub number: u64,
    pub title: String,
    pub url: Url,
    pub head_ref: String,
    pub author: Option<String>,
    pub draft: bool,
    pub checks: CheckStatus,
    chrome: PullRequestChrome,
}

impl SidebarPullRequest {
    /// `project_id` is required because a pull-request number is only unique
    /// within its project, and the focus identity is built here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_id: ProjectId,
        number: u64,
        title: String,
        url: Url,
        head_ref: String,
        author: Option<String>,
        draft: bool,
        checks: CheckStatus,
    ) -> Self {
        let mut metadata = format!("#{number} · {head_ref}");
        if let Some(author) = &author {
            metadata.push_str(" · @");
            metadata.push_str(author);
        }
        let check_label = check_description(checks);
        let chrome = PullRequestChrome {
            folded_title: fold(&title),
            folded_head_ref: fold(&head_ref),
            folded_author: author.as_deref().map(fold),
            number_text: number.to_string().into_boxed_str(),
            open_id: stable_id(format!("project.{project_id}.pr.{number}.open")),
            accessible_name: format!("Open pull request {number}: {title}, checks {check_label}")
                .into_boxed_str(),
            metadata: metadata.into_boxed_str(),
        };

        Self {
            number,
            title,
            url,
            head_ref,
            author,
            draft,
            checks,
            chrome,
        }
    }

    pub fn from_github(
        project_id: ProjectId,
        pull_request: &PullRequest,
        checks: &[PullRequestCheck],
    ) -> Self {
        Self::new(
            project_id,
            pull_request.number,
            pull_request.title.clone(),
            pull_request.url.clone(),
            pull_request.head_ref_name.clone(),
            pull_request
                .author
                .as_ref()
                .map(|author| author.login.clone()),
            pull_request.is_draft,
            CheckStatus::from_checks(checks),
        )
    }

    fn matches(&self, folded_query: &str) -> bool {
        self.chrome.folded_title.contains(folded_query)
            || self.chrome.folded_head_ref.contains(folded_query)
            || self
                .chrome
                .folded_author
                .as_deref()
                .is_some_and(|author| author.contains(folded_query))
            || self.chrome.number_text.contains(folded_query)
    }

    fn needs_attention(&self) -> bool {
        matches!(self.checks, CheckStatus::Failing | CheckStatus::Pending)
    }
}

/// Presentation strings and widget identities a pull request determines, built
/// once in `SidebarPullRequest::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestChrome {
    folded_title: Box<str>,
    folded_head_ref: Box<str>,
    folded_author: Option<Box<str>>,
    number_text: Box<str>,
    open_id: Id,
    accessible_name: Box<str>,
    metadata: Box<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarSelection {
    Project(ProjectId),
    Worktree {
        project_id: ProjectId,
        worktree_id: WorktreeId,
    },
    PullRequest {
        project_id: ProjectId,
        number: u64,
    },
}

/// Persistent interaction state owned by the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    query: String,
    filter: SidebarFilter,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    expanded_pull_requests: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
}

impl SidebarState {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn filter(&self) -> SidebarFilter {
        self.filter
    }

    pub fn controls_expanded(&self) -> bool {
        self.controls_expanded
    }

    /// Applies only local, pure presentation transitions. Commands such as
    /// opening a worktree remain for the application to handle.
    pub fn apply(&mut self, action: &SidebarAction) {
        match action {
            SidebarAction::SearchChanged(query) => self.query.clone_from(query),
            SidebarAction::SetFilter(filter) => self.filter = *filter,
            SidebarAction::ToggleControls => self.controls_expanded = !self.controls_expanded,
            SidebarAction::ToggleProject(project_id) => {
                toggle_membership(&mut self.collapsed_projects, *project_id);
            }
            SidebarAction::TogglePullRequests(project_id) => {
                toggle_membership(&mut self.expanded_pull_requests, *project_id);
            }
            SidebarAction::OpenRepository { project_id, .. } => {
                self.selection = Some(SidebarSelection::Project(*project_id));
            }
            SidebarAction::OpenWorktree {
                project_id,
                worktree_id,
                ..
            } => {
                self.selection = Some(SidebarSelection::Worktree {
                    project_id: *project_id,
                    worktree_id: *worktree_id,
                });
            }
            SidebarAction::OpenPullRequest {
                project_id, number, ..
            } => {
                self.selection = Some(SidebarSelection::PullRequest {
                    project_id: *project_id,
                    number: *number,
                });
            }
            SidebarAction::OpenRepositoryPicker
            | SidebarAction::NewWorktree { .. }
            | SidebarAction::Refresh
            | SidebarAction::CreatePullRequest { .. }
            | SidebarAction::OpenSettings
            | SidebarAction::Resize { .. }
            | SidebarAction::ResizeFinished => {}
        }
    }
}

/// All sidebar intents. Payloads are self-contained so `app.rs` does not need
/// to recover a path or URL from ephemeral row indexes.
/// Not `Eq`: the resize request carries a width, and widths are measurements.
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarAction {
    SearchChanged(String),
    SetFilter(SidebarFilter),
    ToggleControls,
    ToggleProject(ProjectId),
    TogglePullRequests(ProjectId),
    OpenRepositoryPicker,
    OpenRepository {
        project_id: ProjectId,
        repository: PathBuf,
    },
    OpenWorktree {
        project_id: ProjectId,
        worktree_id: WorktreeId,
        path: PathBuf,
    },
    NewWorktree {
        project_id: ProjectId,
        repository: PathBuf,
    },
    /// Opens the pull request using the configured link destination.
    OpenPullRequest {
        project_id: ProjectId,
        number: u64,
        url: Url,
    },
    Refresh,
    CreatePullRequest {
        project_id: ProjectId,
        repository: PathBuf,
    },
    OpenSettings,
    /// A width requested by the pane's resize handle, already clamped to the
    /// rendered bounds. Published on every pointer move of a drag, so the
    /// application should apply it and nothing more.
    Resize {
        width: f32,
    },
    /// The end of a resize gesture: the last `Resize` is the one worth
    /// persisting. Keeping the write off the drag spares the settings file one
    /// atomic rewrite per pointer sample.
    ResizeFinished,
}

/// Whether the pane would render the search input if its controls were open.
///
/// `view` only lays the controls block out for a project list that is not
/// empty, and only at the expanded density, so those are the conditions under
/// which [`search_focus_id`] names something on screen. A caller that focused
/// it otherwise would blur whatever held focus and focus nothing, because
/// `focus` unfocuses every widget whose id does not match.
pub fn search_is_reachable(model: &SidebarModel, mode: SidebarMode) -> bool {
    mode != SidebarMode::Compact && !model.projects.is_empty()
}

pub fn search_focus_id() -> Id {
    stable_id("search")
}

/// The focus target of the pane's resize handle.
pub fn resize_handle_id() -> Id {
    stable_id("resize-handle")
}

/// Returns the rendered pane width. Persisted expanded widths are constrained
/// to the workbench range even when an older settings file contains a broader
/// backend-valid value.
pub fn effective_width(mode: SidebarMode, persisted_width: f32) -> f32 {
    if mode == SidebarMode::Compact {
        COMPACT_WIDTH
    } else {
        clamp_expanded_width(persisted_width)
    }
}

/// Constrains a requested expanded width to the pane's rendered bounds.
pub fn clamp_expanded_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(EXPANDED_WIDTH_MIN, EXPANDED_WIDTH_MAX)
    } else {
        DEFAULT_EXPANDED_WIDTH
    }
}

/// The sidebar's single public rendering entry point.
///
/// `modal_open` says a dialog is on screen. The dialog is an overlay, so a
/// keystroke it does not consume still reaches this pane, and the pane still
/// holds whatever focus it had when the dialog opened - libcosmic's modal
/// popover declines to deliver a focus operation to the content beneath it, so
/// that focus cannot be moved away while the dialog is up.
pub fn view<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    mode: SidebarMode,
    persisted_width: f32,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let compact = mode == SidebarMode::Compact;
    let width = effective_width(mode, persisted_width);
    let loading = matches!(&model.data_state, SidebarDataState::Loading);
    let refresh_enabled = !model.projects.is_empty() && !loading;
    let mut content = widget::column::with_capacity(5).push(
        widget::container(header(model, state, compact, loading)).padding([0, spacing.space_xxxs]),
    );

    // Each density is given the controls its width can hold, and
    // `filtered_projects` applies exactly the state that has a control here:
    // the rail fits the filter as an icon column but cannot fit a text input,
    // so it offers the filter alone and its query is left unapplied.
    if compact && !model.projects.is_empty() {
        content = content
            .push(widget::container(compact_filters(state.filter)).padding(spacing.space_xxxs));
    } else if state.controls_expanded && search_is_reachable(model, mode) {
        content = content.push(
            widget::container(controls(state, refresh_enabled, loading))
                .class(theme::Container::Secondary)
                .padding(spacing.space_xxs),
        );
    }

    content = content
        .push(
            widget::container(body(model, state, compact))
                .padding([spacing.space_xxxs, spacing.space_xxxs])
                .height(Length::Fill),
        )
        .push(widget::divider::horizontal::light())
        .push(footer(model, state, compact))
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

    widget::container(
        widget::row::with_children(vec![
            widget::container(content)
                .class(theme::Container::Background)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            // The compact rail has one width, so its edge is a rule and
            // nothing more: a drag there would silently retune a width the
            // rail does not render.
            if compact {
                widget::divider::vertical::default().into()
            } else {
                resize_handle(width, modal_open).into()
            },
        ])
        .spacing(0),
    )
    .class(theme::Container::Background)
    .width(Length::Fixed(width))
    .height(Length::Fill)
    .into()
}

fn header<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
    loading: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        return widget::container(icon_action(
            "folder-open-symbolic",
            "Open repository",
            SidebarAction::OpenRepositoryPicker,
            stable_id("open-repository"),
        ))
        .center_x(Length::Fill)
        .height(Length::Fixed(HEADER_HEIGHT))
        .into();
    }

    let mut children: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(6);
    children.push(widget::text::caption_heading("Projects").into());
    children.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
    // `view` renders the controls only for a non-empty project list, so with
    // no projects this would offer, and paint as selected, a control that
    // opens nothing. A failed refresh keeps the projects it already had - it
    // replaces the body, not the list - so this stays rendered there, which is
    // right: search and filter still describe what the pane is holding.
    if !model.projects.is_empty() {
        let selected = state.controls_expanded
            || !state.query.is_empty()
            || state.filter != SidebarFilter::All;
        children.push(
            button::icon(icon::from_name("view-list-symbolic").size(ROW_ICON_SIZE))
                .id(stable_id("toggle-controls"))
                .name("Search and filter projects")
                .tooltip("Search and filter projects")
                .selected(selected)
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press(SidebarAction::ToggleControls)
                .into(),
        );
    }
    if loading {
        children.push(
            widget::progress_bar::circular::Circular::new()
                .size(f32::from(ROW_ICON_SIZE))
                .into(),
        );
    }
    children.push(icon_action(
        "folder-open-symbolic",
        "Open repository",
        SidebarAction::OpenRepositoryPicker,
        stable_id("open-repository"),
    ));
    if let Some(project) = selected_project(model, state) {
        children.push(icon_action(
            "list-add-symbolic",
            &*project.chrome.new_worktree_name,
            SidebarAction::NewWorktree {
                project_id: project.id,
                repository: project.repository.clone(),
            },
            project.chrome.header_new_worktree_id.clone(),
        ));
    } else {
        children.push(icon_action(
            "list-add-symbolic",
            "Open a repository and create a worktree",
            SidebarAction::OpenRepositoryPicker,
            stable_id("new-worktree"),
        ));
    }

    widget::row::with_children(children)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(Length::Fixed(HEADER_HEIGHT))
        .width(Length::Fill)
        .into()
}

fn selected_project<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
) -> Option<&'a SidebarProject> {
    let project_id = state.selection.map(|selection| match selection {
        SidebarSelection::Project(project_id)
        | SidebarSelection::Worktree { project_id, .. }
        | SidebarSelection::PullRequest { project_id, .. } => project_id,
    })?;
    model
        .projects
        .iter()
        .find(|project| project.id == project_id)
}

fn controls<'a>(
    state: &'a SidebarState,
    refresh_enabled: bool,
    loading: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
    actions.push(filters(state.filter));
    actions.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
    if loading {
        actions.push(
            widget::progress_bar::circular::Circular::new()
                .size(f32::from(ROW_ICON_SIZE))
                .into(),
        );
    } else if refresh_enabled {
        actions.push(icon_action(
            "view-refresh-symbolic",
            "Refresh projects",
            SidebarAction::Refresh,
            stable_id("refresh"),
        ));
    }
    widget::column::with_children(vec![
        search(state),
        widget::row::with_children(actions)
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxxs)
            .into(),
    ])
    .spacing(spacing.space_xxxs)
    .width(Length::Fill)
    .into()
}

fn footer<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
    compact: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let settings = icon_action(
        "emblem-system-symbolic",
        "Settings",
        SidebarAction::OpenSettings,
        stable_id("settings"),
    );
    if compact {
        // The rail renders no project header, and the window's header bar is
        // no substitute: without these the two commands have no route at all
        // below the breakpoint.
        let mut children: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
        if let Some(project) = selected_project(model, state) {
            children.extend([
                icon_action(
                    "list-add-symbolic",
                    &*project.chrome.new_worktree_name,
                    SidebarAction::NewWorktree {
                        project_id: project.id,
                        repository: project.repository.clone(),
                    },
                    project.chrome.new_worktree_id.clone(),
                ),
                icon_action(
                    "document-new-symbolic",
                    &*project.chrome.create_pull_request_name,
                    SidebarAction::CreatePullRequest {
                        project_id: project.id,
                        repository: project.repository.clone(),
                    },
                    project.chrome.create_pull_request_id.clone(),
                ),
            ]);
        }
        children.push(settings);
        return widget::column::with_children(children)
            .align_x(Alignment::Center)
            .spacing(spacing.space_xxxs)
            .padding(spacing.space_xxxs)
            .width(Length::Fill)
            .into();
    }
    widget::row::with_children(vec![settings])
        .align_y(Alignment::Center)
        .padding(spacing.space_xxxs)
        .width(Length::Fill)
        .into()
}

fn search<'a>(state: &'a SidebarState) -> Element<'a, SidebarAction> {
    let width = Length::Fill;
    let input = widget::text_input::search_input("Search projects", &state.query)
        .on_input(SidebarAction::SearchChanged)
        .width(width);
    let input = if state.query.is_empty() {
        input
    } else {
        input.on_clear(SidebarAction::SearchChanged(String::new()))
    };
    input.id(search_focus_id()).into()
}

/// The filter choices, in the order both densities present them: the rail
/// renders the icon, the expanded row renders the label, and both name the
/// control identically for assistive technologies.
const FILTER_CHOICES: [(SidebarFilter, &str, &str, &str); 3] = [
    (
        SidebarFilter::All,
        "view-list-symbolic",
        "All",
        "Show all projects",
    ),
    (
        SidebarFilter::Attention,
        "dialog-warning-symbolic",
        "Attention",
        "Show projects needing attention",
    ),
    (
        SidebarFilter::PullRequests,
        "vcs-merge-request-symbolic",
        "PRs",
        "Show projects with pull requests",
    ),
];

fn filters(filter: SidebarFilter) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut row = widget::row::with_capacity(FILTER_CHOICES.len())
        .spacing(spacing.space_xxxs)
        .align_y(Alignment::Center);
    for (value, _, visible_label, accessible_label) in FILTER_CHOICES {
        row = row.push(
            button::custom(widget::text::caption(visible_label))
                .class(theme::Button::Text)
                .id(filter_id(value))
                .name(accessible_label)
                .selected(filter == value)
                .on_press(SidebarAction::SetFilter(value)),
        );
    }
    row.into()
}

/// The filter as an icon column, which is all the rail has room for. Without
/// it a filter chosen while expanded would keep hiding projects with no
/// control left to clear it.
fn compact_filters(filter: SidebarFilter) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut column = widget::column::with_capacity(FILTER_CHOICES.len())
        .spacing(spacing.space_xxxs)
        .align_x(Alignment::Center)
        .width(Length::Fill);
    for (value, icon_name, _, accessible_label) in FILTER_CHOICES {
        column = column.push(
            button::icon(icon::from_name(icon_name).size(ROW_ICON_SIZE))
                .id(filter_id(value))
                .name(accessible_label)
                .tooltip(accessible_label)
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .selected(filter == value)
                .on_press(SidebarAction::SetFilter(value)),
        );
    }
    column.into()
}

fn body<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Element<'a, SidebarAction> {
    match &model.data_state {
        SidebarDataState::Loading if !model.projects.is_empty() => {
            project_list(model, state, compact)
        }
        SidebarDataState::Loading => loading(compact),
        SidebarDataState::Error(message) => error(message, compact),
        SidebarDataState::Ready => project_list(model, state, compact),
    }
}

/// The scrollable project list. `responsive` is avoided here because it does
/// not forward child accessibility nodes, and every row in this list is a
/// named a11y target.
fn project_list<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Element<'a, SidebarAction> {
    let projects = filtered_projects(model, state, compact);
    if projects.is_empty() {
        return empty(state, compact);
    }

    // Search results are always revealed. Outside search, projects start open
    // and the set stores only the user's explicit collapses. The rail renders
    // no search input and applies no query, so nothing there is a result.
    let searching = !compact && !state.query.trim().is_empty();
    let spacing = cosmic::theme::spacing();
    let mut column = widget::column::with_capacity(projects.len()).spacing(if compact {
        spacing.space_xxxs
    } else {
        spacing.space_xxs
    });
    for project in &projects {
        let project_id = project.project.id;
        column = column.push(project_view(
            project,
            searching || !state.collapsed_projects.contains(&project_id),
            searching || state.expanded_pull_requests.contains(&project_id),
            compact,
            state.selection,
        ));
    }

    widget::scrollable(column)
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn loading(compact: bool) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let content = widget::column::with_children(vec![
        widget::progress_bar::circular::Circular::new()
            .size(f32::from(STATUS_ICON_SIZE))
            .into(),
        widget::text::caption(if compact {
            "Loading"
        } else {
            "Loading projects…"
        })
        .into(),
    ])
    .align_x(Alignment::Center)
    .spacing(spacing.space_xxs);
    widget::container(content)
        .center_x(Length::Fill)
        .padding(spacing.space_xxs)
        .width(Length::Fill)
        .into()
}
fn error<'a>(message: &'a str, compact: bool) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        return widget::column::with_children(vec![
            widget::icon::from_name("dialog-warning-symbolic")
                .size(STATUS_ICON_SIZE)
                .into(),
            icon_action(
                "view-refresh-symbolic",
                format!("Couldn't load projects. {message}. Try again"),
                SidebarAction::Refresh,
                stable_id("error.retry"),
            ),
        ])
        .spacing(spacing.space_xxs)
        .align_x(Alignment::Center)
        .into();
    }
    widget::container(
        widget::column::with_children(vec![
            widget::icon::from_name("dialog-warning-symbolic")
                .size(STATUS_ICON_SIZE)
                .into(),
            widget::text::heading("Couldn’t load projects").into(),
            widget::text::caption(message)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            button::standard("Try again")
                .id(stable_id("error.retry"))
                .on_press(SidebarAction::Refresh)
                .into(),
        ])
        .spacing(spacing.space_xxs),
    )
    .padding(spacing.space_xs)
    .width(Length::Fill)
    .into()
}

fn empty(state: &SidebarState, compact: bool) -> Element<'static, SidebarAction> {
    // The rail neither renders the search input nor applies its query, so a
    // query is never what emptied the list there.
    let searching = !compact && !state.query.trim().is_empty();
    let (icon_name, title, detail, action_label, action) = if searching {
        (
            "edit-find-symbolic",
            "No matches",
            "Try a different project, branch, or path.",
            "Clear search",
            SidebarAction::SearchChanged(String::new()),
        )
    } else {
        match state.filter {
            SidebarFilter::All => (
                "folder-open-symbolic",
                "No projects yet",
                "Open a repository to add its worktrees.",
                "Open repository",
                SidebarAction::OpenRepositoryPicker,
            ),
            SidebarFilter::Attention => (
                "emblem-ok-symbolic",
                "No projects need attention",
                "Everything currently shown is up to date.",
                "Show all",
                SidebarAction::SetFilter(SidebarFilter::All),
            ),
            SidebarFilter::PullRequests => (
                "vcs-merge-request-symbolic",
                "No pull requests found",
                "Open a repository or change the current filter.",
                "Show all",
                SidebarAction::SetFilter(SidebarFilter::All),
            ),
        }
    };
    if compact {
        return icon_action(
            icon_name,
            format!("{title}. {detail} {action_label}"),
            action,
            stable_id("empty.action"),
        );
    }
    let spacing = cosmic::theme::spacing();
    let action_button: Element<'static, SidebarAction> =
        if !searching && state.filter == SidebarFilter::All {
            button::suggested(action_label)
                .id(stable_id("empty.action"))
                .on_press(action)
                .into()
        } else {
            button::standard(action_label)
                .id(stable_id("empty.action"))
                .on_press(action)
                .into()
        };
    widget::container(
        widget::column::with_children(vec![
            widget::icon::from_name(icon_name)
                .size(STATUS_ICON_SIZE)
                .into(),
            widget::text::heading(title).into(),
            widget::text::caption(detail)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            action_button,
        ])
        .spacing(spacing.space_xxs),
    )
    .padding(spacing.space_xs)
    .width(Length::Fill)
    .into()
}

fn project_view<'a>(
    project: &FilteredProject<'a>,
    expanded: bool,
    pull_requests_expanded: bool,
    compact: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let entity = project.project;
    let chrome = &entity.chrome;
    let project_id = entity.id;
    if compact {
        return compact_project(project, selection);
    }

    let spacing = cosmic::theme::spacing();
    let worktree_count = project.worktrees.len();
    let pull_request_count = project.pull_requests.len();
    let has_children = worktree_count > 0 || pull_request_count > 0;
    let project_identity = button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon::from_name("folder-symbolic").size(ROW_ICON_SIZE))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            widget::text::caption_heading(entity.name.as_str())
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs),
    )
    .class(theme::Button::Text)
    .id(chrome.open_id.clone())
    .name(&*chrome.open_name)
    .selected(selection == Some(SidebarSelection::Project(project_id)))
    .padding([0, spacing.space_xxxs])
    .height(Length::Fixed(PROJECT_HEADER_HEIGHT))
    .width(Length::Fill)
    .on_press(SidebarAction::OpenRepository {
        project_id,
        repository: entity.repository.clone(),
    });
    let project_identity: Element<'a, SidebarAction> = widget::tooltip::tooltip(
        project_identity,
        widget::text::caption(entity.repository.display().to_string()),
        widget::tooltip::Position::Bottom,
    )
    .into();

    let leading: Element<'a, SidebarAction> = if has_children {
        icon_action(
            if expanded {
                "go-down-symbolic"
            } else {
                "go-next-symbolic"
            },
            if expanded {
                &*chrome.collapse_name
            } else {
                &*chrome.expand_name
            },
            SidebarAction::ToggleProject(project_id),
            chrome.toggle_id.clone(),
        )
    } else {
        cosmic::iced::widget::Space::new()
            .width(Length::Fixed(ICON_CONTROL_SIZE))
            .height(Length::Fixed(ICON_CONTROL_SIZE))
            .into()
    };
    let header = widget::row::with_children(vec![
        leading,
        project_identity,
        icon_action(
            "list-add-symbolic",
            &*chrome.new_worktree_name,
            SidebarAction::NewWorktree {
                project_id,
                repository: entity.repository.clone(),
            },
            chrome.new_worktree_id.clone(),
        ),
        icon_action(
            "document-new-symbolic",
            &*chrome.create_pull_request_name,
            SidebarAction::CreatePullRequest {
                project_id,
                repository: entity.repository.clone(),
            },
            chrome.create_pull_request_id.clone(),
        ),
    ])
    .align_y(Alignment::Center)
    .spacing(spacing.space_xxxs)
    .height(Length::Fixed(PROJECT_HEADER_HEIGHT));
    let mut column = widget::column::with_capacity(worktree_count + 2)
        .spacing(spacing.space_xxxs)
        .push(header);

    if expanded {
        for &worktree in &project.worktrees {
            column = column.push(
                widget::container(worktree_view(
                    project_id,
                    worktree,
                    is_primary_worktree(&entity.repository, &worktree.path),
                    selection,
                ))
                .padding([0, 0, 0, spacing.space_l]),
            );
        }
        if pull_request_count > 0 {
            column = column.push(
                widget::container(pull_request_group(
                    entity,
                    &project.pull_requests,
                    pull_requests_expanded,
                    selection,
                ))
                .padding([spacing.space_xxxs, 0, 0, spacing.space_l]),
            );
        }
    }

    widget::container(column).width(Length::Fill).into()
}

fn is_primary_worktree(repository: &Path, worktree: &Path) -> bool {
    repository == worktree
        || (repository.file_name().is_some_and(|name| name == ".git")
            && repository.parent() == Some(worktree))
}

fn compact_project<'a>(
    project: &FilteredProject<'a>,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let entity = project.project;
    let chrome = &entity.chrome;
    let selected = matches!(
        selection,
        Some(SidebarSelection::Project(project_id))
            | Some(SidebarSelection::Worktree { project_id, .. })
            | Some(SidebarSelection::PullRequest { project_id, .. })
            if project_id == entity.id
    );
    let action = button::icon(icon::from_name("folder-symbolic").size(ROW_ICON_SIZE))
        .id(chrome.open_id.clone())
        .name(&*chrome.compact_open_name)
        .tooltip(&*chrome.compact_open_name)
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .selected(selected)
        .on_press(SidebarAction::OpenRepository {
            project_id: entity.id,
            repository: entity.repository.clone(),
        });
    widget::container(action)
        .center_x(Length::Fill)
        .padding(cosmic::theme::spacing().space_xxxs)
        .width(Length::Fill)
        .into()
}

fn worktree_view<'a>(
    project_id: ProjectId,
    worktree: &'a SidebarWorktree,
    primary: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &worktree.chrome;
    let spacing = cosmic::theme::spacing();
    let mut row = widget::row::with_capacity(8)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .push(
            widget::container(widget::icon::from_name("vcs-branch-symbolic").size(ROW_ICON_SIZE))
                .center_x(Length::Fixed(ROW_ICON_WIDTH)),
        )
        .push(
            widget::text::body(&*chrome.label)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill),
        );
    if primary {
        row = row.push(badge("primary", BadgeTone::Neutral));
    }
    if worktree.detached {
        row = row.push(badge("detached", BadgeTone::Neutral));
    }
    if let Some(label) = chrome.dirty_badge.as_deref() {
        row = row.push(badge(label, BadgeTone::Warning));
    }
    if let Some(label) = chrome.ahead_badge.as_deref() {
        row = row.push(badge(label, BadgeTone::Accent));
    }
    if let Some(label) = chrome.behind_badge.as_deref() {
        row = row.push(badge(label, BadgeTone::Warning));
    }
    if worktree.locked {
        row = row.push(badge("locked", BadgeTone::Neutral));
    }

    let action = button::custom(row)
        .class(theme::Button::Text)
        .id(chrome.open_id.clone())
        .selected(
            selection
                == Some(SidebarSelection::Worktree {
                    project_id,
                    worktree_id: worktree.id,
                }),
        )
        .name(&*chrome.accessible_name)
        .padding([0, spacing.space_xxxs])
        .height(Length::Fixed(WORKTREE_ROW_HEIGHT))
        .width(Length::Fill)
        .on_press(SidebarAction::OpenWorktree {
            project_id,
            worktree_id: worktree.id,
            path: worktree.path.clone(),
        });
    widget::tooltip::tooltip(
        action,
        widget::text::caption(&*chrome.tooltip),
        widget::tooltip::Position::Bottom,
    )
    .into()
}

fn pull_request_group<'a>(
    project: &'a SidebarProject,
    pull_requests: &[&'a SidebarPullRequest],
    expanded: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &project.chrome;
    let spacing = cosmic::theme::spacing();
    let mut column = widget::column::with_capacity(pull_requests.len() + 1)
        .spacing(spacing.space_xxxs)
        .push(
            widget::row::with_children(vec![
                icon_action(
                    if expanded {
                        "go-down-symbolic"
                    } else {
                        "go-next-symbolic"
                    },
                    if expanded {
                        &*chrome.collapse_pull_requests_name
                    } else {
                        &*chrome.expand_pull_requests_name
                    },
                    SidebarAction::TogglePullRequests(project.id),
                    chrome.toggle_pull_requests_id.clone(),
                ),
                widget::text::caption_heading(format!("Pull requests ({})", pull_requests.len()))
                    .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxxs),
        );
    if expanded {
        for &pull_request in pull_requests {
            column = column.push(pull_request_view(project.id, pull_request, selection));
        }
    }
    column.into()
}

fn pull_request_view<'a>(
    project_id: ProjectId,
    pull_request: &'a SidebarPullRequest,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &pull_request.chrome;
    let action = SidebarAction::OpenPullRequest {
        project_id,
        number: pull_request.number,
        url: pull_request.url.clone(),
    };

    let spacing = cosmic::theme::spacing();
    let check_label = check_description(pull_request.checks);
    let mut detail = widget::row::with_children(vec![
        widget::text::caption(&*chrome.metadata)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into(),
        badge(check_label, check_tone(pull_request.checks)),
    ])
    .align_y(Alignment::Center)
    .spacing(spacing.space_xxxs);
    if pull_request.draft {
        detail = detail.push(badge("draft", BadgeTone::Neutral));
    }
    let row = widget::row::with_children(vec![
        widget::container(
            widget::icon::from_name("vcs-merge-request-symbolic").size(ROW_ICON_SIZE),
        )
        .center_x(Length::Fixed(ROW_ICON_WIDTH))
        .into(),
        widget::column::with_children(vec![
            widget::text::body(pull_request.title.as_str())
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
            detail.into(),
        ])
        .spacing(0)
        .width(Length::Fill)
        .into(),
    ])
    .spacing(spacing.space_xxxs)
    .align_y(Alignment::Center);

    let action = button::custom(row)
        .class(theme::Button::Text)
        .id(chrome.open_id.clone())
        .name(&*chrome.accessible_name)
        .selected(
            selection
                == Some(SidebarSelection::PullRequest {
                    project_id,
                    number: pull_request.number,
                }),
        )
        .padding([0, spacing.space_xxs])
        .height(Length::Fixed(PULL_REQUEST_ROW_HEIGHT))
        .width(Length::Fill)
        .on_press(action);
    widget::tooltip::tooltip(
        action,
        widget::text::caption(pull_request.title.as_str()),
        widget::tooltip::Position::Bottom,
    )
    .into()
}

fn icon_action<'a>(
    icon_name: &'static str,
    label: impl Into<Cow<'a, str>>,
    action: SidebarAction,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    button::icon(icon::from_name(icon_name).size(ROW_ICON_SIZE))
        .id(id)
        .name(label.clone())
        .tooltip(label)
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .on_press(action)
        .into()
}

/// The rule between the pane and the workspace, doubling as the pane's resize
/// handle.
///
/// A widget of its own rather than `widget::mouse_area`, because `mouse_area`
/// reports pointer motion only while the pointer is inside its own bounds: a
/// handle this narrow would lose the gesture on the first pixel of travel. A
/// widget sees every event, so the grab survives the pointer leaving it.
fn resize_handle(width: f32, modal_open: bool) -> ResizeHandle<'static> {
    ResizeHandle {
        content: widget::container(widget::divider::vertical::default())
            .center_x(Length::Fixed(RESIZE_HANDLE_WIDTH))
            .height(Length::Fill)
            .into(),
        width,
        modal_open,
        id: resize_handle_id(),
    }
}

struct ResizeHandle<'a> {
    content: Element<'a, SidebarAction>,
    /// The width the pane renders at, and the origin every gesture measures
    /// from. Owned by the application, so the handle stores nothing itself.
    width: f32,
    /// Set while a dialog is on screen, when the keyboard is not ours to read.
    modal_open: bool,
    id: Id,
}

/// Where a drag began. The width is captured with the pointer so the pane
/// tracks the pointer exactly instead of accumulating each frame's rounding.
#[derive(Debug, Clone, Copy)]
struct ResizeOrigin {
    pointer_x: f32,
    width: f32,
}

#[derive(Debug, Default)]
struct ResizeHandleState {
    drag: Option<ResizeOrigin>,
    is_focused: bool,
    /// Set while an arrow key repeats. A held key is one gesture, so the
    /// width settles when it is released rather than on every repeat.
    repeating: bool,
}

impl operation::Focusable for ResizeHandleState {
    fn is_focused(&self) -> bool {
        self.is_focused
    }

    fn focus(&mut self) {
        self.is_focused = true;
    }

    fn unfocus(&mut self) {
        self.is_focused = false;
    }
}

impl ResizeHandle<'_> {
    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("resize handle content layout")
            .with_virtual_offset(layout.virtual_offset())
    }

    fn resize(&self, width: f32) -> SidebarAction {
        SidebarAction::Resize {
            width: clamp_expanded_width(width),
        }
    }
}

/// The width a drag requests: the pointer's travel added to the width the
/// gesture began at. Measured from the origin rather than the previous frame,
/// so a pointer returning to where it started restores the width it started
/// with, whatever the clamp did in between.
fn dragged_width(origin: ResizeOrigin, pointer_x: f32) -> f32 {
    clamp_expanded_width(origin.width + pointer_x - origin.pointer_x)
}

impl Widget<SidebarAction, Theme, Renderer> for ResizeHandle<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<ResizeHandleState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(ResizeHandleState::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let content = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        layout::Node::with_children(content.size(), vec![content])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let bounds = layout.bounds();
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
        let state = tree.state.downcast_mut::<ResizeHandleState>();
        operation.focusable(Some(&self.id), bounds, state);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, SidebarAction>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<ResizeHandleState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                // Focus follows the press, and is surrendered when the press
                // lands elsewhere, exactly as libcosmic's buttons do: the
                // arrow keys must never resize the pane from a text field.
                state.is_focused = cursor.is_over(bounds);
                if let Some(position) = cursor.position().filter(|_| state.is_focused) {
                    state.drag = Some(ResizeOrigin {
                        pointer_x: position.x,
                        width: self.width,
                    });
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if let Some(origin) = state.drag {
                    shell.publish(SidebarAction::Resize {
                        width: dragged_width(origin, position.x),
                    });
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.drag.take().is_some() {
                    shell.publish(SidebarAction::ResizeFinished);
                    shell.capture_event();
                    return;
                }
            }
            // The release that ends a drag is delivered to whichever surface
            // owns the pointer, so a lost window ends the grab instead.
            Event::Window(window::Event::Unfocused) => {
                let dragging = state.drag.take().is_some();
                let repeating = std::mem::take(&mut state.repeating);
                if dragging || repeating {
                    shell.publish(SidebarAction::ResizeFinished);
                }
            }
            // A single keystroke is a whole gesture, so it settles the width
            // at once. A held key is one gesture still under way: settling on
            // every repeat would rewrite and fsync the settings file at the
            // key-repeat rate, so a repeated gesture settles on release.
            // An arrow carrying a modifier belongs to whoever reserved that
            // chord - `Alt+Left` and `Alt+Right` navigate columns by default -
            // so only a bare arrow resizes the pane. The four are read
            // individually rather than as `is_empty`, which would also consult
            // the lock flags this iced fork carries: a user holding caps lock
            // would silently lose the gesture.
            //
            // A dialog on screen takes the keyboard even though this widget
            // keeps the focus flag it had when the dialog opened: an arrow
            // aimed at a dialog control must not quietly retune the pane and
            // write the settings file behind it.
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                repeat,
                ..
            }) if state.is_focused && !self.modal_open && !chorded(*modifiers) => {
                if let Some(step) = resize_step(key) {
                    shell.publish(self.resize(self.width + step));
                    if *repeat {
                        state.repeating = true;
                    } else {
                        shell.publish(SidebarAction::ResizeFinished);
                    }
                    shell.capture_event();
                    return;
                }
            }
            Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if state.repeating => {
                if resize_step(key).is_some() {
                    state.repeating = false;
                    shell.publish(SidebarAction::ResizeFinished);
                    shell.capture_event();
                    return;
                }
            }
            Event::A11y(id, request) if *id == self.id => {
                use iced_accessibility::accesskit::Action;
                match request.action {
                    Action::Focus => state.is_focused = true,
                    Action::Blur => state.is_focused = false,
                    // A dialog is not ours to resize behind, whoever asks:
                    // the keyboard path stands down for one, and an
                    // assistive technology reaches this widget by another
                    // road rather than with another right.
                    Action::Increment if !self.modal_open => {
                        shell.publish(self.resize(self.width + RESIZE_STEP));
                        shell.publish(SidebarAction::ResizeFinished);
                    }
                    Action::Decrement if !self.modal_open => {
                        shell.publish(self.resize(self.width - RESIZE_STEP));
                        shell.publish(SidebarAction::ResizeFinished);
                    }
                    _ => {}
                }
                shell.capture_event();
                return;
            }
            _ => {}
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            Self::child_layout(layout),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<ResizeHandleState>();
        if state.drag.is_some() || cursor.is_over(layout.bounds()) {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            Self::child_layout(layout),
            cursor,
            viewport,
        );
        // The rule alone cannot show focus: it looks the same either way.
        if tree.state.downcast_ref::<ResizeHandleState>().is_focused {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                cosmic::iced::Background::Color(theme.cosmic().accent.base.into()),
            );
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: cosmic::iced::Vector,
    ) -> Option<overlay::Element<'a, SidebarAction, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _state: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Action, Node, Rect, Role};

        let Rectangle {
            x,
            y,
            width,
            height,
        } = layout.bounds();
        let mut node = Node::new(Role::Splitter);
        node.add_action(Action::Focus);
        node.add_action(Action::Increment);
        node.add_action(Action::Decrement);
        node.set_bounds(Rect::new(
            f64::from(x),
            f64::from(y),
            f64::from(x + width),
            f64::from(y + height),
        ));
        node.set_label(RESIZE_HANDLE_NAME);
        node.set_numeric_value(f64::from(self.width));
        node.set_min_numeric_value(f64::from(EXPANDED_WIDTH_MIN));
        node.set_max_numeric_value(f64::from(EXPANDED_WIDTH_MAX));
        node.set_numeric_value_step(f64::from(RESIZE_STEP));
        iced_accessibility::A11yTree::leaf(node, self.id.clone())
    }

    fn id(&self) -> Option<Id> {
        Some(self.id.clone())
    }

    fn set_id(&mut self, id: Id) {
        self.id = id;
    }
}

impl<'a> From<ResizeHandle<'a>> for Element<'a, SidebarAction> {
    fn from(handle: ResizeHandle<'a>) -> Self {
        Self::new(handle)
    }
}

/// The width delta a key requests, or `None` for keys the handle ignores.
fn resize_step(key: &keyboard::Key) -> Option<f32> {
    match key {
        keyboard::Key::Named(keyboard::key::Named::ArrowLeft) => Some(-RESIZE_STEP),
        keyboard::Key::Named(keyboard::key::Named::ArrowRight) => Some(RESIZE_STEP),
        _ => None,
    }
}

/// Whether a keystroke carries a modifier that could make it someone's chord.
///
/// The four are read individually because `Modifiers::is_empty` also consults
/// the lock flags, which say what the keyboard is latched to rather than what
/// the user is holding down.
fn chorded(modifiers: keyboard::Modifiers) -> bool {
    modifiers.control() || modifiers.alt() || modifiers.shift() || modifiers.logo()
}

#[derive(Debug, Clone, Copy)]
enum BadgeTone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}

fn badge<'a>(label: impl Into<Cow<'a, str>> + 'a, tone: BadgeTone) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    widget::container(
        widget::text::caption(label).wrapping(cosmic::iced::widget::text::Wrapping::None),
    )
    .class(theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let component = match tone {
            BadgeTone::Neutral => &cosmic.secondary(false).component,
            BadgeTone::Accent => &cosmic.accent,
            BadgeTone::Success => &cosmic.success,
            BadgeTone::Warning => &cosmic.warning,
            BadgeTone::Danger => &cosmic.destructive,
        };
        cosmic::iced::widget::container::Style {
            icon_color: Some(component.on.into()),
            text_color: Some(component.on.into()),
            background: Some(cosmic::iced::Background::Color(match tone {
                BadgeTone::Neutral => cosmic::iced::Color::TRANSPARENT,
                _ => component.base.into(),
            })),
            border: cosmic::iced::Border {
                radius: cosmic.corner_radii.radius_s.into(),
                width: 1.0,
                color: component.border.into(),
            },
            shadow: cosmic::iced::Shadow::default(),
            snap: true,
        }
    }))
    .padding([spacing.space_none, spacing.space_xxxs])
    .width(Length::Shrink)
    .into()
}

fn status_summary(
    detached: bool,
    locked: bool,
    dirty: Option<&DirtyState>,
    ahead: Option<u64>,
    behind: Option<u64>,
) -> String {
    let mut parts = Vec::with_capacity(4);
    match dirty {
        Some(dirty) if dirty.is_dirty() => parts.push(format!("{} changes", dirty_count(dirty))),
        Some(_) => parts.push("clean".to_owned()),
        None => parts.push("status unavailable".to_owned()),
    }
    if detached {
        parts.push("detached".to_owned());
    }
    if let Some(ahead) = ahead.filter(|ahead| *ahead > 0) {
        parts.push(format!("{ahead} ahead"));
    }
    if let Some(behind) = behind.filter(|behind| *behind > 0) {
        parts.push(format!("{behind} behind"));
    }
    if locked {
        parts.push("locked".to_owned());
    }
    parts.join(", ")
}

fn dirty_count(dirty: &DirtyState) -> u64 {
    u64::from(dirty.staged)
        + u64::from(dirty.unstaged)
        + u64::from(dirty.untracked)
        + u64::from(dirty.conflicted)
        + u64::from(dirty.submodule)
}

fn check_tone(status: CheckStatus) -> BadgeTone {
    match status {
        CheckStatus::Passing => BadgeTone::Success,
        CheckStatus::Pending => BadgeTone::Warning,
        CheckStatus::Failing => BadgeTone::Danger,
        CheckStatus::Unknown => BadgeTone::Neutral,
    }
}

fn check_description(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Passing => "passing",
        CheckStatus::Pending => "pending",
        CheckStatus::Failing => "failing",
        CheckStatus::Unknown => "unknown",
    }
}

#[derive(Debug)]
struct FilteredProject<'a> {
    project: &'a SidebarProject,
    worktrees: Vec<&'a SidebarWorktree>,
    pull_requests: Vec<&'a SidebarPullRequest>,
}

/// The projects each density shows. `compact` is the density the caller
/// renders, not a hint: the rail has no search input, so a query typed while
/// expanded must not keep hiding projects once the window narrows. The filter
/// has an icon control in both densities, so it always applies.
fn filtered_projects<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
    compact: bool,
) -> Vec<FilteredProject<'a>> {
    let query = if compact {
        String::new()
    } else {
        state.query.trim().to_ascii_lowercase()
    };
    model
        .projects
        .iter()
        .filter_map(|project| {
            let project_matches = query.is_empty() || project.matches(&query);
            let worktrees = project
                .worktrees
                .iter()
                .filter(|worktree| match state.filter {
                    SidebarFilter::All => true,
                    SidebarFilter::Attention => worktree.needs_attention(),
                    SidebarFilter::PullRequests => false,
                })
                .filter(|worktree| project_matches || worktree.matches(&query))
                .collect::<Vec<_>>();
            let pull_requests = project
                .pull_requests
                .iter()
                .filter(|pull_request| match state.filter {
                    SidebarFilter::All | SidebarFilter::PullRequests => true,
                    SidebarFilter::Attention => pull_request.needs_attention(),
                })
                .filter(|pull_request| project_matches || pull_request.matches(&query))
                .collect::<Vec<_>>();
            let visible = match state.filter {
                SidebarFilter::All => {
                    project_matches || !worktrees.is_empty() || !pull_requests.is_empty()
                }
                SidebarFilter::Attention => !worktrees.is_empty() || !pull_requests.is_empty(),
                SidebarFilter::PullRequests => !pull_requests.is_empty(),
            };
            visible.then_some(FilteredProject {
                project,
                worktrees,
                pull_requests,
            })
        })
        .collect()
}

/// Case-folds a filter key. Entities store the result so the free-text filter
/// never folds a candidate while rendering.
fn fold(value: &str) -> Box<str> {
    value.to_ascii_lowercase().into_boxed_str()
}

fn is_check_failure(value: &str) -> bool {
    matches!(
        value,
        "fail"
            | "failure"
            | "failed"
            | "error"
            | "cancel"
            | "cancelled"
            | "canceled"
            | "timed_out"
            | "action_required"
    )
}

fn is_check_pending(value: &str) -> bool {
    matches!(
        value,
        "pending" | "queued" | "in_progress" | "waiting" | "expected"
    )
}

fn is_check_success(value: &str) -> bool {
    matches!(
        value,
        "pass" | "success" | "successful" | "skipping" | "skipped" | "neutral"
    )
}

fn toggle_membership<T: Ord + Copy>(set: &mut BTreeSet<T>, value: T) {
    if !set.remove(&value) {
        set.insert(value);
    }
}

fn stable_id(value: impl Into<String>) -> Id {
    Id::new(format!("sidebar.{}", value.into()))
}

fn entity_id(kind: &str, id: impl std::fmt::Display) -> Id {
    stable_id(format!("{kind}.{id}"))
}

fn filter_id(filter: SidebarFilter) -> Id {
    stable_id(match filter {
        SidebarFilter::All => "filter.all",
        SidebarFilter::Attention => "filter.attention",
        SidebarFilter::PullRequests => "filter.pull_requests",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{github::RemoteValue, repository::WorktreeLock};
    use uuid::Uuid;

    fn project_id(value: u128) -> ProjectId {
        ProjectId::from_uuid(Uuid::from_u128(value))
    }

    fn worktree(id: u128, branch: &str, dirty: bool) -> SidebarWorktree {
        SidebarWorktree::from_repository(
            &WorktreeRecord {
                id: WorktreeId::from_uuid(Uuid::from_u128(id)),
                path: PathBuf::from(format!("/repo/{branch}")),
                head_oid: None,
                branch: Some(format!("refs/heads/{branch}")),
                detached: false,
                bare: false,
                lock: WorktreeLock::default(),
                prunable_reason: None,
            },
            Some(&StatusSnapshot {
                head: crate::repository::HeadState::Unknown,
                tracking: TrackingState::NoUpstream,
                dirty: DirtyState {
                    unstaged: if dirty { 1 } else { 0 },
                    ..DirtyState::default()
                },
            }),
        )
    }

    fn project() -> SidebarProject {
        SidebarProject::new(
            project_id(1),
            RepositoryId(Uuid::from_u128(10)),
            "Cagents".into(),
            "/repo/cagents".into(),
            vec![worktree(2, "main", false), worktree(3, "fix-sidebar", true)],
            vec![SidebarPullRequest::new(
                project_id(1),
                42,
                "Polish sidebar".into(),
                Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
                "fix-sidebar".into(),
                Some("octocat".into()),
                false,
                CheckStatus::Failing,
            )],
        )
    }

    /// The widget identities of the controls an element renders, in tree
    /// order.
    ///
    /// An identity is what the module derives once per entity, and what
    /// focus, tooltips and assistive technologies address a control by, so
    /// the set of them is the set of controls a density offers. It is read
    /// off the widget tree, which a test can build; accessible names cannot
    /// be, because `widget::tooltip` reports no a11y nodes of its own and
    /// every icon control in this module carries a tooltip.
    fn control_ids(element: &Element<'_, SidebarAction>) -> Vec<String> {
        fn collect(tree: &Tree, ids: &mut Vec<String>) {
            ids.extend(tree.id.as_ref().map(ToString::to_string));
            for child in &tree.children {
                collect(child, ids);
            }
        }

        let mut ids = Vec::new();
        collect(&Tree::new(element), &mut ids);
        ids
    }

    #[test]
    fn no_two_controls_in_one_frame_share_an_identity() {
        // Two widgets carrying the same custom id are not a cosmetic clash.
        // The runtime keeps state for named widgets in a map keyed by that
        // name, and drains it before every diff: with a name repeated, one node
        // is handed nothing back, keeps the tag that says it is stateful, and
        // the next focus traversal or draw panics on the downcast. The header's
        // command for the selected project and that project's own row are the
        // pair that collided, so selection is part of the cross product.
        // More than one of everything: a constant id minted inside a
        // per-entity view renders once with a thin fixture and passes, so the
        // pane carries two projects, and the first has two worktrees and two
        // pull requests.
        let mut crowded = project();
        crowded.pull_requests.push(SidebarPullRequest::new(
            project_id(1),
            43,
            "Rename the pane".into(),
            Url::parse("https://github.com/example/cagents/pull/43").unwrap(),
            "main".into(),
            None,
            true,
            CheckStatus::Passing,
        ));
        let other = SidebarProject::new(
            project_id(4),
            RepositoryId(Uuid::from_u128(11)),
            "Orca".into(),
            "/repo/orca".into(),
            vec![worktree(5, "main", false)],
            vec![SidebarPullRequest::new(
                project_id(4),
                7,
                "Trim the deps".into(),
                Url::parse("https://github.com/example/orca/pull/7").unwrap(),
                "main".into(),
                Some("octocat".into()),
                false,
                CheckStatus::Passing,
            )],
        );
        let populated = vec![crowded, other];
        let selections = [
            None,
            Some(SidebarSelection::Project(project_id(1))),
            Some(SidebarSelection::Worktree {
                project_id: project_id(1),
                worktree_id: populated[0].worktrees[0].id,
            }),
            Some(SidebarSelection::PullRequest {
                project_id: project_id(1),
                number: 42,
            }),
        ];

        for mode in [SidebarMode::Compact, SidebarMode::Expanded] {
            // Every data state carries the projects it already had: a failed
            // refresh replaces the body and keeps the list, which is the state
            // the collision was found in.
            for data_state in [
                SidebarDataState::Ready,
                SidebarDataState::Loading,
                SidebarDataState::Error("git exited with status 128".to_owned()),
            ] {
                for projects in [Vec::new(), populated.clone()] {
                    let model = SidebarModel {
                        mode,
                        data_state: data_state.clone(),
                        projects,
                    };
                    for selection in &selections {
                        for controls_expanded in [false, true] {
                            let mut state = SidebarState {
                                controls_expanded,
                                selection: *selection,
                                ..SidebarState::default()
                            };
                            state.apply(&SidebarAction::TogglePullRequests(project_id(1)));
                            state.apply(&SidebarAction::TogglePullRequests(project_id(4)));

                            let element = view(&model, &state, mode, DEFAULT_EXPANDED_WIDTH, false);
                            let mut seen = BTreeSet::new();
                            for id in control_ids(&element) {
                                // Only named identities share the runtime's
                                // map; an unnamed widget gets a fresh unique
                                // id per instance, which `Display` renders as
                                // "Undefined" and which cannot collide.
                                if id == "Undefined" {
                                    continue;
                                }
                                assert!(
                                    seen.insert(id.clone()),
                                    "{id} is rendered twice at {mode:?} \
                                     with {} project(s), {data_state:?}, \
                                     selection {selection:?}, controls \
                                     {controls_expanded}",
                                    model.projects.len()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn local_state_transitions_are_idempotent_and_scoped() {
        let id = project_id(1);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleProject(id));
        assert!(state.collapsed_projects.contains(&id));
        state.apply(&SidebarAction::ToggleProject(id));
        assert!(!state.collapsed_projects.contains(&id));

        state.apply(&SidebarAction::ToggleControls);
        state.apply(&SidebarAction::TogglePullRequests(id));
        state.apply(&SidebarAction::SearchChanged(" Sidebar ".into()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(state.controls_expanded());
        assert!(state.expanded_pull_requests.contains(&id));
        assert_eq!(state.query(), " Sidebar ");
        assert_eq!(state.filter(), SidebarFilter::Attention);
    }

    #[test]
    fn filtering_matches_descendants_and_attention_state() {
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged("FIX-SIDEBAR".into()));
        let filtered = filtered_projects(&model, &state, false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].worktrees.len(), 1);
        assert_eq!(filtered[0].pull_requests.len(), 1);

        state.apply(&SidebarAction::SearchChanged(String::new()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        let filtered = filtered_projects(&model, &state, false);
        assert_eq!(filtered[0].worktrees.len(), 1);
        assert_eq!(
            filtered[0].worktrees[0].branch.as_deref(),
            Some("fix-sidebar")
        );
        assert_eq!(filtered[0].pull_requests.len(), 1);
    }

    #[test]
    fn pull_request_filter_hides_worktrees_without_losing_project_context() {
        let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
        let filtered = filtered_projects(&model, &state, true);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].worktrees.is_empty());
        assert_eq!(filtered[0].pull_requests[0].number, 42);
    }

    #[test]
    fn project_search_focus_id_is_stable() {
        assert_eq!(search_focus_id().to_string(), "sidebar.search");
        assert_eq!(search_focus_id().to_string(), search_focus_id().to_string());
    }

    #[test]
    fn the_search_input_carries_the_focus_target_the_application_reveals() {
        let mut state = SidebarState::default();
        assert_eq!(
            search(&state).as_widget().id().map(|id| id.to_string()),
            Some(search_focus_id().to_string())
        );

        // A query adds the clear action, which must not cost the input the id
        // the application focuses.
        state.apply(&SidebarAction::SearchChanged("cagents".into()));
        assert_eq!(
            search(&state).as_widget().id().map(|id| id.to_string()),
            Some(search_focus_id().to_string())
        );
    }

    #[test]
    fn the_search_is_reachable_exactly_when_the_pane_renders_its_input() {
        // The application refuses to focus the input when this is false, so a
        // pane that renders one anyway would leave the shortcut dead, and one
        // that renders none while this is true would blur the focused widget
        // and focus nothing.
        let empty = SidebarModel::ready(SidebarMode::Expanded, Vec::new());
        let populated = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleControls);
        assert!(state.controls_expanded());

        for (model, mode) in [
            (&empty, SidebarMode::Compact),
            (&empty, SidebarMode::Expanded),
            (&populated, SidebarMode::Compact),
            (&populated, SidebarMode::Expanded),
        ] {
            let rendered = control_ids(&view(model, &state, mode, DEFAULT_EXPANDED_WIDTH, false))
                .contains(&search_focus_id().to_string());
            assert_eq!(
                search_is_reachable(model, mode),
                rendered,
                "reachability disagrees with the pane at {mode:?} over {} projects",
                model.projects.len()
            );
        }
    }

    #[test]
    fn each_density_applies_only_the_state_it_gives_a_control() {
        let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
        let chrome = &model.projects[0].chrome;
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::OpenRepository {
            project_id: project_id(1),
            repository: "/repo/cagents".into(),
        });
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.apply(&SidebarAction::SearchChanged(
            "no project answers to this".into(),
        ));

        let rail = control_ids(&view(
            &model,
            &state,
            SidebarMode::Compact,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ));
        for id in [
            stable_id("open-repository"),
            filter_id(SidebarFilter::All),
            filter_id(SidebarFilter::Attention),
            filter_id(SidebarFilter::PullRequests),
            chrome.open_id.clone(),
            chrome.new_worktree_id.clone(),
            chrome.create_pull_request_id.clone(),
            stable_id("settings"),
        ] {
            assert!(
                rail.contains(&id.to_string()),
                "the rail must render {id:?}: a filter it cannot clear hides projects, and the \
                 window's header bar is no route to the commands it drops. It renders {rail:?}"
            );
        }

        // The rail holds no text input, so it renders none and applies no
        // query: the project it lists matches nothing in the query. The
        // expanded pane renders the input, so there the query applies and the
        // pane offers to clear it instead of listing the project.
        assert!(!rail.contains(&search_focus_id().to_string()));
        assert!(!rail.contains(&stable_id("toggle-controls").to_string()));
        assert_eq!(filtered_projects(&model, &state, true).len(), 1);
        assert!(filtered_projects(&model, &state, false).is_empty());

        let pane = control_ids(&view(
            &model,
            &state,
            SidebarMode::Expanded,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ));
        assert!(pane.contains(&stable_id("toggle-controls").to_string()));
        assert!(pane.contains(&stable_id("empty.action").to_string()));
        assert!(!pane.contains(&chrome.open_id.to_string()));
        // The expanded search and filter live behind the toggle, which starts
        // closed.
        assert!(!pane.contains(&search_focus_id().to_string()));
        assert!(!pane.contains(&filter_id(SidebarFilter::Attention).to_string()));

        state.apply(&SidebarAction::ToggleControls);
        let opened = control_ids(&view(
            &model,
            &state,
            SidebarMode::Expanded,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ));
        assert!(opened.contains(&search_focus_id().to_string()));
        assert!(opened.contains(&filter_id(SidebarFilter::Attention).to_string()));
    }

    #[test]
    fn the_controls_toggle_is_offered_only_where_it_opens_something() {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleControls);
        let toggle = stable_id("toggle-controls").to_string();
        let offered = |model: &SidebarModel| {
            control_ids(&view(
                model,
                &state,
                SidebarMode::Expanded,
                DEFAULT_EXPANDED_WIDTH,
                false,
            ))
            .contains(&toggle)
        };

        assert!(offered(&SidebarModel::ready(
            SidebarMode::Expanded,
            vec![project()]
        )));
        // The pane renders the controls only for a non-empty project list, and
        // `SidebarModel::error` empties that list, so in either empty state the
        // toggle would offer, and paint itself selected over, a panel that
        // never opens.
        assert!(!offered(&SidebarModel::ready(
            SidebarMode::Expanded,
            Vec::new()
        )));
        assert!(!offered(&SidebarModel::error(
            SidebarMode::Expanded,
            "disk on fire"
        )));
    }

    #[test]
    fn check_status_prefers_failure_then_pending_then_success() {
        let check = |bucket: &str, state: &str| PullRequestCheck {
            bucket: RemoteValue(bucket.into()),
            completed_at: None,
            description: String::new(),
            event: String::new(),
            link: Url::parse("https://github.com/example/check").unwrap(),
            name: "check".into(),
            started_at: None,
            state: RemoteValue(state.into()),
            workflow: String::new(),
        };
        assert_eq!(
            CheckStatus::from_checks(&[check("pass", "success")]),
            CheckStatus::Passing
        );
        assert_eq!(
            CheckStatus::from_checks(&[check("pending", "queued")]),
            CheckStatus::Pending
        );
        assert_eq!(
            CheckStatus::from_checks(&[check("pending", "queued"), check("fail", "failure")]),
            CheckStatus::Failing
        );
        assert_eq!(CheckStatus::from_checks(&[]), CheckStatus::Unknown);
    }

    #[test]
    fn sidebar_width_uses_compact_rail_and_clamped_persisted_expansion() {
        assert_eq!(effective_width(SidebarMode::Compact, 320.0), COMPACT_WIDTH);
        assert_eq!(
            effective_width(SidebarMode::Expanded, DEFAULT_EXPANDED_WIDTH),
            DEFAULT_EXPANDED_WIDTH
        );
        assert_eq!(
            effective_width(SidebarMode::Expanded, EXPANDED_WIDTH_MIN - 120.0),
            EXPANDED_WIDTH_MIN
        );
        assert_eq!(
            effective_width(SidebarMode::Expanded, EXPANDED_WIDTH_MAX + 120.0),
            EXPANDED_WIDTH_MAX
        );
        assert_eq!(
            effective_width(SidebarMode::Expanded, f32::NAN),
            DEFAULT_EXPANDED_WIDTH
        );
    }

    #[test]
    fn primary_worktree_matches_repository_root_or_git_directory() {
        let root = Path::new("/repo/cagents");
        assert!(is_primary_worktree(root, root));
        assert!(is_primary_worktree(&root.join(".git"), root));
        assert!(!is_primary_worktree(
            &root.join(".git"),
            Path::new("/repo/cagents-worktree")
        ));
    }

    #[test]
    fn dragging_the_handle_tracks_the_pointer_within_the_rendered_bounds() {
        let origin = ResizeOrigin {
            pointer_x: 600.0,
            width: DEFAULT_EXPANDED_WIDTH,
        };

        assert_eq!(dragged_width(origin, 640.0), DEFAULT_EXPANDED_WIDTH + 40.0);
        assert_eq!(dragged_width(origin, 560.0), DEFAULT_EXPANDED_WIDTH - 40.0);
        assert_eq!(dragged_width(origin, 4_000.0), EXPANDED_WIDTH_MAX);
        assert_eq!(dragged_width(origin, 0.0), EXPANDED_WIDTH_MIN);
        // A gesture that returns to its origin restores the width it began
        // at, however far past the bounds it travelled first.
        assert_eq!(dragged_width(origin, 600.0), DEFAULT_EXPANDED_WIDTH);
    }

    #[test]
    fn the_resize_handle_answers_to_the_keyboard_in_whole_steps() {
        let step = |named| resize_step(&keyboard::Key::Named(named));

        assert_eq!(step(keyboard::key::Named::ArrowRight), Some(RESIZE_STEP));
        assert_eq!(step(keyboard::key::Named::ArrowLeft), Some(-RESIZE_STEP));
        assert_eq!(step(keyboard::key::Named::Enter), None);
        assert_eq!(step(keyboard::key::Named::ArrowUp), None);
    }

    #[test]
    fn the_resize_handle_is_a_named_focusable_splitter() {
        use iced_accessibility::accesskit::{Action, Role};

        let handle: Element<'_, SidebarAction> =
            resize_handle(DEFAULT_EXPANDED_WIDTH, false).into();
        assert_eq!(
            handle.as_widget().id().map(|id| id.to_string()),
            Some(resize_handle_id().to_string()),
            "a mouse-only resize is an accessibility regression: the handle must be a focus target"
        );

        let tree = Tree::new(&handle);
        let bounds = Size::new(RESIZE_HANDLE_WIDTH, 480.0);
        let node = layout::Node::with_children(bounds, vec![layout::Node::new(bounds)]);
        let accessible = handle.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            cosmic::iced::advanced::mouse::Cursor::Unavailable,
        );

        let root = accessible.root();
        assert_eq!(root.len(), 1);
        let node = root[0].node();
        assert_eq!(node.role(), Role::Splitter);
        assert_eq!(node.label(), Some(RESIZE_HANDLE_NAME));
        assert_eq!(
            node.numeric_value(),
            Some(f64::from(DEFAULT_EXPANDED_WIDTH))
        );
        assert_eq!(
            node.min_numeric_value(),
            Some(f64::from(EXPANDED_WIDTH_MIN))
        );
        assert_eq!(
            node.max_numeric_value(),
            Some(f64::from(EXPANDED_WIDTH_MAX))
        );
        assert_eq!(node.numeric_value_step(), Some(f64::from(RESIZE_STEP)));
        for action in [Action::Focus, Action::Increment, Action::Decrement] {
            assert!(
                node.supports_action(action),
                "the handle must expose {action:?} to assistive technologies"
            );
        }
    }

    /// libcosmic labels a button from `name` alone; its visible text is not a
    /// fallback, so an unnamed button reaches a screen reader as "button".
    #[test]
    fn every_pane_state_offers_its_action_under_an_accessible_name() {
        /// The accessible names of the button nodes only: the surrounding
        /// text contributes labels of its own, which are no substitute.
        fn button_labels(element: &Element<'_, SidebarAction>, children: usize) -> Vec<String> {
            let tree = Tree::new(element);
            let leaf = || layout::Node::new(Size::new(120.0, 32.0));
            // container > column > [icon, heading, caption, button > label]
            let column = layout::Node::with_children(
                Size::new(240.0, 320.0),
                (0..children)
                    .map(|_| layout::Node::with_children(Size::new(120.0, 32.0), vec![leaf()]))
                    .collect(),
            );
            let root = layout::Node::with_children(Size::new(240.0, 320.0), vec![column]);
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&root),
                &tree,
                cosmic::iced::advanced::mouse::Cursor::Unavailable,
            );
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
                .filter_map(|node| node.node().label().map(str::to_owned))
                .collect()
        }

        assert!(
            button_labels(&error("disk on fire", false), 4)
                .iter()
                .any(|label| label == "Try again"),
            "the error state's retry button must carry its own accessible name"
        );

        for (query, filter, expected) in [
            (String::new(), SidebarFilter::All, "Open repository"),
            (String::new(), SidebarFilter::Attention, "Show all"),
            ("nothing".to_owned(), SidebarFilter::All, "Clear search"),
        ] {
            let mut state = SidebarState::default();
            state.apply(&SidebarAction::SearchChanged(query));
            state.apply(&SidebarAction::SetFilter(filter));
            let empty = empty(&state, false);
            assert!(
                button_labels(&empty, 4)
                    .iter()
                    .any(|label| label == expected),
                "the empty state's action must be named {expected:?}"
            );
        }
    }

    #[test]
    fn opening_entities_updates_only_the_selected_navigation_identity() {
        let project = project_id(1);
        let worktree = worktree(2, "main", false);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::OpenRepository {
            project_id: project,
            repository: "/repo/cagents".into(),
        });
        assert_eq!(state.selection, Some(SidebarSelection::Project(project)));

        state.apply(&SidebarAction::OpenWorktree {
            project_id: project,
            worktree_id: worktree.id,
            path: worktree.path.clone(),
        });
        assert_eq!(
            state.selection,
            Some(SidebarSelection::Worktree {
                project_id: project,
                worktree_id: worktree.id,
            })
        );

        state.apply(&SidebarAction::OpenPullRequest {
            project_id: project,
            number: 42,
            url: Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
        });
        assert_eq!(
            state.selection,
            Some(SidebarSelection::PullRequest {
                project_id: project,
                number: 42,
            })
        );
    }
}
