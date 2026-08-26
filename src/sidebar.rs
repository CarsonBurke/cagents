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

use cosmic::{
    Element,
    iced::{Alignment, Length, advanced::text::EllipsizeHeightLimit, id::Id},
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

const EXPANDED_WIDTH_MIN: f32 = 240.0;
const EXPANDED_WIDTH_MAX: f32 = 360.0;
const COMPACT_WIDTH: f32 = 56.0;
const COMPACT_BREAKPOINT: f32 = 1_200.0;
const PROJECT_HEADER_HEIGHT: f32 = 48.0;
const WORKTREE_ROW_HEIGHT: f32 = 48.0;
const PULL_REQUEST_ROW_HEIGHT: f32 = 48.0;
const ICON_CONTROL_SIZE: f32 = 32.0;
const ROW_ICON_WIDTH: f32 = 24.0;

/// The responsive density selected by the application from its available
/// width. Compact mode retains every command as a named, focusable icon.
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
            path_label: path_label.into_boxed_str(),
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
    path_label: Box<str>,
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
            accessible_name: format!(
                "Open pull request {number}: {title}, checks {check_label}"
            )
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
    PullRequest { project_id: ProjectId, number: u64 },
}

/// Persistent interaction state owned by the application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    query: String,
    filter: SidebarFilter,
    expanded_projects: BTreeSet<ProjectId>,
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

    /// Applies only local, pure presentation transitions. Commands such as
    /// opening a worktree remain for the application to handle.
    pub fn apply(&mut self, action: &SidebarAction) {
        match action {
            SidebarAction::SearchChanged(query) => self.query.clone_from(query),
            SidebarAction::SetFilter(filter) => self.filter = *filter,
            SidebarAction::ToggleProject(project_id) => {
                toggle_membership(&mut self.expanded_projects, *project_id);
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
            | SidebarAction::OpenShortcuts => {}
        }
    }
}

/// All sidebar intents. Payloads are self-contained so `app.rs` does not need
/// to recover a path or URL from ephemeral row indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarAction {
    SearchChanged(String),
    SetFilter(SidebarFilter),
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
    /// Opens the pull request in an application-owned embedded browser card.
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
    OpenShortcuts,
}

pub fn search_focus_id() -> Id {
    stable_id("search")
}

/// Returns the rendered pane width. Persisted expanded widths are constrained
/// to the workbench range even when an older settings file contains a broader
/// backend-valid value.
pub fn effective_width(mode: SidebarMode, persisted_width: f32) -> f32 {
    if mode == SidebarMode::Compact {
        COMPACT_WIDTH
    } else if persisted_width.is_finite() {
        persisted_width.clamp(EXPANDED_WIDTH_MIN, EXPANDED_WIDTH_MAX)
    } else {
        280.0
    }
}

/// The sidebar's single public rendering entry point.
pub fn view<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    mode: SidebarMode,
    persisted_width: f32,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let compact = mode == SidebarMode::Compact;
    let width = effective_width(mode, persisted_width);
    let loading = matches!(&model.data_state, SidebarDataState::Loading);
    let refresh_enabled = !model.projects.is_empty() && !loading;
    let mut content = widget::column::with_capacity(7)
        .push(
            widget::container(header(
                compact,
                model.projects.len(),
                refresh_enabled,
                loading,
            ))
            .padding([0, spacing.space_xxs]),
        )
        .push(widget::divider::horizontal::light());

    if !model.projects.is_empty() {
        content = content
            .push(widget::container(search(state, compact)).padding([
                spacing.space_xxxs,
                spacing.space_xxs,
            ]))
            .push(widget::container(filters(state.filter, compact)).padding([
                0,
                spacing.space_xxs,
            ]));
    }

    content = content
        .push(
            widget::container(body(model, state, compact))
                .padding([spacing.space_xxxs, spacing.space_xxs])
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
            widget::divider::vertical::default().into(),
        ])
        .spacing(0),
    )
    .class(theme::Container::Background)
    .width(Length::Fixed(width))
    .height(Length::Fill)
    .into()
}

fn header(
    compact: bool,
    project_count: usize,
    refresh_enabled: bool,
    loading: bool,
) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        return widget::container(icon_action(
            "document-open-symbolic",
            "Open repository",
            SidebarAction::OpenRepositoryPicker,
            stable_id("open-repository"),
        ))
        .center_x(Length::Fill)
        .height(Length::Fixed(PROJECT_HEADER_HEIGHT))
        .into();
    }
    let mut children: Vec<Element<'static, SidebarAction>> = vec![
        widget::text::caption_heading("Projects").into(),
        widget::text::caption(project_count.to_string()).into(),
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    ];
    if loading {
        children.push(
            widget::progress_bar::circular::Circular::new()
                .size(16.0)
                .into(),
        );
    } else if refresh_enabled {
        children.push(icon_action(
            "view-refresh-symbolic",
            format!("Refresh {project_count} projects"),
            SidebarAction::Refresh,
            stable_id("refresh"),
        ));
    }
    children.push(icon_action(
        "document-open-symbolic",
        "Open repository",
        SidebarAction::OpenRepositoryPicker,
        stable_id("open-repository"),
    ));
    widget::row::with_children(children)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(Length::Fixed(PROJECT_HEADER_HEIGHT))
        .width(Length::Fill)
        .into()
}

fn footer<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        let selected_project_id = state.selection.map(|selection| match selection {
            SidebarSelection::Project(project_id)
            | SidebarSelection::Worktree { project_id, .. }
            | SidebarSelection::PullRequest { project_id, .. } => project_id,
        });
        let mut children: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(4);
        if let Some(project) = selected_project_id
            .and_then(|project_id| model.projects.iter().find(|project| project.id == project_id))
        {
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
        children.extend([
            icon_action(
                "input-keyboard-symbolic",
                "Keyboard shortcuts",
                SidebarAction::OpenShortcuts,
                stable_id("shortcuts"),
            ),
            icon_action(
                "emblem-system-symbolic",
                "Settings",
                SidebarAction::OpenSettings,
                stable_id("settings"),
            ),
        ]);
        return widget::column::with_children(children)
            .align_x(Alignment::Center)
            .spacing(spacing.space_xxxs)
            .padding(spacing.space_xxxs)
            .width(Length::Fill)
            .into();
    }
    widget::column::with_children(vec![
        button::custom(
            widget::row::with_children(vec![
                widget::icon::from_name("input-keyboard-symbolic")
                    .size(16)
                    .into(),
                widget::text::body("Keyboard shortcuts").into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
        )
        .class(theme::Button::MenuItem)
        .id(stable_id("shortcuts"))
        .name("Keyboard shortcuts")
        .height(Length::Fixed(40.0))
        .width(Length::Fill)
        .on_press(SidebarAction::OpenShortcuts)
        .into(),
        button::custom(
            widget::row::with_children(vec![
                widget::icon::from_name("emblem-system-symbolic")
                    .size(16)
                    .into(),
                widget::text::body("Settings").into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
        )
        .class(theme::Button::MenuItem)
        .id(stable_id("settings"))
        .name("Settings")
        .height(Length::Fixed(40.0))
        .width(Length::Fill)
        .on_press(SidebarAction::OpenSettings)
        .into(),
    ])
    .padding(spacing.space_xxxs)
    .width(Length::Fill)
    .into()
}

fn search<'a>(state: &'a SidebarState, _compact: bool) -> Element<'a, SidebarAction> {
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

fn filters(filter: SidebarFilter, compact: bool) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let choices = [
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
    if compact {
        let mut column = widget::column::with_capacity(choices.len())
            .spacing(spacing.space_xxxs)
            .align_x(Alignment::Center)
            .width(Length::Fill);
        for (value, icon_name, _, accessible_label) in choices {
            column = column.push(
                button::icon(icon::from_name(icon_name).size(16))
                    .id(filter_id(value))
                    .name(accessible_label)
                    .tooltip(accessible_label)
                    .width(ICON_CONTROL_SIZE)
                    .height(ICON_CONTROL_SIZE)
                    .selected(filter == value)
                    .on_press(SidebarAction::SetFilter(value)),
            );
        }
        return column.into();
    }

    let mut row = widget::row::with_capacity(choices.len())
        .spacing(spacing.space_xxxs)
        .width(Length::Fill)
        .align_y(Alignment::Center);
    for (value, _, visible_label, accessible_label) in choices {
        row = row.push(
            button::custom(widget::text(visible_label))
                .class(theme::Button::Text)
                .id(filter_id(value))
                .name(accessible_label)
                .selected(filter == value)
                .on_press(SidebarAction::SetFilter(value)),
        );
    }
    row.into()
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
    let projects = filtered_projects(model, state);
    if projects.is_empty() {
        return empty(state, compact);
    }

    // A search reveals matches wherever they live, so every section it kept
    // opens regardless of the persisted collapse state.
    let searching = !state.query.trim().is_empty();
    let mut column = widget::column::with_capacity(projects.len() * 2).spacing(0);
    for (index, project) in projects.iter().enumerate() {
        if index > 0 {
            column = column.push(widget::divider::horizontal::light());
        }
        let project_id = project.project.id;
        column = column.push(project_view(
            project,
            searching || state.expanded_projects.contains(&project_id),
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
            .size(20.0)
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
                .size(20)
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
                .size(20)
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
    let (icon_name, title, detail, action_label, action) = if !state.query.trim().is_empty() {
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
                "document-open-symbolic",
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
        if state.query.trim().is_empty() && state.filter == SidebarFilter::All {
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
            widget::icon::from_name(icon_name).size(20).into(),
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
    // Header, the worktree caption, every worktree row, and the pull-request
    // group: the exact child count when the section is open.
    let mut column =
        widget::column::with_capacity(worktree_count + 3).spacing(spacing.space_xxxs);
    let toggle_icon = if expanded {
        "go-down-symbolic"
    } else {
        "go-next-symbolic"
    };
    let project_identity = button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon::from_name("folder-symbolic").size(16))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            widget::column::with_children(vec![
                widget::text::caption_heading(entity.name.as_str())
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                widget::text::caption(format!(
                    "{worktree_count} worktrees · {pull_request_count} PRs"
                ))
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
            ])
            .spacing(0)
            .width(Length::Fill)
            .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs),
    )
    .class(theme::Button::MenuItem)
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
        widget::text::caption(entity.name.as_str()),
        widget::tooltip::Position::Bottom,
    )
    .into();
    let header = widget::container(
        widget::row::with_children(vec![
            icon_action(
                toggle_icon,
                if expanded {
                    &*chrome.collapse_name
                } else {
                    &*chrome.expand_name
                },
                SidebarAction::ToggleProject(project_id),
                chrome.toggle_id.clone(),
            ),
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
        .spacing(spacing.space_xxxs),
    )
    .class(theme::Container::Background)
    .height(Length::Fixed(PROJECT_HEADER_HEIGHT));
    column = column.push(header);

    if expanded {
        let has_worktrees = worktree_count > 0;
        let has_pull_requests = pull_request_count > 0;
        if has_worktrees {
            column = column.push(
                widget::container(widget::text::caption_heading(format!(
                    "Worktrees ({worktree_count})"
                )))
                .padding([spacing.space_xxxs, spacing.space_s]),
            );
        }
        for &worktree in &project.worktrees {
            column = column.push(
                widget::container(worktree_view(project_id, worktree, false, selection))
                    .padding([0, 0, 0, spacing.space_s]),
            );
        }
        if has_pull_requests {
            column = column.push(
                widget::container(pull_request_group(
                    entity,
                    &project.pull_requests,
                    pull_requests_expanded,
                    false,
                    selection,
                ))
                .padding([spacing.space_xxxs, 0, 0, spacing.space_s]),
            );
        }
        if !has_worktrees && !has_pull_requests {
            column = column.push(
                widget::container(widget::text::caption(
                    "No matching worktrees or pull requests",
                ))
                .padding([spacing.space_xxs, spacing.space_s]),
            );
        }
    }

    widget::container(column).width(Length::Fill).into()
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
    let action = button::icon(icon::from_name("folder-symbolic").size(16))
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
    compact: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &worktree.chrome;
    if compact {
        return icon_action(
            "vcs-branch-symbolic",
            &*chrome.accessible_name,
            SidebarAction::OpenWorktree {
                project_id,
                worktree_id: worktree.id,
                path: worktree.path.clone(),
            },
            chrome.open_id.clone(),
        );
    }

    let spacing = cosmic::theme::spacing();
    let mut row = widget::row::with_capacity(7)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .push(
            widget::container(widget::icon::from_name("vcs-branch-symbolic").size(16))
                .center_x(Length::Fixed(ROW_ICON_WIDTH)),
        )
        .push(
            widget::column::with_children(vec![
                widget::text::body(&*chrome.label)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                widget::text::caption(&*chrome.path_label)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(0)
            .width(Length::Fill),
        );
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
        .class(theme::Button::MenuItem)
        .id(chrome.open_id.clone())
        .selected(
            selection
                == Some(SidebarSelection::Worktree {
                    project_id,
                    worktree_id: worktree.id,
                }),
        )
        .name(&*chrome.accessible_name)
        .padding([0, spacing.space_xxs])
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
    compact: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &project.chrome;
    let spacing = cosmic::theme::spacing();
    let mut column =
        widget::column::with_capacity(pull_requests.len() + 1).spacing(spacing.space_xxxs);
    if !compact {
        column = column.push(
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
    }
    if expanded {
        for &pull_request in pull_requests {
            column =
                column.push(pull_request_view(project.id, pull_request, compact, selection));
        }
    }
    column.into()
}

fn pull_request_view<'a>(
    project_id: ProjectId,
    pull_request: &'a SidebarPullRequest,
    compact: bool,
    selection: Option<SidebarSelection>,
) -> Element<'a, SidebarAction> {
    let chrome = &pull_request.chrome;
    let action = SidebarAction::OpenPullRequest {
        project_id,
        number: pull_request.number,
        url: pull_request.url.clone(),
    };
    if compact {
        return icon_action(
            "internet-web-browser-symbolic",
            &*chrome.accessible_name,
            action,
            chrome.open_id.clone(),
        );
    }

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
        widget::container(widget::icon::from_name("vcs-merge-request-symbolic").size(16))
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
        .class(theme::Button::MenuItem)
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
    button::icon(icon::from_name(icon_name).size(16))
        .id(id)
        .name(label.clone())
        .tooltip(label)
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .on_press(action)
        .into()
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
        widget::text::caption_heading(label).wrapping(cosmic::iced::widget::text::Wrapping::None),
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
            background: Some(cosmic::iced::Background::Color(component.base.into())),
            border: cosmic::iced::Border {
                radius: cosmic.corner_radii.radius_s.into(),
                width: 1.0,
                color: component.border.into(),
            },
            shadow: cosmic::iced::Shadow::default(),
            snap: true,
        }
    }))
    .padding([spacing.space_xxxs, spacing.space_xxs])
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

fn filtered_projects<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
) -> Vec<FilteredProject<'a>> {
    let query = state.query.trim().to_ascii_lowercase();
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

    #[test]
    fn local_state_transitions_are_idempotent_and_scoped() {
        let id = project_id(1);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleProject(id));
        assert!(state.expanded_projects.contains(&id));
        state.apply(&SidebarAction::ToggleProject(id));
        assert!(!state.expanded_projects.contains(&id));

        state.apply(&SidebarAction::TogglePullRequests(id));
        state.apply(&SidebarAction::SearchChanged(" Sidebar ".into()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(state.expanded_pull_requests.contains(&id));
        assert_eq!(state.query(), " Sidebar ");
        assert_eq!(state.filter(), SidebarFilter::Attention);
    }

    #[test]
    fn filtering_matches_descendants_and_attention_state() {
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged("FIX-SIDEBAR".into()));
        let filtered = filtered_projects(&model, &state);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].worktrees.len(), 1);
        assert_eq!(filtered[0].pull_requests.len(), 1);

        state.apply(&SidebarAction::SearchChanged(String::new()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        let filtered = filtered_projects(&model, &state);
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
        let filtered = filtered_projects(&model, &state);
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
    fn compact_and_expanded_search_controls_share_the_real_focus_target() {
        for compact in [true, false] {
            let mut state = SidebarState::default();
            let control = search(&state, compact);
            assert_eq!(
                control.as_widget().id().map(|id| id.to_string()),
                Some(search_focus_id().to_string())
            );
            drop(control);

            state.apply(&SidebarAction::SearchChanged("cagents".into()));
            assert_eq!(state.query(), "cagents");
            let control = search(&state, compact);
            assert_eq!(
                control.as_widget().id().map(|id| id.to_string()),
                Some(search_focus_id().to_string())
            );
        }
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
        assert_eq!(effective_width(SidebarMode::Compact, 320.0), 56.0);
        assert_eq!(effective_width(SidebarMode::Expanded, 280.0), 280.0);
        assert_eq!(effective_width(SidebarMode::Expanded, 120.0), 240.0);
        assert_eq!(effective_width(SidebarMode::Expanded, 480.0), 360.0);
        assert_eq!(effective_width(SidebarMode::Expanded, f32::NAN), 280.0);
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
        assert_eq!(
            state.selection,
            Some(SidebarSelection::Project(project))
        );

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
