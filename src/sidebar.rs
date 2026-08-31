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
    design::{self, NavigationLayout},
    github::{PullRequest, PullRequestCheck},
    model::{ProjectId, ProjectState, WorktreeId},
    repository::{
        ChangeArea, ChangeKind, ChangedPath, CommitIdentity, CommitSummary, ConflictKind,
        DirtyState, DiscardRequest, HISTORY_PAGE_SIZE, HeadState, Oid, PathSelection, PushTarget,
        RemoteRecord, RepoPath, RepositoryId, RepositoryInfo, RepositorySnapshot, StatusSnapshot,
        TrackingState, ValidatedBranchName, ValidatedRefName, WorktreeRecord, build_file_tree,
    },
    splitter::{Edge as SplitterEdge, Splitter},
};

/// The rendered bounds of the expanded pane, and the only bounds it has:
/// `crate::settings` validates persisted widths against these, so a width can
/// no longer pass validation and then silently render clamped.
pub const EXPANDED_WIDTH_MIN: f32 = design::NAVIGATION_MIN_WIDTH;
pub const EXPANDED_WIDTH_MAX: f32 = design::NAVIGATION_MAX_WIDTH;
/// Rendered when a persisted width is not a finite number. Mirrors
/// `settings::AppSettings::default`, so a corrupt file looks like a fresh one.
const DEFAULT_EXPANDED_WIDTH: f32 = design::NAVIGATION_DEFAULT_WIDTH;
const COMPACT_WIDTH: f32 = design::NAVIGATION_RAIL_WIDTH;
const HEADER_HEIGHT: f32 = design::HEADER_HEIGHT;
const PROJECT_HEADER_HEIGHT: f32 = design::ROW_HEIGHT;
const WORKTREE_ROW_HEIGHT: f32 = design::ROW_HEIGHT;
const PULL_REQUEST_ROW_HEIGHT: f32 = design::ROW_HEIGHT + (2.0 * design::GRID_UNIT);
const ICON_CONTROL_SIZE: f32 = design::PRIMARY_CONTROL_SIZE;
const ROW_ICON_WIDTH: f32 = 20.0;
/// Icons that sit inside a row, beside its text.
const ROW_ICON_SIZE: u16 = 16;
/// Icons that stand alone as the illustration of a whole pane state.
const STATUS_ICON_SIZE: u16 = 20;
const RESIZE_HANDLE_NAME: &str = "Resize sidebar";
/// Height of one source-control panel row. Denser than the navigation rows
/// above it because the panel lists paths rather than entities.
const PANEL_ROW_HEIGHT: f32 = design::DENSE_ROW_HEIGHT;
/// Width of the compact-mode source-control popover. Independent of
/// [`EXPANDED_WIDTH_MAX`]: the popover floats over the workspace rather than
/// inside the rail, so the pane's own bounds do not constrain it.
const COMPACT_PANEL_WIDTH: f32 = 360.0;
/// How many file-tree rows the `Files` section renders at once.
///
/// `git ls-files` on a large repository returns six-figure path counts, and
/// this list is built eagerly like the project navigation above it. The bound
/// is explicit - the section says how many rows it withheld and the quick
/// filter narrows to them - rather than a silent truncation.
const MAX_VISIBLE_FILE_ROWS: usize = 400;
/// How many change rows one `Changes` group renders at once, bounded for the
/// same reason as the file tree. "Stage all" and "Unstage all" act on exactly
/// the rows that are visible, so the bound is never a hidden side effect.
const MAX_VISIBLE_CHANGE_ROWS: usize = 200;
/// How many open pull requests one listing asks `gh` for.
///
/// Defined here, beside the only surfaces that render a number out of the
/// result, because they are the only things that can be wrong about it: a page
/// that comes back full is a floor on the repository, not a count of it, and
/// nothing downstream can tell the difference from the [`Vec`] alone.
/// `app.rs`'s `ListPullRequestsRequest` reads this rather than repeating the
/// number, so the page size and the surfaces that qualify it cannot drift.
pub const PULL_REQUEST_LIST_LIMIT: u16 = 50;

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
        match design::ShellLayout::for_width(window_width).navigation {
            NavigationLayout::Rail => Self::Compact,
            NavigationLayout::Expanded => Self::Expanded,
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
    /// Source-control data for the active worktree, and only for it. The panel
    /// shows exactly one worktree, so loading a snapshot per project would read
    /// Git for surfaces nobody is looking at.
    pub source_control: Option<SourceControlModel>,
}

impl SidebarModel {
    pub fn ready(mode: SidebarMode, projects: Vec<SidebarProject>) -> Self {
        Self {
            mode,
            data_state: SidebarDataState::Ready,
            projects,
            source_control: None,
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
            source_control: None,
        }
    }

    pub fn with_source_control(mut self, source_control: Option<SourceControlModel>) -> Self {
        self.source_control = source_control;
        self
    }

    pub fn project(&self, project_id: ProjectId) -> Option<&SidebarProject> {
        self.projects
            .iter()
            .find(|project| project.id == project_id)
    }

    /// The worktree whose path is exactly `path`.
    ///
    /// Equality is exact rather than a prefix or a canonicalizing comparison:
    /// a terminal running in a subdirectory of a worktree is not the worktree,
    /// and resolving symlinks here would be filesystem I/O inside the view.
    pub fn worktree_at(&self, path: &Path) -> Option<ActiveWorktreeKey> {
        self.projects.iter().find_map(|project| {
            project
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| ActiveWorktreeKey {
                    project_id: project.id,
                    worktree_id: worktree.id,
                    canonical_path: worktree.path.clone(),
                })
        })
    }

    /// True while `key` still names a worktree this model knows about.
    pub fn contains_worktree(&self, key: &ActiveWorktreeKey) -> bool {
        self.project(key.project_id).is_some_and(|project| {
            project.worktrees.iter().any(|worktree| {
                worktree.id == key.worktree_id && worktree.path == key.canonical_path
            })
        })
    }

    /// The source-control panel's key when it is bound to exactly `worktree`.
    ///
    /// This is a question about the binding, not about any watcher's health:
    /// it says which worktree the panel is reading Git for, and nothing about
    /// whether anything is watching that worktree or how completely.
    ///
    /// It exists because the panel is subscribed to nothing. It reloads when
    /// the user asks, when it rebinds, and when one of its own mutations
    /// completes - while the filesystem watcher that keeps a repository card
    /// current belongs to the card. `app.rs` routes each of the card's
    /// observations through here to decide whether the panel is reading the
    /// tree that moved.
    pub fn source_control_bound_to(&self, worktree: &Path) -> Option<&ActiveWorktreeKey> {
        self.source_control
            .as_ref()
            .map(|panel| &panel.key)
            .filter(|key| key.canonical_path == worktree)
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
            open_ids: PullRequestSurface::ALL.map(|surface| {
                stable_id(format!(
                    "project.{project_id}.pr.{number}.open.{}",
                    surface.key()
                ))
            }),
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
    /// One identity per surface that draws this row. The navigation list and
    /// the source-control panel both render the project's pull requests in the
    /// same frame, and iced keys persistent widget state by custom name in one
    /// map per frame: two nodes under one name means the second visited
    /// overwrites the first, and the loser's state is dropped.
    open_ids: [Id; 2],
    accessible_name: Box<str>,
    metadata: Box<str>,
}

impl PullRequestChrome {
    fn open_id(&self, surface: PullRequestSurface) -> &Id {
        &self.open_ids[surface as usize]
    }
}

/// Which of the two surfaces that draw pull-request rows is drawing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestSurface {
    Navigation,
    Panel,
}

impl PullRequestSurface {
    const ALL: [Self; 2] = [Self::Navigation, Self::Panel];

    /// The fragment that keeps one surface's widget names distinct.
    fn key(self) -> &'static str {
        match self {
            Self::Navigation => "nav",
            Self::Panel => "panel",
        }
    }
}

/// A number of rows a section header prints, and whether anything downstream
/// may read it as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowCount {
    value: usize,
    at_least: bool,
}

impl RowCount {
    /// A count of rows this application holds all of.
    const fn exact(value: usize) -> Self {
        Self {
            value,
            at_least: false,
        }
    }

    /// A count nothing may read as complete: the listing that produced it came
    /// back exactly as full as it was allowed to be.
    const fn at_least(value: usize) -> Self {
        Self {
            value,
            at_least: true,
        }
    }
}

impl From<usize> for RowCount {
    fn from(value: usize) -> Self {
        Self::exact(value)
    }
}

impl std::fmt::Display for RowCount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.value)?;
        if self.at_least {
            formatter.write_str("+")?;
        }
        Ok(())
    }
}

/// Identifies the worktree the source-control panel is bound to.
///
/// All three parts travel together because none of them identifies a worktree
/// on its own: an id is unique only within a project, and a path can be reused
/// by a worktree that was removed and recreated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActiveWorktreeKey {
    pub project_id: ProjectId,
    pub worktree_id: WorktreeId,
    pub canonical_path: PathBuf,
}

/// The application's focused card, reduced to what selecting a worktree needs.
///
/// `app.rs` maps its `CardKind` onto this; the sidebar deliberately cannot see
/// card state, so focus arrives as data rather than as a reach into the
/// application.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SidebarFocus {
    /// Nothing is focused, or the focused card names no directory at all.
    #[default]
    None,
    /// A repository card, identified by the worktree it is open on.
    Repository { worktree: PathBuf },
    /// A terminal card, identified by its working directory.
    Terminal { working_directory: PathBuf },
}

impl SidebarFocus {
    /// The directory this focus names, if any.
    fn path(&self) -> Option<&Path> {
        match self {
            Self::None => None,
            Self::Repository { worktree } => Some(worktree),
            Self::Terminal { working_directory } => Some(working_directory),
        }
    }
}

/// The four collapsible sections of the source-control panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PanelSection {
    Files,
    Changes,
    History,
    PullRequests,
}

impl PanelSection {
    pub const ALL: [Self; 4] = [
        Self::Files,
        Self::Changes,
        Self::History,
        Self::PullRequests,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Changes => "Changes",
            Self::History => "History",
            Self::PullRequests => "Pull requests",
        }
    }

    /// `Changes` is the section a source-control surface exists for, so it is
    /// the one that starts open.
    fn expanded_by_default(self) -> bool {
        matches!(self, Self::Changes)
    }
}

/// Which source-control mutation is currently running.
///
/// Exactly one runs at a time per worktree - the application serializes them
/// per Git common directory - so this is an option, not a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Stage,
    Unstage,
    Discard,
    Commit,
    Push,
}

impl MutationKind {
    fn progress_label(self) -> &'static str {
        match self {
            Self::Stage => "Staging…",
            Self::Unstage => "Unstaging…",
            Self::Discard => "Discarding…",
            Self::Commit => "Committing…",
            Self::Push => "Pushing…",
        }
    }
}

/// A discard the user has been asked to confirm.
///
/// The request is built once, when the intent is raised, so the paths the
/// dialog counts are exactly the paths the confirmed request destroys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardConfirmation {
    pub key: ActiveWorktreeKey,
    /// The request with `confirmed` still false. Only
    /// [`SidebarAction::ConfirmDiscard`] carries the confirmed copy.
    pub request: DiscardRequest,
}

impl DiscardConfirmation {
    /// The one action that may actually destroy work: the same request with
    /// the user's confirmation recorded. Building it here keeps the confirmed
    /// paths identical to the paths the dialog counted.
    pub fn confirm_action(&self) -> SidebarAction {
        SidebarAction::ConfirmDiscard {
            key: self.key.clone(),
            request: DiscardRequest {
                confirmed: true,
                ..self.request.clone()
            },
        }
    }

    fn tracked_count(&self) -> usize {
        self.request.tracked.len()
    }

    fn untracked_count(&self) -> usize {
        self.request.untracked.len()
    }
}

/// The publish picker shown for a branch that has no upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDraft {
    pub key: ActiveWorktreeKey,
    pub branch: ValidatedBranchName,
    /// The remote to publish to, chosen by [`Self::new`] wherever choosing one
    /// is not a guess. Empty only for a repository with several remotes and no
    /// `origin` among them.
    pub remote: String,
    /// Free text, defaulted to the local branch name. Git's own
    /// `check-ref-format` remains the authority; this surface only refuses to
    /// send a name it can already see is not a branch name.
    pub remote_branch: String,
}

impl PublishDraft {
    /// Opens the picker for `branch`, with a remote already selected wherever
    /// selecting one is not a guess.
    ///
    /// With one remote there is nothing to choose. With several, `origin` is
    /// the remote `git push` itself uses for a branch that has no upstream, so
    /// selecting it is this repository's own default rather than this panel's
    /// invention - and it is a selection the picker shows, the user can change,
    /// and a separate press confirms. Several remotes and no `origin` is the
    /// one genuinely ambiguous case; nothing is selected there and
    /// [`Self::blocked_reason`] says so.
    pub fn new(key: ActiveWorktreeKey, branch: ValidatedBranchName, remotes: &[String]) -> Self {
        let remote = match remotes {
            [only] => only.clone(),
            _ => remotes
                .iter()
                .find(|remote| *remote == "origin")
                .cloned()
                .unwrap_or_default(),
        };
        Self {
            remote_branch: branch.as_str().to_owned(),
            key,
            branch,
            remote,
        }
    }

    /// The push this draft describes, once a remote is chosen and the remote
    /// branch text is a branch name. `None` means the picker is incomplete,
    /// never that the push is silently defaulted.
    pub fn push_action(&self) -> Option<SidebarAction> {
        if self.remote.is_empty() {
            return None;
        }
        Some(SidebarAction::Push {
            key: self.key.clone(),
            branch: self.branch.clone(),
            target: PushTarget::SetUpstream {
                remote: self.remote.clone(),
                remote_branch: ValidatedBranchName::parse(self.remote_branch.trim()).ok()?,
            },
        })
    }

    /// Why `Publish` is refused, or `None` when it is not.
    ///
    /// A disabled button that reports nothing is indistinguishable from a
    /// broken one. `None` exactly when [`Self::push_action`] is `Some` and no
    /// mutation is running, so the sentence and the button cannot disagree.
    pub fn blocked_reason(&self, busy: bool) -> Option<&'static str> {
        if busy {
            return Some("A Git command is already running.");
        }
        if self.remote.is_empty() {
            return Some("Choose a remote to publish to.");
        }
        if ValidatedBranchName::parse(self.remote_branch.trim()).is_err() {
            return Some("Enter a branch name Git will accept.");
        }
        None
    }
}

/// One row of the `Files` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub path: RepoPath,
    pub depth: usize,
    pub directory: bool,
    chrome: FileRowChrome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileRowChrome {
    label: Box<str>,
    folded: Box<str>,
    accessible_name: Box<str>,
}

/// One row of a `Changes` group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRow {
    pub entry: ChangedPath,
    chrome: ChangeRowChrome,
}

impl ChangeRow {
    fn new(worktree_id: WorktreeId, entry: &ChangedPath) -> Self {
        let key = format!("{worktree_id}.{:?}.{}", entry.area, path_key(&entry.path));
        let label = match &entry.old_path {
            Some(old_path) => format!("{} → {}", old_path.display(), entry.path.display()),
            None => entry.path.display().into_owned(),
        };
        let counts = match (entry.added_lines, entry.removed_lines) {
            // Both halves are required: Git reports `-` for a binary side, and
            // a missing count is not a count of zero.
            (Some(added), Some(removed)) => format!("+{added} −{removed}"),
            _ => "±unknown".to_owned(),
        };
        let status = entry
            .conflict
            .map(conflict_label)
            .unwrap_or_else(|| change_kind_label(entry.kind));
        // Which end of the label the row may throw away, decided once.
        //
        // A repository path is elided from the front, because its tail is the
        // file and `…/src/lib.rs` still names one; so is a rename, whose tail
        // is the name the file now has. A file in the worktree root is nothing
        // but its name and has no head to spare.
        let ellipsize_start = entry.old_path.is_some() || label.contains('/');
        let chrome = ChangeRowChrome {
            accessible_name: format!("{status} {label}, {counts}").into_boxed_str(),
            stage_name: format!("Stage {label}").into_boxed_str(),
            unstage_name: format!("Unstage {label}").into_boxed_str(),
            discard_name: format!("Discard {label}").into_boxed_str(),
            label: label.into_boxed_str(),
            status: status.into_boxed_str(),
            counts: counts.into_boxed_str(),
            ellipsize_start,
            stage_id: stable_id(format!("panel.change.stage.{key}")),
            unstage_id: stable_id(format!("panel.change.unstage.{key}")),
            discard_id: stable_id(format!("panel.change.discard.{key}")),
        };
        Self {
            entry: entry.clone(),
            chrome,
        }
    }

    pub fn selection(&self) -> PathSelection {
        PathSelection::from_changed_path(&self.entry)
    }

    /// The added/removed summary. An unmeasured count renders as unknown
    /// rather than as zero.
    pub fn counts_label(&self) -> &str {
        &self.chrome.counts
    }

    pub fn status_label(&self) -> &str {
        &self.chrome.status
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeRowChrome {
    label: Box<str>,
    status: Box<str>,
    counts: Box<str>,
    /// True when the label's tail is what identifies it; see
    /// [`ChangeRow::new`].
    ellipsize_start: bool,
    stage_id: Id,
    unstage_id: Id,
    discard_id: Id,
    accessible_name: Box<str>,
    /// Each command names itself and the path it acts on: three icons sharing
    /// the row's own name would reach a screen reader as one control repeated.
    stage_name: Box<str>,
    unstage_name: Box<str>,
    discard_name: Box<str>,
}

/// One `Changes` group: conflicts, staged, unstaged, or untracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeGroup {
    pub area: ChangeArea,
    rows: Vec<ChangeRow>,
    chrome: ChangeGroupChrome,
}

impl ChangeGroup {
    fn new(worktree_id: WorktreeId, area: ChangeArea, rows: Vec<ChangeRow>) -> Self {
        let title = match area {
            ChangeArea::Conflict => "Conflicts",
            ChangeArea::Staged => "Staged",
            ChangeArea::Unstaged => "Changes",
            ChangeArea::Untracked => "Untracked",
        };
        let key = format!("{worktree_id}.{area:?}");
        let chrome = ChangeGroupChrome {
            title: format!("{title} ({})", rows.len()).into_boxed_str(),
            stage_all_id: stable_id(format!("panel.group.stage.{key}")),
            unstage_all_id: stable_id(format!("panel.group.unstage.{key}")),
            discard_all_id: stable_id(format!("panel.group.discard.{key}")),
            stage_all_name: format!("Stage all visible {title}").into_boxed_str(),
            unstage_all_name: format!("Unstage all visible {title}").into_boxed_str(),
            discard_all_name: format!("Discard all visible {title}").into_boxed_str(),
        };
        Self { area, rows, chrome }
    }

    pub fn rows(&self) -> &[ChangeRow] {
        &self.rows
    }

    /// The rows this group actually renders. "Stage all" and "Unstage all" act
    /// on exactly these, so the display bound is never a hidden side effect.
    pub fn visible(&self) -> &[ChangeRow] {
        &self.rows[..self.rows.len().min(MAX_VISIBLE_CHANGE_ROWS)]
    }

    pub fn hidden(&self) -> usize {
        self.rows.len().saturating_sub(MAX_VISIBLE_CHANGE_ROWS)
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The path selection covering every visible row, rename halves included.
    pub fn visible_selection(&self) -> Vec<PathSelection> {
        self.visible().iter().map(ChangeRow::selection).collect()
    }

    /// The untracked paths among the visible rows, which discard deletes
    /// rather than restores.
    fn visible_untracked(&self) -> Vec<RepoPath> {
        self.visible()
            .iter()
            .map(|row| row.entry.path.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeGroupChrome {
    title: Box<str>,
    stage_all_id: Id,
    unstage_all_id: Id,
    discard_all_id: Id,
    stage_all_name: Box<str>,
    unstage_all_name: Box<str>,
    discard_all_name: Box<str>,
}

/// One `History` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    pub oid: Oid,
    chrome: CommitRowChrome,
}

impl CommitRow {
    fn new(commit: &CommitSummary) -> Self {
        let merge = commit.parents.len() > 1;
        let mut metadata = format!(
            "{} · {} · {}",
            commit.oid.short(),
            commit.author.name,
            format_commit_time(&commit.author)
        );
        if merge {
            metadata.push_str(" · merge");
        }
        let chrome = CommitRowChrome {
            accessible_name: format!(
                "Commit {} {}, {metadata}",
                commit.oid.short(),
                commit.subject
            )
            .into_boxed_str(),
            subject: commit.subject.clone().into_boxed_str(),
            metadata: metadata.into_boxed_str(),
        };
        Self {
            oid: commit.oid.clone(),
            chrome,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitRowChrome {
    subject: Box<str>,
    metadata: Box<str>,
    accessible_name: Box<str>,
}

/// Immutable source-control data for one worktree.
///
/// Everything a frame needs is derived once, here: filter keys, widget
/// identities, accessible names, and the status and count strings. The view
/// reads them and allocates nothing per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceControlModel {
    pub key: ActiveWorktreeKey,
    pub data_state: SidebarDataState,
    /// The checked-out branch as Git named it, and therefore the branch every
    /// mutation this panel offers targets.
    ///
    /// `None` for a detached or unborn `HEAD`, for a `HEAD` that could not be
    /// read, and for a branch whose ref name Git reported in bytes this cannot
    /// hand back to it - which is a branch that exists and still cannot be
    /// pushed. The label the header shows is separate and may be lossy; see
    /// [`Self::no_branch_reason`].
    pub branch: Option<ValidatedBranchName>,
    pub detached: bool,
    /// The commit `HEAD` names, or `None` when `HEAD` is unborn or could not be
    /// read. History is a walk from `HEAD`, so a `HEAD` that moved invalidates
    /// every page already loaded.
    pub head_oid: Option<Box<str>>,
    /// True only for an unborn `HEAD`. That is the one state in which "no
    /// commits yet" is a true statement about a repository; a `HEAD` this
    /// surface could not read is not that state and must not be reported as it.
    pub unborn: bool,
    /// The push target when the branch already has an upstream.
    pub upstream: Option<PushTarget>,
    /// True when the branch has an upstream and is not ahead of it.
    pub up_to_date: bool,
    /// What `git status` measured the branch against, retained so an
    /// ahead/behind pair is only ever reported beside the ref it was measured
    /// against.
    pub tracking: TrackingState,
    /// Configured remote names, in the order Git reported them.
    pub remotes: Vec<String>,
    /// The mutation currently in flight, which disables every command.
    pub mutation: Option<MutationKind>,
    /// The most recent mutation failure, retained until the next attempt.
    pub error: Option<String>,
    pub staged_count: usize,
    pub files: Vec<FileRow>,
    groups: Vec<ChangeGroup>,
    pub history: Vec<CommitRow>,
    /// False until a page has landed. Without it an empty `history` is
    /// indistinguishable from a branch that has no commits.
    pub history_loaded: bool,
    /// True once a page has been asked for and landed, and then for as long as
    /// the panel stays bound to this worktree. Separate from `history_loaded`,
    /// which a moved `HEAD` clears; see [`Self::history_requested`].
    history_requested: bool,
    /// False while another [`HISTORY_PAGE_SIZE`] page may exist.
    pub history_complete: bool,
    /// The next zero-based page `History` should request.
    pub next_history_page: u32,
    /// True while these rows are a snapshot older than the reload in flight
    /// for them. Set only by [`Self::carry_mutation_into`], which is what keeps
    /// a reload from blanking the panel.
    reloading: bool,
    chrome: SourceControlChrome,
}

impl SourceControlModel {
    /// The panel before its first snapshot has arrived.
    pub fn loading(key: ActiveWorktreeKey) -> Self {
        Self::empty(key, SidebarDataState::Loading)
    }

    /// The panel after its Git load failed. The reason is shown with a retry;
    /// the panel is never silently blank.
    pub fn failed(key: ActiveWorktreeKey, reason: impl Into<String>) -> Self {
        Self::empty(key, SidebarDataState::Error(reason.into()))
    }

    fn empty(key: ActiveWorktreeKey, data_state: SidebarDataState) -> Self {
        let chrome = SourceControlChrome::new(&key, None, &[]);
        Self {
            key,
            data_state,
            branch: None,
            detached: false,
            head_oid: None,
            unborn: false,
            upstream: None,
            up_to_date: false,
            tracking: TrackingState::NoUpstream,
            remotes: Vec::new(),
            mutation: None,
            error: None,
            staged_count: 0,
            files: Vec::new(),
            groups: Vec::new(),
            history: Vec::new(),
            history_loaded: false,
            history_requested: false,
            history_complete: false,
            next_history_page: 0,
            reloading: false,
            chrome,
        }
    }

    /// Builds the panel from a loaded snapshot.
    pub fn ready(key: ActiveWorktreeKey, snapshot: &RepositorySnapshot) -> Self {
        let worktree_id = key.worktree_id;
        // The header's label and the branch a push names come from the same
        // `HEAD` and are not the same value: the label is a lossy decoding of
        // the bytes `git status` reported, while the branch is the ref
        // `for-each-ref` was able to name. Deriving both from the label offered
        // a push for a branch Git would then refuse to be given.
        let branch = head_branch(snapshot);
        let remotes: Vec<String> = snapshot
            .remotes
            .iter()
            .map(|remote| remote.name.clone())
            .collect();
        let chrome = SourceControlChrome::new(&key, snapshot.status.branch(), &remotes);
        let groups = [
            ChangeArea::Conflict,
            ChangeArea::Staged,
            ChangeArea::Unstaged,
            ChangeArea::Untracked,
        ]
        .into_iter()
        .map(|area| {
            let rows = snapshot
                .status
                .area(area)
                .map(|entry| ChangeRow::new(worktree_id, entry))
                .collect();
            ChangeGroup::new(worktree_id, area, rows)
        })
        .collect::<Vec<_>>();
        let staged_count = groups
            .iter()
            .find(|group| group.area == ChangeArea::Staged)
            .map_or(0, |group| group.rows.len());
        let files = build_file_tree(&snapshot.files.paths)
            .into_iter()
            .map(|entry| {
                let label = entry.path.file_name().into_owned();
                let kind = if entry.directory { "folder" } else { "file" };
                let chrome = FileRowChrome {
                    folded: fold(&entry.path.display()),
                    accessible_name: format!("Open {kind} {}", entry.path.display())
                        .into_boxed_str(),
                    label: label.into_boxed_str(),
                };
                FileRow {
                    path: entry.path,
                    depth: entry.depth,
                    directory: entry.directory,
                    chrome,
                }
            })
            .collect();

        // An unborn `HEAD` has no commit and no history to walk; an unreadable
        // one has neither either, but it is not an empty repository.
        let (head_oid, unborn): (Option<Box<str>>, bool) = match &snapshot.status.head {
            HeadState::Branch { oid, .. } | HeadState::Detached { oid } => {
                (Some(oid.as_str().into()), false)
            }
            HeadState::Unborn { .. } => (None, true),
            HeadState::Unknown => (None, false),
        };

        Self {
            data_state: SidebarDataState::Ready,
            detached: matches!(snapshot.status.head, HeadState::Detached { .. }),
            head_oid,
            unborn,
            upstream: upstream_push_target(snapshot),
            up_to_date: matches!(
                snapshot.status.tracking,
                TrackingState::Compared { ahead: 0, .. }
            ),
            tracking: snapshot.status.tracking.clone(),
            remotes,
            mutation: None,
            error: None,
            staged_count,
            files,
            groups,
            history: Vec::new(),
            history_loaded: false,
            history_requested: false,
            history_complete: false,
            next_history_page: 0,
            reloading: false,
            branch,
            key,
            chrome,
        }
    }

    /// Appends one loaded history page. A short page is the last one.
    ///
    /// An empty page counts as loaded: the answer "this walk produced nothing"
    /// is a fact the section is allowed to state, while "nobody asked" is not.
    pub fn append_history(&mut self, page: &[CommitSummary]) {
        self.history.reserve(page.len());
        for commit in page {
            self.history.push(CommitRow::new(commit));
        }
        self.history_loaded = true;
        self.history_requested = true;
        self.history_complete = page.len() < HISTORY_PAGE_SIZE as usize;
        self.next_history_page = self.next_history_page.saturating_add(1);
    }

    /// True once a page has been asked for and landed for this binding, and
    /// then for as long as the panel stays bound to this worktree.
    ///
    /// Expanding `History` is the only thing that ever asks for a page, and the
    /// count is a walk from `HEAD` that a commit invalidates. So a section the
    /// user opened and closed again would lose its count at the next commit and
    /// never get it back - and a closed section's count is the only thing
    /// saying what is behind it.
    pub fn history_requested(&self) -> bool {
        self.history_requested
    }

    /// The count the `History` header shows, or `None` while no page has been
    /// asked for.
    ///
    /// `History (0)` beside a repository full of commits is a false claim. An
    /// unborn `HEAD` really does have zero commits, so there the count is the
    /// truth.
    pub fn history_count(&self) -> Option<usize> {
        (self.history_loaded || self.unborn).then_some(self.history.len())
    }

    /// True when the application should ask for the first history page.
    pub fn wants_history(&self) -> bool {
        matches!(self.data_state, SidebarDataState::Ready)
            && !self.history_loaded
            && self.head_oid.is_some()
    }

    /// True while a further page can exist. An unborn `HEAD` has no walk to
    /// page through, so it is offered no pager.
    fn can_load_history(&self) -> bool {
        !self.history_complete && !self.unborn
    }

    /// Records that the first page has been asked for, so expanding, closing,
    /// and reopening `History` cannot queue a second walk.
    pub fn mark_history_requested(&mut self) {
        self.history_requested = true;
    }

    /// Marks the panel as re-reading Git. The rows already on screen stay:
    /// a refresh replaces them when it lands rather than blanking the surface
    /// while it runs.
    pub fn set_reloading(&mut self, reloading: bool) {
        self.reloading = reloading;
    }

    pub fn set_mutation(&mut self, mutation: Option<MutationKind>) {
        self.mutation = mutation;
    }

    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Moves everything the replacing model does not have onto it: the rows a
    /// reload has not read yet, the in-flight mutation, its last failure, and
    /// the history already loaded while `HEAD` still names the same commit.
    ///
    /// A reload of the same worktree keeps the panel it is replacing on
    /// screen. `next` is the empty placeholder installed before Git has run,
    /// and rendering it would blank the whole panel on every refresh - bearable
    /// for a refresh the user asked for, and not for one the filesystem asked
    /// for. Rebinding to another worktree is a first load and keeps the
    /// placeholder, because the rows on screen belong to a worktree nobody is
    /// looking at any more.
    ///
    /// Neither the mutation nor the failure belongs to a snapshot, so a reload
    /// must not drop them. A push can easily outlive the refresh it triggered,
    /// and a panel rebuilt without its mutation re-enables every command.
    ///
    /// History is carried on a narrower condition, because it is a walk from
    /// `HEAD` and only survives as long as `HEAD` does not move. That a page
    /// was asked for at all outlives the walk, and is carried whatever `HEAD`
    /// did.
    pub fn carry_mutation_into(&self, next: &mut Self) {
        if self.key != next.key {
            return;
        }
        if matches!(self.data_state, SidebarDataState::Ready)
            && matches!(next.data_state, SidebarDataState::Loading)
        {
            // The placeholder holds nothing this does not, so taking the whole
            // panel over is also how the mutation, the failure and the history
            // below are carried.
            *next = self.clone();
            next.reloading = true;
            return;
        }
        next.mutation = self.mutation;
        next.error = self.error.clone();
        next.history_requested |= self.history_requested;
        // The loading placeholder between two snapshots has not read `HEAD`
        // yet, and that gap must not look like a `HEAD` that moved.
        if next.head_oid.is_none() && !next.unborn {
            next.head_oid = self.head_oid.clone();
        }
        if next.head_oid == self.head_oid && next.history.is_empty() {
            next.history = self.history.clone();
            next.history_loaded = self.history_loaded;
            next.history_complete = self.history_complete;
            next.next_history_page = self.next_history_page;
        }
    }

    /// What the panel says is happening, beside a spinner: the mutation it is
    /// running, or the reload it is waiting on.
    fn progress_label(&self) -> Option<&'static str> {
        self.mutation
            .map(MutationKind::progress_label)
            .or_else(|| self.reloading.then_some("Refreshing…"))
    }

    pub fn groups(&self) -> &[ChangeGroup] {
        &self.groups
    }

    pub fn group(&self, area: ChangeArea) -> Option<&ChangeGroup> {
        self.groups.iter().find(|group| group.area == area)
    }

    /// True while a mutation is running, which disables every command so two
    /// index-touching operations can never be launched from one surface.
    pub fn busy(&self) -> bool {
        self.mutation.is_some()
    }

    /// Commit needs a message, something staged, and no mutation in flight.
    pub fn can_commit(&self, message: &str) -> bool {
        !message.trim().is_empty() && self.staged_count > 0 && !self.busy()
    }

    /// Push needs a non-detached branch that is actually ahead of an upstream.
    /// A branch with no upstream is published instead; see
    /// [`Self::can_publish`].
    pub fn can_push(&self) -> bool {
        self.branch.is_some()
            && !self.detached
            && !self.busy()
            && self.upstream.is_some()
            && !self.up_to_date
    }

    /// Publishing needs a branch Git could name, a remote to publish to, no
    /// upstream from either source, and no mutation in flight.
    ///
    /// Both sources are consulted because they fail apart. [`Self::upstream`]
    /// is built from the ref listing and is `None` both when the branch has no
    /// upstream and when this panel could not name the one it has, while
    /// `tracking` is what `git status` reported and knows the difference.
    pub fn can_publish(&self) -> bool {
        self.branch.is_some()
            && !self.detached
            && !self.busy()
            && self.upstream.is_none()
            && !self.upstream_reported()
            && !self.remotes.is_empty()
    }

    /// Whether `git status` reported an upstream for the checked-out branch,
    /// whether or not it could be compared against or named.
    pub fn upstream_reported(&self) -> bool {
        match &self.tracking {
            TrackingState::NoUpstream => false,
            TrackingState::Compared { .. } => true,
            TrackingState::Unknown { upstream } => upstream.is_some(),
        }
    }

    /// Why this panel offers no branch mutation, for the caption that replaces
    /// the button.
    ///
    /// The four states with no branch to push are four different facts: a
    /// detached `HEAD` is on no branch, an unborn one has no commit to send, a
    /// branch whose ref name Git reported in bytes this cannot represent has
    /// both and still cannot be named back to Git, and a `HEAD` that could not
    /// be read is not any of the three.
    pub fn no_branch_reason(&self) -> &'static str {
        if self.detached {
            "Detached HEAD — nothing to push"
        } else if self.unborn {
            "Nothing committed yet — nothing to push"
        } else if self.head_oid.is_some() {
            "This branch's name is not representable — nothing to push"
        } else {
            "HEAD could not be read — nothing to push"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceControlChrome {
    heading: Box<str>,
    subheading: Box<str>,
    refresh_id: Id,
    refresh_name: Box<str>,
    /// The `Try again` button of the failed panel, which cannot reuse
    /// [`Self::refresh_id`]: the header's refresh icon is pushed on every
    /// frame, including the failed ones, so both buttons exist together and
    /// the one iced visits second is handed the emptied state the first left
    /// behind.
    retry_id: Id,
    popover_id: Id,
    /// The popover surface's own name, which cannot be [`Self::popover_id`].
    ///
    /// The popover wraps the anchor button, so both nodes exist in the same
    /// frame, and iced keys persistent widget state by custom name in one map
    /// per frame. The second node visited under a shared name overwrites the
    /// first and the loser's state is dropped - and the popover is stateless
    /// while the button holds real state, so the button can end up with a
    /// state its next downcast panics on.
    popover_surface_id: Id,
    close_id: Id,
    open_panel_name: Box<str>,
    close_panel_name: Box<str>,
    files_query_id: Id,
    commit_input_id: Id,
    commit_id: Id,
    push_id: Id,
    /// One entry per configured remote, parallel to
    /// [`SourceControlModel::remotes`], so the publish picker builds no
    /// identity while rendering.
    publish_remote_ids: Vec<Id>,
    publish_remote_names: Vec<Box<str>>,
    publish_id: Id,
    publish_cancel_id: Id,
    publish_branch_id: Id,
    more_history_id: Id,
    section_ids: [Id; 4],
}

impl SourceControlChrome {
    /// `label` is what the header calls the checked-out branch: the lossy
    /// decoding of what `git status` reported, because a name this panel cannot
    /// hand back to Git is still the name on screen.
    fn new(key: &ActiveWorktreeKey, label: Option<&str>, remotes: &[String]) -> Self {
        let worktree_id = key.worktree_id;
        let name = key
            .canonical_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.canonical_path.display().to_string());
        Self {
            heading: format!("Source control — {name}").into_boxed_str(),
            subheading: match label {
                Some(label) => label.to_owned().into_boxed_str(),
                None => "detached HEAD".into(),
            },
            refresh_id: entity_id("panel.refresh", worktree_id),
            refresh_name: format!("Reload source control for {name}").into_boxed_str(),
            retry_id: entity_id("panel.retry", worktree_id),
            popover_id: entity_id("panel.popover", worktree_id),
            popover_surface_id: entity_id("panel.popover.surface", worktree_id),
            close_id: entity_id("panel.close", worktree_id),
            open_panel_name: format!("Open source control for {name}").into_boxed_str(),
            close_panel_name: format!("Close source control for {name}").into_boxed_str(),
            files_query_id: entity_id("panel.files.query", worktree_id),
            commit_input_id: entity_id("panel.commit.message", worktree_id),
            commit_id: entity_id("panel.commit", worktree_id),
            push_id: entity_id("panel.push", worktree_id),
            publish_remote_ids: remotes
                .iter()
                .map(|remote| stable_id(format!("panel.publish.remote.{worktree_id}.{remote}")))
                .collect(),
            publish_remote_names: remotes
                .iter()
                .map(|remote| format!("Publish to {remote}").into_boxed_str())
                .collect(),
            publish_id: entity_id("panel.publish", worktree_id),
            publish_cancel_id: entity_id("panel.publish.cancel", worktree_id),
            publish_branch_id: entity_id("panel.publish.branch", worktree_id),
            more_history_id: entity_id("panel.history.more", worktree_id),
            section_ids: PanelSection::ALL
                .map(|section| entity_id("panel.section", format!("{worktree_id}.{section:?}"))),
        }
    }

    fn section_id(&self, section: PanelSection) -> &Id {
        &self.section_ids[section as usize]
    }
}

/// The branch `HEAD` is on, as Git itself named it.
///
/// Read from the ref listing rather than from the status label: `git status`
/// reports a branch name as bytes and the label is a lossy decoding of them,
/// while `for-each-ref` leaves out a ref whose name cannot be handed back to
/// Git at all. A branch this returns `None` for is one no push, publish, or
/// other branch mutation can name - as is an unborn branch, which has no ref
/// yet.
fn head_branch(snapshot: &RepositorySnapshot) -> Option<ValidatedBranchName> {
    let name = snapshot
        .local_refs
        .iter()
        .find(|record| record.head)?
        .name
        .as_str()
        .strip_prefix("refs/heads/")?;
    ValidatedBranchName::parse(name).ok()
}

/// The push target for a branch that already has an upstream.
fn upstream_push_target(snapshot: &RepositorySnapshot) -> Option<PushTarget> {
    let upstream = snapshot
        .local_refs
        .iter()
        .find(|record| record.head)?
        .upstream
        .clone()?;
    let remote = remote_of_ref(&snapshot.remotes, &upstream)?;
    Some(PushTarget::Upstream {
        remote,
        remote_ref: upstream,
    })
}

/// The remote naming `refs/remotes/<remote>/<branch>`, taken from the
/// configured remote names.
///
/// A remote name may itself contain a slash, so the split has to come from the
/// configured names and the longest match wins; splitting on the first slash
/// would mis-attribute `refs/remotes/team/fork/main`.
fn remote_of_ref(remotes: &[RemoteRecord], reference: &ValidatedRefName) -> Option<String> {
    let rest = reference.as_str().strip_prefix("refs/remotes/")?;
    remotes
        .iter()
        .filter(|remote| {
            rest.strip_prefix(&remote.name)
                .is_some_and(|branch| branch.starts_with('/'))
        })
        .max_by_key(|remote| remote.name.len())
        .map(|remote| remote.name.clone())
}

fn change_kind_label(kind: ChangeKind) -> String {
    match kind {
        ChangeKind::Added => "A".to_owned(),
        ChangeKind::Modified => "M".to_owned(),
        ChangeKind::Deleted => "D".to_owned(),
        ChangeKind::Renamed => "R".to_owned(),
        ChangeKind::Copied => "C".to_owned(),
        ChangeKind::TypeChanged => "T".to_owned(),
        ChangeKind::Untracked => "?".to_owned(),
        ChangeKind::Ignored => "!".to_owned(),
        ChangeKind::Unmerged => "U".to_owned(),
        ChangeKind::Unknown(code) => code.to_string(),
    }
}

fn conflict_label(conflict: ConflictKind) -> String {
    match conflict {
        ConflictKind::BothDeleted => "both deleted",
        ConflictKind::AddedByUs => "added by us",
        ConflictKind::DeletedByThem => "deleted by them",
        ConflictKind::AddedByThem => "added by them",
        ConflictKind::DeletedByUs => "deleted by us",
        ConflictKind::BothAdded => "both added",
        ConflictKind::BothModified => "both modified",
    }
    .to_owned()
}

/// A widget-identity fragment for a repository path.
///
/// The lossy display alone could collide between two paths that differ only in
/// undecodable bytes, so the byte length disambiguates them.
fn path_key(path: &RepoPath) -> String {
    format!("{}#{}", path.display(), path.as_bytes().len())
}

/// Formats a commit time in the offset the commit itself recorded, so a commit
/// reads in the timezone it was made in rather than the reader's.
fn format_commit_time(identity: &CommitIdentity) -> String {
    let local = identity
        .timestamp
        .saturating_add(i64::from(identity.offset_minutes) * 60);
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    let seconds = local.rem_euclid(86_400);
    let (hour, minute) = (seconds / 3_600, (seconds % 3_600) / 60);
    let offset = identity.offset_minutes.abs();
    let sign = if identity.offset_minutes < 0 {
        '-'
    } else {
        '+'
    };
    format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} {sign}{:02}{:02}",
        offset / 60,
        offset % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic
/// Gregorian date, exact over the whole range a commit can record.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
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

/// What the source-control panel currently highlights.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PanelSelection {
    File(RepoPath),
    Change { path: RepoPath, area: ChangeArea },
}

/// Persistent interaction state owned by the application.
///
/// Everything here is local to the surface: what is typed, what is open, and
/// what is selected. Nothing that a Git or GitHub call produced lives here -
/// those are immutable snapshots on [`SidebarModel`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    query: String,
    filter: SidebarFilter,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    expanded_pull_requests: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
    active_worktree: Option<ActiveWorktreeKey>,
    /// Sections toggled away from their default. Storing the toggles rather
    /// than the open set keeps `default()` meaningful: `Changes` starts open
    /// and the rest start closed.
    toggled_sections: BTreeSet<PanelSection>,
    /// Compact mode only: whether the source-control popover is showing.
    panel_open: bool,
    files_query: String,
    commit_message: String,
    collapsed_directories: BTreeSet<RepoPath>,
    publish: Option<PublishDraft>,
    discard: Option<DiscardConfirmation>,
    panel_selection: Option<PanelSelection>,
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

    /// The worktree the source-control panel is bound to.
    pub fn active_worktree(&self) -> Option<&ActiveWorktreeKey> {
        self.active_worktree.as_ref()
    }

    pub fn files_query(&self) -> &str {
        &self.files_query
    }

    pub fn commit_message(&self) -> &str {
        &self.commit_message
    }

    /// The discard awaiting confirmation, which the application mounts with
    /// [`discard_confirmation_dialog`].
    pub fn pending_discard(&self) -> Option<&DiscardConfirmation> {
        self.discard.as_ref()
    }

    pub fn publish_draft(&self) -> Option<&PublishDraft> {
        self.publish.as_ref()
    }

    /// Clears the commit input. A successful commit calls this; a failed one
    /// must not, because the message is the user's work.
    pub fn clear_commit_message(&mut self) {
        self.commit_message.clear();
    }

    /// Rebinds the source-control panel after a focus or model change.
    ///
    /// A card that names a known worktree always takes over. A card that names
    /// none - a browser, a pull-request card, or a terminal outside every
    /// worktree - preserves the last valid key, but only while that worktree
    /// still exists: once it is closed or removed, the panel falls back to
    /// whatever the currently focused card names, and to nothing if it names
    /// no worktree either.
    ///
    /// Returns true when the bound worktree changed, which is the application's
    /// signal to load a snapshot for the new one.
    pub fn sync_active_worktree(&mut self, model: &SidebarModel, focus: &SidebarFocus) -> bool {
        let next = match focus.path().and_then(|path| model.worktree_at(path)) {
            Some(key) => Some(key),
            None => self
                .active_worktree
                .clone()
                .filter(|key| model.contains_worktree(key)),
        };
        if self.active_worktree == next {
            return false;
        }
        self.active_worktree = next;
        // Every panel-local draft belongs to the worktree it was typed for.
        self.files_query.clear();
        self.commit_message.clear();
        self.collapsed_directories.clear();
        self.publish = None;
        self.discard = None;
        self.panel_selection = None;
        true
    }

    /// Whether one panel section is showing.
    ///
    /// Public because expanding a section is the only request the `History`
    /// section has: the application asks for the first page when this becomes
    /// true, and nothing else in the panel asks for one.
    pub fn section_expanded(&self, section: PanelSection) -> bool {
        self.toggled_sections.contains(&section) != section.expanded_by_default()
    }

    /// Applies only local, pure presentation transitions. Commands such as
    /// opening a worktree, staging, or pushing remain for the application.
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
            SidebarAction::ToggleSection { section, .. } => {
                toggle_membership(&mut self.toggled_sections, *section);
            }
            SidebarAction::ToggleSourceControlPanel { .. } => self.panel_open = !self.panel_open,
            SidebarAction::FilesQueryChanged { query, .. } => self.files_query.clone_from(query),
            SidebarAction::ToggleDirectory { path, .. } => {
                if !self.collapsed_directories.remove(path) {
                    self.collapsed_directories.insert(path.clone());
                }
            }
            SidebarAction::OpenFile { path, .. } => {
                self.panel_selection = Some(PanelSelection::File(path.clone()));
            }
            SidebarAction::OpenChange { path, area, .. } => {
                self.panel_selection = Some(PanelSelection::Change {
                    path: path.clone(),
                    area: *area,
                });
            }
            SidebarAction::CommitMessageChanged { message, .. } => {
                self.commit_message.clone_from(message);
            }
            SidebarAction::RequestDiscard { key, request } => {
                self.discard = Some(DiscardConfirmation {
                    key: key.clone(),
                    request: request.clone(),
                });
            }
            SidebarAction::ConfirmDiscard { .. } | SidebarAction::CancelDiscard => {
                self.discard = None;
            }
            SidebarAction::OpenPublish {
                key,
                branch,
                remotes,
            } => {
                self.publish = Some(PublishDraft::new(key.clone(), branch.clone(), remotes));
            }
            SidebarAction::PublishRemoteChanged { remote, .. } => {
                if let Some(publish) = self.publish.as_mut() {
                    publish.remote.clone_from(remote);
                }
            }
            SidebarAction::PublishBranchChanged { remote_branch, .. } => {
                if let Some(publish) = self.publish.as_mut() {
                    publish.remote_branch.clone_from(remote_branch);
                }
            }
            SidebarAction::CancelPublish | SidebarAction::Push { .. } => self.publish = None,
            SidebarAction::OpenRepositoryPicker
            | SidebarAction::NewWorktree { .. }
            | SidebarAction::Refresh
            | SidebarAction::RefreshWorktree { .. }
            | SidebarAction::CreatePullRequest { .. }
            | SidebarAction::OpenSettings
            | SidebarAction::Stage { .. }
            | SidebarAction::Unstage { .. }
            | SidebarAction::Commit { .. }
            | SidebarAction::LoadMoreHistory { .. }
            | SidebarAction::Resize { .. }
            | SidebarAction::ResizeFinished => {}
        }
    }
}

/// All sidebar intents. Payloads are self-contained so `app.rs` does not need
/// to recover a path or URL from ephemeral row indexes, and every
/// source-control intent carries the whole [`ActiveWorktreeKey`] so a result
/// arriving after the panel rebound is recognisably stale.
///
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
        worktree: Option<PathBuf>,
    },
    OpenSettings,

    // Source-control panel.
    ToggleSection {
        key: ActiveWorktreeKey,
        section: PanelSection,
    },
    /// Compact mode: shows or hides the source-control popover.
    ToggleSourceControlPanel {
        key: ActiveWorktreeKey,
    },
    /// Reloads the bound worktree's snapshot, leaving every other surface
    /// alone.
    RefreshWorktree {
        key: ActiveWorktreeKey,
    },
    FilesQueryChanged {
        key: ActiveWorktreeKey,
        query: String,
    },
    ToggleDirectory {
        key: ActiveWorktreeKey,
        path: RepoPath,
    },
    /// Opens one tracked file in the worktree's repository card.
    OpenFile {
        key: ActiveWorktreeKey,
        path: RepoPath,
    },
    /// Opens one changed path in the repository card's diff surface, on the
    /// basis the area names.
    OpenChange {
        key: ActiveWorktreeKey,
        path: RepoPath,
        area: ChangeArea,
    },
    Stage {
        key: ActiveWorktreeKey,
        selection: Vec<PathSelection>,
    },
    Unstage {
        key: ActiveWorktreeKey,
        selection: Vec<PathSelection>,
    },
    /// Raises the confirmation dialog. `request.confirmed` is false here, and
    /// the service rejects an unconfirmed discard, so this can never destroy
    /// anything on its own.
    RequestDiscard {
        key: ActiveWorktreeKey,
        request: DiscardRequest,
    },
    /// The confirmed discard, carrying exactly the request the dialog counted.
    ConfirmDiscard {
        key: ActiveWorktreeKey,
        request: DiscardRequest,
    },
    CancelDiscard,
    CommitMessageChanged {
        key: ActiveWorktreeKey,
        message: String,
    },
    Commit {
        key: ActiveWorktreeKey,
        message: String,
    },
    Push {
        key: ActiveWorktreeKey,
        branch: ValidatedBranchName,
        target: PushTarget,
    },
    /// Opens the remote picker for a branch with no upstream.
    ///
    /// The configured remotes travel with the intent so the draft can choose
    /// between them itself; see [`PublishDraft::new`].
    OpenPublish {
        key: ActiveWorktreeKey,
        branch: ValidatedBranchName,
        remotes: Vec<String>,
    },
    PublishRemoteChanged {
        key: ActiveWorktreeKey,
        remote: String,
    },
    PublishBranchChanged {
        key: ActiveWorktreeKey,
        remote_branch: String,
    },
    CancelPublish,
    LoadMoreHistory {
        key: ActiveWorktreeKey,
        page: u32,
    },
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
/// that focus cannot be moved away while the dialog is up. Every surface below
/// is handed the bit, and every command any of them mounts is withheld by
/// [`gated`] while it is set: the application refuses a pane action for as long
/// as a modal is up, so a control that still published one would report itself
/// enabled, take the press, and do nothing.
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
    let header_loading = loading && !model.projects.is_empty();
    let mut content = widget::column::with_capacity(7).push(
        widget::container(header(model, state, compact, header_loading, modal_open))
            .padding([0, spacing.space_xxxs]),
    );

    // Each density is given the controls its width can hold, and
    // `filtered_projects` applies exactly the state that has a control here:
    // the rail fits the filter as an icon column but cannot fit a text input,
    // so it offers the filter alone and its query is left unapplied.
    if compact && !model.projects.is_empty() {
        content = content.push(
            widget::container(compact_filters(state.filter, modal_open))
                .padding(spacing.space_xxxs),
        );
    } else if state.controls_expanded && search_is_reachable(model, mode) {
        content = content.push(
            widget::container(controls(state, refresh_enabled, modal_open))
                .class(theme::Container::Secondary)
                .padding(spacing.space_xxs),
        );
    }

    // The navigation list and the source-control panel share the pane. The
    // panel is the working surface, so it gets the larger share; both scroll
    // independently rather than one pushing the other off-screen.
    let panel = source_control_surface(model, state, compact, modal_open);
    content = content.push(
        widget::container(body(model, state, compact, modal_open))
            .padding([spacing.space_xxxs, spacing.space_xxxs])
            .height(if panel.is_some() {
                Length::FillPortion(1)
            } else {
                Length::Fill
            }),
    );
    if let Some(panel) = panel {
        content = content.push(widget::divider::horizontal::light()).push(
            widget::container(panel).height(if compact {
                Length::Shrink
            } else {
                Length::FillPortion(2)
            }),
        );
    }
    content = content
        .push(widget::divider::horizontal::light())
        .push(footer(model, state, compact, modal_open))
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
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        return widget::container(icon_action(
            "folder-open-symbolic",
            "Open repository",
            gated(SidebarAction::OpenRepositoryPicker, modal_open),
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
                .on_press_maybe(gated(SidebarAction::ToggleControls, modal_open))
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
        gated(SidebarAction::OpenRepositoryPicker, modal_open),
        stable_id("open-repository"),
    ));
    if let Some(project) = selected_project(model, state) {
        children.push(icon_action(
            "list-add-symbolic",
            &*project.chrome.new_worktree_name,
            gated(
                SidebarAction::NewWorktree {
                    project_id: project.id,
                    repository: project.repository.clone(),
                },
                modal_open,
            ),
            project.chrome.header_new_worktree_id.clone(),
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
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
    actions.push(filters(state.filter, modal_open));
    actions.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
    if refresh_enabled {
        actions.push(icon_action(
            "view-refresh-symbolic",
            "Refresh projects",
            gated(SidebarAction::Refresh, modal_open),
            stable_id("refresh"),
        ));
    }
    widget::column::with_children(vec![
        search(state, modal_open),
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
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let settings = icon_action(
        "emblem-system-symbolic",
        "Settings",
        gated(SidebarAction::OpenSettings, modal_open),
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
                    gated(
                        SidebarAction::NewWorktree {
                            project_id: project.id,
                            repository: project.repository.clone(),
                        },
                        modal_open,
                    ),
                    project.chrome.new_worktree_id.clone(),
                ),
                icon_action(
                    "document-new-symbolic",
                    &*project.chrome.create_pull_request_name,
                    gated(
                        SidebarAction::CreatePullRequest {
                            project_id: project.id,
                            worktree: None,
                        },
                        modal_open,
                    ),
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

/// The project filter input.
///
/// A text input with no `on_input` is disabled, which is what standing it down
/// means here: the pane keeps the focus it held when the dialog opened, so a
/// keystroke the overlay does not consume would otherwise still be typed into
/// this box.
fn search<'a>(state: &'a SidebarState, modal_open: bool) -> Element<'a, SidebarAction> {
    let width = Length::Fill;
    let mut input = widget::text_input::search_input("Search projects", &state.query).width(width);
    if !modal_open {
        input = input.on_input(SidebarAction::SearchChanged);
        if !state.query.is_empty() {
            input = input.on_clear(SidebarAction::SearchChanged(String::new()));
        }
    }
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

fn filters(filter: SidebarFilter, modal_open: bool) -> Element<'static, SidebarAction> {
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
                .on_press_maybe(gated(SidebarAction::SetFilter(value), modal_open)),
        );
    }
    row.into()
}

/// The filter as an icon column, which is all the rail has room for. Without
/// it a filter chosen while expanded would keep hiding projects with no
/// control left to clear it.
fn compact_filters(filter: SidebarFilter, modal_open: bool) -> Element<'static, SidebarAction> {
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
                .on_press_maybe(gated(SidebarAction::SetFilter(value), modal_open)),
        );
    }
    column.into()
}

fn body<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    match &model.data_state {
        SidebarDataState::Loading if !model.projects.is_empty() => {
            project_list(model, state, compact, modal_open)
        }
        SidebarDataState::Loading => loading(compact),
        SidebarDataState::Error(message) => error(message, compact, modal_open),
        SidebarDataState::Ready => project_list(model, state, compact, modal_open),
    }
}

/// The scrollable project list. `responsive` is avoided here because it does
/// not forward child accessibility nodes, and every row in this list is a
/// named a11y target.
fn project_list<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let projects = filtered_projects(model, state, compact);
    if projects.is_empty() {
        return empty(state, compact, modal_open);
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
            modal_open,
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
        .center(Length::Fill)
        .padding(spacing.space_xs)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn error<'a>(message: &'a str, compact: bool, modal_open: bool) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    if compact {
        return widget::container(
            widget::column::with_children(vec![
                widget::icon::from_name("dialog-warning-symbolic")
                    .size(STATUS_ICON_SIZE)
                    .into(),
                icon_action(
                    "view-refresh-symbolic",
                    format!("Couldn't load projects. {message}. Try again"),
                    gated(SidebarAction::Refresh, modal_open),
                    stable_id("error.retry"),
                ),
            ])
            .spacing(spacing.space_xxs)
            .align_x(Alignment::Center),
        )
        .center(Length::Fill)
        .height(Length::Fill)
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
                .on_press_maybe(gated(SidebarAction::Refresh, modal_open))
                .into(),
        ])
        .align_x(Alignment::Center)
        .spacing(spacing.space_xxs),
    )
    .center(Length::Fill)
    .padding(spacing.space_s)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn empty(state: &SidebarState, compact: bool, modal_open: bool) -> Element<'static, SidebarAction> {
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
            gated(action, modal_open),
            stable_id("empty.action"),
        );
    }
    let spacing = cosmic::theme::spacing();
    let action = gated(action, modal_open);
    let action_button: Element<'static, SidebarAction> =
        if !searching && state.filter == SidebarFilter::All {
            button::suggested(action_label)
                .id(stable_id("empty.action"))
                .on_press_maybe(action)
                .into()
        } else {
            button::standard(action_label)
                .id(stable_id("empty.action"))
                .on_press_maybe(action)
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
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let entity = project.project;
    let chrome = &entity.chrome;
    let project_id = entity.id;
    if compact {
        return compact_project(project, selection, modal_open);
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
    .on_press_maybe(gated(
        SidebarAction::OpenRepository {
            project_id,
            repository: entity.repository.clone(),
        },
        modal_open,
    ));
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
            gated(SidebarAction::ToggleProject(project_id), modal_open),
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
            gated(
                SidebarAction::NewWorktree {
                    project_id,
                    repository: entity.repository.clone(),
                },
                modal_open,
            ),
            chrome.new_worktree_id.clone(),
        ),
        icon_action(
            "document-new-symbolic",
            &*chrome.create_pull_request_name,
            gated(
                SidebarAction::CreatePullRequest {
                    project_id,
                    worktree: None,
                },
                modal_open,
            ),
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
                    modal_open,
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
                    modal_open,
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
    modal_open: bool,
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
        .on_press_maybe(gated(
            SidebarAction::OpenRepository {
                project_id: entity.id,
                repository: entity.repository.clone(),
            },
            modal_open,
        ));
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
    modal_open: bool,
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
        .on_press_maybe(gated(
            SidebarAction::OpenWorktree {
                project_id,
                worktree_id: worktree.id,
                path: worktree.path.clone(),
            },
            modal_open,
        ));
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
    modal_open: bool,
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
                    gated(SidebarAction::TogglePullRequests(project.id), modal_open),
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
            column = column.push(pull_request_view(
                project.id,
                pull_request,
                PullRequestSurface::Navigation,
                selection,
                modal_open,
            ));
        }
    }
    column.into()
}

fn pull_request_view<'a>(
    project_id: ProjectId,
    pull_request: &'a SidebarPullRequest,
    surface: PullRequestSurface,
    selection: Option<SidebarSelection>,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let chrome = &pull_request.chrome;
    let action = gated(
        SidebarAction::OpenPullRequest {
            project_id,
            number: pull_request.number,
            url: pull_request.url.clone(),
        },
        modal_open,
    );

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
        .id(chrome.open_id(surface).clone())
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
        .on_press_maybe(action);
    widget::tooltip::tooltip(
        action,
        widget::text::caption(pull_request.title.as_str()),
        widget::tooltip::Position::Bottom,
    )
    .into()
}

/// The source-control surface for the active worktree.
///
/// Expanded mode renders the panel inline below the navigation; compact mode
/// renders the very same panel inside a COSMIC popover behind a named
/// source-control icon. The panel body is built by one function in both cases,
/// so a command cannot exist in one density and not the other.
///
/// `modal_open` stands every command below here down; see [`gated`].
fn source_control_surface<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
    modal_open: bool,
) -> Option<Element<'a, SidebarAction>> {
    let key = state.active_worktree()?;
    let source = model
        .source_control
        .as_ref()
        .filter(|source| &source.key == key)?;
    let project = model.project(key.project_id);
    if !compact {
        return Some(source_control_panel(state, source, project, modal_open));
    }

    let spacing = cosmic::theme::spacing();
    let chrome = &source.chrome;
    let label: &str = if state.panel_open {
        &chrome.close_panel_name
    } else {
        &chrome.open_panel_name
    };
    let anchor = widget::container(
        button::icon(icon::from_name("emblem-shared-symbolic").size(ROW_ICON_SIZE))
            .id(chrome.popover_id.clone())
            .name(label)
            .tooltip(label)
            .width(ICON_CONTROL_SIZE)
            .height(ICON_CONTROL_SIZE)
            .selected(state.panel_open)
            .on_press_maybe(gated(
                SidebarAction::ToggleSourceControlPanel { key: key.clone() },
                modal_open,
            )),
    )
    .center_x(Length::Fill)
    .padding(spacing.space_xxxs);
    if !state.panel_open {
        return Some(anchor.into());
    }
    // The popover is modal so a click cannot land on the workspace behind a
    // 360-wide overlay by accident; the header's close button is therefore the
    // explicit way back out.
    let popup = widget::container(
        widget::column::with_children(vec![
            widget::row::with_children(vec![
                widget::text::caption_heading(&*chrome.heading)
                    .width(Length::Fill)
                    .into(),
                icon_action(
                    "window-close-symbolic",
                    &*chrome.close_panel_name,
                    gated(
                        SidebarAction::ToggleSourceControlPanel { key: key.clone() },
                        modal_open,
                    ),
                    chrome.close_id.clone(),
                ),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs)
            .into(),
            source_control_panel(state, source, project, modal_open),
        ])
        .spacing(spacing.space_xxs),
    )
    .class(theme::Container::Dialog(false))
    .padding(spacing.space_xs)
    .width(Length::Fixed(COMPACT_PANEL_WIDTH))
    .max_height(640.0);
    Some(
        widget::popover(anchor)
            .id(chrome.popover_surface_id.clone())
            .position(widget::popover::Position::Bottom)
            .modal(true)
            .popup(popup)
            .into(),
    )
}

/// The four collapsible sections. Identical in both densities.
fn source_control_panel<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    project: Option<&'a SidebarProject>,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let chrome = &source.chrome;
    let key = &source.key;
    let mut column = widget::column::with_capacity(8).spacing(spacing.space_xxxs);
    column = column.push(
        widget::row::with_children(vec![
            widget::column::with_children(vec![
                widget::text::caption_heading(&*chrome.heading)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                widget::text::caption(&*chrome.subheading)
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
            icon_action(
                "view-refresh-symbolic",
                &*chrome.refresh_name,
                gated(
                    SidebarAction::RefreshWorktree { key: key.clone() },
                    modal_open,
                ),
                chrome.refresh_id.clone(),
            ),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs),
    );

    // Both notices are permanent slots. The failure arm below this ends in a
    // named `Try again` button, and iced reconciles a named child at the index
    // its name held last frame while writing the positionally unmatched
    // children over those same indices: a notice arriving above the button left
    // the reconciled child list one entry short, so the button had no tree at
    // all and disappeared from the one frame that was asking for a press.
    column = column.push(slot(source.progress_label().map(|label| {
        widget::row::with_children(vec![
            widget::progress_bar::circular::Circular::new()
                .size(14.0)
                .into(),
            widget::text::caption(label).into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs)
        .into()
    })));
    column = column.push(slot(source.error.as_deref().map(|error| {
        widget::text::caption(error)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into()
    })));

    match &source.data_state {
        SidebarDataState::Loading => {
            column = column.push(widget::text::caption("Loading source control…"));
            return widget::container(column)
                .padding([spacing.space_xxxs, spacing.space_xxs])
                .width(Length::Fill)
                .into();
        }
        SidebarDataState::Error(message) => {
            column = column
                .push(
                    widget::text::caption(format!("Couldn’t read this worktree. {message}"))
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                )
                .push(
                    button::standard("Try again")
                        .id(chrome.retry_id.clone())
                        .on_press_maybe(gated(
                            SidebarAction::RefreshWorktree { key: key.clone() },
                            modal_open,
                        )),
                );
            return widget::container(column)
                .padding([spacing.space_xxxs, spacing.space_xxs])
                .width(Length::Fill)
                .into();
        }
        SidebarDataState::Ready => {}
    }

    let mut sections = widget::column::with_capacity(4).spacing(spacing.space_xxxs);
    sections = sections.push(panel_section(
        key,
        PanelSection::Files,
        chrome.section_id(PanelSection::Files).clone(),
        Some(source.files.len().into()),
        state.section_expanded(PanelSection::Files),
        modal_open,
        || files_section(state, source, modal_open),
    ));
    sections = sections.push(panel_section(
        key,
        PanelSection::Changes,
        chrome.section_id(PanelSection::Changes).clone(),
        Some(
            source
                .groups()
                .iter()
                .map(|group| group.rows().len())
                .sum::<usize>()
                .into(),
        ),
        state.section_expanded(PanelSection::Changes),
        modal_open,
        || changes_section(state, source, modal_open),
    ));
    sections = sections.push(panel_section(
        key,
        PanelSection::History,
        chrome.section_id(PanelSection::History).clone(),
        source.history_count().map(RowCount::exact),
        state.section_expanded(PanelSection::History),
        modal_open,
        || history_section(source, modal_open),
    ));
    sections = sections.push(panel_section(
        key,
        PanelSection::PullRequests,
        chrome.section_id(PanelSection::PullRequests).clone(),
        pull_request_section_count(project),
        state.section_expanded(PanelSection::PullRequests),
        modal_open,
        || pull_requests_section(state, project, modal_open),
    ));

    column = column.push(
        widget::scrollable(sections)
            .class(cosmic::style::iced::Scrollable::Minimal)
            .width(Length::Fill)
            .height(Length::Fill),
    );
    widget::container(column)
        .padding([spacing.space_xxxs, spacing.space_xxs])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// How many pull requests the panel's own section header prints.
///
/// A listing that came back exactly as full as `PULL_REQUEST_LIST_LIMIT`
/// allowed is a floor, not a count, and nothing downstream can tell the
/// difference from the `Vec` alone.
fn pull_request_section_count(project: Option<&SidebarProject>) -> Option<RowCount> {
    let project = project?;
    let count = project.pull_requests.len();
    Some(if count >= usize::from(PULL_REQUEST_LIST_LIMIT) {
        RowCount::at_least(count)
    } else {
        RowCount::exact(count)
    })
}

/// The parts of one section header row: what it shows, and the single intent
/// the whole row raises.
///
/// Derived as one value so the chevron and the title beside it cannot drift
/// into two different intents: the row is one control, and what it says and
/// what it does are decided together.
///
/// Not `Eq`: it holds a [`SidebarAction`], and a resize request carries a
/// width.
#[derive(Debug, Clone, PartialEq)]
struct SectionHeader {
    icon: &'static str,
    title: String,
    /// The accessible name and the tooltip. Both name the action rather than
    /// the section, because the row is a control.
    action_name: String,
    /// `None` while a dialog is on screen; see [`gated`]. The stand-down lives
    /// in the value rather than at the widget because the widget is wrapped in
    /// a tooltip that keeps it out of the accessibility tree, so this is the
    /// only place the decision can be read.
    toggle: Option<SidebarAction>,
}

impl SectionHeader {
    fn new(
        key: &ActiveWorktreeKey,
        section: PanelSection,
        count: Option<RowCount>,
        expanded: bool,
        modal_open: bool,
    ) -> Self {
        Self {
            icon: if expanded {
                "go-down-symbolic"
            } else {
                "go-next-symbolic"
            },
            title: match count {
                Some(count) => format!("{} ({count})", section.title()),
                None => section.title().to_owned(),
            },
            action_name: format!(
                "{} {}",
                if expanded { "Collapse" } else { "Expand" },
                section.title()
            ),
            toggle: gated(
                SidebarAction::ToggleSection {
                    key: key.clone(),
                    section,
                },
                modal_open,
            ),
        }
    }
}

/// One collapsible section. The body closure runs only when it is expanded, so
/// a collapsed file tree costs nothing to build.
///
/// The whole header row is the disclosure control rather than only the chevron
/// on its left: the row is what a pointer aims at, and a title that sits inside
/// a toggle and does nothing reads as a dead surface. One control also keeps
/// one accessible name for one action, where a chevron button beside a title
/// button would announce the same toggle twice.
fn panel_section<'a>(
    key: &ActiveWorktreeKey,
    section: PanelSection,
    id: Id,
    count: Option<RowCount>,
    expanded: bool,
    modal_open: bool,
    body: impl FnOnce() -> Element<'a, SidebarAction>,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let parts = SectionHeader::new(key, section, count, expanded, modal_open);
    let row = button::custom(
        widget::row::with_children(vec![
            widget::container(icon::from_name(parts.icon).size(ROW_ICON_SIZE))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            widget::text::caption_heading(parts.title)
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
    .class(theme::Button::MenuItem)
    .id(id)
    .name(parts.action_name.clone())
    .padding([0, spacing.space_xxxs])
    .height(Length::Fixed(PANEL_ROW_HEIGHT))
    .width(Length::Fill)
    .on_press_maybe(parts.toggle);
    let header: Element<'a, SidebarAction> = widget::tooltip::tooltip(
        row,
        widget::text::caption(parts.action_name),
        widget::tooltip::Position::Bottom,
    )
    .into();
    let mut column = widget::column::with_capacity(2).spacing(0).push(header);
    if expanded {
        column = column.push(widget::container(body()).padding([
            0,
            0,
            spacing.space_xxs,
            spacing.space_s,
        ]));
    }
    column.into()
}

/// The tracked-file tree, its filter, and whatever the filter left out.
///
/// Four children, always, in this order: the filter, the notice, the rows, and
/// the cap's remainder. Both notices used to be pushed only when they had
/// something to say, which moved every row behind them; the rows are their own
/// nested column so that a list whose length is the repository's business
/// cannot move anything that is not a row.
///
/// Inside that nested column the rows are the only child kind and they carry no
/// [`Id`], which is what makes an arity nobody can fix survivable — see
/// [`panel_row`].
fn files_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let key = &source.key;
    let filter_key = key.clone();
    // A text input with no `on_input` is disabled, which is what standing the
    // filter down means here: the pane keeps the focus it held when the dialog
    // opened, so a keystroke the overlay does not consume would otherwise still
    // be typed into this box.
    let mut filter = widget::text_input::search_input("Filter files", state.files_query())
        .id(source.chrome.files_query_id.clone())
        .width(Length::Fill);
    if !modal_open {
        filter = filter.on_input(move |query| SidebarAction::FilesQueryChanged {
            key: filter_key.clone(),
            query,
        });
    }
    let folded = fold(state.files_query().trim());
    let (visible, hidden) = visible_file_rows(&source.files, &state.collapsed_directories, &folded);
    // An empty worktree and an over-narrow filter are different facts, and
    // neither is worth saying while there are rows to read.
    let notice = if source.files.is_empty() {
        Some("This worktree has no tracked files.")
    } else if visible.is_empty() {
        Some("No files match this filter.")
    } else {
        None
    };
    let mut rows = widget::column::with_capacity(visible.len()).spacing(0);
    for row in visible {
        let chrome = &row.chrome;
        let action = if row.directory {
            SidebarAction::ToggleDirectory {
                key: key.clone(),
                path: row.path.clone(),
            }
        } else {
            SidebarAction::OpenFile {
                key: key.clone(),
                path: row.path.clone(),
            }
        };
        let collapsed = state.collapsed_directories.contains(&row.path);
        let icon_name = if row.directory {
            if collapsed {
                "folder-symbolic"
            } else {
                "folder-open-symbolic"
            }
        } else {
            "text-x-generic-symbolic"
        };
        let selected = matches!(
            state.panel_selection.as_ref(),
            Some(PanelSelection::File(path)) if *path == row.path
        );
        rows = rows.push(panel_row(
            &*chrome.accessible_name,
            // The query flattens the tree, so a filtered row carries no
            // ancestry to indent against.
            if folded.is_empty() { row.depth } else { 0 },
            widget::row::with_children(vec![
                icon::from_name(icon_name).size(ROW_ICON_SIZE).into(),
                widget::text::caption(&*chrome.label)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxs),
            action,
            selected,
            modal_open,
        ));
    }
    widget::column::with_children(vec![
        filter.into(),
        slot(notice.map(|notice| widget::text::caption(notice).into())),
        rows.into(),
        slot((hidden > 0).then(|| {
            widget::text::caption(format!("{hidden} more files not shown — narrow the filter"))
                .into()
        })),
    ])
    .spacing(0)
    .into()
}

/// The file rows a frame renders, and how many further rows the cap hid.
///
/// A query flattens the tree to matching files, because a filtered hierarchy
/// would have to keep unmatched ancestors and would read as a list of folders.
/// Without a query the pre-order tree is walked and a collapsed directory's
/// whole subtree is skipped.
///
/// At most [`MAX_VISIBLE_FILE_ROWS`] references are ever collected. This runs
/// on every view rebuild, including rebuilds caused by terminal output the
/// panel has nothing to do with, so a worktree of a hundred thousand tracked
/// paths must not make a frame fill - and drop - a hundred-thousand-entry
/// vector to render four hundred rows. Counting the remainder allocates
/// nothing.
fn visible_file_rows<'a>(
    rows: &'a [FileRow],
    collapsed: &BTreeSet<RepoPath>,
    folded_query: &str,
) -> (Vec<&'a FileRow>, usize) {
    let mut visible: Vec<&'a FileRow> = Vec::with_capacity(rows.len().min(MAX_VISIBLE_FILE_ROWS));
    let mut hidden = 0usize;
    if !folded_query.is_empty() {
        for row in rows {
            if row.directory || !row.chrome.folded.contains(folded_query) {
                continue;
            }
            if visible.len() < MAX_VISIBLE_FILE_ROWS {
                visible.push(row);
            } else {
                hidden += 1;
            }
        }
        return (visible, hidden);
    }
    let mut skip_below: Option<usize> = None;
    for row in rows {
        if let Some(depth) = skip_below {
            if row.depth > depth {
                continue;
            }
            skip_below = None;
        }
        if visible.len() < MAX_VISIBLE_FILE_ROWS {
            visible.push(row);
        } else {
            hidden += 1;
        }
        if row.directory && collapsed.contains(&row.path) {
            skip_below = Some(row.depth);
        }
    }
    (visible, hidden)
}

fn changes_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut column =
        widget::column::with_capacity(source.groups().len() + 2).spacing(spacing.space_xxxs);
    if source.groups().iter().all(ChangeGroup::is_empty) {
        column = column.push(widget::text::caption("No changes in this worktree."));
    }
    for group in source.groups() {
        if group.is_empty() {
            continue;
        }
        column = column.push(change_group_view(state, source, group, modal_open));
    }
    column = column.push(widget::divider::horizontal::light());
    column = column.push(changes_footer(state, source, modal_open));
    column.into()
}

fn change_group_view<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    group: &'a ChangeGroup,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let chrome = &group.chrome;
    let mut header_children: Vec<Element<'a, SidebarAction>> = vec![
        widget::text::caption_heading(&*chrome.title)
            .width(Length::Fill)
            .into(),
    ];
    header_children.extend(
        group_commands(source, group, modal_open)
            .into_iter()
            .map(IconCommand::into_element),
    );
    let mut column = widget::column::with_capacity(group.visible().len() + 2).spacing(0);
    column = column.push(
        widget::row::with_children(header_children)
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxxs),
    );
    for row in group.visible() {
        column = column.push(change_row_view(state, source, group.area, row, modal_open));
    }
    let hidden = group.hidden();
    if hidden > 0 {
        column = column.push(widget::text::caption(format!(
            "{hidden} more paths not shown; group actions apply only to the rows above"
        )));
    }
    column.into()
}

fn change_row_view<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    area: ChangeArea,
    row: &'a ChangeRow,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let key = &source.key;
    let chrome = &row.chrome;
    let selected = matches!(
        state.panel_selection.as_ref(),
        Some(PanelSelection::Change { path, area: selected })
            if *path == row.entry.path && *selected == area
    );
    let identity = panel_row(
        &*chrome.accessible_name,
        0,
        widget::row::with_children(vec![
            widget::container(widget::text::caption(&*chrome.status))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            widget::text::caption(&*chrome.label)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(if chrome.ellipsize_start {
                    cosmic::iced::widget::text::Ellipsize::Start(EllipsizeHeightLimit::Lines(1))
                } else {
                    cosmic::iced::widget::text::Ellipsize::End(EllipsizeHeightLimit::Lines(1))
                })
                .width(Length::Fill)
                .into(),
            widget::text::caption(&*chrome.counts).into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs),
        SidebarAction::OpenChange {
            key: key.clone(),
            path: row.entry.path.clone(),
            area,
        },
        selected,
        modal_open,
    );
    let mut children: Vec<Element<'a, SidebarAction>> = vec![identity];
    children.extend(
        row_commands(source, area, row, modal_open)
            .into_iter()
            .map(IconCommand::into_element),
    );
    widget::row::with_children(children)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .into()
}

/// One icon command a change header or a change row offers, derived rather
/// than pushed inline.
///
/// The value exists for the same reason [`SectionHeader`] does: libcosmic
/// carries an icon button's tooltip in a wrapper that contributes no
/// accessibility node, so a built frame cannot be asked what an icon command
/// does and the decision is made somewhere a test can read it.
struct IconCommand<'a> {
    icon: &'static str,
    name: &'a str,
    /// `None` while a dialog is on screen; see [`gated`].
    action: Option<SidebarAction>,
    id: Id,
}

impl<'a> IconCommand<'a> {
    fn into_element(self) -> Element<'a, SidebarAction> {
        icon_action(self.icon, self.name, self.action, self.id)
    }
}

/// The commands one change group's header offers, in the order it draws them.
///
/// Which commands exist is a fact about the area: conflicts are staged to mark
/// them resolved, there is no unstage for an unmerged path, and untracked paths
/// have nothing to unstage from. An in-flight Git command withholds all three,
/// because a second mutation against the same index would race the first.
fn group_commands<'a>(
    source: &'a SourceControlModel,
    group: &'a ChangeGroup,
    modal_open: bool,
) -> Vec<IconCommand<'a>> {
    let key = &source.key;
    let chrome = &group.chrome;
    if source.busy() {
        return Vec::new();
    }
    let mut commands = Vec::with_capacity(2);
    if group.area == ChangeArea::Staged {
        commands.push(IconCommand {
            icon: "list-remove-symbolic",
            name: &chrome.unstage_all_name,
            action: gated(
                SidebarAction::Unstage {
                    key: key.clone(),
                    selection: group.visible_selection(),
                },
                modal_open,
            ),
            id: chrome.unstage_all_id.clone(),
        });
    } else {
        commands.push(IconCommand {
            icon: "list-add-symbolic",
            name: &chrome.stage_all_name,
            action: gated(
                SidebarAction::Stage {
                    key: key.clone(),
                    selection: group.visible_selection(),
                },
                modal_open,
            ),
            id: chrome.stage_all_id.clone(),
        });
    }
    if matches!(group.area, ChangeArea::Unstaged | ChangeArea::Untracked) {
        commands.push(IconCommand {
            icon: "edit-undo-symbolic",
            name: &chrome.discard_all_name,
            action: gated(
                SidebarAction::RequestDiscard {
                    key: key.clone(),
                    request: discard_request(key, group.area, group.visible_selection(), group),
                },
                modal_open,
            ),
            id: chrome.discard_all_id.clone(),
        });
    }
    commands
}

/// The same three commands for one row, against that row alone.
fn row_commands<'a>(
    source: &'a SourceControlModel,
    area: ChangeArea,
    row: &'a ChangeRow,
    modal_open: bool,
) -> Vec<IconCommand<'a>> {
    let key = &source.key;
    let chrome = &row.chrome;
    if source.busy() {
        return Vec::new();
    }
    let mut commands = Vec::with_capacity(2);
    if area == ChangeArea::Staged {
        commands.push(IconCommand {
            icon: "list-remove-symbolic",
            name: &chrome.accessible_name,
            action: gated(
                SidebarAction::Unstage {
                    key: key.clone(),
                    selection: vec![row.selection()],
                },
                modal_open,
            ),
            id: chrome.unstage_id.clone(),
        });
    } else {
        commands.push(IconCommand {
            icon: "list-add-symbolic",
            name: &chrome.accessible_name,
            action: gated(
                SidebarAction::Stage {
                    key: key.clone(),
                    selection: vec![row.selection()],
                },
                modal_open,
            ),
            id: chrome.stage_id.clone(),
        });
    }
    if matches!(area, ChangeArea::Unstaged | ChangeArea::Untracked) {
        commands.push(IconCommand {
            icon: "edit-undo-symbolic",
            name: &chrome.accessible_name,
            action: gated(
                SidebarAction::RequestDiscard {
                    key: key.clone(),
                    request: single_discard_request(key, area, row),
                },
                modal_open,
            ),
            id: chrome.discard_id.clone(),
        });
    }
    commands
}

/// An unconfirmed discard for a whole group's visible rows.
fn discard_request(
    key: &ActiveWorktreeKey,
    area: ChangeArea,
    selection: Vec<PathSelection>,
    group: &ChangeGroup,
) -> DiscardRequest {
    match area {
        // Untracked paths exist nowhere but the worktree, so discarding them
        // deletes files rather than restoring them from the index.
        ChangeArea::Untracked => DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: Vec::new(),
            untracked: group.visible_untracked(),
            confirmed: false,
        },
        _ => DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: selection,
            untracked: Vec::new(),
            confirmed: false,
        },
    }
}

fn single_discard_request(
    key: &ActiveWorktreeKey,
    area: ChangeArea,
    row: &ChangeRow,
) -> DiscardRequest {
    match area {
        ChangeArea::Untracked => DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: Vec::new(),
            untracked: vec![row.entry.path.clone()],
            confirmed: false,
        },
        _ => DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: vec![row.selection()],
            untracked: Vec::new(),
            confirmed: false,
        },
    }
}

fn changes_footer<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let key = &source.key;
    let chrome = &source.chrome;
    let message = state.commit_message();
    let input_key = key.clone();
    let mut commit_input = widget::text_input("Commit message", message)
        .id(chrome.commit_input_id.clone())
        .width(Length::Fill);
    if !modal_open {
        commit_input = commit_input.on_input(move |message| SidebarAction::CommitMessageChanged {
            key: input_key.clone(),
            message,
        });
    }

    let mut commit = button::suggested("Commit").id(chrome.commit_id.clone());
    if source.can_commit(message) && !modal_open {
        commit = commit.on_press(SidebarAction::Commit {
            key: key.clone(),
            message: message.to_owned(),
        });
    }

    let mut column = widget::column::with_capacity(4).spacing(spacing.space_xxxs);
    column = column.push(commit_input);
    let mut buttons = widget::row::with_capacity(2).spacing(spacing.space_xxs);
    buttons = buttons.push(commit);
    match (&source.branch, &source.upstream) {
        (Some(branch), Some(target)) => {
            let mut push = button::standard("Push").id(chrome.push_id.clone());
            if source.can_push() && !modal_open {
                push = push.on_press(SidebarAction::Push {
                    key: key.clone(),
                    branch: branch.clone(),
                    target: target.clone(),
                });
            }
            buttons = buttons.push(push);
        }
        (Some(branch), None) if !source.upstream_reported() => {
            let mut publish = button::standard("Publish Branch").id(chrome.push_id.clone());
            if source.can_publish() && !modal_open {
                publish = publish.on_press(SidebarAction::OpenPublish {
                    key: key.clone(),
                    branch: branch.clone(),
                    remotes: source.remotes.clone(),
                });
            }
            buttons = buttons.push(publish);
        }
        (Some(_), None) => {
            // `git status` reported an upstream this panel could not name, so
            // publishing would re-point the branch at whatever remote the
            // picker offered rather than push to the upstream it has.
            buttons = buttons.push(widget::text::caption(
                "Upstream could not be read — nothing to push",
            ));
        }
        (None, _) => {
            // A branch this panel cannot name is not one it may invent a
            // mutation for, and each reason is stated as itself.
            buttons = buttons.push(widget::text::caption(source.no_branch_reason()));
        }
    }
    column = column.push(buttons);
    if let Some(publish) = state.publish_draft().filter(|draft| &draft.key == key) {
        column = column.push(publish_picker(source, publish, modal_open));
    }
    column.into()
}

/// The remote picker shown for a branch with no upstream. The remote branch
/// text defaults to the local branch name and Git's own `check-ref-format`
/// remains the authority on whether it is legal.
///
/// `Publish` is refused out loud. It used to be refused in silence: a click on
/// the disabled button reported nothing, ran no Git and created no ref, and
/// with the remote unselected by default that was every first publish.
fn publish_picker<'a>(
    source: &'a SourceControlModel,
    publish: &'a PublishDraft,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let chrome = &source.chrome;
    let key = &publish.key;
    let mut remotes = widget::row::with_capacity(source.remotes.len()).spacing(spacing.space_xxxs);
    for (index, remote) in source.remotes.iter().enumerate() {
        let id = chrome
            .publish_remote_ids
            .get(index)
            .cloned()
            .unwrap_or_else(|| entity_id("panel.publish.remote", index));
        let name: &str = chrome
            .publish_remote_names
            .get(index)
            .map_or(remote.as_str(), |name| name);
        remotes = remotes.push(
            button::custom(widget::text::caption(remote.as_str()))
                .class(theme::Button::Text)
                .id(id)
                .name(name)
                .selected(publish.remote == *remote)
                .on_press_maybe(gated(
                    SidebarAction::PublishRemoteChanged {
                        key: key.clone(),
                        remote: remote.clone(),
                    },
                    modal_open,
                )),
        );
    }
    let branch_key = key.clone();
    let mut branch_input = widget::text_input("Remote branch", &publish.remote_branch)
        .id(chrome.publish_branch_id.clone())
        .width(Length::Fill);
    if !modal_open {
        branch_input =
            branch_input.on_input(move |remote_branch| SidebarAction::PublishBranchChanged {
                key: branch_key.clone(),
                remote_branch,
            });
    }
    let blocked = publish.blocked_reason(source.busy());
    let mut confirm = button::suggested("Publish").id(chrome.publish_id.clone());
    if let Some(push) = publish
        .push_action()
        .filter(|_| blocked.is_none() && !modal_open)
    {
        confirm = confirm.on_press(push);
    }
    widget::column::with_children(vec![
        widget::text::caption("Publish to").into(),
        remotes.into(),
        branch_input.into(),
        // A permanent slot: the reason comes and goes as the picker is filled
        // in, and the named controls below it must not move when it does.
        slot(blocked.map(|reason| {
            widget::text::caption(reason)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into()
        })),
        widget::row::with_children(vec![
            button::standard("Cancel")
                .id(chrome.publish_cancel_id.clone())
                .on_press_maybe(gated(SidebarAction::CancelPublish, modal_open))
                .into(),
            confirm.into(),
        ])
        .spacing(spacing.space_xxs)
        .into(),
    ])
    .spacing(spacing.space_xxxs)
    .into()
}

/// This branch's own commits.
///
/// The list is a walk from `HEAD` — the application loads it with
/// [`crate::repository::HistoryRevision::Head`] — so it is the branch's
/// history. The rows are text: this application has no surface that renders a
/// commit's diff, so a row that took a press would have nowhere to send it.
fn history_section<'a>(
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let key = &source.key;
    // The notice, the row list, and the pager. The notice and the pager are
    // slots because both come and go — the notice the moment the first page
    // lands, the pager when the walk runs out — and the pager is a named
    // button.
    let mut column = widget::column::with_capacity(3).spacing(spacing.space_xxxs);
    column = column.push(slot(history_notice(source).map(|notice| {
        widget::text::caption(notice)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into()
    })));
    let mut rows = widget::column::with_capacity(source.history.len()).spacing(0);
    for row in &source.history {
        let chrome = &row.chrome;
        rows = rows.push(
            widget::container(
                widget::column::with_children(vec![
                    widget::text::caption(&*chrome.subject)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                    widget::text::caption(&*chrome.metadata)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                ])
                .spacing(0)
                .width(Length::Fill),
            )
            .padding([0, spacing.space_xxs])
            .width(Length::Fill),
        );
    }
    column = column.push(rows);
    column = column.push(slot(source.can_load_history().then(|| {
        // The first page is not "more": labelling it that way while the list is
        // empty hides that nothing has been asked for yet.
        button::standard(if source.history.is_empty() {
            format!("Load {HISTORY_PAGE_SIZE} commits")
        } else {
            format!("Load {HISTORY_PAGE_SIZE} more commits")
        })
        .id(source.chrome.more_history_id.clone())
        .on_press_maybe(gated(
            SidebarAction::LoadMoreHistory {
                key: key.clone(),
                page: source.next_history_page,
            },
            modal_open,
        ))
        .into()
    })));
    column.into()
}

/// What the `History` section says instead of, or above, its commit rows.
///
/// Returns `None` only when there are rows to show. "No commits yet." is
/// reserved for an unborn `HEAD`, because a branch whose commits nobody has
/// asked for yet is a different fact: reporting it as an empty repository is
/// how this section came to contradict the commits the repository card beside
/// it was listing at the same moment.
fn history_notice(source: &SourceControlModel) -> Option<&'static str> {
    if source.unborn {
        return Some("No commits yet.");
    }
    if !source.history_loaded {
        return Some("This branch’s commits have not been loaded yet.");
    }
    if source.history.is_empty() {
        return Some("HEAD names a commit, but the page of history that loaded held none.");
    }
    None
}

/// The project's pull requests, inside the panel.
///
/// The rows are the same rows the navigation list draws, under the same rich
/// rendering: this section presents them beside the worktree they belong to
/// rather than replacing them.
fn pull_requests_section<'a>(
    state: &'a SidebarState,
    project: Option<&'a SidebarProject>,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    // The panel's own copy of the project's pull requests: the navigation list
    // renders the same rows in the same frame under its own names.
    let surface = PullRequestSurface::Panel;
    let Some(project) = project else {
        return widget::text::caption("This worktree's project is no longer loaded.").into();
    };
    if project.pull_requests.is_empty() {
        return widget::text::caption("No open pull requests for this project.").into();
    }
    let mut column =
        widget::column::with_capacity(project.pull_requests.len()).spacing(spacing.space_xxxs);
    for pull_request in &project.pull_requests {
        column = column.push(pull_request_view(
            project.id,
            pull_request,
            surface,
            state.selection,
            modal_open,
        ));
    }
    column.into()
}

/// One selectable panel row, indented by tree depth.
///
/// A row carries no [`Id`], and none of the lists that build rows may give it
/// one. A list's length and order are whatever the repository holds and
/// whatever the filter admits, so a row cannot be pinned to an index, and
/// `Tree::diff_children_custom` reconciles a named child by *name* while
/// leaving `children` in its old order: the row that took a departed sibling's
/// index is then handed that sibling's tree, the tags agree because every row
/// is the same button, and the state that frame's `take_all_named` already
/// lifted out is never put back. The next draw downcasts a `State::None` and
/// aborts the process — which is precisely what one keystroke into
/// `Filter files` used to do.
///
/// Nameless, the row is matched positionally instead, so it inherits a live
/// button state from whichever row held its index. The accessible name it
/// still carries is what labels it; the id only ever bought a stable AccessKit
/// node number, and nothing addresses a row by id.
fn panel_row<'a>(
    name: impl Into<Cow<'a, str>>,
    depth: usize,
    content: impl Into<Element<'a, SidebarAction>>,
    action: SidebarAction,
    selected: bool,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let indent = u16::try_from(depth).unwrap_or(u16::MAX).saturating_mul(12);
    button::custom(
        widget::row::with_children(vec![
            cosmic::iced::widget::Space::new()
                .width(Length::Fixed(f32::from(indent)))
                .into(),
            content.into(),
        ])
        .align_y(Alignment::Center)
        .spacing(0),
    )
    .class(theme::Button::MenuItem)
    .name(name)
    .selected(selected)
    .padding([0, spacing.space_xxs])
    .height(Length::Fixed(PANEL_ROW_HEIGHT))
    .width(Length::Fill)
    .on_press_maybe(gated(action, modal_open))
    .into()
}

/// The confirmation the application mounts before any discard runs.
///
/// It lives here so the copy, the counted paths, and the confirmed request all
/// come from the surface that raised the intent; `app.rs` only decides where
/// to put it.
pub fn discard_confirmation_dialog(pending: &DiscardConfirmation) -> Element<'_, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let tracked = pending.tracked_count();
    let untracked = pending.untracked_count();
    let mut detail = Vec::with_capacity(2);
    if tracked > 0 {
        detail.push(format!(
            "{tracked} tracked {} will be restored from the index",
            if tracked == 1 { "path" } else { "paths" }
        ));
    }
    if untracked > 0 {
        detail.push(format!(
            "{untracked} untracked {} will be deleted from the worktree",
            if untracked == 1 { "file" } else { "files" }
        ));
    }
    widget::container(
        widget::column::with_children(vec![
            widget::text::title3("Discard changes?").into(),
            widget::text::body(detail.join(". "))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            widget::text::caption("Discarded work is not recoverable from Git.").into(),
            widget::row::with_children(vec![
                button::standard("Cancel")
                    .id(discard_cancel_id())
                    .on_press(SidebarAction::CancelDiscard)
                    .into(),
                button::destructive("Discard")
                    .id(discard_confirm_id())
                    .on_press(pending.confirm_action())
                    .into(),
            ])
            .spacing(spacing.space_xxs)
            .into(),
        ])
        .spacing(spacing.space_xs),
    )
    .class(theme::Container::Dialog(false))
    .padding(spacing.space_m)
    .max_width(520)
    .into()
}

/// The discard confirmation's two answers. Named here because the dialog is
/// mounted by the application rather than by [`view`].
pub fn discard_cancel_id() -> Id {
    stable_id("discard.cancel")
}

pub fn discard_confirm_id() -> Id {
    stable_id("discard.confirm")
}

/// One position in a row or column that may be empty.
///
/// A child that comes and goes shifts every sibling behind it, and iced
/// reconciles a child carrying a custom widget name at the index that name held
/// last frame while leaving the tree at its new index alone — so a named
/// control that moved downcasts a foreign sibling's state and panics. Mounting
/// the optional child as a permanent, sometimes-empty slot pins every index
/// behind it. An empty column has no size of its own, so an unused slot costs
/// only the enclosing container's spacing.
fn slot<'a>(
    children: impl IntoIterator<Item = Element<'a, SidebarAction>>,
) -> Element<'a, SidebarAction> {
    widget::column::with_children(children)
        .spacing(cosmic::theme::spacing().space_xxxs)
        .into()
}

/// `action`, or nothing at all while a dialog is on screen.
///
/// The dialog is an overlay, so the reasons [`view`] gives apply to every
/// command this pane draws, not only to the resize handle: the pane keeps the
/// focus it held when the dialog opened, a keystroke the overlay does not
/// consume still reaches the base tree, and libcosmic's modal popover captures
/// pointer and touch but leaves the accessibility action path alone - a
/// `Click` request addressed to a button behind the dialog reaches it
/// untouched. The panel's commands stage, unstage, discard, commit and push, so
/// the one that must not fire from behind a dialog is exactly the one that
/// would be hardest to undo.
///
/// Withholding the message is the whole of standing a command down. A libcosmic
/// button with no `on_press` publishes nothing for a bare `Enter`, nothing for
/// an accessibility `Click`, reports itself disabled to assistive technologies,
/// and paints disabled - and it keeps its place in the layout and in the
/// accessibility tree, so no index and no name moves while the dialog is up.
fn gated(action: SidebarAction, modal_open: bool) -> Option<SidebarAction> {
    (!modal_open).then_some(action)
}

/// One icon command.
///
/// The value exists because libcosmic carries an icon button's tooltip in the
/// `iced_widget::tooltip` wrapper, and that wrapper contributes no
/// accessibility node: a built frame cannot be asked what an icon command
/// does, so the decision is made somewhere a test can read it.
fn icon_action<'a>(
    icon_name: &'static str,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    button::icon(icon::from_name(icon_name).size(ROW_ICON_SIZE))
        .id(id)
        .name(label.clone())
        .tooltip(label)
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .on_press_maybe(action)
        .into()
}

/// The rule between the pane and the workspace, doubling as the pane's resize
/// handle.
///
/// A widget of its own rather than `widget::mouse_area`, because `mouse_area`
/// reports pointer motion only while the pointer is inside its own bounds: a
/// handle this narrow would lose the gesture on the first pixel of travel. A
/// widget sees every event, so the grab survives the pointer leaving it.
fn resize_handle(width: f32, modal_open: bool) -> Splitter<'static, SidebarAction> {
    Splitter::new(
        resize_handle_id(),
        RESIZE_HANDLE_NAME,
        width,
        EXPANDED_WIDTH_MIN,
        EXPANDED_WIDTH_MAX,
        SplitterEdge::Trailing,
        modal_open,
        |width| SidebarAction::Resize { width },
        || SidebarAction::ResizeFinished,
    )
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
    use crate::{
        github::RemoteValue,
        repository::{
            ChangeArea, ChangeKind, ChangedPath, HeadState, LocalRefRecord, RemoteHead,
            RemoteRecord, RemoteRefRecord, RepoPath, WorkingFiles, WorktreeLock,
        },
    };
    use cosmic::iced::{
        Size,
        advanced::{Layout, layout, widget::Tree},
    };
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
                // `dirty` counts what `entries` lists, so a fixture that
                // claimed a modified file without naming one would describe a
                // snapshot the parser cannot produce.
                entries: if dirty {
                    vec![ChangedPath {
                        path: RepoPath::parse("src/lib.rs").unwrap(),
                        old_path: None,
                        area: ChangeArea::Unstaged,
                        kind: ChangeKind::Modified,
                        conflict: None,
                        submodule: false,
                        added_lines: None,
                        removed_lines: None,
                    }]
                } else {
                    Vec::new()
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
                        source_control: None,
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
            search(&state, false)
                .as_widget()
                .id()
                .map(|id| id.to_string()),
            Some(search_focus_id().to_string())
        );

        // A query adds the clear action, which must not cost the input the id
        // the application focuses.
        state.apply(&SidebarAction::SearchChanged("cagents".into()));
        assert_eq!(
            search(&state, false)
                .as_widget()
                .id()
                .map(|id| id.to_string()),
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
    fn new_worktree_requires_a_selected_project() {
        let empty = SidebarModel::ready(SidebarMode::Expanded, Vec::new());
        let state = SidebarState::default();
        let empty_ids = control_ids(&view(
            &empty,
            &state,
            SidebarMode::Expanded,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ));
        assert!(!empty_ids.contains(&stable_id("new-worktree").to_string()));

        let populated = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut selected = SidebarState::default();
        selected.apply(&SidebarAction::OpenRepository {
            project_id: project_id(1),
            repository: "/repo/cagents".into(),
        });
        let populated_ids = control_ids(&view(
            &populated,
            &selected,
            SidebarMode::Expanded,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ));
        assert!(
            populated_ids.contains(
                &populated.projects[0]
                    .chrome
                    .header_new_worktree_id
                    .to_string()
            )
        );
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
        let bounds = Size::new(crate::splitter::WIDTH, 480.0);
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
        assert_eq!(
            node.numeric_value_step(),
            Some(f64::from(crate::splitter::KEYBOARD_STEP))
        );
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
            button_labels(&error("disk on fire", false, false), 4)
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
            let empty = empty(&state, false, false);
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

    fn repo_path(value: &str) -> RepoPath {
        RepoPath::parse(value).expect("valid repository path")
    }

    fn ref_name(value: &str) -> ValidatedRefName {
        ValidatedRefName::parse(value).expect("valid ref name")
    }

    fn oid_of(seed: u8) -> Oid {
        Oid::parse(format!("{seed:02x}").repeat(20)).expect("valid object id")
    }

    fn main_key() -> ActiveWorktreeKey {
        ActiveWorktreeKey {
            project_id: project_id(1),
            worktree_id: WorktreeId::from_uuid(Uuid::from_u128(2)),
            canonical_path: PathBuf::from("/repo/main"),
        }
    }

    fn fix_key() -> ActiveWorktreeKey {
        ActiveWorktreeKey {
            project_id: project_id(1),
            worktree_id: WorktreeId::from_uuid(Uuid::from_u128(3)),
            canonical_path: PathBuf::from("/repo/fix-sidebar"),
        }
    }

    fn changed(
        path: RepoPath,
        area: ChangeArea,
        kind: ChangeKind,
        counts: Option<(u64, u64)>,
    ) -> ChangedPath {
        ChangedPath {
            path,
            old_path: None,
            area,
            kind,
            conflict: None,
            submodule: false,
            added_lines: counts.map(|(added, _)| added),
            removed_lines: counts.map(|(_, removed)| removed),
        }
    }

    fn panel_remote(name: &str, url: &str) -> RemoteRecord {
        RemoteRecord {
            name: name.into(),
            fetch_url: Some(url.into()),
            push_url: None,
        }
    }

    fn panel_remote_ref(name: &str) -> RemoteRefRecord {
        let reference = ref_name(name);
        let rest = name.strip_prefix("refs/remotes/").unwrap_or(name);
        let (remote, branch) = rest.split_once('/').unwrap_or((rest, ""));
        RemoteRefRecord {
            name: reference,
            oid: oid_of(0xaa),
            remote: remote.into(),
            branch: branch.into(),
        }
    }

    /// A worktree on `main`, ahead of `origin/main`, with one entry in each of
    /// the four change areas and one unmeasured (binary) count.
    fn snapshot() -> RepositorySnapshot {
        RepositorySnapshot {
            status: StatusSnapshot {
                head: HeadState::Branch {
                    name: "main".into(),
                    oid: oid_of(0x11).to_string(),
                },
                tracking: TrackingState::Compared {
                    upstream: "refs/remotes/origin/main".into(),
                    ahead: 2,
                    behind: 0,
                },
                dirty: DirtyState {
                    staged: 1,
                    unstaged: 1,
                    untracked: 1,
                    conflicted: 1,
                    submodule: 0,
                },
                entries: vec![
                    changed(
                        repo_path("src/app.rs"),
                        ChangeArea::Staged,
                        ChangeKind::Modified,
                        Some((12, 3)),
                    ),
                    changed(
                        repo_path("assets/logo.png"),
                        ChangeArea::Unstaged,
                        ChangeKind::Modified,
                        None,
                    ),
                    changed(
                        repo_path("notes.txt"),
                        ChangeArea::Untracked,
                        ChangeKind::Untracked,
                        None,
                    ),
                    ChangedPath {
                        conflict: Some(ConflictKind::BothModified),
                        ..changed(
                            repo_path("src/sidebar.rs"),
                            ChangeArea::Conflict,
                            ChangeKind::Unmerged,
                            None,
                        )
                    },
                ],
            },
            files: WorkingFiles {
                paths: vec![
                    repo_path("README.md"),
                    repo_path("src/app.rs"),
                    repo_path("src/sidebar.rs"),
                ],
                tracked_count: 3,
            },
            remotes: vec![panel_remote(
                "origin",
                "https://github.com/example/cagents.git",
            )],
            local_refs: vec![LocalRefRecord {
                name: ref_name("refs/heads/main"),
                oid: oid_of(0x11),
                upstream: Some(ref_name("refs/remotes/origin/main")),
                head: true,
            }],
            remote_refs: vec![panel_remote_ref("refs/remotes/origin/main")],
            remote_heads: vec![RemoteHead {
                remote: "origin".into(),
                target: ref_name("refs/remotes/origin/main"),
            }],
        }
    }

    fn source_control() -> SourceControlModel {
        SourceControlModel::ready(main_key(), &snapshot())
    }

    fn panel_worktree(key: &ActiveWorktreeKey, branch: &str) -> SidebarWorktree {
        SidebarWorktree::from_repository(
            &WorktreeRecord {
                id: key.worktree_id,
                path: key.canonical_path.clone(),
                head_oid: None,
                branch: Some(format!("refs/heads/{branch}")),
                detached: false,
                bare: false,
                lock: WorktreeLock::default(),
                prunable_reason: None,
            },
            None,
        )
    }

    /// A project holding both fixture worktrees, so the panel has somewhere to
    /// bind and somewhere to rebind to.
    fn panel_model() -> SidebarModel {
        SidebarModel::ready(
            SidebarMode::Expanded,
            vec![SidebarProject::new(
                project_id(1),
                RepositoryId(Uuid::from_u128(9)),
                "cagents".into(),
                "/repo/cagents".into(),
                vec![
                    panel_worktree(&main_key(), "main"),
                    panel_worktree(&fix_key(), "fix-sidebar"),
                ],
                Vec::new(),
            )],
        )
        .with_source_control(Some(source_control()))
    }

    fn focus_on(key: &ActiveWorktreeKey) -> SidebarFocus {
        SidebarFocus::Repository {
            worktree: key.canonical_path.clone(),
        }
    }

    /// A pane bound to `main` with every panel section open.
    fn panel_state(model: &SidebarModel) -> SidebarState {
        let mut state = SidebarState::default();
        assert!(state.sync_active_worktree(model, &focus_on(&main_key())));
        for section in PanelSection::ALL {
            if !state.section_expanded(section) {
                state.apply(&SidebarAction::ToggleSection {
                    key: main_key(),
                    section,
                });
            }
        }
        state
    }

    /// A headless `cosmic::Renderer`. `Widget::update` and `Widget::operate`
    /// take one by reference; with no graphics device it falls back to
    /// `tiny-skia`, which needs neither a device nor a surface.
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

    /// Everything a frame publishes when each of its focusable controls is
    /// focused in turn and answered with a bare `Enter`.
    ///
    /// This is the path a dialog leaves open. The overlay captures pointer and
    /// touch, but the pane keeps whatever focus it held when the dialog opened
    /// and libcosmic's modal popover declines to deliver a focus operation to
    /// the content beneath it, so a focused control keeps answering the
    /// keyboard. Focusing by id is also the only way to reach this module's
    /// icon commands at all: each carries a libcosmic tooltip, and a tooltip
    /// contributes no accessibility node.
    fn frame_enter_presses(
        renderer: &cosmic::Renderer,
        mut element: Element<'_, SidebarAction>,
    ) -> Vec<SidebarAction> {
        use cosmic::iced::advanced::{Shell, clipboard, mouse, widget::operation};
        use cosmic::iced::{Event, Rectangle, keyboard};

        fn identities(tree: &Tree, into: &mut Vec<Id>) {
            into.extend(tree.id.clone());
            for child in &tree.children {
                identities(child, into);
            }
        }

        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(1280.0, 4_000.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let enter = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut identifiers = Vec::new();
        identities(&tree, &mut identifiers);
        let mut fired = Vec::new();
        for id in identifiers {
            let mut focus = operation::focusable::focus::<()>(id);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut focus);
            let mut published = Vec::new();
            let mut shell = Shell::new(&mut published);
            element.as_widget_mut().update(
                &mut tree,
                &enter,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            drop(shell);
            fired.extend(published);
        }
        fired
    }

    #[test]
    fn the_panel_mounts_for_the_active_worktree_and_rebinds_rather_than_leaking_the_previous_one() {
        let model = panel_model();
        let mut state = SidebarState::default();

        assert!(
            source_control_surface(&model, &state, false, false).is_none(),
            "with nothing focused the panel must not mount: there is no worktree it could be \
             reading"
        );

        assert!(
            state.sync_active_worktree(&model, &focus_on(&main_key())),
            "focusing a known worktree must bind the panel to it"
        );
        assert_eq!(state.active_worktree(), Some(&main_key()));
        assert!(
            source_control_surface(&model, &state, false, false).is_some(),
            "the panel mounts once the bound worktree is the one the model holds"
        );

        // Focus moves to the sibling worktree. The model still holds `main`'s
        // rows, and those rows may not be shown under `fix-sidebar`'s name: a
        // press on one would stage a path in the wrong tree.
        assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
        assert_eq!(state.active_worktree(), Some(&fix_key()));
        assert!(
            source_control_surface(&model, &state, false, false).is_none(),
            "a rebind must withhold the panel until the new worktree's own snapshot lands"
        );

        // A card that names no worktree - a browser, a pull-request list -
        // leaves the binding where it is rather than tearing the panel down.
        assert!(
            !state.sync_active_worktree(&model, &SidebarFocus::None),
            "focus that names no directory is not a rebind"
        );
        assert_eq!(state.active_worktree(), Some(&fix_key()));

        // A worktree the project list no longer holds cannot stay bound.
        let emptied = SidebarModel::ready(SidebarMode::Expanded, Vec::new());
        assert!(emptied.source_control.is_none());
        assert!(state.sync_active_worktree(&emptied, &SidebarFocus::None));
        assert_eq!(state.active_worktree(), None);
        assert!(source_control_surface(&emptied, &state, false, false).is_none());
    }

    #[test]
    fn rebinding_drops_every_draft_typed_for_the_worktree_being_left() {
        let model = panel_model();
        let mut state = SidebarState::default();
        assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));

        let key = main_key();
        state.apply(&SidebarAction::FilesQueryChanged {
            key: key.clone(),
            query: "sidebar".into(),
        });
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "wip".into(),
        });
        state.apply(&SidebarAction::ToggleDirectory {
            key: key.clone(),
            path: repo_path("src"),
        });
        state.apply(&SidebarAction::RequestDiscard {
            key: key.clone(),
            request: DiscardRequest {
                worktree: key.canonical_path.clone(),
                tracked: vec![PathSelection::from_changed_path(&changed(
                    repo_path("src/app.rs"),
                    ChangeArea::Unstaged,
                    ChangeKind::Modified,
                    None,
                ))],
                untracked: Vec::new(),
                confirmed: false,
            },
        });
        assert_eq!(state.files_query(), "sidebar");
        assert_eq!(state.commit_message(), "wip");
        assert!(state.pending_discard().is_some());

        assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
        assert_eq!(
            state.files_query(),
            "",
            "a filter typed against one worktree's file tree cannot survive onto another's"
        );
        assert_eq!(
            state.commit_message(),
            "",
            "a commit message is written for the tree it will be committed to"
        );
        assert!(state.collapsed_directories.is_empty());
        assert!(
            state.pending_discard().is_none(),
            "a confirmation counted against the worktree that was left must not be answerable"
        );
    }

    #[test]
    fn every_panel_section_expands_and_collapses() {
        let key = main_key();
        let mut state = SidebarState::default();

        // `Changes` is what a source-control surface exists for, so it is the
        // one section that starts open.
        assert!(state.section_expanded(PanelSection::Changes));
        for section in [
            PanelSection::Files,
            PanelSection::History,
            PanelSection::PullRequests,
        ] {
            assert!(
                !state.section_expanded(section),
                "{section:?} starts closed"
            );
        }

        for section in PanelSection::ALL {
            let before = state.section_expanded(section);
            state.apply(&SidebarAction::ToggleSection {
                key: key.clone(),
                section,
            });
            assert_eq!(
                state.section_expanded(section),
                !before,
                "{section:?} must answer its own toggle"
            );
            for other in PanelSection::ALL
                .into_iter()
                .filter(|other| *other != section)
            {
                assert_eq!(
                    state.section_expanded(other),
                    other.expanded_by_default(),
                    "toggling {section:?} moved {other:?}"
                );
            }
            state.apply(&SidebarAction::ToggleSection {
                key: key.clone(),
                section,
            });
            assert_eq!(
                state.section_expanded(section),
                before,
                "{section:?} must return to where it was"
            );
        }
    }

    /// A path Git holds that Rust cannot decode.
    ///
    /// Every mutation this panel raises names its paths by these bytes, so a
    /// lossy label is the only thing a decode may ever produce. A `String`
    /// anywhere on that route would either refuse the row or stage a different
    /// file.
    fn non_utf8_path() -> RepoPath {
        RepoPath::from_bytes(b"src/caf\xff.rs").expect("a repository path Git can hold")
    }

    #[test]
    fn a_change_row_stages_unstages_and_discards_exactly_its_own_path() {
        let mut snapshot = snapshot();
        snapshot.status.entries = vec![
            changed(
                repo_path("src/app.rs"),
                ChangeArea::Staged,
                ChangeKind::Modified,
                Some((12, 3)),
            ),
            changed(
                non_utf8_path(),
                ChangeArea::Unstaged,
                ChangeKind::Modified,
                None,
            ),
            changed(
                repo_path("notes.txt"),
                ChangeArea::Untracked,
                ChangeKind::Untracked,
                None,
            ),
        ];
        let source = SourceControlModel::ready(main_key(), &snapshot);
        let key = main_key();

        let staged = source
            .group(ChangeArea::Staged)
            .expect("a staged group exists");
        let row = &staged.rows()[0];
        assert_eq!(
            row_commands(&source, ChangeArea::Staged, row, false)
                .into_iter()
                .map(|command| command.action)
                .collect::<Vec<_>>(),
            vec![Some(SidebarAction::Unstage {
                key: key.clone(),
                selection: vec![row.selection()],
            })],
            "a staged row offers unstage, and never a discard: there is nothing in the worktree \
             to throw away"
        );

        // The unstaged row's path cannot be decoded, and every command it
        // raises must still name it byte for byte.
        let unstaged = source
            .group(ChangeArea::Unstaged)
            .expect("an unstaged group exists");
        let row = &unstaged.rows()[0];
        assert_eq!(row.entry.path, non_utf8_path());
        let actions = row_commands(&source, ChangeArea::Unstaged, row, false)
            .into_iter()
            .map(|command| command.action)
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                Some(SidebarAction::Stage {
                    key: key.clone(),
                    selection: vec![row.selection()],
                }),
                Some(SidebarAction::RequestDiscard {
                    key: key.clone(),
                    request: DiscardRequest {
                        worktree: key.canonical_path.clone(),
                        tracked: vec![row.selection()],
                        untracked: Vec::new(),
                        confirmed: false,
                    },
                }),
            ]
        );

        // An untracked path exists nowhere but the worktree, so discarding it
        // deletes a file rather than restoring one from the index.
        let untracked = source
            .group(ChangeArea::Untracked)
            .expect("an untracked group exists");
        let row = &untracked.rows()[0];
        assert_eq!(
            row_commands(&source, ChangeArea::Untracked, row, false)
                .into_iter()
                .filter_map(|command| command.action)
                .find(|action| matches!(action, SidebarAction::RequestDiscard { .. })),
            Some(SidebarAction::RequestDiscard {
                key: key.clone(),
                request: DiscardRequest {
                    worktree: key.canonical_path.clone(),
                    tracked: Vec::new(),
                    untracked: vec![repo_path("notes.txt")],
                    confirmed: false,
                },
            })
        );

        // The group command acts on exactly the rows the group drew.
        assert_eq!(
            group_commands(&source, unstaged, false)
                .into_iter()
                .next()
                .and_then(|command| command.action),
            Some(SidebarAction::Stage {
                key,
                selection: unstaged.visible_selection(),
            })
        );
    }

    #[test]
    fn a_conflict_is_staged_to_resolve_it_and_never_unstaged_or_discarded() {
        let source = source_control();
        let group = source
            .group(ChangeArea::Conflict)
            .expect("the fixture holds an unmerged path");
        let icons = group_commands(&source, group, false)
            .into_iter()
            .map(|command| command.icon)
            .collect::<Vec<_>>();
        assert_eq!(
            icons,
            vec!["list-add-symbolic"],
            "an unmerged path is staged to mark it resolved; there is no index side to unstage \
             from and no clean copy to restore"
        );
    }

    #[test]
    fn a_running_mutation_withholds_every_further_command_against_the_same_index() {
        let mut source = source_control();
        source.set_mutation(Some(MutationKind::Stage));
        let group = source
            .group(ChangeArea::Unstaged)
            .expect("an unstaged group exists");
        assert!(
            group_commands(&source, group, false).is_empty()
                && row_commands(&source, ChangeArea::Unstaged, &group.rows()[0], false).is_empty(),
            "two `git add` runs against one index corrupt each other's lock, so the second is not \
             offered at all"
        );
    }

    #[test]
    fn a_discard_is_raised_unconfirmed_and_only_the_dialog_can_confirm_it() {
        let source = source_control();
        let key = main_key();
        let group = source
            .group(ChangeArea::Unstaged)
            .expect("an unstaged group exists");
        let request = group_commands(&source, group, false)
            .into_iter()
            .find_map(|command| match command.action {
                Some(SidebarAction::RequestDiscard { request, .. }) => Some(request),
                _ => None,
            })
            .expect("the unstaged group offers a discard");
        assert!(
            !request.confirmed,
            "the surface may never build a confirmed discard: the service refuses an unconfirmed \
             one, and that refusal is the guard"
        );

        let mut state = SidebarState::default();
        state.apply(&SidebarAction::RequestDiscard {
            key: key.clone(),
            request: request.clone(),
        });
        let pending = state.pending_discard().expect("the dialog is now mounted");
        let SidebarAction::ConfirmDiscard {
            key: confirmed_key,
            request: confirmed,
        } = pending.confirm_action()
        else {
            panic!("the dialog's answer is the confirmed discard");
        };
        assert_eq!(confirmed_key, key);
        assert!(confirmed.confirmed);
        assert_eq!(
            (&confirmed.tracked, &confirmed.untracked),
            (&request.tracked, &request.untracked),
            "the confirmed request must name exactly the paths the dialog counted"
        );

        state.apply(&SidebarAction::CancelDiscard);
        assert!(state.pending_discard().is_none());
    }

    #[test]
    fn no_panel_command_is_pressable_while_a_dialog_is_open() {
        let renderer = headless_renderer();
        let model = panel_model();
        let state = panel_state(&model);

        // Every section open, so every command the panel has is in the frame.
        let live = frame_enter_presses(
            &renderer,
            view(&model, &state, SidebarMode::Expanded, 320.0, false),
        );
        assert!(
            live.iter()
                .any(|action| matches!(action, SidebarAction::Stage { .. })),
            "the fixture must actually draw the commands this test is about; it published {live:?}"
        );

        let gated = frame_enter_presses(
            &renderer,
            view(&model, &state, SidebarMode::Expanded, 320.0, true),
        );
        assert!(
            gated.is_empty(),
            "a dialog is an overlay: the pane keeps the focus it held when the dialog opened and \
             answers Enter behind it, and the application refuses every pane action while a modal \
             is up - so a control that still published one would report itself enabled, take the \
             press, and do nothing. It published {gated:?}"
        );

        // The compact rail renders the same panel inside a popover, and its
        // commands stand down on the same terms.
        let mut compact = panel_state(&model);
        compact.apply(&SidebarAction::ToggleSourceControlPanel { key: main_key() });
        assert!(
            frame_enter_presses(
                &renderer,
                view(&model, &compact, SidebarMode::Compact, 320.0, true),
            )
            .is_empty(),
            "the rail's popover is the same panel and must stand down with it"
        );
    }

    #[test]
    fn the_discard_confirmations_own_answers_survive_the_modal_it_is_itself() {
        let renderer = headless_renderer();
        let key = main_key();
        let pending = DiscardConfirmation {
            key: key.clone(),
            request: DiscardRequest {
                worktree: key.canonical_path.clone(),
                tracked: Vec::new(),
                untracked: vec![repo_path("notes.txt")],
                confirmed: false,
            },
        };
        let fired = frame_enter_presses(&renderer, discard_confirmation_dialog(&pending));
        assert!(
            fired.contains(&SidebarAction::CancelDiscard),
            "refusing the dialog's own Cancel while the dialog is up would strand it with no way \
             out; it published {fired:?}"
        );
        assert!(
            fired.contains(&pending.confirm_action()),
            "the dialog's Discard is the only route to a confirmed request; it published {fired:?}"
        );
    }

    #[test]
    fn the_panel_and_the_navigation_list_never_draw_one_pull_request_under_one_name() {
        let mut model = panel_model();
        let project = &mut model.projects[0];
        project.pull_requests = vec![SidebarPullRequest::new(
            project_id(1),
            42,
            "Polish sidebar".into(),
            Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
            "fix-sidebar".into(),
            Some("octocat".into()),
            false,
            CheckStatus::Passing,
        )];
        let mut state = panel_state(&model);
        state.apply(&SidebarAction::TogglePullRequests(project_id(1)));

        // Two nodes under one custom name means the second visited overwrites
        // the first in iced's per-frame state map, and the loser's next
        // downcast aborts the process.
        // Only named identities share the runtime's per-frame map. An unnamed
        // widget gets a fresh unique id and a composed one gets an anonymous
        // set; `Display` renders those as "Undefined" and "Set", and neither
        // can collide.
        let ids = control_ids(&view(&model, &state, SidebarMode::Expanded, 320.0, false))
            .into_iter()
            .filter(|id| id != "Undefined" && id != "Set")
            .collect::<Vec<_>>();
        assert!(
            ids.iter().any(|id| id.contains("pr.42.open.panel"))
                && ids.iter().any(|id| id.contains("pr.42.open.nav")),
            "the fixture must draw the same pull request in both surfaces: {ids:?}"
        );
        let mut seen = BTreeSet::new();
        for id in &ids {
            assert!(
                seen.insert(id.clone()),
                "the panel and the navigation list drew a shared identity {id}: {ids:?}"
            );
        }
    }

    #[test]
    fn a_pull_request_row_carries_its_rich_rendering_into_the_panel() {
        let model = panel_model();
        let state = panel_state(&model);
        let pull_request = SidebarPullRequest::new(
            project_id(1),
            42,
            "Polish sidebar".into(),
            Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
            "fix-sidebar".into(),
            Some("octocat".into()),
            true,
            CheckStatus::Failing,
        );
        let mut project = model.projects[0].clone();
        project.pull_requests = vec![pull_request];
        let section = pull_requests_section(&state, Some(&project), false);
        let ids = control_ids(&section);
        assert!(
            ids.contains(
                &project.pull_requests[0]
                    .chrome
                    .open_id(PullRequestSurface::Panel)
                    .to_string()
            ),
            "the panel presents the project's own rows rather than replacing them: {ids:?}"
        );
    }

    #[test]
    fn a_history_page_is_asked_for_once_and_a_short_page_ends_the_walk() {
        let mut source = source_control();
        assert!(
            source.wants_history(),
            "a worktree whose HEAD names a commit has a walk worth offering"
        );
        assert!(!source.history_requested());
        source.mark_history_requested();
        assert!(
            source.history_requested(),
            "expanding History twice must not queue a second walk"
        );

        assert_eq!(source.history_count(), None, "nobody has asked yet");
        source.append_history(&[]);
        assert_eq!(
            source.history_count(),
            Some(0),
            "an empty page is an answer; \"nobody asked\" is not"
        );
        assert!(!source.can_load_history(), "a short page ends the walk");
    }
}
