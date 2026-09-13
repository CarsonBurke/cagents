//! Project/worktree navigation and the active worktree's Git panel.
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
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
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
    model::{CardId, ProjectId, ProjectState, ThreadId, WorktreeId},
    repository::{
        BranchComparison, ChangeArea, ChangeKind, ChangedPath, CommitIdentity, CommitSummary,
        ConflictKind, DirtyState, DiscardRequest, HISTORY_PAGE_SIZE, HeadState, Oid, PathSelection,
        PushTarget, RemoteRecord, RepoPath, RepositoryId, RepositoryInfo, RepositorySnapshot,
        StatusSnapshot, TrackingState, ValidatedBranchName, ValidatedRefName, WorktreeRecord,
        build_file_tree, build_file_tree_with_directories,
    },
    splitter::{Edge as SplitterEdge, Splitter},
};
use row_interaction::RowInteraction;

/// The rendered bounds of the expanded pane, and the only bounds it has:
/// `crate::settings` validates persisted widths against these, so a width can
/// no longer pass validation and then silently render clamped.
pub const EXPANDED_WIDTH_MIN: f32 = design::NAVIGATION_MIN_WIDTH;
pub const EXPANDED_WIDTH_MAX: f32 = design::NAVIGATION_MAX_WIDTH;
/// Rendered when a persisted width is not a finite number. Mirrors
/// `settings::AppSettings::default`, so a corrupt file looks like a fresh one.
const DEFAULT_EXPANDED_WIDTH: f32 = design::NAVIGATION_DEFAULT_WIDTH;
const COMPACT_WIDTH: f32 = design::NAVIGATION_RAIL_WIDTH;
const HEADER_HEIGHT: f32 = design::HEADER_HEIGHT - design::BORDER_WIDTH;
const PROJECT_HEADER_HEIGHT: f32 = design::ROW_HEIGHT;
const WORKTREE_ROW_HEIGHT: f32 = design::ROW_HEIGHT;
const ICON_CONTROL_SIZE: f32 = design::PRIMARY_CONTROL_SIZE;
const ROW_ICON_WIDTH: f32 = 20.0;
/// Icons that sit inside a row, beside its text.
const ROW_ICON_SIZE: u16 = 16;
/// Icons that stand alone as the illustration of a whole pane state.
const STATUS_ICON_SIZE: u16 = 20;

fn worktree_icon() -> widget::icon::Handle {
    static ICON: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(&br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="3" r="1.5"/><path d="M4 4.5v7M12 4.5v1a4 4 0 0 1-4 4H4"/></g></svg>"#[..]).symbolic(true)
    });
    ICON.clone()
}
const RESIZE_HANDLE_NAME: &str = "Resize sidebar";
/// Height of one source-control panel row. Denser than the navigation rows
/// above it because the panel lists paths rather than entities.
const PANEL_ROW_HEIGHT: f32 = design::DENSE_ROW_HEIGHT;
/// How many file-tree rows the `Files` section renders at once.
///
/// `git ls-files` on a large repository returns six-figure path counts, and
/// this list is built eagerly like the project navigation above it. The bound
/// is explicit - the section says how many rows it withheld and the quick
/// filter narrows to them - rather than a silent truncation.
pub(crate) const MAX_VISIBLE_FILE_ROWS: usize = 400;
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
    pub external_editors: Vec<SidebarEditor>,
    /// Enables standalone editor file actions; other hosts keep editor-only menus.
    pub file_management: bool,
    pub projects: Vec<SidebarProject>,
    /// Source-control data for the active worktree, and only for it. The panel
    /// shows exactly one worktree, so loading a snapshot per project would read
    /// Git for surfaces nobody is looking at.
    pub source_control: Option<SourceControlModel>,
}

impl SidebarModel {
    /// Apply timestamps from successful, persisted worktree creation journals.
    /// Never substitute a commit time or a discovery time for creation.
    pub fn apply_worktree_creation_times<P: AsRef<Path>>(
        &mut self,
        times: impl IntoIterator<Item = (P, i64)>,
    ) {
        for (path, created) in times {
            for project in &mut self.projects {
                if let Some(worktree) = project
                    .worktrees
                    .iter_mut()
                    .find(|row| row.path == path.as_ref())
                {
                    worktree.created_at_unix_ms = Some(
                        worktree
                            .created_at_unix_ms
                            .map_or(created, |known| known.max(created)),
                    );
                }
            }
        }
    }

    /// Known projects follow the durable order; newly discovered projects keep
    /// their relative order at the end until the next persisted reorder.
    pub fn apply_project_order(&mut self, order: &[ProjectId]) {
        self.projects.sort_by_key(|project| {
            order
                .iter()
                .position(|id| *id == project.id)
                .unwrap_or(usize::MAX)
        });
    }

    pub fn reorder_project(&mut self, project: ProjectId, before: Option<ProjectId>) -> bool {
        let Some(from) = self.projects.iter().position(|entry| entry.id == project) else {
            return false;
        };
        let target = match before {
            Some(id) => match self.projects.iter().position(|entry| entry.id == id) {
                Some(index) => index,
                None => return false,
            },
            None => self.projects.len(),
        };
        let to = target.saturating_sub(usize::from(from < target));
        if from == to {
            return false;
        }
        let project = self.projects.remove(from);
        self.projects.insert(to, project);
        true
    }

    pub fn ready(mode: SidebarMode, projects: Vec<SidebarProject>) -> Self {
        Self {
            mode,
            data_state: SidebarDataState::Ready,
            projects,
            source_control: None,
            external_editors: Vec::new(),
            file_management: false,
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
            external_editors: Vec::new(),
            file_management: false,
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
pub struct WorktreeOperationRow {
    pub id: crate::model::OperationId,
    pub label: String,
    pub cancellable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedDisplay {
    pub rows: Vec<SidebarPullRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarProject {
    pub id: ProjectId,
    pub repository_id: RepositoryId,
    pub name: String,
    pub repository: PathBuf,
    pub worktrees: Vec<SidebarWorktree>,
    pub threads: Vec<SidebarThread>,
    pub pull_requests: Vec<SidebarPullRequest>,
    pub github_status: String,
    pub github_ready: bool,
    pub github_stale: bool,
    pub cached_display: Option<CachedDisplay>,
    pub operations: Vec<WorktreeOperationRow>,
    pub avatar: Option<cosmic::iced::advanced::image::Handle>,
    chrome: ProjectChrome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarThread {
    pub id: ThreadId,
    pub title: String,
    pub selected: bool,
    pub activity: Option<crate::agent::ActivityState>,
    pub unread: bool,
    folded_title: Box<str>,
    open_id: Id,
    accessible_name: Box<str>,
}

impl SidebarThread {
    pub fn new(
        id: ThreadId,
        title: String,
        selected: bool,
        activity: Option<crate::agent::ActivityState>,
        unread: bool,
    ) -> Self {
        Self {
            id,
            selected,
            activity,
            unread,
            folded_title: fold(&title),
            open_id: entity_id("thread.open", id),
            accessible_name: format!(
                "Open thread {title}{}",
                if unread { ", unread agent update" } else { "" }
            )
            .into_boxed_str(),
            title,
        }
    }
}

impl SidebarProject {
    pub fn new(
        id: ProjectId,
        repository_id: RepositoryId,
        name: String,
        repository: PathBuf,
        mut worktrees: Vec<SidebarWorktree>,
        pull_requests: Vec<SidebarPullRequest>,
    ) -> Self {
        let chrome = ProjectChrome::new(id, &name, &repository);
        for worktree in &mut worktrees {
            if let Some(pull_request) = matching_pull_request(worktree, &pull_requests) {
                worktree.set_pull_request(pull_request.clone());
            } else {
                worktree.clear_pull_request();
            }
        }
        Self {
            id,
            repository_id,
            name,
            repository,
            worktrees,
            threads: Vec::new(),
            pull_requests,
            github_status: "GitHub has not been refreshed.".into(),
            github_ready: false,
            github_stale: false,
            cached_display: None,
            operations: Vec::new(),
            avatar: None,
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

    pub fn display_pull_requests(&self) -> &[SidebarPullRequest] {
        self.cached_display
            .as_ref()
            .map_or(&self.pull_requests, |cached| &cached.rows)
    }

    pub fn revoke_github(&mut self) {
        if !self.pull_requests.is_empty() {
            self.cached_display = Some(CachedDisplay {
                rows: std::mem::take(&mut self.pull_requests),
            });
        }
        self.github_ready = false;
        self.github_stale = self
            .cached_display
            .as_ref()
            .is_some_and(|cached| !cached.rows.is_empty());
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
    new_thread_id: Id,
    open_name: Box<str>,
    compact_open_name: Box<str>,
    collapse_name: Box<str>,
    expand_name: Box<str>,
    new_worktree_name: Box<str>,
    new_thread_name: Box<str>,
}

impl ProjectChrome {
    fn new(id: ProjectId, name: &str, repository: &Path) -> Self {
        Self {
            folded_name: fold(name),
            folded_repository: fold(&repository.to_string_lossy()),
            open_id: entity_id("project.open", id),
            toggle_id: entity_id("project.toggle", id),
            new_worktree_id: entity_id("project.new_worktree", id),
            new_thread_id: entity_id("project.new_thread", id),
            open_name: format!("Open project {name}").into_boxed_str(),
            compact_open_name: format!("Open {name}").into_boxed_str(),
            collapse_name: format!("Collapse {name} project").into_boxed_str(),
            expand_name: format!("Expand {name} project").into_boxed_str(),
            new_worktree_name: format!("New worktree for {name}").into_boxed_str(),
            new_thread_name: format!("New thread for {name}").into_boxed_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarEditor {
    pub id: String,
    pub name: String,
    pub icon: String,
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
    pub created_at_unix_ms: Option<i64>,
    pub pull_request_state: Option<WorktreePullRequestState>,
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
        let label = worktree_display_name(&path, branch.as_deref(), detached);
        let path_label = path.display().to_string();
        let status = status_summary(detached, locked, dirty.as_ref(), ahead, behind);
        let chrome = WorktreeChrome {
            folded_label: fold(&label),
            folded_branch: branch.as_deref().map(fold),
            folded_path: fold(&path.to_string_lossy()),
            open_id: entity_id("worktree.open", id),
            accessible_name: format!(
                "Open worktree {label}, {}, {status}",
                branch.as_deref().unwrap_or(&path_label)
            )
            .into_boxed_str(),
            tooltip: format!(
                "{label} — {} — {path_label} — {status}",
                branch.as_deref().unwrap_or("Detached HEAD")
            )
            .into_boxed_str(),
            dirty_badge: dirty
                .as_ref()
                .filter(|dirty| dirty.is_dirty())
                .map(|dirty| format!("{} changes", dirty_count(dirty)).into_boxed_str()),
            label: label.into_boxed_str(),
            pull_request: None,
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
            created_at_unix_ms: None,
            pull_request_state: None,
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

        let mut row = Self::new(
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
        );
        row.created_at_unix_ms = record.created_at_unix_ms;
        row
    }

    /// Apply each authoritative read, including the transition back to clean.
    pub fn apply_status(&mut self, status: &StatusSnapshot) {
        let (ahead, behind) = match &status.tracking {
            TrackingState::Compared { ahead, behind, .. } => (Some(*ahead), Some(*behind)),
            _ => (None, None),
        };
        if self.dirty.as_ref() == Some(&status.dirty)
            && self.ahead == ahead
            && self.behind == behind
        {
            return;
        }
        self.dirty = Some(status.dirty.clone());
        self.ahead = ahead;
        self.behind = behind;
        self.refresh_status_chrome();
    }

    fn matches(&self, folded_query: &str) -> bool {
        if self.chrome.folded_label.contains(folded_query) {
            return true;
        }
        self.chrome
            .folded_branch
            .as_deref()
            .is_some_and(|branch| branch.contains(folded_query))
            || self.chrome.folded_path.contains(folded_query)
    }

    pub fn set_pull_request(&mut self, pull_request: SidebarPullRequest) {
        self.pull_request_state = pull_request.state;
        self.chrome.pull_request = Some(pull_request);
        self.refresh_status_chrome();
    }

    pub fn clear_pull_request(&mut self) {
        let changed =
            self.pull_request_state.take().is_some() || self.chrome.pull_request.is_some();
        self.chrome.pull_request = None;
        if changed {
            self.refresh_status_chrome();
        }
    }

    fn refresh_status_chrome(&mut self) {
        let label = self
            .chrome
            .pull_request
            .as_ref()
            .map(|pr| pr.title.trim())
            .filter(|title| !title.is_empty())
            .map_or_else(
                || worktree_display_name(&self.path, self.branch.as_deref(), self.detached),
                str::to_owned,
            );
        if label != self.chrome.label.as_ref() {
            self.chrome.folded_label = fold(&label);
            self.chrome.label = label.into();
        }
        self.chrome.dirty_badge = self
            .dirty
            .as_ref()
            .filter(|dirty| dirty.is_dirty())
            .map(|dirty| format!("{} changes", dirty_count(dirty)).into_boxed_str());
        let mut status = status_summary(
            self.detached,
            self.locked,
            self.dirty.as_ref(),
            self.ahead,
            self.behind,
        );
        if let Some(pr) = &self.chrome.pull_request {
            use std::fmt::Write as _;
            let _ = write!(
                status,
                ", pull request #{}, checks {}",
                pr.number,
                check_description(pr.checks)
            );
        }
        let branch = self.branch.as_deref().unwrap_or("Detached HEAD");
        self.chrome.accessible_name =
            format!("Open worktree {}, {branch}, {status}", self.chrome.label).into_boxed_str();
        self.chrome.tooltip = format!(
            "{} — {branch} — {} — {status}",
            self.chrome.label,
            self.path.display()
        )
        .into_boxed_str();
    }
}

fn worktree_display_name(path: &Path, branch: Option<&str>, detached: bool) -> String {
    let name = branch
        .filter(|_| !detached)
        .and_then(|branch| branch.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map(Cow::Borrowed)
        .unwrap_or_else(|| {
            path.file_name()
                .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
        });
    name.replace(['-', '_'], " ")
}

/// Branch names are case-sensitive and only attached worktrees can own a PR.
/// Restricting this lookup to the containing project prevents same-named
/// branches in another repository from lending this row their CI status.
fn matching_pull_request<'a>(
    worktree: &SidebarWorktree,
    pull_requests: &'a [SidebarPullRequest],
) -> Option<&'a SidebarPullRequest> {
    if worktree.detached {
        return None;
    }
    let branch = worktree.branch.as_deref()?;
    pull_requests
        .iter()
        .find(|pull_request| pull_request.head_ref == branch)
}

/// Presentation strings and widget identities a worktree determines, built
/// once in `SidebarWorktree::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeChrome {
    folded_label: Box<str>,
    folded_branch: Option<Box<str>>,
    folded_path: Box<str>,
    open_id: Id,
    label: Box<str>,
    accessible_name: Box<str>,
    tooltip: Box<str>,
    dirty_badge: Option<Box<str>>,
    pull_request: Option<SidebarPullRequest>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreePullRequestState {
    Open,
    Draft,
    Closed,
    Merged,
}

impl WorktreePullRequestState {
    pub fn from_github(pull_request: &PullRequest) -> Option<Self> {
        match pull_request.state.0.as_str() {
            "OPEN" if pull_request.is_draft => Some(Self::Draft),
            "OPEN" => Some(Self::Open),
            "CLOSED" => Some(Self::Closed),
            "MERGED" => Some(Self::Merged),
            _ => None,
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
    pub state: Option<WorktreePullRequestState>,
    pub needs_review: bool,
    pub head_oid: Option<String>,
}

impl SidebarPullRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _project_id: ProjectId,
        number: u64,
        title: String,
        url: Url,
        head_ref: String,
        author: Option<String>,
        draft: bool,
        checks: CheckStatus,
    ) -> Self {
        Self {
            number,
            title,
            url,
            head_ref,
            author,
            draft,
            checks,
            state: None,
            needs_review: false,
            head_oid: None,
        }
    }

    pub fn from_github(
        project_id: ProjectId,
        pull_request: &PullRequest,
        checks: &[PullRequestCheck],
    ) -> Self {
        let mut row = Self::new(
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
            if checks.is_empty() {
                CheckStatus::from_checks(&pull_request.check_summaries())
            } else {
                CheckStatus::from_checks(checks)
            },
        );
        row.state = WorktreePullRequestState::from_github(pull_request);
        row.head_oid = Some(pull_request.head_ref_oid.clone());
        row.needs_review = pull_request
            .review_decision
            .as_ref()
            .is_some_and(|decision| {
                matches!(decision.0.as_str(), "REVIEW_REQUIRED" | "CHANGES_REQUESTED")
            });
        row
    }

    fn has_pending_checks(&self) -> bool {
        self.checks == CheckStatus::Pending
            && !matches!(
                self.state,
                Some(WorktreePullRequestState::Merged | WorktreePullRequestState::Closed)
            )
    }
}

/// One semantic mapping for navigation, the inspector and its tab.
pub fn pull_request_visual(pr: &SidebarPullRequest) -> (widget::icon::Handle, cosmic::iced::Color) {
    let theme = cosmic::theme::active();
    let cosmic = theme.cosmic();
    let (name, color) = match pr.state {
        Some(WorktreePullRequestState::Merged) => (
            "merged-pull-request-symbolic",
            cosmic::iced::Color::from_rgb8(163, 113, 247),
        ),
        Some(WorktreePullRequestState::Closed) => (
            "closed-pull-request-symbolic",
            cosmic.destructive.base.into(),
        ),
        _ if pr.checks == CheckStatus::Failing || pr.needs_review => {
            ("pull-request-symbolic", cosmic.destructive.base.into())
        }
        _ if pr.checks == CheckStatus::Pending => {
            ("pull-request-symbolic", cosmic.warning.base.into())
        }
        _ if pr.draft => (
            "pull-request-symbolic",
            theme.current_container().component.on_disabled.into(),
        ),
        _ if pr.checks == CheckStatus::Passing => {
            ("pull-request-symbolic", cosmic.success.base.into())
        }
        _ => (
            "pull-request-symbolic",
            theme.current_container().component.on.into(),
        ),
    };
    (crate::ui_controls::symbolic_icon(name), color)
}

/// Exact number of rows in a section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowCount {
    value: usize,
}

impl RowCount {
    /// A count of rows this application holds all of.
    const fn exact(value: usize) -> Self {
        Self { value }
    }
}

impl From<usize> for RowCount {
    fn from(value: usize) -> Self {
        Self::exact(value)
    }
}

impl std::fmt::Display for RowCount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.value)
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

/// Section identities retained in presentation state; Files has its own panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PanelSection {
    Files,
    Changes,
    History,
}

impl PanelSection {
    pub const ALL: [Self; 3] = [Self::Files, Self::Changes, Self::History];

    fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Changes => "Changes",
            Self::History => "History",
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
        let counts = crate::repository::line_change_counts(entry.added_lines, entry.removed_lines);
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
            accessible_name: if counts.is_empty() {
                format!("{status} {label}")
            } else {
                format!("{status} {label}, {counts}")
            }
            .into_boxed_str(),
            stage_name: format!("Stage {label}").into_boxed_str(),
            unstage_name: format!("Unstage {label}").into_boxed_str(),
            discard_name: format!("Discard {label}").into_boxed_str(),
            label: label.into_boxed_str(),
            status: status.into_boxed_str(),
            counts_split: counts.find(' '),
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

    /// The added/removed summary. Unmeasured counts are omitted, not invented.
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
    counts_split: Option<usize>,
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
    tree: Vec<FileRow>,
    path_order: Vec<usize>,
    chrome: ChangeGroupChrome,
}

impl ChangeGroup {
    fn visible_path(&self, path: &RepoPath) -> Option<&ChangeRow> {
        self.path_order
            .binary_search_by(|index| self.rows[*index].entry.path.cmp(path))
            .ok()
            .map(|index| &self.rows[self.path_order[index]])
    }

    fn new(worktree_id: WorktreeId, area: ChangeArea, rows: Vec<ChangeRow>) -> Self {
        let title = match area {
            ChangeArea::Conflict => "Conflicts",
            ChangeArea::Staged => "Staged",
            ChangeArea::Unstaged => "Unstaged",
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
        let paths = rows
            .iter()
            .take(MAX_VISIBLE_CHANGE_ROWS)
            .map(|row| row.entry.path.clone())
            .collect::<Vec<_>>();
        let tree = file_rows(&paths);
        let mut path_order: Vec<usize> = (0..paths.len()).collect();
        path_order.sort_unstable_by(|left, right| paths[*left].cmp(&paths[*right]));
        Self {
            area,
            rows,
            tree,
            path_order,
            chrome,
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChangeRow {
    pub entry: crate::repository::DiffEntry,
    label: Box<str>,
    status: Box<str>,
    counts: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChangesModel {
    pub base_ref: ValidatedRefName,
    pub head: Oid,
    pub merge_base: Oid,
    pub rows: Vec<BranchChangeRow>,
    tree: Vec<FileRow>,
    title: Box<str>,
    base_label: Box<str>,
}

impl BranchChangesModel {
    pub fn new(comparison: BranchComparison) -> Self {
        let mut rows: Vec<_> = comparison
            .entries
            .into_iter()
            .map(|entry| {
                let label = match &entry.old_path {
                    Some(old) => format!("{} → {}", old.display(), entry.path.display()),
                    None => entry.path.display().into_owned(),
                }
                .into_boxed_str();
                let status = change_kind_label(entry.kind).into_boxed_str();
                let counts =
                    crate::repository::line_change_counts(entry.added_lines, entry.removed_lines)
                        .into_boxed_str();
                BranchChangeRow {
                    entry,
                    label,
                    status,
                    counts,
                }
            })
            .collect();
        rows.sort_unstable_by(|left, right| left.entry.path.cmp(&right.entry.path));
        let paths: Vec<_> = rows
            .iter()
            .take(MAX_VISIBLE_CHANGE_ROWS)
            .map(|row| row.entry.path.clone())
            .collect();
        Self {
            title: format!("Branch changes ({})", rows.len()).into_boxed_str(),
            base_label: format!("Since branching from {}", comparison.base_ref.short_name())
                .into_boxed_str(),
            base_ref: comparison.base_ref,
            head: comparison.head,
            merge_base: comparison.merge_base,
            rows,
            tree: file_rows(&paths),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchChanges {
    Unavailable(&'static str),
    Loading(ValidatedRefName),
    Ready(Arc<BranchChangesModel>),
    Error {
        base_ref: ValidatedRefName,
        message: String,
    },
}

/// Immutable source-control data for one worktree.
///
/// Everything a frame needs is derived once, here: filter keys, widget
/// identities, accessible names, and the status and count strings. The view
/// reads them and allocates nothing per row.
/// On-demand file-name search for a plain folder. Git worktrees keep
/// `Inactive` and filter the already-loaded `git ls-files` list in memory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FileSearch {
    #[default]
    Inactive,
    Pending {
        query: String,
    },
    Ready {
        query: String,
        files: Vec<FileRow>,
        hidden: usize,
    },
    Failed {
        query: String,
        message: String,
    },
}

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
    pub branch_changes: BranchChanges,
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
    pub file_search: FileSearch,
}

fn file_rows(paths: &[RepoPath]) -> Vec<FileRow> {
    file_rows_from_tree(build_file_tree(paths))
}

fn file_rows_from_listing(files: &[crate::repository_widget::RepositoryFile]) -> Vec<FileRow> {
    let mut file_paths = Vec::new();
    let mut directories = Vec::new();
    for file in files {
        let Ok(path) = RepoPath::parse(&file.path) else {
            continue;
        };
        if file.directory {
            directories.push(path);
        } else {
            file_paths.push(path);
        }
    }
    file_rows_from_tree(build_file_tree_with_directories(&file_paths, &directories))
}

fn file_rows_from_tree(entries: Vec<crate::repository::FileTreeEntry>) -> Vec<FileRow> {
    entries
        .into_iter()
        .map(|entry| {
            let kind = if entry.directory { "folder" } else { "file" };
            let chrome = FileRowChrome {
                folded: fold(&entry.path.display()),
                accessible_name: format!("Open {kind} {}", entry.path.display()).into_boxed_str(),
                label: entry.path.file_name().into_owned().into_boxed_str(),
            };
            FileRow {
                path: entry.path,
                depth: entry.depth,
                directory: entry.directory,
                chrome,
            }
        })
        .collect()
}

impl SourceControlModel {
    /// File navigation for a plain folder, without invented Git data.
    pub fn directory(key: ActiveWorktreeKey, paths: &[RepoPath]) -> Self {
        Self::directory_listing(
            key,
            &paths
                .iter()
                .map(|path| crate::repository_widget::RepositoryFile {
                    path: path.as_path().to_path_buf(),
                    ..crate::repository_widget::RepositoryFile::default()
                })
                .collect::<Vec<_>>(),
        )
    }

    pub fn directory_listing(
        key: ActiveWorktreeKey,
        files: &[crate::repository_widget::RepositoryFile],
    ) -> Self {
        let mut model = Self::empty(key, SidebarDataState::Ready);
        model.chrome.subheading = model
            .key
            .canonical_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| model.key.canonical_path.display().to_string())
            .into_boxed_str();
        model.files = file_rows_from_listing(files);
        model
    }

    pub fn set_file_search(&mut self, search: FileSearch) {
        self.file_search = search;
    }

    pub fn file_search_results(
        query: String,
        files: &[crate::repository_widget::RepositoryFile],
        hidden: usize,
    ) -> FileSearch {
        FileSearch::Ready {
            query,
            files: file_rows_from_listing(files),
            hidden,
        }
    }

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
            branch_changes: BranchChanges::Unavailable("No branch comparison available."),
            history: Vec::new(),
            history_loaded: false,
            history_requested: false,
            history_complete: false,
            next_history_page: 0,
            reloading: false,
            chrome,
            file_search: FileSearch::Inactive,
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
        let files = file_rows(&snapshot.files.paths);

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
            branch_changes: BranchChanges::Unavailable("No branch comparison available."),
            history: Vec::new(),
            history_loaded: false,
            history_requested: false,
            history_complete: false,
            next_history_page: 0,
            reloading: false,
            branch,
            key,
            chrome,
            file_search: FileSearch::Inactive,
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

    pub fn has_changes(&self) -> bool {
        self.groups.iter().any(|group| !group.is_empty())
            || matches!(&self.branch_changes, BranchChanges::Ready(branch) if !branch.rows.is_empty())
    }

    /// Whether this panel is bound to a Git worktree, so Changes can load.
    pub fn offers_changes(&self) -> bool {
        self.branch.is_some() || self.unborn || self.detached || self.head_oid.is_some()
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
    subheading: Box<str>,
    files_filter_id: Id,
    retry_id: Id,
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
    section_ids: [Id; 3],
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
            subheading: match label {
                Some(label) => label.to_owned().into_boxed_str(),
                None => format!("{name} (detached)").into_boxed_str(),
            },
            files_filter_id: entity_id("panel.files.filter", worktree_id),
            retry_id: entity_id("panel.retry", worktree_id),
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
        ChangeKind::Untracked => "Untracked file".to_owned(),
        ChangeKind::Ignored => "!".to_owned(),
        ChangeKind::Unmerged => "U".to_owned(),
        ChangeKind::Unknown(_) => "Unknown change".to_owned(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    Branch(RepoPath),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileListMode {
    #[default]
    Tree,
    Flat,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SidebarView {
    query: String,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
    #[serde(default)]
    expanded_directories: BTreeSet<RepoPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneActivity {
    owner: Option<(ProjectId, WorktreeId)>,
    activity: crate::agent::Activity,
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
    selection: Option<SidebarSelection>,
    active_worktree: Option<ActiveWorktreeKey>,
    /// Sections toggled away from their default. Storing the toggles rather
    /// than the open set keeps `default()` meaningful: `Changes` starts open
    /// and the rest start closed.
    toggled_sections: BTreeSet<PanelSection>,
    /// Compact mode only: whether the source-control popover is showing.
    panel_open: bool,
    files_query: String,
    files_filter_open: bool,
    change_list_mode: FileListMode,
    worktree_sort: crate::settings::WorktreeSort,
    agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    agent_activity: BTreeMap<CardId, PaneActivity>,
    commit_message: String,
    /// File-tree folders the user opened. Everything else renders collapsed,
    /// so a fresh worktree shows its top level and loading a whole project
    /// tree stays cheap. Diff and change review ignore this and expand all.
    expanded_directories: BTreeSet<RepoPath>,
    publish: Option<PublishDraft>,
    discard: Option<DiscardConfirmation>,
    panel_selection: Option<PanelSelection>,
}

impl SidebarState {
    pub fn saved_view(&self) -> SidebarView {
        SidebarView {
            query: self.query.clone(),
            controls_expanded: self.controls_expanded,
            collapsed_projects: self.collapsed_projects.clone(),
            selection: self.selection,
            expanded_directories: self.expanded_directories.clone(),
        }
    }

    pub fn restore_view(&mut self, view: SidebarView) {
        self.query = view.query;
        self.controls_expanded = view.controls_expanded;
        self.collapsed_projects = view.collapsed_projects;
        self.selection = view.selection;
        self.expanded_directories = view.expanded_directories;
    }

    /// Folders the user opened, as display paths, for session persistence.
    pub fn expanded_display_paths(&self) -> Vec<String> {
        self.expanded_directories
            .iter()
            .map(|path| path.display().into_owned())
            .collect()
    }

    /// Restores folders opened in a previous session. Unknown or invalid
    /// paths are skipped: the tree still renders, just collapsed.
    pub fn restore_expanded_display_paths(&mut self, paths: &[String]) {
        use std::ffi::OsStr;
        self.expanded_directories = paths
            .iter()
            .filter_map(|path| crate::repository::RepoPath::parse(OsStr::new(path)).ok())
            .collect();
    }
    pub fn set_worktree_sort(&mut self, order: crate::settings::WorktreeSort) {
        self.worktree_sort = order;
    }

    pub fn files_filter_open(&self) -> bool {
        self.files_filter_open
    }

    pub fn reveal_project(&mut self, project: ProjectId) {
        self.collapsed_projects.remove(&project);
        self.query.clear();
        self.filter = SidebarFilter::All;
    }

    /// Records unread attention without requiring a repository-owned pane.
    /// Ordinary terminal output and activity alone never call this.
    pub fn observe_card_update(&mut self, card: CardId) {
        self.agent_updates.entry(card).or_insert(None);
    }

    pub fn observe_agent_update(&mut self, card: CardId, project: ProjectId, worktree: WorktreeId) {
        self.agent_updates.insert(card, Some((project, worktree)));
    }

    pub fn acknowledge_agent_update(&mut self, card: CardId) {
        self.agent_updates.remove(&card);
    }

    pub fn has_card_update(&self, card: CardId) -> bool {
        self.agent_updates.contains_key(&card)
    }

    pub fn has_agent_update(&self, project: ProjectId, worktree: WorktreeId) -> bool {
        self.agent_updates
            .values()
            .any(|owner| *owner == Some((project, worktree)))
    }

    pub fn card_activity(&self, card: CardId) -> Option<crate::agent::Activity> {
        self.agent_activity.get(&card).map(|entry| entry.activity)
    }

    pub fn clear_agent_activity(&mut self, card: CardId) {
        self.agent_activity.remove(&card);
    }

    pub fn set_card_activity(&mut self, card: CardId, activity: crate::agent::Activity) {
        self.agent_activity.insert(
            card,
            PaneActivity {
                owner: None,
                activity,
            },
        );
        if let Some(owner) = self.agent_updates.get_mut(&card) {
            *owner = None;
        }
    }

    pub fn set_agent_activity(
        &mut self,
        card: CardId,
        project: ProjectId,
        worktree: WorktreeId,
        activity: crate::agent::Activity,
    ) {
        let owner = Some((project, worktree));
        self.agent_activity
            .insert(card, PaneActivity { owner, activity });
        if let Some(attribution) = self.agent_updates.get_mut(&card) {
            *attribution = owner;
        }
    }

    pub fn agent_activity(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<crate::agent::ActivityState> {
        use crate::agent::ActivityState;
        let mut status = None;
        for entry in self.agent_activity.values() {
            if entry.owner != Some((project, worktree)) {
                continue;
            }
            match entry.activity.state {
                ActivityState::Working => return Some(ActivityState::Working),
                ActivityState::Waiting => status = Some(ActivityState::Waiting),
                _ => {}
            }
        }
        status
    }

    /// Synchronize when editor focus changes outside the file navigator.
    pub fn select_file(&mut self, key: &ActiveWorktreeKey, path: Option<&Path>) {
        if self.active_worktree.as_ref() != Some(key) {
            return;
        }
        let current = self
            .panel_selection
            .as_ref()
            .map(|selection| match selection {
                PanelSelection::File(path)
                | PanelSelection::Branch(path)
                | PanelSelection::Change { path, .. } => path.as_path(),
            });
        if current != path {
            self.panel_selection = path
                .and_then(|path| RepoPath::parse(path).ok())
                .map(PanelSelection::File);
        }
    }

    pub fn directory_is_expanded(&self, path: &RepoPath) -> bool {
        self.expanded_directories.contains(path)
    }

    pub fn expanded_directory_paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.expanded_directories.iter()
    }

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
        self.selection = next.as_ref().map(|key| SidebarSelection::Worktree {
            project_id: key.project_id,
            worktree_id: key.worktree_id,
        });
        if self.active_worktree == next {
            return false;
        }
        self.active_worktree = next;
        // Every panel-local draft belongs to the worktree it was typed for.
        self.files_query.clear();
        self.files_filter_open = false;
        self.commit_message.clear();
        self.expanded_directories.clear();
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
            SidebarAction::FilesQueryChanged { query, .. } => {
                self.files_query.clone_from(query);
                self.files_filter_open = true;
            }
            SidebarAction::ToggleFilesFilter { .. } => {
                self.files_filter_open = !self.files_filter_open;
                if !self.files_filter_open {
                    self.files_query.clear();
                }
            }
            SidebarAction::SetChangeListMode { mode, .. } => self.change_list_mode = *mode,
            SidebarAction::SetWorktreePanel { .. } => {}
            SidebarAction::ToggleDirectory { path, .. } => {
                if !self.expanded_directories.remove(path) {
                    self.expanded_directories.insert(path.clone());
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
            SidebarAction::OpenBranchFile { path, .. } => {
                self.panel_selection = Some(PanelSelection::Branch(path.clone()));
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
            | SidebarAction::CancelWorktreeOperation(_)
            | SidebarAction::ProjectCommand { .. }
            | SidebarAction::OpenFileInEditor { .. }
            | SidebarAction::FileCommand { .. }
            | SidebarAction::NewWorktree { .. }
            | SidebarAction::NewThread { .. }
            | SidebarAction::OpenThread { .. }
            | SidebarAction::ReorderProject { .. }
            | SidebarAction::DragWindow
            | SidebarAction::MaximizeWindow
            | SidebarAction::WindowMenu
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

/// File operations offered by hosts that enable file management.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCommand {
    Rename,
    CopyFile,
    CopyPath,
    CopyRelativePath,
}

/// All sidebar intents. Payloads are self-contained so `app.rs` does not need
/// to recover a path or URL from ephemeral row indexes, and every
/// source-control intent carries the whole [`ActiveWorktreeKey`] so a result
/// arriving after the panel rebound is recognisably stale.
///
/// Not `Eq`: the resize request carries a width, and widths are measurements.
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarAction {
    DragWindow,
    MaximizeWindow,
    WindowMenu,
    ReorderProject {
        project: ProjectId,
        before: Option<ProjectId>,
    },
    ProjectCommand {
        project_id: ProjectId,
        worktree_id: Option<WorktreeId>,
        command: ProjectCommand,
    },
    CancelWorktreeOperation(crate::model::OperationId),
    SearchChanged(String),
    SetFilter(SidebarFilter),
    ToggleControls,
    ToggleProject(ProjectId),
    OpenRepositoryPicker,
    NewThread {
        project_id: ProjectId,
    },
    OpenThread {
        thread_id: ThreadId,
    },
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
    ToggleFilesFilter {
        key: ActiveWorktreeKey,
    },
    FilesQueryChanged {
        key: ActiveWorktreeKey,
        query: String,
    },
    SetChangeListMode {
        key: ActiveWorktreeKey,
        mode: FileListMode,
    },
    SetWorktreePanel {
        panel: WorktreePanel,
    },
    OpenFileInEditor {
        key: ActiveWorktreeKey,
        path: RepoPath,
        editor_id: String,
    },
    FileCommand {
        key: ActiveWorktreeKey,
        path: RepoPath,
        command: FileCommand,
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
    /// Opens exactly the committed comparison, including paths absent from disk.
    OpenBranchFile {
        key: ActiveWorktreeKey,
        path: RepoPath,
        old_path: Option<RepoPath>,
        base_ref: ValidatedRefName,
        head: Oid,
        merge_base: Oid,
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

/// Whether opening search can reveal a focusable input. Compact navigation
/// uses a popover so the rail never squeezes a text input into its icon width.
pub fn search_is_reachable(model: &SidebarModel, _mode: SidebarMode) -> bool {
    !model.projects.is_empty()
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

fn sidebar_scroll_direction() -> cosmic::iced::widget::scrollable::Direction {
    cosmic::iced::widget::scrollable::Direction::Vertical(design::scrollbar())
}

/// The left project/worktree navigation.
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
    let header_loading = loading && !model.projects.is_empty();
    let mut content = widget::column::with_capacity(7)
        .push(
            widget::container(header(model, state, compact, header_loading, modal_open))
                .padding([0, spacing.space_xxxs]),
        )
        .push(widget::divider::horizontal::default());

    if !compact && state.controls_expanded && search_is_reachable(model, mode) {
        content = content.push(
            widget::container(controls(state, modal_open))
                .class(theme::Container::Primary)
                .padding(spacing.space_xxs),
        );
    }

    content = content.push(
        widget::container(body(model, state, compact, modal_open))
            .width(Length::Fill)
            .height(Length::Fill),
    );
    content = content
        .push(widget::divider::horizontal::default())
        .push(footer(compact, modal_open))
        .spacing(0)
        .width(Length::Fill)
        .height(Length::Fill);

    widget::container(crate::interaction::layer(
        widget::container(content)
            .class(theme::Container::Primary)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        // The compact rail has one width, so its edge is a rule and
        // nothing more: a drag there would silently retune a width the
        // rail does not render.
        widget::container(if compact {
            Element::from(
                widget::container(widget::divider::vertical::default())
                    .width(crate::splitter::WIDTH)
                    .height(Length::Fill),
            )
        } else {
            resize_handle(width, modal_open).into()
        })
        .align_x(Alignment::End)
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    ))
    .class(theme::Container::Primary)
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
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
    if !model.projects.is_empty() {
        let attention = state.filter == SidebarFilter::Attention;
        actions.push(crate::interaction::tooltip(
            button::icon(crate::ui_controls::symbolic_icon("notifications-symbolic"))
                .class(sidebar_button_style(attention))
                .id(filter_id(SidebarFilter::Attention))
                .name("Show projects needing attention")
                .selected(attention)
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(gated(
                    SidebarAction::SetFilter(if attention {
                        SidebarFilter::All
                    } else {
                        SidebarFilter::Attention
                    }),
                    modal_open,
                )),
            if attention {
                "Show all projects"
            } else {
                "Show projects needing attention"
            },
        ));
        let selected = state.controls_expanded || (!compact && !state.query.is_empty());
        let search_button = crate::interaction::tooltip(
            button::icon(icon::from_name("edit-find-symbolic").size(ROW_ICON_SIZE))
                .class(sidebar_button_style(selected))
                .id(stable_id("toggle-controls"))
                .name("Search projects")
                .selected(selected)
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(gated(SidebarAction::ToggleControls, modal_open)),
            "Search projects",
        );
        if compact {
            actions.push(crate::ui_controls::popover(
                search_button,
                (state.controls_expanded && !modal_open).then(|| {
                    widget::container(controls(state, false))
                        .width(DEFAULT_EXPANDED_WIDTH)
                        .into()
                }),
                widget::popover::Position::Point(cosmic::iced::Point::new(COMPACT_WIDTH, 0.0)),
                SidebarAction::ToggleControls,
            ));
        } else {
            actions.push(search_button);
        }
    }
    actions.push(icon_action(
        "list-add-symbolic",
        "New project",
        gated(SidebarAction::OpenRepositoryPicker, modal_open),
        stable_id("open-repository"),
    ));
    if compact {
        return widget::column::with_children(actions)
            .align_x(Alignment::Center)
            .padding([spacing.space_xxxs, 0])
            .spacing(spacing.space_xxxs)
            .width(Length::Fill)
            .into();
    }

    let mut children: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(6);
    children.push(design::heading("Projects").into());
    children.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
    if loading {
        children.push(crate::ui_controls::activity_spinner(
            f32::from(ROW_ICON_SIZE),
            cosmic::theme::active().cosmic().warning.base.into(),
        ));
    }
    children.extend(actions);

    let header = widget::row::with_children(children)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(Length::Fixed(HEADER_HEIGHT))
        .width(Length::Fill);
    if modal_open {
        header.into()
    } else {
        widget::mouse_area(header)
            .on_drag(SidebarAction::DragWindow)
            .on_double_press(SidebarAction::MaximizeWindow)
            .on_right_press(SidebarAction::WindowMenu)
            .into()
    }
}

pub(crate) fn selected_project<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
) -> Option<&'a SidebarProject> {
    let project_id = state
        .active_worktree()
        .map(|key| key.project_id)
        .or_else(|| {
            state.selection.map(|selection| match selection {
                SidebarSelection::Project(project_id)
                | SidebarSelection::Worktree { project_id, .. }
                | SidebarSelection::PullRequest { project_id, .. } => project_id,
            })
        })?;
    model
        .projects
        .iter()
        .find(|project| project.id == project_id)
}

pub fn files_filter_focus_id(key: &ActiveWorktreeKey) -> Id {
    entity_id("panel.files.query", key.worktree_id)
}

fn controls<'a>(state: &'a SidebarState, modal_open: bool) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(3);
    actions.push(filters(state.filter, modal_open));
    actions.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
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

fn footer<'a>(compact: bool, modal_open: bool) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let settings = icon_action(
        "emblem-system-symbolic",
        "Settings",
        gated(SidebarAction::OpenSettings, modal_open),
        stable_id("settings"),
    );
    if compact {
        let children: Vec<Element<'a, SidebarAction>> = vec![settings];
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

/// Secondary filters live beside the search input. Attention has its own
/// independent header toggle rather than a duplicate control here.
const FILTER_CHOICES: [(SidebarFilter, &str, &str); 2] = [
    (SidebarFilter::All, "All", "Show all projects"),
    (
        SidebarFilter::PullRequests,
        "Pull requests",
        "Show projects with pull requests",
    ),
];

fn filters(filter: SidebarFilter, modal_open: bool) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut row = widget::row::with_capacity(FILTER_CHOICES.len())
        .spacing(spacing.space_xxxs)
        .align_y(Alignment::Center);
    for (value, visible_label, accessible_label) in FILTER_CHOICES {
        row = row.push(
            button::custom(design::caption(visible_label))
                .class(sidebar_button_style(filter == value))
                .id(filter_id(value))
                .name(accessible_label)
                .selected(filter == value)
                .on_press_maybe(gated(SidebarAction::SetFilter(value), modal_open)),
        );
    }
    row.into()
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
        return project_scroll(empty(state, compact, modal_open), state.filter);
    }

    // Search results are revealed while their input is available, including
    // the compact popover. Otherwise preserve explicit project collapses.
    let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
    let selection = state
        .active_worktree()
        .map(|key| SidebarSelection::Worktree {
            project_id: key.project_id,
            worktree_id: key.worktree_id,
        })
        .or(state.selection);
    let spacing = cosmic::theme::spacing();
    let sticky_viewport = row_interaction::StickyViewport::default();
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
            compact,
            selection,
            modal_open,
            sticky_viewport.clone(),
        ));
    }

    let column = RowInteraction::projects(
        column.into(),
        projects.iter().map(|project| project.project.id).collect(),
        !modal_open && !compact,
        sticky_viewport,
    );
    project_scroll(
        widget::container(column)
            .padding(spacing.space_xxxs)
            .width(Length::Fill)
            .into(),
        state.filter,
    )
}

fn project_scroll(
    content: Element<'_, SidebarAction>,
    filter: SidebarFilter,
) -> Element<'_, SidebarAction> {
    RowInteraction::filtered_scroll(
        widget::scrollable(content)
            .id(stable_id("projects.scroll"))
            .direction(sidebar_scroll_direction())
            .class(cosmic::style::iced::Scrollable::Minimal)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        filter,
    )
}

fn loading(compact: bool) -> Element<'static, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let content = widget::column::with_children(vec![
        crate::ui_controls::activity_spinner(
            f32::from(STATUS_ICON_SIZE),
            cosmic::theme::active().cosmic().warning.base.into(),
        ),
        design::caption(if compact {
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
            design::heading("Couldn’t load projects").into(),
            design::caption(message)
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
    let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
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
                "No unread agent updates",
                "No panes currently need your attention.",
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
    let action_button: Element<'static, SidebarAction> = button::standard(action_label)
        .class(sidebar_button_style(false))
        .id(stable_id("empty.action"))
        .on_press_maybe(action)
        .into();
    widget::container(
        widget::column::with_children(vec![
            widget::icon(crate::ui_controls::symbolic_icon(icon_name))
                .size(STATUS_ICON_SIZE)
                .into(),
            design::heading(title).into(),
            design::caption(detail)
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
    compact: bool,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    sticky_viewport: row_interaction::StickyViewport,
) -> Element<'a, SidebarAction> {
    let entity = project.project;
    let chrome = &entity.chrome;
    let project_id = entity.id;
    let selection = if entity.threads.iter().any(|thread| thread.selected) {
        None
    } else {
        selection
    };
    if compact {
        return compact_project(project, selection, modal_open, sticky_viewport);
    }

    let spacing = cosmic::theme::spacing();
    let worktree_count = project.worktrees.len();
    let has_children = worktree_count > 0 || !project.threads.is_empty();
    let project_identity = button::custom(
        widget::row::with_children(vec![
            widget::container(project_icon(entity, ROW_ICON_SIZE))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            design::heading(entity.name.as_str())
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .height(Length::Fill)
        .spacing(spacing.space_xxxs),
    )
    .class(sidebar_button_style(
        selection == Some(SidebarSelection::Project(project_id)),
    ))
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
    let project_identity =
        crate::interaction::tooltip(project_identity, entity.repository.display().to_string());

    let disclosure: Element<'a, SidebarAction> = if has_children {
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
    let add_worktree = icon_action(
        "worktree-add-symbolic",
        &*chrome.new_worktree_name,
        gated(
            SidebarAction::NewWorktree {
                project_id,
                repository: entity.repository.clone(),
            },
            modal_open,
        ),
        chrome.new_worktree_id.clone(),
    );
    let add_thread = icon_action(
        "thread-add-symbolic",
        &*chrome.new_thread_name,
        gated(SidebarAction::NewThread { project_id }, modal_open),
        chrome.new_thread_id.clone(),
    );
    let identity = widget::row::with_children(vec![
        project_identity,
        cosmic::iced::widget::Space::new()
            .width(3.0 * ICON_CONTROL_SIZE)
            .into(),
    ])
    .align_y(Alignment::Center)
    .height(PROJECT_HEADER_HEIGHT);
    let header = RowInteraction::actions(
        identity.into(),
        widget::container(
            widget::row::with_children(vec![disclosure, add_thread, add_worktree])
                .align_y(Alignment::Center)
                .spacing(0)
                .height(PROJECT_HEADER_HEIGHT),
        )
        .align_right(Length::Fill)
        .into(),
    );
    let header = context_controls(
        header,
        project_id,
        None,
        modal_open,
        entity.github_ready,
        project.editors,
    );
    let mut column = widget::column::with_capacity(worktree_count + 2).spacing(0);
    // GitHub availability belongs in the Changes panel, not between navigation rows.
    if entity.github_stale {
        column = column.push(design::caption(
            "Cached pull requests — stale; not authorized for mutations.",
        ));
    }
    for operation in &entity.operations {
        column = column.push(
            widget::row::with_children(vec![
                design::caption(&operation.label).width(Length::Fill).into(),
                button::standard("Cancel")
                    .on_press_maybe(
                        (operation.cancellable && !modal_open)
                            .then_some(SidebarAction::CancelWorktreeOperation(operation.id)),
                    )
                    .into(),
            ])
            .spacing(4),
        );
    }

    if expanded {
        for thread in &project.threads {
            column = column.push(
                widget::container(thread_view(thread, modal_open, false)).padding([
                    0.0,
                    0.0,
                    0.0,
                    ICON_CONTROL_SIZE,
                ]),
            );
        }
        for &worktree in &project.worktrees {
            column = column.push(
                widget::container(worktree_view(
                    project_id,
                    worktree,
                    is_primary_worktree(&entity.repository, &worktree.path),
                    selection,
                    modal_open,
                    entity.github_ready,
                    project.editors,
                    project.state.has_agent_update(project_id, worktree.id),
                    project.state.agent_activity(project_id, worktree.id),
                ))
                .padding([0.0, 0.0, 0.0, ICON_CONTROL_SIZE]),
            );
        }
    }

    RowInteraction::group(header, column.into(), sticky_viewport)
}

pub(crate) fn is_primary_worktree(repository: &Path, worktree: &Path) -> bool {
    repository == worktree
        || (repository.file_name().is_some_and(|name| name == ".git")
            && repository.parent() == Some(worktree))
}

fn compact_project<'a>(
    project: &FilteredProject<'a>,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    sticky_viewport: row_interaction::StickyViewport,
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
    let action = button::custom(project_icon(entity, ROW_ICON_SIZE))
        .class(sidebar_button_style(selected))
        .id(chrome.open_id.clone())
        .name(&*chrome.compact_open_name)
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
    let header = widget::container(context_controls(
        crate::interaction::tooltip(action, &*chrome.compact_open_name),
        entity.id,
        None,
        modal_open,
        entity.github_ready,
        project.editors,
    ))
    .center_x(Length::Fill);
    let selection = if entity.threads.iter().any(|thread| thread.selected) {
        None
    } else {
        selection
    };
    let mut column = widget::column::with_capacity(project.worktrees.len() + 1)
        .align_x(Alignment::Center)
        .width(Length::Fill);
    for operation in &entity.operations {
        let action = button::icon(
            icon::from_name(if operation.cancellable {
                "process-stop-symbolic"
            } else {
                "dialog-information-symbolic"
            })
            .size(ROW_ICON_SIZE),
        )
        .name(if operation.cancellable {
            format!("Cancel worktree operation: {}", operation.label)
        } else {
            operation.label.clone()
        })
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .on_press_maybe(
            (operation.cancellable && !modal_open)
                .then_some(SidebarAction::CancelWorktreeOperation(operation.id)),
        );
        column = column.push(crate::interaction::tooltip(action, &operation.label));
    }
    for thread in &project.threads {
        column = column.push(thread_view(thread, modal_open, true));
    }
    for &worktree in &project.worktrees {
        let selected = selection
            == Some(SidebarSelection::Worktree {
                project_id: entity.id,
                worktree_id: worktree.id,
            });
        let action = button::custom(worktree_status_icon(
            worktree,
            project.state.has_agent_update(entity.id, worktree.id),
        ))
        .id(worktree.chrome.open_id.clone())
        .name(worktree_accessible_name(
            worktree,
            project.state.has_agent_update(entity.id, worktree.id),
            project.state.agent_activity(entity.id, worktree.id),
        ))
        .width(ROW_ICON_WIDTH)
        .height(Length::Fixed(WORKTREE_ROW_HEIGHT))
        .class(sidebar_button_style(selected))
        .selected(selected)
        .on_press_maybe(gated(
            SidebarAction::OpenWorktree {
                project_id: entity.id,
                worktree_id: worktree.id,
                path: worktree.path.clone(),
            },
            modal_open,
        ));
        let mut row = widget::row::with_capacity(2)
            .align_y(Alignment::Center)
            .push(crate::interaction::tooltip(
                action,
                &*worktree.chrome.tooltip,
            ));
        let activity = project.state.agent_activity(entity.id, worktree.id);
        let waiting_unread = activity == Some(crate::agent::ActivityState::Waiting)
            && project.state.has_agent_update(entity.id, worktree.id);
        if activity == Some(crate::agent::ActivityState::Working)
            || (!waiting_unread
                && worktree
                    .chrome
                    .pull_request
                    .as_ref()
                    .is_some_and(SidebarPullRequest::has_pending_checks))
        {
            row = row.push(crate::ui_controls::activity_spinner(
                f32::from(ROW_ICON_SIZE),
                cosmic::theme::active().cosmic().warning.base.into(),
            ));
        } else if waiting_unread {
            row = row.push(status_icon(
                "agent-waiting-symbolic",
                "An agent needs input",
                BadgeTone::Danger,
            ));
        } else if let Some(label) = worktree.chrome.dirty_badge.as_deref() {
            row = row.push(status_icon(
                "document-edit-symbolic",
                label,
                BadgeTone::Warning,
            ));
        }
        column = column.push(context_controls(
            row,
            entity.id,
            Some(worktree.id),
            modal_open,
            entity.github_ready,
            project.editors,
        ));
    }
    column = column.push(icon_action(
        "thread-add-symbolic",
        &*chrome.new_thread_name,
        gated(
            SidebarAction::NewThread {
                project_id: entity.id,
            },
            modal_open,
        ),
        chrome.new_thread_id.clone(),
    ));
    column = column.push(icon_action(
        "worktree-add-symbolic",
        &*chrome.new_worktree_name,
        gated(
            SidebarAction::NewWorktree {
                project_id: entity.id,
                repository: entity.repository.clone(),
            },
            modal_open,
        ),
        chrome.new_worktree_id.clone(),
    ));
    widget::container(RowInteraction::group(
        header.into(),
        column.into(),
        sticky_viewport,
    ))
    .padding([cosmic::theme::spacing().space_xxxs, 0])
    .width(Length::Fill)
    .into()
}

fn thread_view(
    thread: &SidebarThread,
    modal_open: bool,
    compact: bool,
) -> Element<'_, SidebarAction> {
    let activity = thread.activity;
    let unread = thread.unread;
    let mut content = widget::row::with_capacity(4)
        .align_y(Alignment::Center)
        .spacing(4)
        .push(
            widget::container(
                widget::icon(crate::ui_controls::symbolic_icon("thread-symbolic"))
                    .size(ROW_ICON_SIZE),
            )
            .class(theme::Container::custom(move |theme| {
                cosmic::iced::widget::container::Style {
                    icon_color: Some(if unread {
                        theme.cosmic().destructive.base.into()
                    } else if activity == Some(crate::agent::ActivityState::Working) {
                        theme.cosmic().warning.base.into()
                    } else {
                        design::muted_color(theme)
                    }),
                    ..Default::default()
                }
            })),
        );
    if !compact {
        content = content.push(
            design::body(&thread.title)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill),
        );
        if thread.activity == Some(crate::agent::ActivityState::Working) {
            content = content.push(crate::ui_controls::activity_spinner(
                f32::from(ROW_ICON_SIZE),
                cosmic::theme::active().cosmic().warning.base.into(),
            ));
        } else if thread.unread {
            content = content.push(status_icon(
                "agent-waiting-symbolic",
                "Thread needs attention",
                BadgeTone::Danger,
            ));
        }
    }
    let action = button::custom(content)
        .id(thread.open_id.clone())
        .name(&*thread.accessible_name)
        .class(sidebar_button_style(thread.selected))
        .selected(thread.selected)
        .padding([0, cosmic::theme::spacing().space_xxxs])
        .height(WORKTREE_ROW_HEIGHT)
        .width(if compact {
            Length::Fixed(ICON_CONTROL_SIZE)
        } else {
            Length::Fill
        })
        .on_press_maybe(gated(
            SidebarAction::OpenThread {
                thread_id: thread.id,
            },
            modal_open,
        ));
    crate::interaction::tooltip(action, &thread.title)
}

fn worktree_accessible_name(
    worktree: &SidebarWorktree,
    unread: bool,
    activity: Option<crate::agent::ActivityState>,
) -> Cow<'_, str> {
    let mut name = Cow::Borrowed(&*worktree.chrome.accessible_name);
    if unread {
        name.to_mut().push_str(", unread agent update");
    }
    match activity {
        Some(crate::agent::ActivityState::Working) => name.to_mut().push_str(", agent working"),
        Some(crate::agent::ActivityState::Waiting) => name.to_mut().push_str(", agent needs input"),
        _ => {}
    }
    name
}

fn worktree_view<'a>(
    project_id: ProjectId,
    worktree: &'a SidebarWorktree,
    primary: bool,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    github_ready: bool,
    editors: &'a [SidebarEditor],
    agent_update: bool,
    activity: Option<crate::agent::ActivityState>,
) -> Element<'a, SidebarAction> {
    let chrome = &worktree.chrome;
    let spacing = cosmic::theme::spacing();
    let mut row = widget::row::with_capacity(8)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(Length::Fill)
        .push(worktree_status_icon(worktree, agent_update));
    row = row.push(
        design::body(&*chrome.label)
            .class(if !agent_update {
                theme::Text::Custom(design::muted_text)
            } else {
                theme::Text::default()
            })
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill),
    );
    if let Some(activity) = activity
        && (activity != crate::agent::ActivityState::Waiting || agent_update)
    {
        let (icon, detail, tone) = match activity {
            crate::agent::ActivityState::Working => (
                "agent-working-symbolic",
                "An agent is actively working",
                BadgeTone::Warning,
            ),
            crate::agent::ActivityState::Waiting => (
                "agent-waiting-symbolic",
                "An agent needs input",
                BadgeTone::Danger,
            ),
            _ => unreachable!("only active agent states are displayed"),
        };
        row = row.push(if activity == crate::agent::ActivityState::Working {
            crate::interaction::tooltip(
                crate::ui_controls::activity_spinner(
                    f32::from(ROW_ICON_SIZE),
                    cosmic::theme::active().cosmic().warning.base.into(),
                ),
                detail,
            )
        } else {
            status_icon(icon, detail, tone)
        });
    }
    if let Some(label) = chrome.dirty_badge.as_deref() {
        row = row.push(status_icon(
            "document-edit-symbolic",
            label,
            BadgeTone::Warning,
        ));
    }
    if chrome
        .pull_request
        .as_ref()
        .is_some_and(SidebarPullRequest::has_pending_checks)
        && activity != Some(crate::agent::ActivityState::Working)
    {
        row = row.push(check_indicator(CheckStatus::Pending));
    }
    if primary {
        row = row.push(status_icon(
            "go-home-symbolic",
            "Primary worktree",
            BadgeTone::Neutral,
        ));
    }
    if worktree.detached {
        row = row.push(status_icon(
            "dialog-information-symbolic",
            "Detached HEAD",
            BadgeTone::Neutral,
        ));
    }
    if worktree.locked {
        row = row.push(status_icon(
            "changes-prevent-symbolic",
            "Locked worktree",
            BadgeTone::Neutral,
        ));
    }

    let action = button::custom(row)
        .class(sidebar_button_style(
            selection
                == Some(SidebarSelection::Worktree {
                    project_id,
                    worktree_id: worktree.id,
                }),
        ))
        .id(chrome.open_id.clone())
        .selected(
            selection
                == Some(SidebarSelection::Worktree {
                    project_id,
                    worktree_id: worktree.id,
                }),
        )
        .name(worktree_accessible_name(worktree, agent_update, activity))
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
    context_controls(
        crate::interaction::tooltip(crate::interaction::hover(action), &*chrome.tooltip),
        project_id,
        Some(worktree.id),
        modal_open,
        github_ready,
        editors,
    )
}

fn project_icon(project: &SidebarProject, size: u16) -> Element<'_, SidebarAction> {
    match &project.avatar {
        Some(handle) => cosmic::iced::widget::image(handle.clone())
            .width(size)
            .height(size)
            .into(),
        None => widget::container(widget::icon::from_name("folder-symbolic").size(size))
            .class(theme::Container::custom(move |theme| {
                cosmic::iced::widget::container::Style {
                    icon_color: Some(design::muted_color(theme)),
                    ..Default::default()
                }
            }))
            .into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectCommand {
    Reveal,
    NewTerminal,
    NewWorktree,
    RepositoryUrl,
    OpenEditor(String),
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextCommand<'a>(ProjectId, Option<WorktreeId>, &'a ProjectCommand);

impl widget::menu::Action for ContextCommand<'_> {
    type Message = SidebarAction;

    fn message(&self) -> SidebarAction {
        SidebarAction::ProjectCommand {
            project_id: self.0,
            worktree_id: self.1,
            command: self.2.clone(),
        }
    }
}

fn context_controls<'a>(
    content: impl Into<Element<'a, SidebarAction>>,
    project_id: ProjectId,
    worktree_id: Option<WorktreeId>,
    modal_open: bool,
    github_ready: bool,
    editors: &'a [SidebarEditor],
) -> Element<'a, SidebarAction> {
    use widget::menu::{Item, items};
    let menu = (!modal_open).then(|| {
        let mut commands = vec![
            ("Open folder".to_owned(), None, ProjectCommand::Reveal),
            ("New terminal".to_owned(), None, ProjectCommand::NewTerminal),
            ("New worktree".to_owned(), None, ProjectCommand::NewWorktree),
        ];
        commands.extend(editors.iter().map(|editor| {
            (
                editor.name.clone(),
                Some(editor_icon(editor)),
                ProjectCommand::OpenEditor(editor.id.clone()),
            )
        }));
        if github_ready {
            commands.push((
                "Open repository on GitHub".to_owned(),
                None,
                ProjectCommand::RepositoryUrl,
            ));
        }
        if worktree_id.is_some() {
            commands.push(("Delete worktree…".to_owned(), None, ProjectCommand::Delete));
        }
        let entries = commands
            .iter()
            .map(|(label, icon, command)| {
                Item::Button(
                    label.clone(),
                    icon.clone(),
                    ContextCommand(project_id, worktree_id, command),
                )
            })
            .collect();
        items(&std::collections::HashMap::new(), entries)
    });
    match menu {
        Some(menu) => crate::interaction::context_menu(content, menu),
        None => content.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileContextCommand<'a> {
    Manage {
        key: &'a ActiveWorktreeKey,
        path: &'a RepoPath,
        command: FileCommand,
    },
    Editor {
        key: &'a ActiveWorktreeKey,
        path: &'a RepoPath,
        editor_id: &'a str,
    },
}

impl widget::menu::Action for FileContextCommand<'_> {
    type Message = SidebarAction;

    fn message(&self) -> SidebarAction {
        match self {
            Self::Manage { key, path, command } => SidebarAction::FileCommand {
                key: (*key).clone(),
                path: (*path).clone(),
                command: *command,
            },
            Self::Editor {
                key,
                path,
                editor_id,
            } => SidebarAction::OpenFileInEditor {
                key: (*key).clone(),
                path: (*path).clone(),
                editor_id: (*editor_id).to_owned(),
            },
        }
    }
}

fn file_context_controls<'a>(
    content: Element<'a, SidebarAction>,
    key: &ActiveWorktreeKey,
    row: &FileRow,
    editors: &'a [SidebarEditor],
    file_management: bool,
    blocked: bool,
) -> Element<'a, SidebarAction> {
    if blocked || (!file_management && (row.directory || editors.is_empty())) {
        return content;
    }
    let path = &row.path;
    let mut entries = Vec::with_capacity(4 + editors.len());
    if file_management {
        for (label, icon, command) in [
            ("Rename…", "edit-rename-symbolic", FileCommand::Rename),
            (
                "Duplicate file…",
                "edit-duplicate-symbolic",
                FileCommand::CopyFile,
            ),
            (
                "Copy path",
                "edit-copy-path-symbolic",
                FileCommand::CopyPath,
            ),
            (
                "Copy relative path",
                "edit-copy-relative-path-symbolic",
                FileCommand::CopyRelativePath,
            ),
        ] {
            if row.directory && matches!(command, FileCommand::Rename | FileCommand::CopyFile) {
                continue;
            }
            entries.push(widget::menu::Item::Button(
                label.to_owned(),
                Some(crate::ui_controls::symbolic_icon(icon)),
                FileContextCommand::Manage { key, path, command },
            ));
        }
    }
    if !row.directory {
        entries.extend(editors.iter().map(|editor| {
            widget::menu::Item::Button(
                editor.name.clone(),
                Some(editor_icon(editor)),
                FileContextCommand::Editor {
                    key,
                    path,
                    editor_id: &editor.id,
                },
            )
        }));
    }
    crate::interaction::context_menu(
        content,
        widget::menu::items(&std::collections::HashMap::new(), entries),
    )
}

fn editor_icon(editor: &SidebarEditor) -> icon::Handle {
    if Path::new(&editor.icon).is_absolute() {
        icon::from_path(PathBuf::from(&editor.icon))
    } else {
        icon::from_name(editor.icon.as_str()).handle()
    }
}

fn worktree_status_icon(worktree: &SidebarWorktree, update: bool) -> Element<'_, SidebarAction> {
    let (handle, color) = worktree
        .chrome
        .pull_request
        .as_ref()
        .map(|pr| {
            let (handle, color) = pull_request_visual(pr);
            (handle, Some(color))
        })
        .unwrap_or_else(|| (worktree_icon(), None));
    let glyph = widget::container(widget::icon(handle).size(ROW_ICON_SIZE)).class(
        theme::Container::custom(move |theme| cosmic::iced::widget::container::Style {
            icon_color: Some(color.unwrap_or_else(|| {
                if !update {
                    design::muted_color(theme)
                } else {
                    theme.current_container().on.into()
                }
            })),
            ..Default::default()
        }),
    );
    let dot = widget::container(cosmic::iced::widget::Space::new().width(4).height(4)).class(
        theme::Container::Custom(Box::new(move |theme| {
            cosmic::iced::widget::container::Style {
                background: update.then(|| {
                    cosmic::iced::Background::Color((*theme.cosmic().accent_color()).into())
                }),
                border: cosmic::iced::Border {
                    radius: 2.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        })),
    );
    crate::interaction::tooltip(
        widget::row::with_children(vec![glyph.into(), dot.into()])
            .align_y(Alignment::Center)
            .spacing(0),
        if update {
            "Unread agent update"
        } else {
            "Worktree status"
        },
    )
}

/// The selected worktree's Git surface, independent of the focused card kind.
///
/// A stale snapshot never mounts mutation controls. Main binds the state to
/// the selected worktree and loads its source-control model before rendering.
pub fn git_panel<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    worktree_panel(
        model,
        state,
        width,
        modal_open,
        WorktreePanel::Changes,
        false,
    )
}

/// File navigation for the active worktree. Opens files in workspace columns;
/// this panel contains only the filter, list-mode controls, and file list.
pub fn file_panel<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    worktree_panel(model, state, width, modal_open, WorktreePanel::Files, false)
}

/// Files or Git changes, with a switch between them. Used by the standalone
/// editor, which has no context-inspector tab strip.
pub fn worktree_sidebar<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
) -> Element<'a, SidebarAction> {
    worktree_panel(model, state, width, modal_open, panel, true)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorktreePanel {
    #[default]
    Files,
    Changes,
}

fn worktree_panel<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
    show_panel_switch: bool,
) -> Element<'a, SidebarAction> {
    let width = if width.is_finite() {
        width.clamp(0.0, design::INSPECTOR_MAX_WIDTH)
    } else {
        design::INSPECTOR_DEFAULT_WIDTH
    };
    let content = match bound_source_control(model, state) {
        Some(source) => source_control_panel(
            state,
            source,
            model
                .projects
                .iter()
                .find(|project| project.id == source.key.project_id)
                .and_then(|project| {
                    project
                        .worktrees
                        .iter()
                        .find(|worktree| worktree.id == source.key.worktree_id)
                }),
            &model.external_editors,
            model.file_management && matches!(model.data_state, SidebarDataState::Ready),
            modal_open,
            panel,
            show_panel_switch,
        ),
        None => widget::container(
            widget::column::with_children(vec![
                worktree_icon().icon().size(28).into(),
                design::heading(if state.active_worktree().is_some() {
                    "Loading worktree"
                } else {
                    "No worktree selected"
                })
                .into(),
                design::caption(if state.active_worktree().is_some() {
                    "Reading files, working-tree changes, and branch history."
                } else {
                    "Choose a worktree in Projects, or open a repository."
                })
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
                button::standard("Open repository")
                    .on_press_maybe(gated(SidebarAction::OpenRepositoryPicker, modal_open))
                    .into(),
            ])
            .spacing(8)
            .width(Length::Fill),
        )
        .padding(12)
        .width(Length::Fill)
        .into(),
    };
    widget::container(content)
        .class(theme::Container::Background)
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .into()
}

fn bound_source_control<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
) -> Option<&'a SourceControlModel> {
    let key = state.active_worktree()?;
    model
        .source_control
        .as_ref()
        .filter(|source| &source.key == key)
}

/// Shared worktree identity and loading slots around file or Git navigation.
fn source_control_panel<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    worktree: Option<&'a SidebarWorktree>,
    editors: &'a [SidebarEditor],
    file_management: bool,
    modal_open: bool,
    panel: WorktreePanel,
    show_panel_switch: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let chrome = &source.chrome;
    let key = &source.key;
    let mut header = widget::row::with_children(vec![
        widget::container(worktree_icon().icon().size(ROW_ICON_SIZE))
            .center_x(Length::Fixed(ROW_ICON_WIDTH))
            .into(),
        widget::container(crate::interaction::tooltip(
            design::heading(worktree.map_or(&*chrome.subheading, |row| &*row.chrome.label))
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill),
            worktree.map_or(&*chrome.subheading, |row| &*row.chrome.tooltip),
        ))
        .width(Length::Fill)
        .into(),
    ]);
    if show_panel_switch {
        let git = source.offers_changes();
        let files = crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("folder-symbolic"),
            "Files",
            matches!(panel, WorktreePanel::Files),
            !modal_open,
            gated(
                SidebarAction::SetWorktreePanel {
                    panel: WorktreePanel::Files,
                },
                modal_open,
            ),
        );
        let changes = crate::ui_controls::icon_control(
            crate::ui_controls::symbolic_icon("document-edit-symbolic"),
            if git {
                "Changes"
            } else {
                "Open a Git repository to review changes"
            },
            matches!(panel, WorktreePanel::Changes),
            !modal_open && git,
            gated(
                SidebarAction::SetWorktreePanel {
                    panel: WorktreePanel::Changes,
                },
                modal_open || !git,
            ),
        );
        let changes: Element<'_, SidebarAction> = if git && source.has_changes() {
            changes
                .icon_color(cosmic::theme::active().cosmic().warning.base.into())
                .into()
        } else {
            changes.into()
        };
        header = header.push(crate::ui_controls::segmented_controls(vec![
            files.into(),
            changes,
        ]));
    }
    if matches!(panel, WorktreePanel::Files) {
        header = header.push(
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon("system-search-symbolic"),
                if state.files_filter_open {
                    "Hide file filter"
                } else {
                    "Filter files"
                },
                state.files_filter_open,
                !modal_open,
                gated(
                    SidebarAction::ToggleFilesFilter { key: key.clone() },
                    modal_open,
                ),
            )
            .id(chrome.files_filter_id.clone()),
        );
    }
    // File browsing is always a tree, so only change review offers the
    // tree/flat switch: the flat list exists for viewing diffs, not files.
    if matches!(panel, WorktreePanel::Changes) {
        header = header.push(list_mode_controls(key, state.change_list_mode, modal_open));
    }
    let header = header
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(design::HEADER_HEIGHT)
        .width(Length::Fill);

    // Both notices are permanent slots. The failure arm below this ends in a
    // named `Try again` button, and iced reconciles a named child at the index
    // its name held last frame while writing the positionally unmatched
    // children over those same indices: a notice arriving above the button left
    // the reconciled child list one entry short, so the button had no tree at
    // all and disappeared from the one frame that was asking for a press.
    let mut notices = widget::column::with_capacity(4).spacing(spacing.space_xxxs);
    notices = notices.push(slot(source.progress_label().map(|label| {
        widget::row::with_children(vec![
            crate::ui_controls::activity_spinner(
                14.0,
                cosmic::theme::active().cosmic().warning.base.into(),
            ),
            design::caption(label).into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxs)
        .into()
    })));
    notices = notices.push(slot(source.error.as_deref().map(|error| {
        design::caption(error)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into()
    })));

    match &source.data_state {
        SidebarDataState::Loading => {
            notices = notices.push(design::caption(if matches!(panel, WorktreePanel::Files) {
                "Loading files…"
            } else {
                "Loading source control…"
            }));
            return worktree_panel_chrome(header.into(), notices.into(), None);
        }
        SidebarDataState::Error(message) => {
            notices = notices
                .push(
                    design::caption(format!("Couldn’t read this worktree. {message}"))
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
            return worktree_panel_chrome(header.into(), notices.into(), None);
        }
        SidebarDataState::Ready => {}
    }

    if matches!(panel, WorktreePanel::Files) {
        return worktree_panel_chrome(
            header.into(),
            notices.into(),
            Some(files_section(
                state,
                source,
                modal_open,
                editors,
                file_management,
            )),
        );
    }

    let mut sections = widget::column::with_capacity(3).spacing(spacing.space_xxxs);
    sections = sections.push(changes_section(state, source, modal_open));
    sections = sections.push(branch_changes_section(state, source, modal_open));
    sections = sections.push(panel_section(
        key,
        PanelSection::History,
        chrome.section_id(PanelSection::History).clone(),
        source.history_count().map(RowCount::exact),
        state.section_expanded(PanelSection::History),
        modal_open,
        || history_section(source, modal_open),
    ));

    worktree_panel_chrome(
        header.into(),
        notices.into(),
        Some(
            widget::scrollable(
                widget::container(sections)
                    .padding([spacing.space_xxxs, spacing.space_xxs])
                    .width(Length::Fill),
            )
            .direction(sidebar_scroll_direction())
            .class(cosmic::style::iced::Scrollable::Minimal)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        ),
    )
}

/// Worktree title, full-width rule, then padded notices and the panel body.
///
/// The projects sidebar already pads its header without padding the rule
/// beneath it. Wrapping both in one inset container left a gap between the
/// rule and the pane's vertical borders, so the files and Git headers looked
/// disconnected from the splitter they share with the editor tab strip.
fn worktree_panel_chrome<'a>(
    header: Element<'a, SidebarAction>,
    notices: Element<'a, SidebarAction>,
    body: Option<Element<'a, SidebarAction>>,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut chrome = widget::column::with_capacity(3).spacing(spacing.space_xxxs);
    chrome = chrome.push(
        widget::container(header)
            .padding([0, spacing.space_xxs])
            .width(Length::Fill),
    );
    chrome = chrome.push(widget::divider::horizontal::default());
    chrome = chrome.push(
        widget::container(notices)
            .padding([0, spacing.space_xxs])
            .width(Length::Fill),
    );
    match body {
        Some(body) => widget::column::with_children(vec![chrome.into(), body])
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        None => chrome.width(Length::Fill).into(),
    }
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
    /// `None` while a dialog is on screen; see [`gated`].
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
            design::heading(parts.title)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs)
        .height(Length::Fill),
    )
    .class(sidebar_button_style(false))
    .id(id)
    .name(parts.action_name.clone())
    .padding([0, spacing.space_xxxs])
    .height(Length::Fixed(PANEL_ROW_HEIGHT))
    .width(Length::Fill)
    .on_press_maybe(parts.toggle);
    let header = crate::interaction::tooltip(crate::interaction::hover(row), parts.action_name);
    let mut column = widget::column::with_capacity(2).spacing(0).push(header);
    if expanded {
        column = column.push(widget::container(body()).padding([0, 0, spacing.space_xxs, 0]));
    }
    column.into()
}

/// The tracked-file tree, its filter, and whatever the filter left out.
///
/// Four children, always: filter and modes, the notice, scrollable rows, and
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
    editors: &'a [SidebarEditor],
    file_management: bool,
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
    // File browsing is always a tree: the flat list exists only for
    // diff/change review. A text filter still flattens to matching files.
    let flat = !folded.is_empty();
    let (visible, hidden, notice): (Vec<&FileRow>, usize, Option<&str>) = match &source.file_search
    {
        FileSearch::Pending { query }
            if fold(query.trim()).as_ref() == folded.as_ref() && !folded.is_empty() =>
        {
            let (visible, hidden) = visible_file_rows(
                &source.files,
                &state.expanded_directories,
                &folded,
                true,
                false,
            );
            (visible, hidden, Some("Searching…"))
        }
        FileSearch::Ready {
            query,
            files,
            hidden,
        } if fold(query.trim()).as_ref() == folded.as_ref() && !folded.is_empty() => {
            let notice = files.is_empty().then_some("No files match this filter.");
            (files.iter().collect(), *hidden, notice)
        }
        FileSearch::Failed { query, message }
            if fold(query.trim()).as_ref() == folded.as_ref() && !folded.is_empty() =>
        {
            (Vec::new(), 0, Some(message.as_str()))
        }
        _ => {
            let (visible, hidden) = visible_file_rows(
                &source.files,
                &state.expanded_directories,
                &folded,
                flat,
                false,
            );
            let notice = if source.files.is_empty() {
                Some("This worktree has no tracked files.")
            } else if visible.is_empty() {
                Some("No files match this filter.")
            } else {
                None
            };
            (visible, hidden, notice)
        }
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
        let expanded = state.expanded_directories.contains(&row.path);
        let file_icon = if row.directory {
            crate::file_icon::folder(expanded)
        } else {
            crate::file_icon::icon(row.path.as_path())
        };
        let selected = matches!(
            state.panel_selection.as_ref(),
            Some(PanelSelection::File(path)) if *path == row.path
        );
        let identity = panel_row(
            &*chrome.accessible_name,
            if flat { 0 } else { row.depth },
            widget::row::with_children(vec![
                widget::container(file_icon)
                    .center_x(Length::Fixed(ROW_ICON_WIDTH))
                    .into(),
                design::body(if flat {
                    row.path.display()
                } else {
                    Cow::Borrowed(&*chrome.label)
                })
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(spacing.space_xxxs),
            Some(action),
            selected,
            modal_open,
        );
        rows = rows.push(file_context_controls(
            identity,
            key,
            row,
            editors,
            file_management,
            modal_open || source.reloading || source.busy(),
        ));
    }
    let list = widget::column::with_children(vec![
        rows.into(),
        slot((hidden > 0).then(|| {
            design::caption(format!("{hidden} more files not shown — narrow the filter"))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into()
        })),
    ]);
    widget::column::with_children(vec![
        widget::container(
            widget::column::with_children(vec![
                slot(state.files_filter_open.then(|| filter.into())),
                slot(notice.map(|notice| design::caption(notice).into())),
            ])
            .spacing(spacing.space_xxxs),
        )
        .padding([0, spacing.space_xxs])
        .width(Length::Fill)
        .into(),
        widget::scrollable(
            widget::container(list)
                .padding([spacing.space_xxxs, spacing.space_xxs])
                .width(Length::Fill),
        )
        .direction(sidebar_scroll_direction())
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
    ])
    .height(Length::Fill)
    .into()
}

/// The file rows a frame renders, and how many further rows the cap hid.
///
/// Flat mode and queries show file paths without directory rows. Tree mode
/// shows only the children of expanded directories: everything starts
/// collapsed, so a whole project tree renders as its top level until the user
/// opens folders. Diff and change trees pass `expand_all` to keep every
/// change visible for review regardless of file-tree expansion.
///
/// At most [`MAX_VISIBLE_FILE_ROWS`] references are ever collected. This runs
/// on every view rebuild, including rebuilds caused by terminal output the
/// panel has nothing to do with, so a worktree of a hundred thousand tracked
/// paths must not make a frame fill - and drop - a hundred-thousand-entry
/// vector to render four hundred rows. Counting the remainder allocates
/// nothing.
fn visible_file_rows<'a>(
    rows: &'a [FileRow],
    expanded: &BTreeSet<RepoPath>,
    folded_query: &str,
    flat: bool,
    expand_all: bool,
) -> (Vec<&'a FileRow>, usize) {
    let mut visible: Vec<&'a FileRow> = Vec::with_capacity(rows.len().min(MAX_VISIBLE_FILE_ROWS));
    let mut hidden = 0usize;
    if flat {
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
        if row.directory && !expand_all && !expanded.contains(&row.path) {
            skip_below = Some(row.depth);
        }
    }
    (visible, hidden)
}

fn list_mode_controls<'a>(
    key: &ActiveWorktreeKey,
    selected: FileListMode,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    crate::ui_controls::segmented_controls(
        [
            (
                "Tree file list",
                "view-list-tree-symbolic",
                FileListMode::Tree,
            ),
            ("Flat file list", "view-list-symbolic", FileListMode::Flat),
        ]
        .into_iter()
        .map(|(label, icon, mode)| {
            let action = SidebarAction::SetChangeListMode {
                key: key.clone(),
                mode,
            };
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon(icon),
                label,
                selected == mode,
                !modal_open,
                gated(action, modal_open),
            )
            .into()
        })
        .collect(),
    )
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
        column = column.push(design::caption("No uncommitted changes."));
    }
    for group in source.groups() {
        if group.is_empty() {
            continue;
        }
        column = column.push(change_group_view(state, source, group, modal_open));
    }
    column = column.push(widget::divider::horizontal::default());
    column = column.push(changes_footer(state, source, modal_open));
    column.into()
}

fn branch_changes_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let mut column = widget::column::with_capacity(4).spacing(spacing.space_xxxs);
    let branch = match &source.branch_changes {
        BranchChanges::Ready(branch) => branch,
        other => {
            column = column.push(design::heading("Branch changes"));
            return match other {
                BranchChanges::Unavailable(reason) => column.push(design::caption(*reason)).into(),
                BranchChanges::Loading(base_ref) => column
                    .push(
                        widget::row::with_children(vec![
                            crate::ui_controls::activity_spinner(
                                14.0,
                                cosmic::theme::active().cosmic().warning.base.into(),
                            ),
                            design::caption(format!("Comparing with {}…", base_ref.short_name()))
                                .into(),
                        ])
                        .spacing(spacing.space_xxs)
                        .align_y(Alignment::Center),
                    )
                    .into(),
                BranchChanges::Error { base_ref, message } => column
                    .push(
                        design::caption(format!(
                            "Could not compare with {}: {message}",
                            base_ref.short_name()
                        ))
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                    )
                    .push(button::standard("Try again").on_press_maybe(gated(
                        SidebarAction::RefreshWorktree {
                            key: source.key.clone(),
                        },
                        modal_open,
                    )))
                    .into(),
                BranchChanges::Ready(_) => unreachable!(),
            };
        }
    };
    column = column
        .push(design::heading(&*branch.title))
        .push(design::caption(&*branch.base_label));
    if branch.rows.is_empty() {
        return column
            .push(design::caption(
                "No committed changes relative to this base.",
            ))
            .into();
    }
    let flat = state.change_list_mode == FileListMode::Flat;
    // Branch review always expands: every changed path stays visible no
    // matter which file-tree folders are open.
    let (tree, hidden) =
        visible_file_rows(&branch.tree, &state.expanded_directories, "", flat, true);
    let mut rows = widget::column::with_capacity(tree.len()).spacing(0);
    for file in tree {
        if file.directory {
            // An inert hierarchy label: branch review always expands, so
            // there is no toggle to offer and no file-tree state to borrow.
            rows = rows.push(panel_row(
                &*file.chrome.label,
                file.depth,
                widget::row::with_children(vec![
                    crate::file_icon::folder(true).into(),
                    design::body(&*file.chrome.label).into(),
                ])
                .spacing(4)
                .align_y(Alignment::Center),
                None,
                false,
                modal_open,
            ));
            continue;
        }
        let Ok(index) = branch
            .rows
            .binary_search_by(|row| row.entry.path.cmp(&file.path))
        else {
            continue;
        };
        let row = &branch.rows[index];
        let tone = match row.entry.kind {
            ChangeKind::Added => BadgeTone::Success,
            ChangeKind::Deleted => BadgeTone::Danger,
            _ => BadgeTone::Warning,
        };
        rows = rows.push(panel_row(
            &*row.label, if flat { 0 } else { file.depth },
            widget::row::with_children(vec![
                crate::file_icon::icon(row.entry.path.as_path()).into(),
                design::body(if !flat && row.entry.old_path.is_none() {
                    row.entry.path.file_name()
                } else { Cow::Borrowed(&*row.label) })
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::Start(EllipsizeHeightLimit::Lines(1)))
                    .width(Length::Fill).into(),
                semantic_caption(&*row.status, tone),
                design::caption(&*row.counts).into(),
            ]).spacing(spacing.space_xxxs).align_y(Alignment::Center),
            Some(SidebarAction::OpenBranchFile {
                key: source.key.clone(),
                path: row.entry.path.clone(),
                old_path: row.entry.old_path.clone(),
                base_ref: branch.base_ref.clone(),
                head: branch.head.clone(),
                merge_base: branch.merge_base.clone(),
            }),
            matches!(&state.panel_selection, Some(PanelSelection::Branch(path)) if path == &row.entry.path),
            modal_open,
        ));
    }
    column = column.push(rows);
    let hidden = hidden + branch.rows.len().saturating_sub(MAX_VISIBLE_CHANGE_ROWS);
    column
        .push(slot((hidden > 0).then(|| {
            design::caption(format!("{hidden} more branch rows not shown")).into()
        })))
        .into()
}

fn change_group_view<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    group: &'a ChangeGroup,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let flat = state.change_list_mode == FileListMode::Flat;
    // Uncommitted-change review always expands, like branch review above.
    let (tree, tree_hidden) =
        visible_file_rows(&group.tree, &state.expanded_directories, "", flat, true);
    let visible: Vec<&ChangeRow> = tree
        .iter()
        .filter(|row| !row.directory)
        .filter_map(|file| group.visible_path(&file.path))
        .collect();
    let header = widget::container(design::heading(&*group.chrome.title))
        .width(Length::Fill)
        .height(PANEL_ROW_HEIGHT)
        .center_y(PANEL_ROW_HEIGHT);
    let commands = group_commands(source, group, &visible, modal_open);
    let actions = widget::container(widget::row::with_children(
        commands.into_iter().map(IconCommand::into_element),
    ))
    .align_right(Length::Fill)
    .height(PANEL_ROW_HEIGHT);
    let mut column = widget::column::with_capacity(tree.len() + 2)
        .spacing(0)
        .push(RowInteraction::actions(header.into(), actions.into()));
    for file in tree {
        if file.directory {
            // An inert hierarchy label, like branch review above.
            column = column.push(panel_row(
                &*file.chrome.label,
                file.depth,
                widget::row::with_children(vec![
                    crate::file_icon::folder(true).into(),
                    design::body(&*file.chrome.label).into(),
                ])
                .spacing(4)
                .align_y(Alignment::Center),
                None,
                false,
                modal_open,
            ));
        } else if let Some(row) = group.visible_path(&file.path) {
            column = column.push(change_row_view(
                state,
                source,
                group.area,
                row,
                modal_open,
                if flat { 0 } else { file.depth },
                !flat,
            ));
        }
    }
    let hidden = group.hidden() + tree_hidden;
    if hidden > 0 {
        column = column.push(design::caption(format!(
            "{hidden} more rows not shown; group actions apply only to visible files"
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
    depth: usize,
    tree: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let chrome = &row.chrome;
    let selected = matches!(
        state.panel_selection.as_ref(),
        Some(PanelSelection::Change { path, area: selected })
            if *path == row.entry.path && *selected == area
    );
    let tone = if row.entry.conflict.is_some() {
        BadgeTone::Danger
    } else {
        match row.entry.kind {
            ChangeKind::Added | ChangeKind::Untracked => BadgeTone::Success,
            ChangeKind::Deleted | ChangeKind::Unmerged => BadgeTone::Danger,
            _ => BadgeTone::Warning,
        }
    };
    let mut status = widget::row::with_capacity(3)
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs);
    status = status.push(
        if row.entry.conflict.is_none()
            && matches!(row.entry.kind, ChangeKind::Added | ChangeKind::Untracked)
        {
            status_icon("document-new-symbolic", "Added file", tone)
        } else {
            semantic_caption(
                if row.entry.conflict.is_some() {
                    "U"
                } else {
                    &chrome.status
                },
                tone,
            )
        },
    );
    if let Some(split) = chrome.counts_split {
        status = status
            .push(semantic_caption(
                &chrome.counts[..split],
                BadgeTone::Success,
            ))
            .push(semantic_caption(
                &chrome.counts[split + 1..],
                BadgeTone::Danger,
            ));
    } else if !chrome.counts.is_empty() {
        status = status.push(semantic_caption(
            &*chrome.counts,
            if chrome.counts.starts_with('+') {
                BadgeTone::Success
            } else {
                BadgeTone::Danger
            },
        ));
    }
    let identity = panel_row(
        &*chrome.accessible_name,
        depth,
        widget::row::with_children(vec![
            widget::container(crate::file_icon::icon(row.entry.path.as_path()))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            design::body(if tree && row.entry.old_path.is_none() {
                row.entry.path.file_name()
            } else {
                Cow::Borrowed(&*chrome.label)
            })
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(if chrome.ellipsize_start {
                cosmic::iced::widget::text::Ellipsize::Start(EllipsizeHeightLimit::Lines(1))
            } else {
                cosmic::iced::widget::text::Ellipsize::End(EllipsizeHeightLimit::Lines(1))
            })
            .width(Length::Fill)
            .into(),
            widget::container(status)
                .align_right(CHANGE_STATUS_WIDTH)
                .center_y(Length::Fill)
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(spacing.space_xxxs),
        Some(SidebarAction::OpenChange {
            key: source.key.clone(),
            path: row.entry.path.clone(),
            area,
        }),
        selected,
        modal_open,
    );
    let commands = row_commands(source, area, row, modal_open);
    if commands.is_empty() {
        return identity;
    }
    let actions = widget::container(
        widget::row::with_children(commands.into_iter().map(IconCommand::into_element))
            .align_y(Alignment::Center)
            .spacing(0),
    )
    .align_right(CHANGE_STATUS_WIDTH)
    .height(PANEL_ROW_HEIGHT)
    .class(theme::Container::Custom(Box::new(move |theme| {
        let base: cosmic::iced::Color = theme.current_container().base.into();
        let foreground: cosmic::iced::Color = theme.current_container().on.into();
        let alpha = if selected { 0.16 } else { 0.06 };
        cosmic::iced::widget::container::Style {
            background: Some(cosmic::iced::Background::Color(cosmic::iced::Color {
                r: foreground.r * alpha + base.r * (1.0 - alpha),
                g: foreground.g * alpha + base.g * (1.0 - alpha),
                b: foreground.b * alpha + base.b * (1.0 - alpha),
                a: 1.0,
            })),
            ..Default::default()
        }
    })));
    RowInteraction::actions(
        identity,
        widget::container(actions)
            .padding([0, spacing.space_xxxs])
            .align_right(Length::Fill)
            .into(),
    )
}

const CHANGE_STATUS_WIDTH: f32 = 100.0;

fn semantic_caption<'a>(
    label: impl Into<Cow<'a, str>> + 'a,
    tone: BadgeTone,
) -> Element<'a, SidebarAction> {
    design::caption(label)
        .wrapping(cosmic::iced::widget::text::Wrapping::None)
        .class(theme::Text::Custom(match tone {
            BadgeTone::Success => |theme| cosmic::iced::widget::text::Style {
                color: Some(theme.cosmic().success.base.into()),
                ..Default::default()
            },
            BadgeTone::Warning => |theme| cosmic::iced::widget::text::Style {
                color: Some(theme.cosmic().warning.base.into()),
                ..Default::default()
            },
            BadgeTone::Danger => |theme| cosmic::iced::widget::text::Style {
                color: Some(theme.cosmic().destructive.base.into()),
                ..Default::default()
            },
            BadgeTone::Neutral => |theme| cosmic::iced::widget::text::Style {
                color: Some(theme.current_container().on.into()),
                ..Default::default()
            },
        }))
        .into()
}

/// One icon command a change header or a change row offers, derived rather
/// than pushed inline.
///
/// Derived independently of rendering so group and row mutations share the
/// same path-scoping and modal gating rules.
struct IconCommand<'a> {
    icon: &'static str,
    name: &'a str,
    /// `None` while a dialog is on screen; see [`gated`].
    action: Option<SidebarAction>,
    id: Id,
}

impl<'a> IconCommand<'a> {
    fn into_element(self) -> Element<'a, SidebarAction> {
        panel_icon_action(self.icon, self.name, self.action, self.id)
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
    visible: &[&ChangeRow],
    modal_open: bool,
) -> Vec<IconCommand<'a>> {
    let key = &source.key;
    let chrome = &group.chrome;
    if source.busy() || visible.is_empty() {
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
                    selection: visible.iter().map(|row| row.selection()).collect(),
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
                    selection: visible.iter().map(|row| row.selection()).collect(),
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
                    request: discard_request(
                        key,
                        group.area,
                        visible.iter().map(|row| row.selection()).collect(),
                        group,
                    ),
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
    _group: &ChangeGroup,
) -> DiscardRequest {
    match area {
        // Untracked paths exist nowhere but the worktree, so discarding them
        // deletes files rather than restoring them from the index.
        ChangeArea::Untracked => DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: Vec::new(),
            untracked: selection
                .into_iter()
                .map(|selection| selection.path)
                .collect(),
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
        .width(Length::Fill)
        .padding([6, 8]);
    if !modal_open {
        commit_input = commit_input.on_input(move |message| SidebarAction::CommitMessageChanged {
            key: input_key.clone(),
            message,
        });
    }

    let mut commit = button::custom(design::caption("Commit"))
        .class(sidebar_button_style(true))
        .id(chrome.commit_id.clone())
        .padding([0, 8])
        .height(PANEL_ROW_HEIGHT);
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
            let mut push = button::custom(design::caption("Push"))
                .class(sidebar_button_style(false))
                .id(chrome.push_id.clone())
                .padding([0, 8])
                .height(PANEL_ROW_HEIGHT);
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
            let mut publish = button::custom(design::caption("Publish Branch"))
                .class(sidebar_button_style(false))
                .id(chrome.push_id.clone())
                .padding([0, 8])
                .height(PANEL_ROW_HEIGHT);
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
            buttons = buttons.push(design::caption(
                "Upstream could not be read — nothing to push",
            ));
        }
        (None, _) => {
            // A branch this panel cannot name is not one it may invent a
            // mutation for, and each reason is stated as itself.
            buttons = buttons.push(design::caption(source.no_branch_reason()));
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
            button::custom(design::caption(remote.as_str()))
                .class(sidebar_button_style(publish.remote == *remote))
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
    let mut confirm = button::standard("Publish")
        .class(sidebar_button_style(false))
        .id(chrome.publish_id.clone());
    if let Some(push) = publish
        .push_action()
        .filter(|_| blocked.is_none() && !modal_open)
    {
        confirm = confirm.on_press(push);
    }
    widget::column::with_children(vec![
        design::caption("Publish to").into(),
        remotes.into(),
        branch_input.into(),
        // A permanent slot: the reason comes and goes as the picker is filled
        // in, and the named controls below it must not move when it does.
        slot(blocked.map(|reason| {
            design::caption(reason)
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
        design::caption(notice)
            .wrapping(cosmic::iced::widget::text::Wrapping::Word)
            .into()
    })));
    let mut rows = widget::column::with_capacity(source.history.len()).spacing(0);
    for row in &source.history {
        let chrome = &row.chrome;
        rows = rows.push(
            widget::container(
                widget::row::with_children(vec![
                    widget::container(icon::from_name("vcs-commit-symbolic").size(ROW_ICON_SIZE))
                        .center_x(Length::Fixed(ROW_ICON_WIDTH))
                        .into(),
                    widget::column::with_children(vec![
                        design::body(&*chrome.subject)
                            .wrapping(cosmic::iced::widget::text::Wrapping::None)
                            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                                EllipsizeHeightLimit::Lines(1),
                            ))
                            .width(Length::Fill)
                            .into(),
                        design::caption(&*chrome.metadata)
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
                .spacing(spacing.space_xxxs)
                .align_y(Alignment::Center),
            )
            .padding([spacing.space_xxxs, spacing.space_xxxs])
            .width(Length::Fill),
        );
    }
    column = column.push(rows);
    column = column.push(slot(source.can_load_history().then(|| {
        // The first page is not "more": labelling it that way while the list is
        // empty hides that nothing has been asked for yet.
        button::custom(design::caption(if source.history.is_empty() {
            format!("Load {HISTORY_PAGE_SIZE} commits")
        } else {
            format!("Load {HISTORY_PAGE_SIZE} more commits")
        }))
        .class(sidebar_button_style(false))
        .padding([0, spacing.space_xxxs])
        .height(PANEL_ROW_HEIGHT)
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
    // `None` renders an inert hierarchy label: review trees always expand,
    // so their folders offer no toggle and must not borrow the file tree's.
    action: Option<SidebarAction>,
    selected: bool,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    let indent = u16::try_from(depth).unwrap_or(u16::MAX).saturating_mul(12);
    crate::interaction::hover(
        button::custom(
            widget::row::with_children(vec![
                cosmic::iced::widget::Space::new()
                    .width(Length::Fixed(f32::from(indent)))
                    .into(),
                content.into(),
            ])
            .align_y(Alignment::Center)
            .spacing(0)
            .height(Length::Fill),
        )
        .class(sidebar_button_style(selected))
        .name(name)
        .selected(selected)
        .padding([0, spacing.space_xxxs])
        .height(Length::Fixed(PANEL_ROW_HEIGHT))
        .width(Length::Fill)
        .on_press_maybe(action.and_then(|action| gated(action, modal_open))),
    )
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
    crate::app::dialog_ui::surface(
        widget::column::with_children(vec![
            widget::text::title3("Discard changes?").into(),
            widget::text::body(detail.join(". "))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            design::caption("Discarded work is not recoverable from Git.").into(),
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
        520.0,
    )
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

/// Neutral text, transparent rows, and restrained pointer/keyboard feedback.
pub(crate) fn sidebar_button_style(selected: bool) -> theme::Button {
    fn appearance(
        theme: &cosmic::Theme,
        focused: bool,
        selected: bool,
        alpha: f32,
        disabled: bool,
    ) -> button::Style {
        let mut style = button::Style::new();
        let foreground: cosmic::iced::Color = theme.current_container().on.into();
        let mut text = foreground;
        if disabled {
            text.a *= 0.5;
        }
        style.text_color = Some(text);
        style.icon_color = Some(text);
        style.border_radius = theme.cosmic().corner_radii.radius_s.into();
        if selected || focused || alpha > 0.0 {
            let mut surface = foreground;
            surface.a = if disabled {
                0.04
            } else {
                (if selected { 0.10 } else { 0.0 }) + alpha + if focused { 0.06 } else { 0.0 }
            };
            style.background = Some(cosmic::iced::Background::Color(surface));
        }
        style
    }
    // Noncapturing closures keep the boxed callbacks allocation-free for the
    // hundreds of file rows rebuilt on terminal wakes.
    fn class<const SELECTED: bool>() -> theme::Button {
        theme::Button::Custom {
            active: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.0, false)),
            disabled: Box::new(|theme| appearance(theme, false, SELECTED, 0.0, true)),
            hovered: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.06, false)),
            pressed: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.1, false)),
        }
    }
    if selected {
        class::<true>()
    } else {
        class::<false>()
    }
}

/// Compact source-control commands share the row height and hover geometry.
fn panel_icon_action<'a>(
    icon_name: &'static str,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    crate::interaction::tooltip(
        button::custom(
            widget::container(
                icon::icon(crate::ui_controls::symbolic_icon(icon_name)).size(ROW_ICON_SIZE),
            )
            .center(Length::Fill),
        )
        .class(sidebar_button_style(false))
        .id(id)
        .name(label.clone())
        .padding(0)
        .width(PANEL_ROW_HEIGHT)
        .height(PANEL_ROW_HEIGHT)
        .on_press_maybe(action),
        label,
    )
}

/// One icon command.
fn icon_action<'a>(
    icon_name: &'static str,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    crate::interaction::tooltip(
        button::icon(crate::ui_controls::symbolic_icon(icon_name))
            .class(sidebar_button_style(false))
            .id(id)
            .name(label.clone())
            .width(ICON_CONTROL_SIZE)
            .height(ICON_CONTROL_SIZE)
            .on_press_maybe(action),
        label,
    )
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
    Success,
    Warning,
    Danger,
}

fn badge<'a>(label: impl Into<Cow<'a, str>> + 'a, tone: BadgeTone) -> Element<'a, SidebarAction> {
    let spacing = cosmic::theme::spacing();
    widget::container(
        design::caption(label)
            .class(theme::Text::default())
            .wrapping(cosmic::iced::widget::text::Wrapping::None),
    )
    .class(theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let component = match tone {
            BadgeTone::Neutral => &cosmic.secondary(false).component,
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

fn status_icon<'a>(
    icon_name: &'static str,
    label: &'a str,
    tone: BadgeTone,
) -> Element<'a, SidebarAction> {
    let content = widget::container(
        widget::icon(crate::ui_controls::symbolic_icon(icon_name)).size(ROW_ICON_SIZE),
    )
    .class(theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        cosmic::iced::widget::container::Style {
            icon_color: Some(match tone {
                BadgeTone::Success => cosmic.success.base.into(),
                BadgeTone::Warning => cosmic.warning.base.into(),
                BadgeTone::Danger => cosmic.destructive.base.into(),
                _ => theme.current_container().component.on_disabled.into(),
            }),
            ..Default::default()
        }
    }));
    crate::interaction::tooltip(content, label)
}

fn check_indicator(status: CheckStatus) -> Element<'static, SidebarAction> {
    if status == CheckStatus::Pending {
        return crate::interaction::tooltip(
            crate::ui_controls::activity_spinner(
                f32::from(ROW_ICON_SIZE),
                cosmic::theme::active().cosmic().warning.base.into(),
            ),
            "CI checks pending or running",
        );
    }
    status_icon(
        match status {
            CheckStatus::Passing => "emblem-ok-symbolic",
            CheckStatus::Failing => "dialog-error-symbolic",
            _ => "dialog-information-symbolic",
        },
        check_description(status),
        check_tone(status),
    )
}

#[derive(Debug)]
struct FilteredProject<'a> {
    project: &'a SidebarProject,
    worktrees: Vec<&'a SidebarWorktree>,
    threads: Vec<&'a SidebarThread>,
    editors: &'a [SidebarEditor],
    state: &'a SidebarState,
}

/// A compact query applies only while its search popover is available, so
/// hiding the popover cannot leave an invisible query filtering the rail.
fn filtered_projects<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Vec<FilteredProject<'a>> {
    let query = if compact && !state.controls_expanded {
        String::new()
    } else {
        state.query.trim().to_ascii_lowercase()
    };
    model
        .projects
        .iter()
        .filter_map(|project| {
            let project_matches = query.is_empty() || project.matches(&query);
            let threads = project
                .threads
                .iter()
                .filter(|thread| {
                    (project_matches || thread.folded_title.contains(&query))
                        && match state.filter {
                            SidebarFilter::All => true,
                            SidebarFilter::Attention => thread.unread,
                            SidebarFilter::PullRequests => false,
                        }
                })
                .collect::<Vec<_>>();
            let mut worktrees = project
                .worktrees
                .iter()
                .filter(|worktree| match state.filter {
                    SidebarFilter::All => true,
                    SidebarFilter::PullRequests => {
                        worktree.pull_request_state.is_some()
                            || matching_pull_request(worktree, project.display_pull_requests())
                                .is_some()
                    }
                    SidebarFilter::Attention => state.has_agent_update(project.id, worktree.id),
                })
                .filter(|worktree| project_matches || worktree.matches(&query))
                .collect::<Vec<_>>();
            worktrees.sort_by(|left, right| {
                let primary =
                    |row: &SidebarWorktree| is_primary_worktree(&project.repository, &row.path);
                primary(right)
                    .cmp(&primary(left))
                    .then_with(|| {
                        use crate::settings::WorktreeSort;
                        match state.worktree_sort {
                            WorktreeSort::Alphabetical => {
                                left.chrome.label.cmp(&right.chrome.label)
                            }
                            order => match (left.created_at_unix_ms, right.created_at_unix_ms) {
                                (Some(left), Some(right)) => {
                                    if order == WorktreeSort::Newest {
                                        right.cmp(&left)
                                    } else {
                                        left.cmp(&right)
                                    }
                                }
                                (Some(_), None) => std::cmp::Ordering::Less,
                                (None, Some(_)) => std::cmp::Ordering::Greater,
                                (None, None) => std::cmp::Ordering::Equal,
                            },
                        }
                    })
                    .then_with(|| left.path.cmp(&right.path))
            });
            let visible = match state.filter {
                SidebarFilter::All => {
                    project_matches || !worktrees.is_empty() || !threads.is_empty()
                }
                SidebarFilter::Attention | SidebarFilter::PullRequests => {
                    !worktrees.is_empty() || !threads.is_empty()
                }
            };
            visible.then_some(FilteredProject {
                project,
                worktrees,
                threads,
                editors: &model.external_editors,
                state,
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

/// Event decorators keep native buttons, focus operations, menus and accessible
/// names intact instead of replacing rows with mouse-only hit targets.
mod row_interaction {
    use super::{
        ICON_CONTROL_SIZE, PROJECT_HEADER_HEIGHT, ProjectId, SidebarAction, SidebarFilter,
    };
    use cosmic::iced::advanced::Renderer as _;
    use cosmic::iced::{
        Event, Length, Point, Rectangle, Size, Vector,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
            widget::{Id, Operation, Tree, operation, tree},
        },
        keyboard, mouse, window,
    };
    use cosmic::{Element, Renderer, Theme};
    use std::{cell::Cell, rc::Rc};

    /// One coordinate shared by the list and its groups, refreshed on each widget
    /// callback, including operations/accessibility before the next draw.
    #[derive(Clone, Default)]
    pub(super) struct StickyViewport(Rc<Cell<f32>>);

    fn sticky_header_bounds(group: Rectangle, height: f32, top: f32) -> Rectangle {
        Rectangle {
            y: top
                .max(group.y)
                .min(group.y + (group.height - height).max(0.0)),
            height,
            ..group
        }
    }

    enum Kind {
        FilteredScroll(SidebarFilter),
        Projects {
            ids: Vec<ProjectId>,
            enabled: bool,
            viewport: StickyViewport,
        },
        Group {
            viewport: StickyViewport,
        },
        Actions,
    }

    #[derive(Default)]
    struct State {
        origin: Option<(ProjectId, Point)>,
        dragging: bool,
        drop: Option<(Option<ProjectId>, f32)>,
        hovered: bool,
        focused: bool,
        header_node: Option<layout::Node>,
        scroll: Option<Box<FilterScrollState>>,
    }

    struct FilterScrollState {
        active: SidebarFilter,
        saved: [Option<tree::State>; 3],
    }

    impl FilterScrollState {
        fn index(filter: SidebarFilter) -> usize {
            match filter {
                SidebarFilter::All => 0,
                SidebarFilter::Attention => 1,
                SidebarFilter::PullRequests => 2,
            }
        }
    }

    pub(super) struct RowInteraction<'a> {
        children: Vec<Element<'a, SidebarAction>>,
        kind: Kind,
    }

    #[derive(Default)]
    struct FocusWithin(bool);

    impl Operation for FocusWithin {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }

        fn focusable(
            &mut self,
            _id: Option<&Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.0 |= state.is_focused();
        }
    }

    impl<'a> RowInteraction<'a> {
        pub(super) fn filtered_scroll(
            content: Element<'a, SidebarAction>,
            filter: SidebarFilter,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::FilteredScroll(filter),
            })
        }

        pub(super) fn projects(
            content: Element<'a, SidebarAction>,
            ids: Vec<ProjectId>,
            enabled: bool,
            viewport: StickyViewport,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::Projects {
                    ids,
                    enabled,
                    viewport,
                },
            })
        }

        pub(super) fn group(
            header: Element<'a, SidebarAction>,
            members: Element<'a, SidebarAction>,
            viewport: StickyViewport,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![header, members],
                kind: Kind::Group { viewport },
            })
        }

        pub(super) fn actions(
            base: Element<'a, SidebarAction>,
            actions: Element<'a, SidebarAction>,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![base, actions],
                kind: Kind::Actions,
            })
        }

        fn focus(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
            let mut focus = FocusWithin::default();
            if !matches!(self.kind, Kind::Actions) {
                return;
            }
            for ((child, tree), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(tree, layout, renderer, &mut focus);
            }
            tree.state.downcast_mut::<State>().focused = focus.0;
        }

        fn header_at(&self, layout: Layout<'_>, point: Point) -> Option<ProjectId> {
            let Kind::Projects {
                ids,
                enabled: true,
                viewport,
            } = &self.kind
            else {
                return None;
            };
            ids.iter()
                .zip(layout.children().next()?.children())
                .find_map(|(id, row)| {
                    let bounds =
                        sticky_header_bounds(row.bounds(), PROJECT_HEADER_HEIGHT, viewport.0.get());
                    // Leave expansion and new-worktree buttons as ordinary controls.
                    let header = Rectangle {
                        x: bounds.x,
                        y: bounds.y,
                        width: (bounds.width - 3.0 * ICON_CONTROL_SIZE).max(0.0),
                        height: PROJECT_HEADER_HEIGHT,
                    };
                    header.contains(point).then_some(*id)
                })
        }

        fn drop_at(&self, layout: Layout<'_>, point: Point) -> (Option<ProjectId>, f32) {
            let Kind::Projects { ids, viewport, .. } = &self.kind else {
                unreachable!();
            };
            for (id, row) in ids.iter().zip(layout.children().next().unwrap().children()) {
                let header =
                    sticky_header_bounds(row.bounds(), PROJECT_HEADER_HEIGHT, viewport.0.get());
                if point.y < header.y + PROJECT_HEADER_HEIGHT / 2.0 {
                    return (Some(*id), header.y);
                }
            }
            (None, layout.bounds().y + layout.bounds().height)
        }
    }

    /// Retain the header's layout node rather than cloning a layout subtree on
    /// every frame. All widget entrypoints receive this same translated layout;
    /// the actual header widget, tree, focus and menu state exist only once.
    fn child_layouts<'a>(
        kind: &Kind,
        state: &'a State,
        layout: Layout<'a>,
    ) -> impl DoubleEndedIterator<Item = Layout<'a>> + ExactSizeIterator + use<'a> {
        let top = match kind {
            Kind::Group { viewport } => Some(viewport.0.get()),
            _ => None,
        };
        layout.children().enumerate().map(move |(index, child)| {
            let child = if index == 0
                && let Some(top) = top
            {
                let node = state.header_node.as_ref().expect("group has been laid out");
                let header = sticky_header_bounds(layout.bounds(), node.size().height, top);
                Layout::with_offset(Vector::new(header.x, header.y), node)
            } else {
                child
            };
            child.with_virtual_offset(layout.virtual_offset())
        })
    }

    impl Widget<SidebarAction, Theme, Renderer> for RowInteraction<'_> {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<State>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(State {
                scroll: match self.kind {
                    Kind::FilteredScroll(active) => Some(Box::new(FilterScrollState {
                        active,
                        saved: [None, None, None],
                    })),
                    _ => None,
                },
                ..State::default()
            })
        }

        fn children(&self) -> Vec<Tree> {
            self.children.iter().map(Tree::new).collect()
        }

        fn diff(&mut self, tree: &mut Tree) {
            if let Kind::FilteredScroll(filter) = self.kind {
                let state = tree.state.downcast_mut::<State>();
                let scroll = state.scroll.as_mut().expect("filtered scroll state");
                if scroll.active != filter {
                    // Swap the native scroll state before reconciling shorter
                    // content. No queued viewport event can overwrite another
                    // filter's offset, and only one row subtree stays mounted.
                    let next = scroll.saved[FilterScrollState::index(filter)]
                        .take()
                        .unwrap_or_else(|| self.children[0].as_widget().state());
                    scroll.saved[FilterScrollState::index(scroll.active)] =
                        Some(std::mem::replace(&mut tree.children[0].state, next));
                    scroll.active = filter;
                }
            }
            tree.diff_children(&mut self.children);
            if let Kind::Projects { ids, enabled, .. } = &self.kind {
                let state = tree.state.downcast_mut::<State>();
                if !enabled || state.origin.is_some_and(|(id, _)| !ids.contains(&id)) {
                    *state = State::default();
                }
            }
        }

        fn size(&self) -> Size<Length> {
            if matches!(self.kind, Kind::Group { .. }) {
                Size::new(Length::Fill, Length::Shrink)
            } else {
                self.children[0].as_widget().size()
            }
        }

        fn layout(
            &mut self,
            tree: &mut Tree,
            renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            let base =
                self.children[0]
                    .as_widget_mut()
                    .layout(&mut tree.children[0], renderer, limits);
            let size = base.size();
            if matches!(self.kind, Kind::Group { .. }) {
                let members = self.children[1]
                    .as_widget_mut()
                    .layout(
                        &mut tree.children[1],
                        renderer,
                        &layout::Limits::new(
                            Size::ZERO,
                            Size::new(size.width, limits.max().height),
                        ),
                    )
                    .move_to(Point::new(0.0, size.height));
                let group_size = Size::new(
                    size.width.max(members.size().width),
                    size.height + members.size().height,
                );
                tree.state.downcast_mut::<State>().header_node = Some(base);
                return layout::Node::with_children(
                    group_size,
                    vec![layout::Node::new(size), members],
                );
            }
            let mut nodes = vec![base];
            if self.children.len() == 2 {
                nodes.push(self.children[1].as_widget_mut().layout(
                    &mut tree.children[1],
                    renderer,
                    &layout::Limits::new(Size::ZERO, size),
                ));
            }
            layout::Node::with_children(size, nodes)
        }

        fn operate(
            &mut self,
            tree: &mut Tree,
            layout: Layout<'_>,
            renderer: &Renderer,
            operation: &mut dyn Operation,
        ) {
            if let Kind::Projects { viewport, .. } = &self.kind {
                viewport
                    .0
                    .set(layout.bounds().y + layout.virtual_offset().y);
            }
            operation.container(None, layout.bounds());
            operation.traverse(&mut |operation| {
                for ((child, tree), layout) in
                    self.children
                        .iter_mut()
                        .zip(&mut tree.children)
                        .zip(child_layouts(
                            &self.kind,
                            tree.state.downcast_ref::<State>(),
                            layout,
                        ))
                {
                    child
                        .as_widget_mut()
                        .operate(tree, layout, renderer, operation);
                }
            });
            self.focus(tree, layout, renderer);
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
            if let Kind::Projects {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y);
            }
            let state = tree.state.downcast_mut::<State>();
            let hovered = cursor.is_over(layout.bounds()) && cursor.is_over(*viewport);
            if hovered != state.hovered {
                state.hovered = hovered;
                shell.request_redraw();
            }
            if matches!(self.kind, Kind::Projects { enabled: true, .. }) {
                match event {
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if hovered => {
                        if let Some(point) = cursor.position() {
                            state.origin = self.header_at(layout, point).map(|id| (id, point));
                        }
                    }
                    Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                        if let (Some((_, origin)), Some(position)) =
                            (state.origin, cursor.position())
                        {
                            if !state.dragging && origin.distance(position) >= 6.0 {
                                state.dragging = true;
                                // Cancel the native button press before swallowing the drag.
                                // An unavailable cursor cannot activate the pressed button.
                                self.children[0].as_widget_mut().update(
                                    &mut tree.children[0],
                                    &Event::Mouse(mouse::Event::ButtonReleased(
                                        mouse::Button::Left,
                                    )),
                                    layout
                                        .children()
                                        .next()
                                        .unwrap()
                                        .with_virtual_offset(layout.virtual_offset()),
                                    mouse::Cursor::Unavailable,
                                    renderer,
                                    clipboard,
                                    shell,
                                    viewport,
                                );
                            }
                            if state.dragging {
                                state.drop = hovered.then(|| self.drop_at(layout, position));
                                shell.request_redraw();
                                shell.capture_event();
                                return;
                            }
                        }
                    }
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                        if let Some((project, _)) = state.origin.take() {
                            if state.dragging {
                                if let Some((before, _)) = state.drop.take().filter(|_| hovered) {
                                    shell
                                        .publish(SidebarAction::ReorderProject { project, before });
                                }
                                state.dragging = false;
                                shell.request_redraw();
                                shell.capture_event();
                                return;
                            }
                        }
                    }
                    Event::Window(window::Event::Unfocused)
                    | Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        ..
                    }) => {
                        if state.origin.is_some() {
                            self.children[0].as_widget_mut().update(
                                &mut tree.children[0],
                                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                                layout
                                    .children()
                                    .next()
                                    .unwrap()
                                    .with_virtual_offset(layout.virtual_offset()),
                                mouse::Cursor::Unavailable,
                                renderer,
                                clipboard,
                                shell,
                                viewport,
                            );
                        }
                        state.origin = None;
                        state.dragging = false;
                        state.drop = None;
                        shell.request_redraw();
                    }
                    _ => {}
                }
                if state.dragging {
                    shell.capture_event();
                    return;
                }
            }
            let show_actions = state.hovered || state.focused;
            let group = matches!(self.kind, Kind::Group { .. });
            let header_bounds = child_layouts(&self.kind, state, layout)
                .next()
                .unwrap()
                .bounds();
            // Members cannot receive pointer/touch events under the pinned header.
            let members_viewport = if group {
                let bottom = header_bounds.y + header_bounds.height;
                Rectangle {
                    y: viewport.y.max(bottom),
                    height: (viewport.y + viewport.height - viewport.y.max(bottom)).max(0.0),
                    ..*viewport
                }
            } else {
                *viewport
            };
            for (index, ((child, tree), layout)) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(child_layouts(&self.kind, state, layout))
                .enumerate()
                .rev()
            {
                if index == 1
                    && matches!(self.kind, Kind::Actions)
                    && !show_actions
                    && matches!(event, Event::Touch(_))
                {
                    continue;
                }
                let viewport = if group && index == 1 {
                    &members_viewport
                } else {
                    viewport
                };
                let cursor =
                    if (index == 1 && !show_actions && !group) || !cursor.is_over(*viewport) {
                        mouse::Cursor::Unavailable
                    } else {
                        cursor
                    };
                child.as_widget_mut().update(
                    tree, event, layout, cursor, renderer, clipboard, shell, viewport,
                );
                if shell.is_event_captured() {
                    break;
                }
            }
            self.focus(tree, layout, renderer);
        }

        fn mouse_interaction(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            viewport: &Rectangle,
            renderer: &Renderer,
        ) -> mouse::Interaction {
            let state = tree.state.downcast_ref::<State>();
            if let Kind::Projects {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y);
            }
            if state.dragging {
                return mouse::Interaction::Grabbing;
            }
            if cursor.is_over(*viewport)
                && cursor
                    .position()
                    .and_then(|point| self.header_at(layout, point))
                    .is_some()
            {
                return mouse::Interaction::Pointer;
            }
            self.children
                .iter()
                .zip(&tree.children)
                .zip(child_layouts(&self.kind, state, layout))
                .enumerate()
                .rev()
                .map(|(index, ((child, tree), child_layout))| {
                    let hidden_actions = index == 1
                        && matches!(self.kind, Kind::Actions)
                        && !(state.hovered || state.focused);
                    let covered_members = index == 1
                        && matches!(self.kind, Kind::Group { .. })
                        && cursor.is_over(
                            child_layouts(&self.kind, state, layout)
                                .next()
                                .unwrap()
                                .bounds(),
                        );
                    let cursor = if hidden_actions || covered_members || !cursor.is_over(*viewport)
                    {
                        mouse::Cursor::Unavailable
                    } else {
                        cursor
                    };
                    child.as_widget().mouse_interaction(
                        tree,
                        child_layout,
                        cursor,
                        viewport,
                        renderer,
                    )
                })
                .find(|interaction| *interaction != mouse::Interaction::None)
                .unwrap_or_default()
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
            let state = tree.state.downcast_ref::<State>();
            if let Kind::Projects {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y);
            }
            if matches!(self.kind, Kind::Group { .. }) {
                let mut layouts = child_layouts(&self.kind, state, layout);
                let header = layouts.next().unwrap();
                let members = layouts.next().unwrap();
                self.children[1].as_widget().draw(
                    &tree.children[1],
                    renderer,
                    theme,
                    style,
                    members,
                    cursor,
                    viewport,
                );
                if let Some(clip) = header.bounds().intersection(viewport) {
                    renderer.with_layer(clip, |renderer| {
                        if header.bounds().y > layout.bounds().y {
                            renderer.fill_quad(
                                renderer::Quad {
                                    bounds: header.bounds(),
                                    ..Default::default()
                                },
                                cosmic::iced::Color::from(theme.cosmic().primary(false).base),
                            );
                        }
                        self.children[0].as_widget().draw(
                            &tree.children[0],
                            renderer,
                            theme,
                            style,
                            header,
                            cursor,
                            viewport,
                        );
                    });
                }
                return;
            }
            for (index, ((child, tree), layout)) in self
                .children
                .iter()
                .zip(&tree.children)
                .zip(child_layouts(&self.kind, state, layout))
                .enumerate()
            {
                if index == 1 && !(state.hovered || state.focused) {
                    continue;
                }
                if index == 1 {
                    // A separate renderer layer paints the opaque action strip
                    // after base-row text instead of batching its quad below it.
                    renderer.with_layer(*viewport, |renderer| {
                        child
                            .as_widget()
                            .draw(tree, renderer, theme, style, layout, cursor, viewport);
                    });
                } else {
                    child
                        .as_widget()
                        .draw(tree, renderer, theme, style, layout, cursor, viewport);
                }
            }
            if let Some((_, y)) = state.drop {
                if let Some(clip) = layout.bounds().intersection(viewport) {
                    renderer.with_layer(clip, |renderer| {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle {
                                    x: layout.bounds().x,
                                    y: y - 1.0,
                                    width: layout.bounds().width,
                                    height: 2.0,
                                },
                                ..Default::default()
                            },
                            cosmic::iced::Color::from(*theme.cosmic().accent_color()),
                        );
                    });
                }
            }
        }

        fn overlay<'a>(
            &'a mut self,
            tree: &'a mut Tree,
            layout: Layout<'a>,
            renderer: &Renderer,
            viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'a, SidebarAction, Theme, Renderer>> {
            if let Kind::Projects {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y - translation.y);
            }
            let state = tree.state.downcast_ref::<State>();
            let hide_actions =
                matches!(self.kind, Kind::Actions) && !(state.hovered || state.focused);
            let overlays = self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(child_layouts(
                    &self.kind,
                    tree.state.downcast_ref::<State>(),
                    layout,
                ))
                .enumerate()
                .filter(|(index, _)| !hide_actions || *index == 0)
                .filter_map(|(_, ((child, tree), layout))| {
                    child
                        .as_widget_mut()
                        .overlay(tree, layout, renderer, viewport, translation)
                })
                .collect::<Vec<_>>();
            (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
        }

        fn a11y_nodes(
            &self,
            layout: Layout<'_>,
            tree: &Tree,
            cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            if let Kind::Projects { viewport, .. } = &self.kind {
                viewport
                    .0
                    .set(layout.bounds().y + layout.virtual_offset().y);
            }
            let mut accessible = iced_accessibility::A11yTree::join(
                self.children
                    .iter()
                    .zip(&tree.children)
                    .zip(child_layouts(
                        &self.kind,
                        tree.state.downcast_ref::<State>(),
                        layout,
                    ))
                    .map(|((child, tree), layout)| {
                        child.as_widget().a11y_nodes(layout, tree, cursor)
                    }),
            );
            // libcosmic buttons report content-coordinate bounds; its scrollable
            // forwards a virtual offset without translating their AccessKit nodes.
            // Translate once at the list boundary, not per nested decorator.
            if matches!(self.kind, Kind::Projects { .. }) {
                let offset = layout.virtual_offset();
                let translate = |node: &mut iced_accessibility::A11yNode| {
                    if let Some(bounds) = node.node().bounds() {
                        node.node_mut()
                            .set_bounds(iced_accessibility::accesskit::Rect::new(
                                bounds.x0 - f64::from(offset.x),
                                bounds.y0 - f64::from(offset.y),
                                bounds.x1 - f64::from(offset.x),
                                bounds.y1 - f64::from(offset.y),
                            ));
                    }
                };
                accessible.root_mut().iter_mut().for_each(translate);
                accessible.children_mut().iter_mut().for_each(translate);
            }
            accessible
        }

        fn drag_destinations(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            renderer: &Renderer,
            destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
        ) {
            if let Kind::Projects { viewport, .. } = &self.kind {
                viewport
                    .0
                    .set(layout.bounds().y + layout.virtual_offset().y);
            }
            for ((child, tree), layout) in self.children.iter().zip(&tree.children).zip(
                child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout),
            ) {
                child
                    .as_widget()
                    .drag_destinations(tree, layout, renderer, destinations);
            }
        }
    }
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
                created_at_unix_ms: None,
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

    #[test]
    fn worktree_sort_pins_root_and_uses_creation_evidence_before_stable_import_fallback() {
        let mut entry = project();
        let mut root = worktree(10, "main", false);
        root.path = entry.repository.clone();
        let mut older = worktree(11, "z-old", false);
        older.created_at_unix_ms = Some(100);
        let mut newer = worktree(12, "a-new", false);
        newer.created_at_unix_ms = Some(200);
        let imported = worktree(13, "imported", false);
        entry.worktrees = vec![imported, older, root, newer];
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        let order = |state: &SidebarState| {
            filtered_projects(&model, state, false)[0]
                .worktrees
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>()
        };
        let id = |id| WorktreeId::from_uuid(Uuid::from_u128(id));
        assert_eq!(order(&state), vec![id(10), id(12), id(11), id(13)]);
        state.set_worktree_sort(crate::settings::WorktreeSort::Oldest);
        assert_eq!(order(&state), vec![id(10), id(11), id(12), id(13)]);
        state.set_worktree_sort(crate::settings::WorktreeSort::Alphabetical);
        assert_eq!(order(&state), vec![id(10), id(12), id(13), id(11)]);
    }

    #[test]
    fn worktree_titles_follow_pr_lifecycle_without_changing_git_identity() {
        let branch = "CarsonBurke/claws-turnstile-fix";
        let mut row = worktree(11, branch, false);
        assert_eq!(&*row.chrome.label, "claws turnstile fix");
        assert!(row.matches("carsonburke/"));
        let mut pr = project().pull_requests.remove(0);
        pr.title = "Fix browser challenge handling".into();
        row.set_pull_request(pr);
        assert_eq!(&*row.chrome.label, "Fix browser challenge handling");
        assert!(row.matches("challenge"));
        assert!(row.chrome.tooltip.contains(branch));
        row.clear_pull_request();
        assert_eq!(&*row.chrome.label, "claws turnstile fix");
        assert!(!row.matches("challenge"));
        assert_eq!(row.branch.as_deref(), Some(branch));
    }

    #[test]
    fn project_filters_restore_native_scroll_without_viewport_messages() {
        use cosmic::iced::{
            Rectangle, Vector,
            advanced::widget::{Operation, operation},
        };

        #[derive(Default)]
        struct Offset(Option<f32>);
        impl Operation for Offset {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }

            fn scrollable(
                &mut self,
                id: Option<&Id>,
                _bounds: Rectangle,
                _content_bounds: Rectangle,
                translation: Vector,
                _state: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&stable_id("projects.scroll")) {
                    self.0 = Some(translation.y);
                }
            }
        }

        let renderer = headless_renderer();
        let make = |filter, height| {
            project_scroll(
                cosmic::iced::widget::Space::new()
                    .width(200)
                    .height(height)
                    .into(),
                filter,
            )
        };
        let mut element = make(SidebarFilter::All, 1_000.0);
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(Size::ZERO, Size::new(240.0, 200.0));
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                stable_id("projects.scroll"),
                operation::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(640.0),
                },
            ),
        );

        for (filter, height, expected, scroll_to) in [
            (SidebarFilter::Attention, 500.0, 0.0, Some(170.0)),
            (SidebarFilter::PullRequests, 1_000.0, 0.0, Some(330.0)),
            (SidebarFilter::All, 1_000.0, 640.0, None),
            (SidebarFilter::Attention, 500.0, 170.0, None),
            (SidebarFilter::All, 1_000.0, 640.0, None),
            (SidebarFilter::PullRequests, 1_000.0, 330.0, None),
            (SidebarFilter::Attention, 0.0, 0.0, None),
            (SidebarFilter::All, 1_000.0, 640.0, None),
        ] {
            element = make(filter, height);
            tree.diff(&mut element);
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut offset = Offset::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut offset);
            assert_eq!(offset.0, Some(expected), "{filter:?}");
            if let Some(y) = scroll_to {
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation::scrollable::scroll_to::<()>(
                        stable_id("projects.scroll"),
                        operation::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(y),
                        },
                    ),
                );
            }
        }
    }

    #[test]
    fn thread_attention_keeps_its_project_visible_without_unread_worktrees() {
        let mut entry = project();
        let thread_id = ThreadId::from_uuid(Uuid::from_u128(20));
        entry.threads.push(SidebarThread::new(
            thread_id,
            "Investigate login".into(),
            true,
            Some(crate::agent::ActivityState::Waiting),
            true,
        ));
        let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.apply(&SidebarAction::SearchChanged("login".into()));
        let projects = filtered_projects(&model, &state, false);
        assert_eq!(projects.len(), 1);
        assert!(projects[0].worktrees.is_empty());
        assert_eq!(projects[0].threads[0].id, thread_id);
        model.projects[0].threads[0].unread = false;
        assert!(filtered_projects(&model, &state, false).is_empty());
        assert_eq!(
            model.projects[0].threads[0].activity,
            Some(crate::agent::ActivityState::Waiting),
        );
    }

    #[test]
    fn hiding_file_filter_does_not_leave_an_invisible_query() {
        let mut state = SidebarState::default();
        let key = main_key();
        state.apply(&SidebarAction::FilesQueryChanged {
            key: key.clone(),
            query: "missing".into(),
        });
        state.apply(&SidebarAction::ToggleFilesFilter { key });
        assert!(!state.files_filter_open());
        assert_eq!(state.files_query(), "");
    }

    #[test]
    fn unread_attention_is_owned_and_acknowledged_per_pane() {
        let mut state = SidebarState::default();
        let project = project_id(1);
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
        let first = CardId::from_uuid(Uuid::from_u128(4));
        let second = CardId::from_uuid(Uuid::from_u128(5));
        state.observe_agent_update(first, project, worktree);
        state.observe_agent_update(second, project, worktree);
        state.acknowledge_agent_update(first);
        assert!(!state.has_card_update(first));
        assert!(state.has_card_update(second));
        assert!(state.has_agent_update(project, worktree));
        state.clear_agent_activity(second);
        assert!(
            state.has_agent_update(project, worktree),
            "provider exit is not acknowledgement"
        );
        state.acknowledge_agent_update(second);
        assert!(!state.has_agent_update(project, worktree));
        state.observe_card_update(first);
        assert!(state.has_card_update(first));
        assert!(!state.has_agent_update(project, worktree));
        state.observe_agent_update(first, project, worktree);
        assert!(state.has_agent_update(project, worktree));
        let other = WorktreeId::from_uuid(Uuid::from_u128(3));
        state.observe_agent_update(first, project, other);
        assert!(
            !state.has_agent_update(project, worktree),
            "reattribution cannot leave stale worktree attention"
        );
        assert!(state.has_agent_update(project, other));
    }

    #[test]
    fn working_agents_aggregate_without_clearing_other_agents_or_unread_updates() {
        use crate::agent::{Activity, ActivityState, Provider};
        let mut state = SidebarState::default();
        let project = project_id(1);
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
        let other = WorktreeId::from_uuid(Uuid::from_u128(3));
        let first = CardId::from_uuid(Uuid::from_u128(4));
        let second = CardId::from_uuid(Uuid::from_u128(5));
        let activity = |state| Activity {
            provider: Provider::Omp,
            state,
        };
        state.observe_agent_update(first, project, worktree);
        state.set_agent_activity(first, project, worktree, activity(ActivityState::Working));
        state.set_agent_activity(second, project, worktree, activity(ActivityState::Waiting));
        assert_eq!(
            state.agent_activity(project, worktree),
            Some(ActivityState::Working)
        );
        assert_eq!(state.agent_activity(project, other), None);
        state.set_agent_activity(first, project, worktree, activity(ActivityState::Idle));
        assert_eq!(
            state.agent_activity(project, worktree),
            Some(ActivityState::Waiting)
        );
        state.set_agent_activity(second, project, other, activity(ActivityState::Working));
        assert_eq!(state.agent_activity(project, worktree), None);
        assert_eq!(
            state.agent_activity(project, other),
            Some(ActivityState::Working)
        );
        state.clear_agent_activity(second);
        assert_eq!(state.agent_activity(project, other), None);
        assert!(state.has_agent_update(project, worktree));
    }

    #[test]
    fn standalone_activity_and_reattribution_keep_attention_on_its_pane() {
        use crate::agent::{Activity, ActivityState, Provider};
        let mut state = SidebarState::default();
        let card = CardId::from_uuid(Uuid::from_u128(4));
        let project = project_id(1);
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
        let working = Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        };
        state.set_card_activity(card, working);
        assert_eq!(state.card_activity(card), Some(working));
        assert_eq!(state.agent_activity(project, worktree), None);
        assert!(
            !state.has_card_update(card),
            "activity is not unread attention"
        );
        state.observe_card_update(card);
        state.set_agent_activity(card, project, worktree, working);
        assert!(state.has_agent_update(project, worktree));
        state.set_card_activity(card, working);
        assert!(state.has_card_update(card));
        assert!(!state.has_agent_update(project, worktree));
        assert_eq!(state.agent_activity(project, worktree), None);
        state.clear_agent_activity(card);
        assert_eq!(state.card_activity(card), None);
        assert!(state.has_card_update(card));
    }

    #[test]
    fn group_mutations_never_include_files_hidden_by_the_change_tree() {
        let mut snapshot = snapshot();
        let mut renamed = changed(
            repo_path("keep.rs"),
            ChangeArea::Unstaged,
            ChangeKind::Renamed,
            None,
        );
        renamed.old_path = Some(repo_path("old.rs"));
        snapshot.status.entries = vec![
            changed(
                repo_path("src/hidden.rs"),
                ChangeArea::Unstaged,
                ChangeKind::Modified,
                None,
            ),
            renamed.clone(),
            changed(
                repo_path("keep.rs"),
                ChangeArea::Staged,
                ChangeKind::Modified,
                None,
            ),
        ];
        let source = SourceControlModel::ready(main_key(), &snapshot);
        let group = source.group(ChangeArea::Unstaged).unwrap();
        // Directories start collapsed, so `src/hidden.rs` stays hidden until
        // the user opens `src`.
        let expanded = BTreeSet::new();
        let (tree, _) = visible_file_rows(&group.tree, &expanded, "", false, false);
        let selected = tree
            .iter()
            .filter(|row| !row.directory)
            .filter_map(|row| group.visible_path(&row.path))
            .collect::<Vec<_>>();
        let stage = group_commands(&source, group, &selected, false)
            .into_iter()
            .find_map(|command| match command.action {
                Some(SidebarAction::Stage { selection, .. }) => Some(selection),
                _ => None,
            })
            .unwrap();
        assert_eq!(stage, vec![PathSelection::from_changed_path(&renamed)]);
        assert!(group_commands(&source, group, &[], false).is_empty());
    }

    #[test]
    fn project_reorder_preserves_new_projects_and_handles_forward_moves() {
        let projects = (1..=4)
            .map(|id| {
                let mut entry = project();
                entry.id = project_id(id);
                entry
            })
            .collect();
        let mut model = SidebarModel::ready(SidebarMode::Expanded, projects);
        model.apply_project_order(&[project_id(3), project_id(1)]);
        let ids = |model: &SidebarModel| {
            model
                .projects
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&model),
            vec![project_id(3), project_id(1), project_id(2), project_id(4)]
        );
        assert!(model.reorder_project(project_id(3), Some(project_id(4))));
        assert_eq!(
            ids(&model),
            vec![project_id(1), project_id(2), project_id(3), project_id(4)]
        );
        assert!(!model.reorder_project(project_id(3), Some(project_id(3))));
        assert!(!model.reorder_project(project_id(3), Some(project_id(99))));
        assert!(model.reorder_project(project_id(1), None));
        assert_eq!(
            ids(&model),
            vec![project_id(2), project_id(3), project_id(4), project_id(1)]
        );
    }

    /// The widget identities of the controls an element renders, in tree
    /// order.
    ///
    /// Collects named controls for collision detection and focus routing.
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
    fn file_menu_opens_without_editors_only_when_file_management_is_enabled() {
        use cosmic::iced::{
            Event, Point, Rectangle, Vector,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let key = main_key();
        let rows = file_rows(&[repo_path("src/main.rs")]);
        let configured_editors = [SidebarEditor {
            id: "editor".into(),
            name: "External editor".into(),
            icon: "text-editor-symbolic".into(),
        }];
        for file_management in [false, true] {
            for blocked in [false, true] {
                for editors in [&[][..], &configured_editors[..]] {
                    for row in &rows {
                        let content = cosmic::iced::widget::Space::new()
                            .width(240)
                            .height(32)
                            .into();
                        let mut element = file_context_controls(
                            content,
                            &key,
                            row,
                            editors,
                            file_management,
                            blocked,
                        );
                        let mut tree = Tree::new(&element);
                        let viewport = Rectangle::with_size(Size::new(240.0, 32.0));
                        let node = element.as_widget_mut().layout(
                            &mut tree,
                            &renderer,
                            &layout::Limits::new(Size::ZERO, viewport.size()),
                        );
                        let mut messages = Vec::new();
                        for event in [
                            mouse::Event::ButtonPressed(mouse::Button::Right),
                            mouse::Event::ButtonReleased(mouse::Button::Right),
                        ] {
                            element.as_widget_mut().update(
                                &mut tree,
                                &Event::Mouse(event),
                                Layout::new(&node),
                                mouse::Cursor::Available(Point::new(40.0, 16.0)),
                                &renderer,
                                &mut clipboard::Null,
                                &mut Shell::new(&mut messages),
                                &viewport,
                            );
                        }
                        let opens = element
                            .as_widget_mut()
                            .overlay(
                                &mut tree,
                                Layout::new(&node),
                                &renderer,
                                &viewport,
                                Vector::ZERO,
                            )
                            .is_some();
                        assert_eq!(
                            opens,
                            !blocked
                                && (file_management || (!row.directory && !editors.is_empty())),
                            "file_management={file_management}, blocked={blocked}, directory={}, editors={}",
                            row.directory,
                            editors.len(),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn context_menu_preserves_child_cursors_and_primary_clicks() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let content = widget::row::with_children(vec![
            button::custom(widget::text::body("Open"))
                .width(80)
                .height(32)
                .on_press(SidebarAction::OpenRepositoryPicker)
                .into(),
            button::custom(widget::text::body("Disabled"))
                .width(80)
                .height(32)
                .into(),
            cosmic::iced::widget::Space::new()
                .width(80)
                .height(32)
                .into(),
        ]);
        let mut element = context_controls(content, project_id(1), None, false, false, &[]);
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(240.0, 32.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        for (x, expected) in [
            (40.0, mouse::Interaction::Pointer),
            (120.0, mouse::Interaction::None),
            (200.0, mouse::Interaction::None),
        ] {
            assert_eq!(
                element.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    mouse::Cursor::Available(Point::new(x, 16.0)),
                    &viewport,
                    &renderer,
                ),
                expected,
            );
        }
        let mut messages = Vec::new();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(40.0, 16.0)),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert_eq!(messages, vec![SidebarAction::OpenRepositoryPicker]);
    }

    #[test]
    fn project_drag_does_not_activate_the_pressed_header() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let content = widget::column::with_children((1..=2).map(|id| {
            button::custom(widget::text::body("Project"))
                .height(PROJECT_HEADER_HEIGHT)
                .width(240)
                .on_press(SidebarAction::ToggleProject(project_id(id)))
                .into()
        }));
        let mut element = RowInteraction::projects(
            content.into(),
            vec![project_id(1), project_id(2)],
            true,
            row_interaction::StickyViewport::default(),
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(240.0, 2.0 * PROJECT_HEADER_HEIGHT));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let origin = Point::new(100.0, PROJECT_HEADER_HEIGHT / 2.0);
        let end = Point::new(100.0, 1.9 * PROJECT_HEADER_HEIGHT);
        let mut dispatch = |event: Event, point: Point| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        // A scrollable translates the cursor but not CursorMoved's raw position.
        // Jitter below the threshold must remain a click even after scrolling.
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                origin
            )
            .is_empty()
        );
        let jitter = Point::new(origin.x + 3.0, origin.y);
        let raw_jitter = Point::new(jitter.x, jitter.y - 100.0);
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::CursorMoved {
                    position: raw_jitter
                }),
                jitter
            )
            .is_empty()
        );
        assert_eq!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                jitter
            ),
            vec![SidebarAction::ToggleProject(project_id(1))],
        );
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                origin
            )
            .is_empty()
        );
        let raw_end = Point::new(end.x, end.y - 100.0);
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::CursorMoved { position: raw_end }),
                end
            )
            .is_empty()
        );
        assert_eq!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                end
            ),
            vec![SidebarAction::ReorderProject {
                project: project_id(1),
                before: None
            }],
        );
        // A release outside the pane cancels instead of using a stale target.
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                origin
            )
            .is_empty()
        );
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::CursorMoved { position: end }),
                end
            )
            .is_empty()
        );
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                Point::new(-10.0, end.y),
            )
            .is_empty()
        );
    }

    #[test]
    fn sticky_project_headers_share_displayed_hit_focus_and_push_off_bounds() {
        use cosmic::iced::{
            Event, Point, Rectangle, Vector,
            advanced::{Shell, clipboard, widget::operation},
            keyboard, mouse,
        };
        let renderer = headless_renderer();
        let sticky = row_interaction::StickyViewport::default();
        let header_id = Id::new("sticky-project-regression-header");
        let groups = (1..=2).map(|id| {
            let header = button::custom(widget::text::body("Project"))
                .id(if id == 1 {
                    header_id.clone()
                } else {
                    Id::unique()
                })
                .name(format!("Sticky project {id}"))
                .width(240)
                .height(PROJECT_HEADER_HEIGHT)
                .on_press(SidebarAction::ToggleProject(project_id(id)));
            let member = button::custom(widget::text::body("Worktree"))
                .width(240)
                .height(3.0 * PROJECT_HEADER_HEIGHT)
                .on_press(SidebarAction::OpenRepositoryPicker);
            RowInteraction::group(header.into(), member.into(), sticky.clone())
        });
        let mut element = RowInteraction::projects(
            widget::column::with_children(groups).into(),
            vec![project_id(1), project_id(2)],
            true,
            sticky,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 512.0)),
        );
        // Use precisely the content-coordinate cursor/viewport and virtual scroll
        // offset supplied by iced's scrollable, without relying on a prior draw.
        let at = |offset| Layout::new(&node).with_virtual_offset(Vector::new(0.0, offset));
        let accessible_bounds = |element: &Element<'_, SidebarAction>, tree: &Tree, offset| {
            let accessible =
                element
                    .as_widget()
                    .a11y_nodes(at(offset), tree, mouse::Cursor::Unavailable);
            let bounds = |label| {
                accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .find(|node| node.node().label() == Some(label))
                    .unwrap()
                    .node()
                    .bounds()
                    .unwrap()
            };
            (bounds("Sticky project 1"), bounds("Sticky project 2"))
        };
        assert_eq!(accessible_bounds(&element, &tree, 64.0).0.y0, 0.0);
        let pushed = accessible_bounds(&element, &tree, 112.0);
        assert_eq!((pushed.0.y0, pushed.0.y1, pushed.1.y0), (-16.0, 16.0, 16.0));

        let mut dispatch = |event: Event, offset: f32, point: Point| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                at(offset),
                mouse::Cursor::Available(point),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::new(Point::new(0.0, offset), Size::new(240.0, 96.0)),
            );
            messages
        };
        for (offset, y, project) in [(64.0, 80.0, 1), (112.0, 120.0, 1), (112.0, 140.0, 2)] {
            let point = Point::new(100.0, y);
            assert!(
                dispatch(
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    offset,
                    point
                )
                .is_empty()
            );
            assert_eq!(
                dispatch(
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                    offset,
                    point
                ),
                vec![SidebarAction::ToggleProject(project_id(project))],
                "the displayed header owns the click, not the worktree underneath",
            );
        }
        let origin = Point::new(100.0, 80.0);
        let end = Point::new(100.0, 150.0);
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                64.0,
                origin
            )
            .is_empty()
        );
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::CursorMoved {
                    position: Point::new(end.x, end.y - 64.0)
                }),
                64.0,
                end
            )
            .is_empty()
        );
        assert_eq!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                64.0,
                end
            ),
            vec![SidebarAction::ReorderProject {
                project: project_id(1),
                before: None
            }],
        );
        element.as_widget_mut().operate(
            &mut tree,
            at(64.0),
            &renderer,
            &mut operation::focusable::focus::<()>(header_id),
        );
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            at(64.0),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::new(Point::new(0.0, 64.0), Size::new(240.0, 96.0)),
        );
        assert_eq!(messages, vec![SidebarAction::ToggleProject(project_id(1))]);
    }

    #[test]
    fn context_rows_survive_opening_and_closing_a_modal() {
        let row = |modal: bool| {
            context_controls(
                widget::button::custom(widget::text::body("Worktree"))
                    .name("Worktree")
                    .width(120)
                    .height(32)
                    .on_press_maybe((!modal).then_some(SidebarAction::OpenRepositoryPicker)),
                project_id(1),
                Some(WorktreeId::from_uuid(Uuid::from_u128(2))),
                modal,
                true,
                &[],
            )
        };
        let renderer = headless_renderer();
        let viewport = cosmic::iced::Rectangle::with_size(Size::new(120.0, 32.0));
        let initial = row(false);
        let mut tree = Tree::new(&initial);
        // The upstream context menu's diff assumes Some(menu). Rebuilding a
        // mounted row with None panics when a dialog disables context actions.
        for modal in [true, true, false, false] {
            let mut next = row(modal);
            tree.diff(&mut next);
            let layout = next.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let accessible = next.as_widget().a11y_nodes(
                Layout::new(&layout),
                &tree,
                cosmic::iced::advanced::mouse::Cursor::Unavailable,
            );
            assert!(
                accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .any(|node| node.node().label() == Some("Worktree"))
            );
            assert_eq!(
                next.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&layout),
                    cosmic::iced::mouse::Cursor::Available(cosmic::iced::Point::new(60.0, 16.0)),
                    &viewport,
                    &renderer,
                ),
                if modal {
                    cosmic::iced::mouse::Interaction::None
                } else {
                    cosmic::iced::mouse::Interaction::Pointer
                },
            );
        }
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
                        external_editors: Vec::new(),
                        file_management: false,
                    };
                    for selection in &selections {
                        for controls_expanded in [false, true] {
                            let state = SidebarState {
                                controls_expanded,
                                selection: *selection,
                                ..SidebarState::default()
                            };

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
        state.apply(&SidebarAction::SearchChanged(" Sidebar ".into()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(state.controls_expanded());
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

        state.apply(&SidebarAction::SearchChanged(String::new()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(
            filtered_projects(&model, &state, false).is_empty(),
            "dirty files and failing checks are not unread attention"
        );
        let first = CardId::from_uuid(Uuid::from_u128(4));
        let second = CardId::from_uuid(Uuid::from_u128(5));
        state.observe_agent_update(first, project_id(1), model.projects[0].worktrees[1].id);
        state.observe_agent_update(second, project_id(1), model.projects[0].worktrees[1].id);
        state.acknowledge_agent_update(first);
        let filtered = filtered_projects(&model, &state, false);
        assert_eq!(filtered[0].worktrees.len(), 1);
        assert_eq!(
            filtered[0].worktrees[0].branch.as_deref(),
            Some("fix-sidebar")
        );
        state.acknowledge_agent_update(second);
        assert!(filtered_projects(&model, &state, false).is_empty());
    }

    #[test]
    fn project_header_actions_are_independent_and_attention_can_be_cleared() {
        let renderer = headless_renderer();
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged("main".into()));
        for compact in [false, true] {
            let actions =
                frame_enter_presses(&renderer, header(&model, &state, compact, false, false));
            assert_eq!(
                actions,
                vec![
                    SidebarAction::SetFilter(SidebarFilter::Attention),
                    SidebarAction::ToggleControls,
                    SidebarAction::OpenRepositoryPicker,
                ]
            );
            state.apply(&actions[0]);
            let actions =
                frame_enter_presses(&renderer, header(&model, &state, compact, false, false));
            assert_eq!(actions[0], SidebarAction::SetFilter(SidebarFilter::All));
            state.apply(&actions[0]);
            assert_eq!(state.query(), "main", "attention does not overwrite search");
            assert!(
                frame_enter_presses(&renderer, header(&model, &state, compact, false, true))
                    .is_empty()
            );
        }
    }

    #[test]
    fn compact_search_applies_only_while_reachable_and_pr_filter_remains_available() {
        let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged("fix-sidebar".into()));
        state.apply(&SidebarAction::ToggleControls);
        assert_eq!(
            filtered_projects(&model, &state, true)[0].worktrees.len(),
            1
        );
        state.apply(&SidebarAction::ToggleControls);
        assert_eq!(
            filtered_projects(&model, &state, true)[0].worktrees.len(),
            2
        );
        state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
        let filtered = filtered_projects(&model, &state, true);
        assert_eq!(
            filtered[0].worktrees[0].branch.as_deref(),
            Some("fix-sidebar")
        );
        assert_eq!(filtered[0].worktrees.len(), 1);
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
    fn completed_pull_requests_do_not_present_stale_checks_as_activity() {
        let mut pr = project().pull_requests.remove(0);
        pr.checks = CheckStatus::Pending;
        pr.state = Some(WorktreePullRequestState::Open);
        assert!(pr.has_pending_checks());

        pr.state = Some(WorktreePullRequestState::Merged);
        assert!(!pr.has_pending_checks());
        pr.state = Some(WorktreePullRequestState::Closed);
        assert!(!pr.has_pending_checks());

        pr.state = Some(WorktreePullRequestState::Open);
        assert!(pr.has_pending_checks(), "reopening restores ongoing checks");
    }

    #[test]
    fn worktree_pull_requests_require_an_exact_attached_branch_in_the_same_project() {
        let project = project();
        let pull_request = &project.pull_requests[0];
        let attached = worktree(7, &pull_request.head_ref, false);
        assert_eq!(
            matching_pull_request(&attached, &project.pull_requests)
                .map(|pull_request| pull_request.number),
            Some(pull_request.number),
        );
        let other_branch = worktree(8, &format!("{}-other", pull_request.head_ref), false);
        assert!(matching_pull_request(&other_branch, &project.pull_requests).is_none());
        let case_changed = worktree(9, &pull_request.head_ref.to_ascii_uppercase(), false);
        assert!(matching_pull_request(&case_changed, &project.pull_requests).is_none());
        let mut detached = attached.clone();
        detached.detached = true;
        assert!(matching_pull_request(&detached, &project.pull_requests).is_none());
        assert!(matching_pull_request(&attached, &[]).is_none());
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

    #[test]
    fn worktree_change_badge_clears_after_clean_status_read() {
        let mut worktree =
            SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into()));
        let mut status = StatusSnapshot::default();
        status.dirty.unstaged = 1;
        worktree.apply_status(&status);
        assert!(worktree.chrome.dirty_badge.is_some());
        worktree.apply_status(&StatusSnapshot::default());
        assert!(worktree.chrome.dirty_badge.is_none());
        status.dirty.unstaged = 0;
        status.dirty.untracked = 1;
        worktree.apply_status(&status);
        assert!(worktree.chrome.dirty_badge.is_some());
        worktree.apply_status(&StatusSnapshot::default());
        assert!(worktree.chrome.dirty_badge.is_none());
    }

    #[test]
    fn branch_only_changes_keep_historical_paths_and_accent() {
        let mut source = SourceControlModel::ready(
            main_key(),
            &RepositorySnapshot {
                status: StatusSnapshot::default(),
                files: WorkingFiles::default(),
                remotes: Vec::new(),
                local_refs: Vec::new(),
                remote_refs: Vec::new(),
                remote_heads: Vec::new(),
            },
        );
        assert!(!source.has_changes());
        let comparison = BranchComparison {
            base_ref: ref_name("refs/heads/trunk"),
            base_oid: oid_of(0x22),
            head: oid_of(0x11),
            merge_base: oid_of(0x22),
            entries: vec![
                crate::repository::DiffEntry {
                    path: repo_path("deleted.txt"),
                    old_path: None,
                    kind: ChangeKind::Deleted,
                    added_lines: Some(0),
                    removed_lines: Some(4),
                },
                crate::repository::DiffEntry {
                    path: repo_path("new name.txt"),
                    old_path: Some(repo_path("old name.txt")),
                    kind: ChangeKind::Renamed,
                    added_lines: Some(0),
                    removed_lines: Some(0),
                },
            ],
        };
        let branch = Arc::new(BranchChangesModel::new(comparison));
        assert_eq!(
            branch.rows[1].entry.old_path,
            Some(repo_path("old name.txt"))
        );
        assert!(
            branch
                .tree
                .iter()
                .any(|file| file.path == repo_path("deleted.txt"))
        );
        source.branch_changes = BranchChanges::Ready(branch);
        assert!(source.has_changes());
    }

    #[test]
    fn tree_starts_collapsed_and_flat_ignores_expansion() {
        let source = source_control();
        let mut state = SidebarState::default();
        let paths = |state: &SidebarState, flat, query: &str| {
            visible_file_rows(
                &source.files,
                &state.expanded_directories,
                query,
                flat,
                false,
            )
            .0
            .iter()
            .map(|row| row.path.display().into_owned())
            .collect::<Vec<_>>()
        };
        // A fresh tree shows only top-level rows; loading the whole project
        // tree stays cheap until the user opens folders.
        assert_eq!(paths(&state, false, ""), ["src", "README.md"]);
        state.apply(&SidebarAction::ToggleDirectory {
            key: main_key(),
            path: repo_path("src"),
        });
        assert_eq!(
            paths(&state, false, ""),
            ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
        );
        // Flat mode and filters ignore expansion without losing tree state.
        assert_eq!(
            paths(&state, true, ""),
            ["src/app.rs", "src/sidebar.rs", "README.md"]
        );
        assert_eq!(paths(&state, true, "sidebar"), ["src/sidebar.rs"]);
        assert_eq!(
            paths(&state, false, ""),
            ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
        );
        // Diff and change review always expand, independent of the file tree.
        let (expanded, _) = visible_file_rows(
            &source.files,
            &SidebarState::default().expanded_directories,
            "",
            false,
            true,
        );
        assert_eq!(
            expanded
                .iter()
                .map(|row| row.path.display().into_owned())
                .collect::<Vec<_>>(),
            ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
        );
    }

    #[test]
    fn expanded_folders_round_trip_through_display_paths() {
        let mut state = SidebarState::default();
        state.restore_expanded_display_paths(&[
            "src".into(),
            "src/nested".into(),
            "/absolute".into(),
            "..".into(),
            String::new(),
        ]);
        // Only relative, well-formed folders survive: the tree renders,
        // just collapsed, for anything the session could not name.
        assert_eq!(
            state.expanded_display_paths(),
            ["src".to_owned(), "src/nested".to_owned()]
        );
    }

    fn panel_worktree(key: &ActiveWorktreeKey, branch: &str) -> SidebarWorktree {
        SidebarWorktree::from_repository(
            &WorktreeRecord {
                id: key.worktree_id,
                path: key.canonical_path.clone(),
                created_at_unix_ms: None,
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
    /// keyboard. Focus by id exercises the same commands as keyboard traversal.
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
            bound_source_control(&model, &state).is_none(),
            "with nothing focused the panel must not mount: there is no worktree it could be \
             reading"
        );

        assert!(
            state.sync_active_worktree(&model, &focus_on(&main_key())),
            "focusing a known worktree must bind the panel to it"
        );
        assert_eq!(state.active_worktree(), Some(&main_key()));
        assert!(
            bound_source_control(&model, &state).is_some(),
            "the panel mounts once the bound worktree is the one the model holds"
        );

        // Focus moves to the sibling worktree. The model still holds `main`'s
        // rows, and those rows may not be shown under `fix-sidebar`'s name: a
        // press on one would stage a path in the wrong tree.
        assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
        assert_eq!(state.active_worktree(), Some(&fix_key()));
        assert!(
            bound_source_control(&model, &state).is_none(),
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
        assert!(bound_source_control(&emptied, &state).is_none());
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
        assert!(state.expanded_directories.is_empty());
        assert!(
            state.pending_discard().is_none(),
            "a confirmation counted against the worktree that was left must not be answerable"
        );
    }

    #[test]
    fn every_panel_section_expands_and_collapses() {
        let key = main_key();
        let mut state = SidebarState::default();

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
            group_commands(
                &source,
                unstaged,
                &unstaged.visible().iter().collect::<Vec<_>>(),
                false
            )
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
    fn a_running_mutation_withholds_every_further_command_against_the_same_index() {
        let mut source = source_control();
        source.set_mutation(Some(MutationKind::Stage));
        let group = source
            .group(ChangeArea::Unstaged)
            .expect("an unstaged group exists");
        assert!(
            group_commands(
                &source,
                group,
                &group.visible().iter().collect::<Vec<_>>(),
                false
            )
            .is_empty()
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
        let request = group_commands(
            &source,
            group,
            &group.visible().iter().collect::<Vec<_>>(),
            false,
        )
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
        let mut model = panel_model();
        for project in &mut model.projects {
            project.github_ready = true;
        }
        let state = panel_state(&model);

        // Every section open, so every command the panel has is in the frame.
        let live = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
        assert!(
            live.iter()
                .any(|action| matches!(action, SidebarAction::Stage { .. })),
            "the fixture must actually draw the commands this test is about; it published {live:?}"
        );

        let gated = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, true));
        assert!(
            gated.is_empty(),
            "a dialog is an overlay: the pane keeps the focus it held when the dialog opened and \
             answers Enter behind it, and the application refuses every pane action while a modal \
             is up - so a control that still published one would report itself enabled, take the \
             press, and do nothing. It published {gated:?}"
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
