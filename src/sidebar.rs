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
    iced::{Alignment, Length, Padding, advanced::text::EllipsizeHeightLimit, id::Id},
    theme,
    widget::{self, button, icon},
};
use unicode_casefold::UnicodeCaseFold;
use url::Url;

use crate::{
    design::{self, NavigationLayout, STATUS_ICON_SIZE, space},
    github::{PullRequest, PullRequestCheck},
    model::{CardId, ChannelId, ProjectId, ProjectKind, ProjectState, WorktreeId},
    repository::{
        BranchComparison, ChangeArea, ChangeKind, ChangedPath, CommitIdentity, CommitSummary,
        ConflictKind, DirtyState, DiscardRequest, HISTORY_PAGE_SIZE, HeadState, Oid, PathSelection,
        PushTarget, RemoteRecord, RepoPath, RepositoryId, RepositoryInfo, RepositorySnapshot,
        StatusSnapshot, TrackingState, ValidatedBranchName, ValidatedRefName, WorktreeRecord,
        build_file_tree, build_file_tree_with_directories,
    },
    splitter::{Edge as SplitterEdge, Splitter},
    ui_controls::Icon,
};
use row_interaction::RowInteraction;

mod selection;
pub use selection::{Intent as SelectionIntent, Row as SelectionRow};

pub fn selection_row_focus_id(row: SelectionRow) -> Id {
    match row {
        SelectionRow::Project(id) => entity_id("project.open", id),
        SelectionRow::Worktree(_, id) => entity_id("worktree.open", id),
        SelectionRow::Channel(id) => entity_id("channel.open", id),
    }
}

/// The rendered bounds of the expanded pane, and the only bounds it has:
/// `crate::settings` validates persisted widths against these, so a width can
/// no longer pass validation and then silently render clamped.
pub const EXPANDED_WIDTH_MIN: f32 = design::NAVIGATION_MIN_WIDTH;
pub const EXPANDED_WIDTH_MAX: f32 = design::NAVIGATION_MAX_WIDTH;
/// Rendered when a persisted width is not a finite number. Mirrors
/// `settings::AppSettings::default`, so a corrupt file looks like a fresh one.
const DEFAULT_EXPANDED_WIDTH: f32 = design::NAVIGATION_DEFAULT_WIDTH;
const COMPACT_WIDTH: f32 = design::NAVIGATION_RAIL_WIDTH;
const HEADER_HEIGHT: f32 = design::HEADER_CONTENT_HEIGHT;
const PROJECT_HEADER_HEIGHT: f32 = design::ROW_HEIGHT;
const WORKTREE_ROW_HEIGHT: f32 = design::ROW_HEIGHT;
const ICON_CONTROL_SIZE: f32 = design::PRIMARY_CONTROL_SIZE;
const ROW_ICON_WIDTH: f32 = 20.0;
// Sidebar identities use a smaller glyph while retaining the shared row grid.
const ROW_ICON_SIZE: u16 = 14;
const CHILD_ROW_ICON_SIZE: u16 = ROW_ICON_SIZE;
const CHILD_ROW_TEXT_SIZE: f32 = design::TEXT_BODY;
// Controls and status indicators retain the shared workspace chrome sizes.

/// Horizontal step between one row slot and the next, and the unit the rest of
/// the grid is built from. A compile-time number rather than a theme's
/// spacing, because `row_interaction`'s pointer regions and the rail's
/// row width are derived from it and neither of those can ask a theme.
const ROW_GUTTER: f32 = space::XS;
/// The live-indicator slot, reserved whether or not the row has an indicator
/// this frame. A spinner, an attention badge and a chat count all land here,
/// so a label cannot reflow when an agent starts working or a chat opens.
const ROW_STATUS_WIDTH: f32 = design::ROW_ICON_SIZE as f32;
/// One rail row: the leading glyph and the indicator slot, with no gutter
/// between them. `COMPACT_WIDTH` less the list's own inset and the scrollbar
/// rail leaves exactly this much, which is why the rail spends none of it on a
/// hierarchy indent.
const COMPACT_ROW_WIDTH: f32 = ROW_ICON_WIDTH + ROW_STATUS_WIDTH;
/// The project header's hover-revealed commands: disclose, new channel, new
/// worktree. Reserved inside the header row so a long project name ellipsizes
/// instead of running under them, and read by `row_interaction::header_at` so
/// the same three controls stay out of the group's drag region.
const HEADER_ACTION_WIDTH: f32 = 3.0 * ICON_CONTROL_SIZE;

/// The glyph for "a worktree on a branch", shared with the chat status line so
/// the same idea is not drawn two ways.
pub(crate) fn worktree_icon() -> widget::icon::Handle {
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
            if project.repository == path
                && let Some(key) = project.folder_key()
            {
                return Some(key);
            }
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
            project.folder_key().as_ref() == Some(key)
                || project.worktrees.iter().any(|worktree| {
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
    /// completes, while the periodic freshness poll that keeps a repository
    /// card current belongs to the card. `app.rs` routes each of the card's
    /// observations through here to decide whether the panel is reading the
    /// tree that moved.
    pub fn source_control_bound_to(&self, worktree: &Path) -> Option<&ActiveWorktreeKey> {
        self.source_control
            .as_ref()
            .map(|panel| &panel.key)
            .filter(|key| key.canonical_path == worktree)
    }
}

/// A worktree that is being created, or whose creation ended without one.
///
/// The row stands where the worktree will: the same grid, the same name, with
/// the creation's progress in the reserved status slot. What the creation is
/// doing goes in the tooltip and the accessible name; the label never carries
/// a journal identifier, a stage name, or an elapsed time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeOperationRow {
    pub id: crate::model::OperationId,
    /// The worktree's display name, as its row will read once it exists.
    pub label: String,
    /// What the creation is doing, or why it stopped.
    pub detail: String,
    pub phase: WorktreeOperationPhase,
    pub cancellable: bool,
    /// A failure whose input the create dialog can take back, to edit or to
    /// run again.
    pub reopenable: bool,
}

/// A deletion the user has confirmed, as the row being deleted shows it.
///
/// Deleting closes every session inside and waits on Git, so it runs behind
/// the row rather than behind a dialog: the row goes quiet with a spinner
/// until it disappears, or says why it could not and offers to try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowDeletion {
    Deleting,
    Failed(Box<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeOperationPhase {
    /// Still running, or created and waiting for the checkout to list it.
    Working,
    /// Ended without a worktree. The row stays until it is dismissed.
    Failed,
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
    pub kind: ProjectKind,
    pub navigation_order: Vec<crate::workspace_model::NavigationItem>,
    pub worktrees: Vec<SidebarWorktree>,
    pub channels: Vec<SidebarChannel>,
    pub pull_requests: Vec<SidebarPullRequest>,
    pub github_status: String,
    pub github_ready: bool,
    pub cached_display: Option<CachedDisplay>,
    pub operations: Vec<WorktreeOperationRow>,
    pub avatar: Option<cosmic::iced::advanced::image::Handle>,
    /// Why this project's checkout could not be read, set only by a failed
    /// load. A checkout that cannot be read is one unreadable project rather
    /// than an unreadable pane, so the reason travels on the project's own row
    /// and every other project stays listed beside it.
    pub unreachable: Option<String>,
    /// Whether a read of this project is in flight. An unreadable project's
    /// retry is the one control whose effect is invisible until it finishes,
    /// and the reads that fail are the slow ones, so the row has to say that
    /// it is trying rather than look untouched for several seconds.
    pub reloading: bool,
    chrome: ProjectChrome,
}

/// One project-level channel row.
///
/// A channel is a container: `chats` counts the conversations open inside it,
/// which is what keeps the row distinguishable from a single conversation.
/// `has_panes` is wider: any open card counts, a plain shell included, because
/// what it decides is whether the row is somewhere the user has work open.
/// [`ChannelId`] names the container, while each chat inside it is a card with
/// its own `CardId`, so the two levels cannot be mistaken for one another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarChannel {
    pub id: ChannelId,
    pub title: String,
    pub selected: bool,
    pub activity: Option<crate::agent::ActivityState>,
    pub unread: bool,
    pub acknowledged_attention: bool,
    pub chats: usize,
    pub has_panes: bool,
    /// The creation rank the shared registry assigned. Carried on the row
    /// because the row is what gets ordered, and the registry's rank is the
    /// only order two windows can agree on.
    pub ordinal: u32,
    pub deletion: Option<RowDeletion>,
    pub worktree: Option<(WorktreeId, String)>,
    pub outcome: Option<crate::agent::ActivityOutcome>,
    folded_title: Box<str>,
    open_id: Id,
    delete_id: Id,
    delete_name: Box<str>,
    accessible_name: Box<str>,
    tooltip: Box<str>,
}

impl SidebarChannel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ChannelId,
        title: String,
        selected: bool,
        activity: Option<crate::agent::ActivityState>,
        unread: bool,
        chats: usize,
        has_panes: bool,
        ordinal: u32,
    ) -> Self {
        let contents = match chats {
            1 => "1 chat".to_owned(),
            other => format!("{other} chats"),
        };
        Self {
            id,
            selected,
            activity,
            unread,
            acknowledged_attention: false,
            chats,
            has_panes,
            ordinal,
            deletion: None,
            worktree: None,
            outcome: None,
            folded_title: fold(&title),
            open_id: entity_id("channel.open", id),
            delete_id: entity_id("channel.delete", id),
            delete_name: format!("Delete channel {title}").into_boxed_str(),
            accessible_name: format!(
                "Open channel {title}, {contents}{}{}",
                if has_panes { "" } else { ", no open panes" },
                if unread { ", unread agent update" } else { "" }
            )
            .into_boxed_str(),
            tooltip: format!("{title} · {contents}").into_boxed_str(),
            title,
        }
    }
}

/// Attribute a channel only when all of its launch directories resolve to one
/// known root. Mixed-worktree channels deliberately keep their project scope.
pub fn channel_worktree<'a>(
    worktrees: &[SidebarWorktree],
    directories: impl IntoIterator<Item = &'a Path>,
) -> Option<(WorktreeId, String)> {
    let roots: std::collections::HashMap<_, _> = worktrees
        .iter()
        .map(|worktree| (worktree.path.as_path(), worktree))
        .collect();
    let mut owner = None;
    for directory in directories {
        let worktree = directory
            .ancestors()
            .find_map(|ancestor| roots.get(ancestor).copied())?;
        if owner.as_ref().is_some_and(|(id, _)| *id != worktree.id) {
            return None;
        }
        owner = Some((
            worktree.id,
            worktree
                .branch
                .clone()
                .unwrap_or_else(|| worktree.label().to_owned()),
        ));
    }
    owner
}

impl SidebarChannel {
    pub fn set_attribution(
        &mut self,
        worktree: Option<(WorktreeId, String)>,
        outcome: Option<crate::agent::ActivityOutcome>,
    ) {
        self.worktree = worktree;
        self.outcome = outcome;
        let branch = self
            .worktree
            .as_ref()
            .map(|(_, branch)| format!(" · {branch}"))
            .unwrap_or_default();
        let failure = self
            .outcome
            .as_ref()
            .map(|outcome| {
                outcome.reason().map_or_else(
                    || {
                        if matches!(outcome, crate::agent::ActivityOutcome::Cancelled) {
                            format!(" · {}", outcome.label())
                        } else {
                            String::new()
                        }
                    },
                    |reason| format!(" · {}: {reason}", outcome.label()),
                )
            })
            .unwrap_or_default();
        let contents = format!(
            "{} chat{}",
            self.chats,
            if self.chats == 1 { "" } else { "s" }
        );
        self.tooltip = format!("{} · {contents}{branch}{failure}", self.title).into_boxed_str();
        self.accessible_name = format!(
            "Open channel {}, {contents}{}{}{branch}{failure}",
            self.title,
            if self.has_panes {
                ""
            } else {
                ", no open panes"
            },
            if self.unread {
                ", unread agent update"
            } else {
                ""
            }
        )
        .into_boxed_str();
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
            kind: ProjectKind::Git,
            navigation_order: Vec::new(),
            worktrees,
            channels: Vec::new(),
            pull_requests,
            github_status: "GitHub has not been refreshed.".into(),
            github_ready: false,
            cached_display: None,
            operations: Vec::new(),
            avatar: None,
            unreachable: None,
            reloading: false,
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

    pub fn with_kind(mut self, kind: ProjectKind) -> Self {
        self.kind = kind;
        self
    }

    /// The folder browser reuses the existing directory binding key without
    /// manufacturing a Git worktree row or durable WorktreeState.
    pub fn folder_key(&self) -> Option<ActiveWorktreeKey> {
        (self.kind == ProjectKind::Folder).then(|| ActiveWorktreeKey {
            project_id: self.id,
            worktree_id: WorktreeId::from_uuid(self.id.as_uuid()),
            canonical_path: self.repository.clone(),
        })
    }

    /// Whether one of this project's worktrees has a pull request still
    /// open, whose checks and review can change without anything here
    /// noticing.
    pub fn has_open_pull_request(&self) -> bool {
        self.worktrees.iter().any(|worktree| {
            matches!(
                worktree.pull_request_state,
                Some(WorktreePullRequestState::Open | WorktreePullRequestState::Draft)
            )
        })
    }

    pub fn apply_review_requests(
        &mut self,
        requested: &std::collections::BTreeMap<
            u64,
            crate::github::review_requests::scan::Requested,
        >,
    ) {
        let apply = |row: &mut SidebarPullRequest| {
            let request = requested.get(&row.number);
            row.needs_review = request.is_some();
            row.review_requested_again = request.is_some_and(|request| {
                request.history == crate::github::review_requests::scan::History::ReRequested
            });
        };
        for row in &mut self.pull_requests {
            apply(row);
        }
        if let Some(cached) = &mut self.cached_display {
            for row in &mut cached.rows {
                apply(row);
            }
        }
        for worktree in &mut self.worktrees {
            if let Some(mut row) = worktree.pull_request().cloned() {
                apply(&mut row);
                if worktree.pull_request() != Some(&row) {
                    worktree.set_pull_request(row);
                }
            }
        }
    }

    pub fn replace_pull_requests(&mut self, pull_requests: Vec<SidebarPullRequest>) {
        self.pull_requests = pull_requests;
        self.cached_display = None;
        self.refresh_worktree_pull_requests();
    }

    pub fn refresh_worktree_pull_requests(&mut self) {
        for row in &mut self.worktrees {
            if let Some(pr) = matching_pull_request(row, &self.pull_requests) {
                row.set_pull_request(pr.clone());
            } else if row.pull_request().is_some_and(|pr| {
                !matches!(
                    pr.state,
                    Some(WorktreePullRequestState::Merged | WorktreePullRequestState::Closed)
                )
            }) {
                row.clear_pull_request();
            }
        }
    }

    pub fn display_pull_requests(&self) -> &[SidebarPullRequest] {
        self.cached_display
            .as_ref()
            .map_or(&self.pull_requests, |cached| &cached.rows)
    }

    /// Only a completed refresh that reports the repository unusable revokes
    /// authority. The rows it had already listed survive as presentation so a
    /// revoked project still shows what it last knew.
    pub fn revoke_github(&mut self) {
        if !self.pull_requests.is_empty() {
            self.cached_display = Some(CachedDisplay {
                rows: std::mem::take(&mut self.pull_requests),
            });
        }
        self.github_ready = false;
    }

    fn matches(&self, folded_query: &str) -> bool {
        self.search_rank(folded_query).is_some()
    }

    fn search_rank(&self, query: &str) -> Option<usize> {
        search_score(&self.chrome.folded_name, query).or_else(|| {
            query
                .contains('/')
                .then(|| self.chrome.folded_repository.find(query))
                .flatten()
                .map(|position| 10_000 + position)
        })
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
    new_channel_id: Id,
    retry_id: Id,
    open_name: Box<str>,
    compact_open_name: Box<str>,
    collapse_name: Box<str>,
    expand_name: Box<str>,
    new_worktree_name: Box<str>,
    new_channel_name: Box<str>,
}

impl ProjectChrome {
    fn new(id: ProjectId, name: &str, repository: &Path) -> Self {
        Self {
            folded_name: fold(name),
            folded_repository: fold(&repository.to_string_lossy()),
            open_id: entity_id("project.open", id),
            toggle_id: entity_id("project.toggle", id),
            new_worktree_id: entity_id("project.new_worktree", id),
            new_channel_id: entity_id("project.new_channel", id),
            retry_id: entity_id("project.retry", id),
            open_name: format!("Open project {name}").into_boxed_str(),
            compact_open_name: format!("Open {name}").into_boxed_str(),
            collapse_name: format!("Collapse {name} project").into_boxed_str(),
            expand_name: format!("Expand {name} project").into_boxed_str(),
            new_worktree_name: format!("New worktree for {name}").into_boxed_str(),
            new_channel_name: format!("New channel for {name}").into_boxed_str(),
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
    pub head_repository: Option<crate::github::GitHubRepoKey>,
    pub detached: bool,
    pub locked: bool,
    pub dirty: Option<DirtyState>,
    pub untracked_omitted: bool,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub created_at_unix_ms: Option<i64>,
    pub pull_request_state: Option<WorktreePullRequestState>,
    /// Held by the application rather than read from the checkout, and
    /// reapplied whenever a read replaces the row.
    pub deletion: Option<RowDeletion>,
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
        let status = status_summary(detached, locked, dirty.as_ref(), false, ahead, behind);
        let accessible_name = format!(
            "Open worktree {label}, {}, {status}",
            branch.as_deref().unwrap_or(&path_label)
        );
        let chrome = WorktreeChrome {
            folded_label: fold(&label),
            folded_branch: branch.as_deref().map(fold),
            folded_path: fold(&path.to_string_lossy()),
            open_id: entity_id("worktree.open", id),
            delete_id: entity_id("worktree.delete", id),
            delete_name: format!("Delete merged worktree {label}").into_boxed_str(),
            paneless_accessible_name: paneless_name(&accessible_name),
            accessible_name: accessible_name.into_boxed_str(),
            tooltip: format!(
                "{label}\nBranch: {}\n{path_label}\n{status}",
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
            untracked_omitted: false,
            ahead,
            behind,
            created_at_unix_ms: None,
            head_repository: None,
            pull_request_state: None,
            deletion: None,
            chrome,
        }
    }

    /// The name the row shows: the branch's last segment, or the directory.
    pub fn label(&self) -> &str {
        &self.chrome.label
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
        row.untracked_omitted = status.is_some_and(|status| status.untracked_omitted);
        row.refresh_status_chrome();
        row.created_at_unix_ms = record.created_at_unix_ms;
        row
    }

    /// Apply each authoritative read, including the transition back to clean.
    pub fn apply_repository(&mut self, record: &WorktreeRecord, status: Option<&StatusSnapshot>) {
        let branch = record.branch.as_deref().map(|branch| {
            branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_owned()
        });
        if self.branch != branch || self.detached != record.detached {
            self.clear_pull_request();
            self.branch = branch;
            self.detached = record.detached;
            self.chrome.folded_branch = self.branch.as_deref().map(fold);
        }
        self.locked = record.lock.locked;
        self.created_at_unix_ms = record.created_at_unix_ms;
        if let Some(status) = status {
            self.apply_status(status);
        }
        self.refresh_status_chrome();
    }

    /// Apply each authoritative read, including the transition back to clean.
    pub fn apply_status(&mut self, status: &StatusSnapshot) {
        let (ahead, behind) = match &status.tracking {
            TrackingState::Compared { ahead, behind, .. } => (Some(*ahead), Some(*behind)),
            _ => (None, None),
        };
        if self.untracked_omitted == status.untracked_omitted
            && self.dirty.as_ref() == Some(&status.dirty)
            && self.ahead == ahead
            && self.behind == behind
        {
            return;
        }
        self.dirty = Some(status.dirty.clone());
        self.untracked_omitted = status.untracked_omitted;
        self.ahead = ahead;
        self.behind = behind;
        self.refresh_status_chrome();
    }

    fn matches(&self, folded_query: &str) -> bool {
        self.search_rank(folded_query).is_some()
    }

    fn search_rank(&self, query: &str) -> Option<usize> {
        search_score(&self.chrome.folded_label, query)
            .into_iter()
            .chain(
                self.chrome
                    .folded_branch
                    .as_deref()
                    .and_then(|branch| search_score(branch, query)),
            )
            .min()
            .or_else(|| {
                query
                    .contains('/')
                    .then(|| self.chrome.folded_path.find(query))
                    .flatten()
                    .map(|position| 10_000 + position)
            })
    }

    pub fn set_pull_request(&mut self, pull_request: SidebarPullRequest) {
        self.pull_request_state = pull_request.state;
        self.chrome.pull_request = Some(pull_request);
        self.refresh_status_chrome();
    }

    pub fn pull_request(&self) -> Option<&SidebarPullRequest> {
        self.chrome.pull_request.as_ref()
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
            self.chrome.delete_name = format!("Delete merged worktree {label}").into_boxed_str();
            self.chrome.label = label.into();
        }
        self.chrome.dirty_badge =
            self.dirty
                .as_ref()
                .filter(|dirty| dirty.is_dirty())
                .map(|dirty| {
                    format!(
                        "{} {}changes",
                        dirty_count(dirty),
                        if self.untracked_omitted {
                            "tracked "
                        } else {
                            ""
                        }
                    )
                    .into_boxed_str()
                });
        let mut status = status_summary(
            self.detached,
            self.locked,
            self.dirty.as_ref(),
            self.untracked_omitted,
            self.ahead,
            self.behind,
        );
        if let Some(pr) = &self.chrome.pull_request {
            use std::fmt::Write as _;
            let state = match pr.state {
                Some(WorktreePullRequestState::Draft) => "draft",
                Some(WorktreePullRequestState::Open) => "open",
                Some(WorktreePullRequestState::Closed) => "closed",
                Some(WorktreePullRequestState::Merged) => "merged",
                None => "unknown state",
            };
            let _ = write!(status, ", pull request #{} {state}", pr.number,);
            if let Some(checks) = pr.active_checks() {
                let _ = write!(status, ", checks {}", check_description(checks));
            }
            if pr.review_requested() {
                status.push_str(if pr.review_requested_again {
                    ", review re-requested from you"
                } else {
                    ", needs your review"
                });
            }
        }
        let branch = self.branch.as_deref().unwrap_or("Detached HEAD");
        let accessible_name = format!("Open worktree {}, {branch}, {status}", self.chrome.label);
        self.chrome.paneless_accessible_name = paneless_name(&accessible_name);
        self.chrome.accessible_name = accessible_name.into_boxed_str();
        self.chrome.tooltip = format!(
            "{}\nBranch: {branch}\n{}\n{status}",
            self.chrome.label,
            self.path.display()
        )
        .into_boxed_str();
    }
}

fn paneless_name(accessible_name: &str) -> Box<str> {
    format!("{accessible_name}, no open panes").into_boxed_str()
}

pub(crate) fn worktree_display_name(path: &Path, branch: Option<&str>, detached: bool) -> String {
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
/// A project's PR list includes other forks. Match source repository as well
/// as branch, and refuse ambiguous associations instead of using list order.
fn matching_pull_request<'a>(
    worktree: &SidebarWorktree,
    pull_requests: &'a [SidebarPullRequest],
) -> Option<&'a SidebarPullRequest> {
    if worktree.detached {
        return None;
    }
    let branch = worktree.branch.as_deref()?;
    let repository = worktree.head_repository.as_ref()?;
    let matching = || {
        pull_requests.iter().filter(|pr| {
            pr.head_ref == branch
                && pr
                    .head_repository
                    .as_ref()
                    .is_some_and(|head| head.same_repository(repository))
        })
    };
    let is_open = |pr: &SidebarPullRequest| {
        matches!(
            pr.state,
            Some(WorktreePullRequestState::Open | WorktreePullRequestState::Draft)
        )
    };
    let has_open = matching().any(is_open);
    let mut matching = matching().filter(|pr| !has_open || is_open(pr));
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

/// Presentation strings and widget identities a worktree determines, built
/// once in `SidebarWorktree::new`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeChrome {
    folded_label: Box<str>,
    folded_branch: Option<Box<str>>,
    folded_path: Box<str>,
    open_id: Id,
    delete_id: Id,
    delete_name: Box<str>,
    label: Box<str>,
    accessible_name: Box<str>,
    /// `accessible_name` for the row with nothing open, which is most rows in
    /// a long list, built once so naming them allocates nothing per frame.
    paneless_accessible_name: Box<str>,
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
    pub head_repository: Option<crate::github::GitHubRepoKey>,
    pub author: Option<String>,
    pub draft: bool,
    pub checks: CheckStatus,
    pub state: Option<WorktreePullRequestState>,
    pub needs_review: bool,
    pub review_requested_again: bool,
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
            head_repository: None,
            author,
            draft,
            checks,
            state: None,
            needs_review: false,
            review_requested_again: false,
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
        row.head_repository = pull_request.head_repository_key();
        row.head_oid = Some(pull_request.head_ref_oid.clone());
        // REVIEW_REQUIRED is branch policy, not a request for the current viewer.
        // The authenticated request scanner supplies viewer-specific authority.
        row
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            Some(WorktreePullRequestState::Merged | WorktreePullRequestState::Closed)
        )
    }

    // Keep the last remote facts for a later refresh or reopening, but do not
    // present them as outstanding work after the pull request has finished.
    fn review_requested(&self) -> bool {
        self.needs_review && !self.is_terminal()
    }

    fn active_checks(&self) -> Option<CheckStatus> {
        (!self.is_terminal()).then_some(self.checks)
    }

    fn has_pending_checks(&self) -> bool {
        self.active_checks() == Some(CheckStatus::Pending)
    }
}

/// One semantic mapping for navigation, the inspector and its tab.
///
/// This resolves the tone against the active theme, which locks the
/// process-global theme and clones it. A per-row builder must use
/// [`pull_request_icon`] with [`pull_request_tone`] instead and let the row's
/// own style closure resolve the color from the theme it is handed.
pub fn pull_request_visual(pr: &SidebarPullRequest) -> (widget::icon::Handle, cosmic::iced::Color) {
    let theme = cosmic::theme::active();
    (pull_request_icon(pr), pull_request_tone(pr).color(&theme))
}

/// The glyph a pull request draws. No theme decides it.
fn pull_request_icon(pr: &SidebarPullRequest) -> widget::icon::Handle {
    Icon::handle(match pr.state {
        Some(WorktreePullRequestState::Merged) => Icon::MergedPullRequest,
        Some(WorktreePullRequestState::Closed) => Icon::ClosedPullRequest,
        _ => Icon::PullRequest,
    })
}

/// The semantic role that decides a pull request's tone.
///
/// Small and `Copy` so a row's style closure can carry it and resolve the color
/// itself, which is the only way to keep the lookup off the global theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestTone {
    Review,
    Merged,
    Danger,
    Draft,
    Neutral,
}

fn pull_request_tone(pr: &SidebarPullRequest) -> PullRequestTone {
    match pr.state {
        Some(WorktreePullRequestState::Merged) => PullRequestTone::Merged,
        Some(WorktreePullRequestState::Closed) => PullRequestTone::Danger,
        _ if pr.review_requested() => PullRequestTone::Review,
        _ if pr.draft => PullRequestTone::Draft,
        _ => PullRequestTone::Neutral,
    }
}

impl PullRequestTone {
    fn color(self, theme: &cosmic::Theme) -> cosmic::iced::Color {
        let cosmic = theme.cosmic();
        match self {
            // Merged is GitHub's purple, which no COSMIC role names.
            Self::Merged => cosmic::iced::Color::from_rgb8(163, 113, 247),
            Self::Review => design::accent_color(theme),
            Self::Danger => cosmic.destructive.base.into(),
            Self::Draft | Self::Neutral => design::muted_icon_color(theme),
        }
    }
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

/// The collapsible sections of the Changes tab: one per working-tree change
/// area, so every group discloses the same way rather than `Untracked` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PanelSection {
    Conflicts,
    Staged,
    Unstaged,
    Untracked,
}

impl PanelSection {
    pub const ALL: [Self; 4] = [
        Self::Conflicts,
        Self::Staged,
        Self::Unstaged,
        Self::Untracked,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::Untracked => "Untracked",
        }
    }

    fn for_area(area: ChangeArea) -> Self {
        match area {
            ChangeArea::Conflict => Self::Conflicts,
            ChangeArea::Staged => Self::Staged,
            ChangeArea::Unstaged => Self::Unstaged,
            ChangeArea::Untracked => Self::Untracked,
        }
    }

    /// Show all nonignored working changes immediately, including new files.
    fn expanded_by_default(self) -> bool {
        true
    }
}

/// Which view of the worktree's Git state the Git panel is showing.
///
/// Three peers, not a boolean: working-tree changes, this branch's commits,
/// and the diff against the branch's base are separate questions, and the
/// panel used to answer the third by stacking it under the first. They are
/// ordered from nearest to farthest: what is not committed yet, what this
/// branch changes, then every commit behind it.
///
/// Local to the surface and never persisted; see [`SidebarState`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangesTab {
    #[default]
    Changes,
    Branch,
    History,
}

impl ChangesTab {
    pub const ALL: [Self; 3] = [Self::Changes, Self::Branch, Self::History];

    fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Branch => "Branch",
            Self::History => "History",
        }
    }

    /// The accessible name and tooltip, which say what the short tab label
    /// cannot fit.
    fn description(self) -> &'static str {
        match self {
            Self::Changes => "Uncommitted changes in this worktree",
            Self::Branch => "Everything this branch changed since it left its base",
            Self::History => "Commit history",
        }
    }

    /// Whether the view is a list of changed files, which is what the
    /// tree/flat switch arranges. History lists commits and has no such
    /// choice to offer.
    fn lists_files(self) -> bool {
        matches!(self, Self::Changes | Self::Branch)
    }
}

/// Which source-control mutation is currently running.
///
/// Exactly one runs at a time per worktree - the application serializes them
/// per canonical worktree - so this is an option, not a set.
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
    pub ignored: bool,
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
            stage_all_id: stable_id(format!("panel.group.stage.{key}")),
            unstage_all_id: stable_id(format!("panel.group.unstage.{key}")),
            discard_all_id: stable_id(format!("panel.group.discard.{key}")),
            stage_all_name: format!("Stage all {title}").into_boxed_str(),
            unstage_all_name: format!("Unstage all {title}").into_boxed_str(),
            discard_all_name: format!("Discard all {title}").into_boxed_str(),
        };
        let paths = rows
            .iter()
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

    /// Complete group rows; the view mounts a bounded page of this model.
    pub fn visible(&self) -> &[ChangeRow] {
        &self.rows
    }

    pub fn hidden(&self) -> usize {
        0
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The path selection covering the entire group, rename halves included.
    pub fn visible_selection(&self) -> Vec<PathSelection> {
        self.visible().iter().map(ChangeRow::selection).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangeGroupChrome {
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
    pub parent: Option<Oid>,
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
            parent: commit.parents.first().cloned(),
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
pub struct CommitChanges {
    pub commit: CommitRow,
    pub visible_files: usize,
    pub result: Option<Result<Vec<crate::repository::DiffEntry>, String>>,
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
    /// The row count the Branch tab states beside its name.
    count_label: Box<str>,
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
        let paths: Vec<_> = rows.iter().map(|row| row.entry.path.clone()).collect();
        Self {
            count_label: tab_count_label(rows.len()),
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
    pub ignored_directory_errors: BTreeMap<RepoPath, String>,
    groups: Vec<ChangeGroup>,
    /// How many distinct paths the change groups hold, as the Changes tab
    /// states it; `None` for a clean worktree. A path both staged and
    /// unstaged is one changed file, not two.
    change_count_label: Option<Box<str>>,
    pub branch_changes: BranchChanges,
    pub history: Vec<CommitRow>,
    pub commit_changes: Option<CommitChanges>,
    /// False until a page has landed. Without it an empty `history` is
    /// indistinguishable from a branch that has no commits.
    pub history_loaded: bool,
    /// True once a page has been asked for and landed, and then for as long as
    /// the panel stays bound to this worktree. Separate from `history_loaded`,
    /// which a moved `HEAD` clears; see [`Self::history_requested`].
    history_requested: bool,
    pub history_loading: bool,
    pub history_error: Option<String>,
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
                ignored: false,
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
            ignored_directory_errors: BTreeMap::new(),
            groups: Vec::new(),
            change_count_label: None,
            branch_changes: BranchChanges::Unavailable("No branch comparison available."),
            history: Vec::new(),
            commit_changes: None,
            history_loaded: false,
            history_requested: false,
            history_loading: false,
            history_error: None,
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
        let changed_paths = groups
            .iter()
            .flat_map(|group| group.rows.iter().map(|row| &row.entry.path))
            .collect::<BTreeSet<_>>()
            .len();
        let change_count_label = (changed_paths > 0).then(|| tab_count_label(changed_paths));
        let mut paths;
        let paths = if snapshot.files.ignored.is_empty() {
            &snapshot.files.paths
        } else {
            paths = snapshot.files.paths.clone();
            paths.extend(snapshot.files.ignored.iter().cloned());
            &paths
        };
        let mut files = file_rows_from_tree(build_file_tree_with_directories(
            paths,
            &snapshot.files.ignored_directories,
        ));
        let ignored: BTreeSet<_> = snapshot.files.ignored.iter().collect();
        let ignored_directories: BTreeSet<_> = snapshot
            .files
            .ignored_directories
            .iter()
            .map(|path| path.as_path())
            .collect();
        for row in &mut files {
            row.ignored = ignored.contains(&row.path)
                || row
                    .path
                    .as_path()
                    .ancestors()
                    .any(|path| ignored_directories.contains(path));
            // A force-added file inside an ignored folder is still tracked.
            if row.ignored
                && !row.directory
                && snapshot.files.paths.binary_search(&row.path).is_ok()
            {
                row.ignored = false;
            }
        }

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
            ignored_directory_errors: snapshot.files.ignored_directory_errors.clone(),
            groups,
            change_count_label,
            branch_changes: BranchChanges::Unavailable("No branch comparison available."),
            history: Vec::new(),
            commit_changes: None,
            history_loaded: false,
            history_requested: false,
            history_loading: false,
            history_error: None,
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
        self.history_loading = false;
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
        !self.history_loading && !self.history_complete && !self.unborn
    }

    /// Records that the first page has been asked for, so expanding, closing,
    /// and reopening `History` cannot queue a second walk.
    pub fn history_failed(&mut self, error: String) {
        self.history_loading = false;
        self.history_error = Some(error);
    }

    pub fn mark_history_requested(&mut self) {
        self.history_error = None;
        self.history_loading = true;
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
        next.commit_changes = self.commit_changes.clone();
        // The loading placeholder between two snapshots has not read `HEAD`
        // yet, and that gap must not look like a `HEAD` that moved.
        if next.head_oid.is_none() && !next.unborn {
            next.head_oid = self.head_oid.clone();
        }
        if next.head_oid == self.head_oid && next.history.is_empty() {
            next.history_requested = self.history_requested;
            next.history_loading = self.history_loading;
            next.history_error = self.history_error.clone();
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
    commit_description_id: Id,
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
    changes_tab_ids: [Id; 3],
    panel_tab_ids: [Id; 3],
    list_mode_id: Id,
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
            commit_description_id: entity_id("panel.commit.description", worktree_id),
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
            changes_tab_ids: ChangesTab::ALL
                .map(|tab| entity_id("panel.tab", format!("{worktree_id}.{tab:?}"))),
            panel_tab_ids: WorktreePanel::ALL
                .map(|panel| entity_id("panel.view", format!("{worktree_id}.{panel:?}"))),
            list_mode_id: entity_id("panel.list-mode", worktree_id),
        }
    }

    fn section_id(&self, section: PanelSection) -> &Id {
        &self.section_ids[section as usize]
    }

    fn changes_tab_id(&self, tab: ChangesTab) -> &Id {
        &self.changes_tab_ids[tab as usize]
    }

    fn panel_tab_id(&self, panel: WorktreePanel) -> &Id {
        &self.panel_tab_ids[panel as usize]
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
    Commit { oid: Oid, path: RepoPath },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileListMode {
    #[default]
    Tree,
    Flat,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ChangeTreeScope {
    Branch,
    Area(ChangeArea),
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SidebarView {
    query: String,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
    #[serde(default)]
    expanded_directories: BTreeSet<RepoPath>,
    #[serde(default)]
    collapsed_change_directories: Vec<(ChangeTreeScope, Vec<RepoPath>)>,
    #[serde(default)]
    agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneActivity {
    owner: Option<(ProjectId, WorktreeId)>,
    activity: crate::agent::Activity,
}

/// Shared display priority for panes, channels and worktrees. Live waiting and
/// failed work require intervention even after their unread notice is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AttentionState {
    Cancelled,
    Unread,
    Working,
    Failed,
    Waiting,
}

pub fn cycle_attention(
    cards: &[CardId],
    current: Option<CardId>,
    backwards: bool,
) -> Option<CardId> {
    if cards.is_empty() {
        return None;
    }
    let index = current.and_then(|id| cards.iter().position(|card| *card == id));
    let index = match (index, backwards) {
        (Some(index), true) => (index + cards.len() - 1) % cards.len(),
        (Some(index), false) => (index + 1) % cards.len(),
        (None, true) => cards.len() - 1,
        (None, false) => 0,
    };
    Some(cards[index])
}

pub(crate) fn outcome_priority(outcome: &crate::agent::ActivityOutcome) -> u8 {
    if outcome.reason().is_some() {
        2
    } else if matches!(outcome, crate::agent::ActivityOutcome::Cancelled) {
        1
    } else {
        0
    }
}

pub fn attention_state(
    activity: Option<crate::agent::ActivityState>,
    unread: bool,
    outcome: Option<&crate::agent::ActivityOutcome>,
) -> Option<AttentionState> {
    use crate::agent::{ActivityOutcome, ActivityState};
    match (activity, outcome) {
        (Some(ActivityState::Waiting), _) => Some(AttentionState::Waiting),
        (_, Some(outcome)) if outcome.reason().is_some() => Some(AttentionState::Failed),
        (Some(ActivityState::Working), _) => Some(AttentionState::Working),
        (_, outcome) => match outcome {
            Some(ActivityOutcome::Cancelled) if !unread => Some(AttentionState::Cancelled),
            _ => unread.then_some(AttentionState::Unread),
        },
    }
}

impl AttentionState {
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Waiting | Self::Failed | Self::Unread)
    }
}

/// Persistent interaction state owned by the application.
///
/// Everything here is local to the surface: what is typed, what is open, and
/// what is selected. Nothing that a Git or GitHub call produced lives here -
/// those are immutable snapshots on [`SidebarModel`].
#[derive(Debug, Default)]
struct CommitComposer(cosmic::iced::widget::text_editor::Content<cosmic::Renderer>);

impl Clone for CommitComposer {
    fn clone(&self) -> Self {
        Self(cosmic::iced::widget::text_editor::Content::with_text(
            &self.0.text(),
        ))
    }
}
impl PartialEq for CommitComposer {
    fn eq(&self, other: &Self) -> bool {
        self.0.text() == other.0.text()
    }
}
impl Eq for CommitComposer {}

/// The complete composer travels with its canonical worktree, including
/// cursor and editor history. Revisions distinguish later edits from a commit
/// that was submitted before navigation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CommitDraft {
    message: String,
    subject: String,
    subject_composer: CommitComposer,
    composer: CommitComposer,
    amend: bool,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    rows: selection::Selection,
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
    /// Which view of the Git panel is showing. Local to the surface, like
    /// every other draft here, so a worktree switch resets it.
    changes_tab: ChangesTab,
    worktree_sort: crate::settings::WorktreeSort,
    hide_worktree_changes_icons: bool,
    agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    agent_outcomes: BTreeMap<CardId, crate::agent::ActivityOutcome>,
    retained_attention: BTreeSet<SelectionRow>,
    agent_activity: BTreeMap<CardId, PaneActivity>,
    /// Worktrees holding at least one open pane. Derived by the application
    /// after its updates, never while drawing, so each row's read is one
    /// lookup however many cards are open.
    worktrees_with_panes: BTreeSet<(ProjectId, WorktreeId)>,
    commit_draft: CommitDraft,
    parked_commit_drafts: BTreeMap<PathBuf, CommitDraft>,
    commit_generation: u64,
    /// File-tree folders the user opened. Everything else renders collapsed,
    /// so a fresh worktree shows its top level and loading a whole project
    /// tree stays cheap.
    expanded_directories: BTreeSet<RepoPath>,
    /// Change review starts open; only folders the user folded are stored.
    collapsed_change_directories: BTreeMap<ChangeTreeScope, BTreeSet<RepoPath>>,
    change_pages: BTreeMap<ChangeTreeScope, usize>,
    files_page: usize,
    publish: Option<PublishDraft>,
    discard: Option<DiscardConfirmation>,
    panel_selection: Option<PanelSelection>,
}

impl SidebarState {
    pub fn set_show_worktree_changes_icons(&mut self, show: bool) {
        self.hide_worktree_changes_icons = !show;
    }

    pub fn select_rows(
        &mut self,
        row: SelectionRow,
        intent: SelectionIntent,
        visible: &[SelectionRow],
    ) {
        self.rows.select(row, intent, visible);
    }

    pub fn selected_rows(&self, visible: &[SelectionRow]) -> Vec<SelectionRow> {
        self.rows.ordered(visible)
    }

    pub fn prune_row_selection(&mut self, visible: &[SelectionRow]) {
        self.rows.retain_visible(visible);
    }

    pub fn clear_row_selection(&mut self) {
        self.rows.clear();
    }
    pub fn has_row_selection(&self) -> bool {
        !self.rows.is_empty()
    }
    /// Whether the search controls narrow the list: a query, or the pull
    /// request filter that lives beside it. Attention is not counted; its
    /// toggle stays in the header whether or not the controls show.
    pub fn search_filters_active(&self) -> bool {
        !self.query.trim().is_empty() || self.filter == SidebarFilter::PullRequests
    }

    /// Hiding the search controls ends the search. A query or filter nobody
    /// can see would keep hiding rows, and the pull request filter would keep
    /// changing what a click on a row does, with nothing on screen to say so.
    fn clear_search_filters(&mut self) {
        self.query.clear();
        if self.filter == SidebarFilter::PullRequests {
            self.filter = SidebarFilter::All;
        }
    }

    pub fn saved_view(&self) -> SidebarView {
        SidebarView {
            // A hidden query is not applied, so there is nothing to restore.
            query: if self.controls_expanded {
                self.query.clone()
            } else {
                String::new()
            },
            controls_expanded: self.controls_expanded,
            collapsed_projects: self.collapsed_projects.clone(),
            selection: self.selection,
            agent_updates: self.agent_updates.clone(),
            expanded_directories: self.expanded_directories.clone(),
            collapsed_change_directories: self
                .collapsed_change_directories
                .iter()
                .map(|(scope, paths)| (*scope, paths.iter().cloned().collect()))
                .collect(),
        }
    }

    pub fn restore_view(&mut self, view: SidebarView) {
        // Views saved before hidden queries were dropped may still hold one.
        self.query = if view.controls_expanded {
            view.query
        } else {
            String::new()
        };
        self.controls_expanded = view.controls_expanded;
        self.collapsed_projects = view.collapsed_projects;
        self.selection = view.selection;
        self.agent_updates = view.agent_updates;
        self.expanded_directories = view.expanded_directories;
        self.collapsed_change_directories = view
            .collapsed_change_directories
            .into_iter()
            .map(|(scope, paths)| (scope, paths.into_iter().collect()))
            .collect();
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
        if self.filter == SidebarFilter::Attention
            && let Some(Some((project, worktree))) = self.agent_updates.get(&card)
        {
            self.retained_attention
                .insert(SelectionRow::Worktree(*project, *worktree));
        }
        self.agent_updates.remove(&card);
    }

    pub fn unread_updates(&self) -> &BTreeMap<CardId, Option<(ProjectId, WorktreeId)>> {
        &self.agent_updates
    }

    pub fn restore_unread_updates(
        &mut self,
        updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    ) {
        self.agent_updates = updates;
    }

    pub fn has_card_update(&self, card: CardId) -> bool {
        self.agent_updates.contains_key(&card)
    }

    pub fn has_agent_update(&self, project: ProjectId, worktree: WorktreeId) -> bool {
        self.agent_updates
            .values()
            .any(|owner| *owner == Some((project, worktree)))
    }

    /// Replaces the worktrees that hold an open pane.
    pub fn set_worktrees_with_panes(&mut self, worktrees: BTreeSet<(ProjectId, WorktreeId)>) {
        self.worktrees_with_panes = worktrees;
    }

    pub fn worktree_has_panes(&self, project: ProjectId, worktree: WorktreeId) -> bool {
        self.worktrees_with_panes.contains(&(project, worktree))
    }

    pub fn set_card_outcome(
        &mut self,
        card: CardId,
        outcome: Option<crate::agent::ActivityOutcome>,
    ) {
        if let Some(outcome) = outcome {
            self.agent_outcomes.insert(card, outcome);
        } else {
            self.agent_outcomes.remove(&card);
        }
    }

    pub fn card_outcome(&self, card: CardId) -> Option<&crate::agent::ActivityOutcome> {
        self.agent_outcomes.get(&card)
    }

    pub fn card_attention(&self, card: CardId) -> Option<AttentionState> {
        attention_state(
            self.card_activity(card).map(|a| a.state),
            self.has_card_update(card),
            self.card_outcome(card),
        )
    }

    pub fn worktree_outcome(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<&crate::agent::ActivityOutcome> {
        self.agent_activity
            .iter()
            .filter(|(_, entry)| entry.owner == Some((project, worktree)))
            .filter_map(|(card, _)| self.card_outcome(*card))
            .max_by_key(|outcome| outcome_priority(outcome))
    }

    pub fn worktree_attention(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<AttentionState> {
        self.agent_activity
            .iter()
            .filter(|(_, entry)| entry.owner == Some((project, worktree)))
            .filter_map(|(card, _)| self.card_attention(*card))
            .chain(
                self.has_agent_update(project, worktree)
                    .then_some(AttentionState::Unread),
            )
            .max()
    }

    /// Stable across workspaces; rank intervention ahead of completion, then
    /// use pane identity to break ties without hash-map iteration jitter.
    pub fn attention_cards(&self) -> Vec<CardId> {
        let cards: BTreeSet<_> = self
            .agent_activity
            .keys()
            .chain(self.agent_updates.keys())
            .copied()
            .collect();
        let mut cards: Vec<_> = cards
            .into_iter()
            .filter_map(|card| {
                self.card_attention(card).and_then(|state| {
                    if matches!(state, AttentionState::Waiting | AttentionState::Failed) {
                        Some((state, card))
                    } else if self.has_card_update(card) {
                        Some((AttentionState::Unread, card))
                    } else {
                        None
                    }
                })
            })
            .collect();
        cards.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        cards.into_iter().map(|(_, card)| card).collect()
    }

    pub fn waiting_count(&self) -> usize {
        self.agent_activity
            .values()
            .filter(|entry| entry.activity.state == crate::agent::ActivityState::Waiting)
            .count()
    }

    pub fn intervention_count(&self) -> usize {
        self.agent_activity
            .keys()
            .filter(|card| {
                matches!(
                    self.card_attention(**card),
                    Some(AttentionState::Waiting | AttentionState::Failed)
                )
            })
            .count()
    }

    pub fn attention_retained(&self, row: SelectionRow) -> bool {
        self.filter == SidebarFilter::Attention && self.retained_attention.contains(&row)
    }

    pub fn retain_attention_channel(&mut self, channel: ChannelId) {
        if self.filter == SidebarFilter::Attention {
            self.retained_attention
                .insert(SelectionRow::Channel(channel));
        }
    }

    pub fn card_activity(&self, card: CardId) -> Option<crate::agent::Activity> {
        self.agent_activity.get(&card).map(|entry| entry.activity)
    }

    pub fn clear_agent_activity(&mut self, card: CardId) {
        self.agent_activity.remove(&card);
        self.agent_outcomes.remove(&card);
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
                ActivityState::Waiting => return Some(ActivityState::Waiting),
                ActivityState::Working => status = Some(ActivityState::Working),
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
                | PanelSelection::Change { path, .. }
                | PanelSelection::Commit { path, .. } => path.as_path(),
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
        &self.commit_draft.message
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
        self.commit_draft = CommitDraft::default();
    }

    pub fn submitted_commit_revision(
        &self,
        key: &ActiveWorktreeKey,
        message: &str,
        amend: bool,
    ) -> Option<u64> {
        (self
            .active_worktree
            .as_ref()
            .is_some_and(|active| active.canonical_path == key.canonical_path)
            && self.commit_draft.message == message
            && self.commit_draft.amend == amend)
            .then_some(self.commit_draft.revision)
    }

    /// Settle only the submitted draft, including while another tree is bound.
    pub fn clear_submitted_commit(&mut self, key: &ActiveWorktreeKey, revision: u64) {
        let draft = if self
            .active_worktree
            .as_ref()
            .is_some_and(|active| active.canonical_path == key.canonical_path)
        {
            Some(&mut self.commit_draft)
        } else {
            self.parked_commit_drafts.get_mut(&key.canonical_path)
        };
        if let Some(draft) = draft
            && draft.revision == revision
        {
            *draft = CommitDraft::default();
        }
    }

    fn revise_commit_draft(&mut self) {
        self.commit_generation = self.commit_generation.wrapping_add(1);
        self.commit_draft.revision = self.commit_generation;
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
        self.selection = next.as_ref().map(|key| {
            if model
                .project(key.project_id)
                .is_some_and(|project| project.kind == ProjectKind::Folder)
            {
                SidebarSelection::Project(key.project_id)
            } else {
                SidebarSelection::Worktree {
                    project_id: key.project_id,
                    worktree_id: key.worktree_id,
                }
            }
        });
        if self.active_worktree == next {
            return false;
        }
        if let Some(previous) = &self.active_worktree {
            let draft = std::mem::take(&mut self.commit_draft);
            if draft.revision != 0 || !draft.message.is_empty() || draft.amend {
                self.parked_commit_drafts
                    .insert(previous.canonical_path.clone(), draft);
            }
        }
        self.commit_draft = next
            .as_ref()
            .and_then(|key| self.parked_commit_drafts.remove(&key.canonical_path))
            .unwrap_or_default();
        self.active_worktree = next;
        // Every panel-local draft belongs to the worktree it was typed for.
        self.files_query.clear();
        self.files_filter_open = false;
        self.expanded_directories.clear();
        self.collapsed_change_directories.clear();
        self.change_pages.clear();
        self.files_page = 0;
        // Every section is one worktree's change area now, not a view of the
        // panel, so none of their disclosures carry to the next worktree.
        self.toggled_sections.clear();
        self.changes_tab = ChangesTab::default();
        self.publish = None;
        self.discard = None;
        self.panel_selection = None;
        true
    }

    /// Which Git view the panel is showing.
    ///
    /// Public because opening `History` is the only request that view makes:
    /// the application asks for the first page when this becomes
    /// [`ChangesTab::History`], and nothing else in the panel asks for one.
    pub fn changes_tab(&self) -> ChangesTab {
        self.changes_tab
    }

    /// Whether one change group is disclosed.
    ///
    /// Public because `Untracked` gates what the snapshot reads: a collapsed
    /// group is not worth the untracked walk, so the application reloads when
    /// this changes.
    pub fn section_expanded(&self, section: PanelSection) -> bool {
        self.toggled_sections.contains(&section) != section.expanded_by_default()
    }

    pub fn project_expanded(&self, project: ProjectId) -> bool {
        !self.collapsed_projects.contains(&project)
    }

    /// Applies only local, pure presentation transitions. Commands such as
    /// opening a worktree, staging, or pushing remain for the application.
    pub fn apply(&mut self, action: &SidebarAction) {
        match action {
            SidebarAction::SearchChanged(query) => self.query.clone_from(query),
            SidebarAction::SetFilter(filter) => {
                self.retained_attention.clear();
                self.filter = *filter;
            }
            SidebarAction::ToggleControls => {
                self.controls_expanded = !self.controls_expanded;
                if !self.controls_expanded {
                    self.clear_search_filters();
                }
            }
            SidebarAction::ToggleProject(project_id) => {
                toggle_membership(&mut self.collapsed_projects, *project_id);
            }
            SidebarAction::OpenRepository { project_id, .. }
            | SidebarAction::BrowsePullRequests { project_id } => {
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
            SidebarAction::FilesPage { page, .. } => self.files_page = *page,
            SidebarAction::FilesQueryChanged { query, .. } => {
                self.files_page = 0;
                self.files_query.clone_from(query);
                self.files_filter_open = true;
            }
            SidebarAction::ToggleFilesFilter { .. } => {
                self.files_page = 0;
                self.files_filter_open = !self.files_filter_open;
                if !self.files_filter_open {
                    self.files_query.clear();
                }
            }
            SidebarAction::SetChangeListMode { mode, .. } => self.change_list_mode = *mode,
            SidebarAction::SetChangesTab { tab, .. } => self.changes_tab = *tab,
            SidebarAction::SetWorktreePanel { .. } => {}
            SidebarAction::ToggleDirectory { path, .. } => {
                if !self.expanded_directories.remove(path) {
                    self.expanded_directories.insert(path.clone());
                }
            }
            SidebarAction::ToggleChangeDirectory { scope, path, .. } => {
                self.change_pages.remove(scope);
                let collapsed = self.collapsed_change_directories.entry(*scope).or_default();
                if !collapsed.remove(path) {
                    collapsed.insert(path.clone());
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
            SidebarAction::OpenCommitFile { oid, path, .. } => {
                self.panel_selection = Some(PanelSelection::Commit {
                    oid: oid.clone(),
                    path: path.clone(),
                });
            }
            SidebarAction::CommitMessageChanged { message, .. } => {
                self.revise_commit_draft();
                self.commit_draft.message.clone_from(message);
                let (subject, description) = message.split_once('\n').unwrap_or((message, ""));
                self.commit_draft.subject = subject.to_owned();
                self.commit_draft.subject_composer = CommitComposer(
                    cosmic::iced::widget::text_editor::Content::with_text(subject),
                );
                self.commit_draft.composer =
                    CommitComposer(cosmic::iced::widget::text_editor::Content::with_text(
                        description.trim_start_matches('\n'),
                    ));
            }
            SidebarAction::CommitSubjectEditorCursor { cursor, .. } => {
                self.commit_draft.subject_composer.0.move_to(*cursor);
            }
            SidebarAction::CommitEditorCursor { cursor, .. } => {
                self.commit_draft.composer.0.move_to(*cursor);
            }
            SidebarAction::CommitSubjectEditorAction { action, .. } => {
                self.commit_draft.subject_composer.0.perform(action.clone());
                if action.is_edit() {
                    self.revise_commit_draft();
                    let text = self.commit_draft.subject_composer.0.text();
                    self.commit_draft.subject = text.lines().collect::<Vec<_>>().join(" ");
                    if text.trim_end_matches('\n').contains('\n') {
                        // A pasted message stays a single subject line.
                        self.commit_draft.subject_composer =
                            CommitComposer(cosmic::iced::widget::text_editor::Content::with_text(
                                &self.commit_draft.subject,
                            ));
                    }
                    self.commit_draft.message = compose_commit_message(
                        &self.commit_draft.subject,
                        &self.commit_draft.composer.0.text(),
                    );
                }
            }
            SidebarAction::CommitEditorAction { action, .. } => {
                self.commit_draft.composer.0.perform(action.clone());
                if action.is_edit() {
                    self.revise_commit_draft();
                    self.commit_draft.message = compose_commit_message(
                        &self.commit_draft.subject,
                        &self.commit_draft.composer.0.text(),
                    );
                }
            }
            SidebarAction::AmendChanged { amend, .. } => {
                if self.commit_draft.amend != *amend {
                    self.revise_commit_draft();
                    self.commit_draft.amend = *amend;
                }
            }
            SidebarAction::ChangePage { scope, page, .. } => {
                self.change_pages.insert(*scope, *page);
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
            | SidebarAction::DismissWorktreeOperation(_)
            | SidebarAction::EditWorktreeOperation(_)
            | SidebarAction::RetryWorktreeOperation(_)
            | SidebarAction::RetryWorktreeDeletion { .. }
            | SidebarAction::RetryChannelDeletion { .. }
            | SidebarAction::ProjectCommand { .. }
            | SidebarAction::OpenFileInEditor { .. }
            | SidebarAction::FileCommand { .. }
            | SidebarAction::NewWorktree { .. }
            | SidebarAction::NewChannel { .. }
            | SidebarAction::OpenChannel { .. }
            | SidebarAction::DeleteChannel { .. }
            | SidebarAction::ReorderMember { .. }
            | SidebarAction::ReorderProject { .. }
            | SidebarAction::DragWindow
            | SidebarAction::MaximizeWindow
            | SidebarAction::WindowMenu
            | SidebarAction::Refresh
            | SidebarAction::RefreshProject { .. }
            | SidebarAction::RefreshWorktree { .. }
            | SidebarAction::CreatePullRequest { .. }
            | SidebarAction::OpenSettings
            | SidebarAction::OpenAutomations
            | SidebarAction::Stage { .. }
            | SidebarAction::ChangeGroup { .. }
            | SidebarAction::Unstage { .. }
            | SidebarAction::Commit { .. }
            | SidebarAction::OpenCommit { .. }
            | SidebarAction::CloseCommit { .. }
            | SidebarAction::ShowMoreCommitFiles { .. }
            | SidebarAction::LoadMoreHistory { .. }
            | SidebarAction::Resize { .. }
            | SidebarAction::ResizeFinished
            | SidebarAction::SelectRows { .. }
            | SidebarAction::NavigateRow { .. }
            | SidebarAction::OpenFirstSearchResult
            | SidebarAction::FocusFirstSearchResult
            | SidebarAction::ScrollProjects(_)
            | SidebarAction::ToggleProjects
            | SidebarAction::RenameChannel(_)
            | SidebarAction::MarkAllRead
            | SidebarAction::MarkWorktreeRead(_, _)
            | SidebarAction::MarkChannelRead(_)
            | SidebarAction::DeleteRow(_)
            | SidebarAction::DeleteSelectedRows
            | SidebarAction::ClearRowSelection => {}
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
    RevealInFileManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeGroupCommand {
    Stage,
    Unstage,
    Discard,
}

/// All sidebar intents. Payloads are self-contained so `app.rs` does not need
/// to recover a path or URL from ephemeral row indexes, and every
/// source-control intent carries the whole [`ActiveWorktreeKey`] so a result
/// arriving after the panel rebound is recognisably stale.
///
/// Not `Eq`: the resize request carries a width, and widths are measurements.
#[derive(Debug, Clone, PartialEq)]
pub enum SidebarAction {
    ChangeGroup {
        key: ActiveWorktreeKey,
        area: ChangeArea,
        command: ChangeGroupCommand,
    },
    ChangePage {
        key: ActiveWorktreeKey,
        scope: ChangeTreeScope,
        page: usize,
    },
    NavigateRow {
        row: SelectionRow,
        key: cosmic::iced::keyboard::key::Named,
    },
    OpenFirstSearchResult,
    FocusFirstSearchResult,
    ScrollProjects(f32),
    ToggleProjects,
    RenameChannel(ChannelId),
    MarkChannelRead(ChannelId),
    MarkAllRead,
    MarkWorktreeRead(ProjectId, WorktreeId),
    SelectRows {
        row: SelectionRow,
        intent: SelectionIntent,
    },
    /// Delete or remove one row, confirmed exactly as its context menu is.
    DeleteRow(SelectionRow),
    DeleteSelectedRows,
    ClearRowSelection,
    DragWindow,
    MaximizeWindow,
    WindowMenu,
    ReorderMember {
        project: ProjectId,
        source: SelectionRow,
        before: Option<SelectionRow>,
    },
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
    /// Removes the row of a creation that ended without a worktree.
    DismissWorktreeOperation(crate::model::OperationId),
    /// Reopens a failed creation's input in the create dialog.
    EditWorktreeOperation(crate::model::OperationId),
    /// Runs a failed creation's input again.
    RetryWorktreeOperation(crate::model::OperationId),
    /// Runs a confirmed worktree deletion again after it failed.
    RetryWorktreeDeletion {
        project_id: ProjectId,
        worktree_id: WorktreeId,
    },
    /// Runs a confirmed channel deletion again after it failed.
    RetryChannelDeletion {
        channel_id: ChannelId,
    },
    SearchChanged(String),
    SetFilter(SidebarFilter),
    ToggleControls,
    ToggleProject(ProjectId),
    OpenRepositoryPicker,
    NewChannel {
        project_id: ProjectId,
    },
    OpenChannel {
        channel_id: ChannelId,
    },
    /// Asks to delete a channel. The application confirms before removing
    /// anything, because a channel's conversations live nowhere else.
    DeleteChannel {
        channel_id: ChannelId,
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
    /// Opens the project's PR browser, including PRs without local worktrees.
    BrowsePullRequests {
        project_id: ProjectId,
    },
    /// Opens the pull request using the configured link destination.
    OpenPullRequest {
        project_id: ProjectId,
        number: u64,
        url: Url,
    },
    Refresh,
    /// Reads one project's repository again, leaving every other project's
    /// data alone. `Refresh` re-reads all of them, and `OpenRepository` only
    /// moves the workspace into a project, so neither is what a project that
    /// could not be read has to offer.
    RefreshProject {
        project_id: ProjectId,
        repository: PathBuf,
    },
    CreatePullRequest {
        project_id: ProjectId,
        worktree: Option<PathBuf>,
    },
    OpenSettings,
    OpenAutomations,

    OpenCommit {
        key: ActiveWorktreeKey,
        oid: Oid,
    },
    CloseCommit {
        key: ActiveWorktreeKey,
    },
    ShowMoreCommitFiles {
        key: ActiveWorktreeKey,
        oid: Oid,
    },
    OpenCommitFile {
        key: ActiveWorktreeKey,
        oid: Oid,
        path: RepoPath,
    },

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
    FilesPage {
        key: ActiveWorktreeKey,
        page: usize,
    },
    FilesQueryChanged {
        key: ActiveWorktreeKey,
        query: String,
    },
    SetChangeListMode {
        key: ActiveWorktreeKey,
        mode: FileListMode,
    },
    SetChangesTab {
        key: ActiveWorktreeKey,
        tab: ChangesTab,
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
    ToggleChangeDirectory {
        key: ActiveWorktreeKey,
        scope: ChangeTreeScope,
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
    CommitSubjectEditorCursor {
        key: ActiveWorktreeKey,
        cursor: cosmic::iced::widget::text_editor::Cursor,
    },
    CommitEditorCursor {
        key: ActiveWorktreeKey,
        cursor: cosmic::iced::widget::text_editor::Cursor,
    },
    CommitSubjectEditorAction {
        key: ActiveWorktreeKey,
        action: cosmic::iced::widget::text_editor::Action,
    },
    CommitEditorAction {
        key: ActiveWorktreeKey,
        action: cosmic::iced::widget::text_editor::Action,
    },
    AmendChanged {
        key: ActiveWorktreeKey,
        amend: bool,
    },
    Commit {
        key: ActiveWorktreeKey,
        message: String,
        amend: bool,
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

/// Closing search returns to the control that opens it in both densities.
pub fn search_toggle_focus_id() -> Id {
    stable_id("toggle-controls")
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
    view_inner(
        model,
        state,
        mode,
        persisted_width,
        modal_open,
        None,
        &crate::settings::KeyboardShortcuts::default(),
    )
}

#[derive(Default)]
pub struct NavigationCache(std::cell::RefCell<(Option<NavigationSnapshot>, u64)>);

/// Exactly the inputs to `project_list`. File trees, history and commit
/// composers belong to the source-control panel, which is rendered separately.
/// Keeping them out of this snapshot bounds comparisons and copies by the
/// number of navigation entities, even in a very large repository.
struct NavigationSnapshot {
    projects: Vec<SidebarProject>,
    editors: Vec<SidebarEditor>,
    rows: selection::Selection,
    query: String,
    filter: SidebarFilter,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
    active_worktree: Option<ActiveWorktreeKey>,
    worktree_sort: crate::settings::WorktreeSort,
    hide_worktree_changes_icons: bool,
    agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    agent_outcomes: BTreeMap<CardId, crate::agent::ActivityOutcome>,
    retained_attention: BTreeSet<SelectionRow>,
    agent_activity: BTreeMap<CardId, PaneActivity>,
    worktrees_with_panes: BTreeSet<(ProjectId, WorktreeId)>,
}

impl NavigationSnapshot {
    fn matches(&self, model: &SidebarModel, state: &SidebarState) -> bool {
        self.projects == model.projects
            && self.editors == model.external_editors
            && self.rows == state.rows
            && self.query == state.query
            && self.filter == state.filter
            && self.controls_expanded == state.controls_expanded
            && self.collapsed_projects == state.collapsed_projects
            && self.selection == state.selection
            && self.active_worktree == state.active_worktree
            && self.worktree_sort == state.worktree_sort
            && self.hide_worktree_changes_icons == state.hide_worktree_changes_icons
            && self.agent_outcomes == state.agent_outcomes
            && self.retained_attention == state.retained_attention
            && self.agent_updates == state.agent_updates
            && self.agent_activity == state.agent_activity
            && self.worktrees_with_panes == state.worktrees_with_panes
    }

    fn capture(model: &SidebarModel, state: &SidebarState) -> Self {
        Self {
            projects: model.projects.clone(),
            editors: model.external_editors.clone(),
            rows: state.rows.clone(),
            query: state.query.clone(),
            filter: state.filter,
            controls_expanded: state.controls_expanded,
            collapsed_projects: state.collapsed_projects.clone(),
            selection: state.selection,
            active_worktree: state.active_worktree.clone(),
            worktree_sort: state.worktree_sort,
            hide_worktree_changes_icons: state.hide_worktree_changes_icons,
            agent_updates: state.agent_updates.clone(),
            agent_outcomes: state.agent_outcomes.clone(),
            retained_attention: state.retained_attention.clone(),
            agent_activity: state.agent_activity.clone(),
            worktrees_with_panes: state.worktrees_with_panes.clone(),
        }
    }
}

impl NavigationCache {
    fn generation(&self, model: &SidebarModel, state: &SidebarState) -> u64 {
        let mut snapshot = self.0.borrow_mut();
        if snapshot
            .0
            .as_ref()
            .is_none_or(|previous| !previous.matches(model, state))
        {
            snapshot.0 = Some(NavigationSnapshot::capture(model, state));
            snapshot.1 = snapshot.1.wrapping_add(1);
        }
        snapshot.1
    }
}

pub fn view_cached<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    mode: SidebarMode,
    persisted_width: f32,
    modal_open: bool,
    cache: &'a NavigationCache,
    shortcuts: &crate::settings::KeyboardShortcuts,
) -> Element<'a, SidebarAction> {
    view_inner(
        model,
        state,
        mode,
        persisted_width,
        modal_open,
        Some(cache),
        shortcuts,
    )
}

fn view_inner<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    mode: SidebarMode,
    persisted_width: f32,
    modal_open: bool,
    cache: Option<&'a NavigationCache>,
    shortcuts: &crate::settings::KeyboardShortcuts,
) -> Element<'a, SidebarAction> {
    let compact = mode == SidebarMode::Compact;
    let width = effective_width(mode, persisted_width);
    let loading = matches!(&model.data_state, SidebarDataState::Loading);
    let header_loading = loading && !model.projects.is_empty();
    let mut content = widget::column::with_capacity(7)
        .push(
            widget::container(utilities(compact, modal_open, shortcuts)).padding([0.0, space::XS]),
        )
        .push(design::horizontal_rule())
        .push(
            widget::container(header(
                model,
                state,
                compact,
                header_loading,
                modal_open,
                shortcuts,
            ))
            .padding([0.0, space::XS]),
        );

    if !compact && state.controls_expanded && search_is_reachable(model, mode) {
        content = content.push(
            widget::container(controls(state, modal_open))
                .class(theme::Container::Primary)
                .padding(space::S),
        );
    }

    content = content.push(
        widget::container(
            if let Some(cache) = cache
                && !model.projects.is_empty()
                && !matches!(model.data_state, SidebarDataState::Error(_))
            {
                cosmic::iced::widget::lazy(
                    (cache.generation(model, state), compact, modal_open),
                    move |_| project_list(model, state, compact, modal_open),
                )
                .into()
            } else {
                body(model, state, compact, modal_open)
            },
        )
        .width(Length::Fill)
        .height(Length::Fill),
    );
    content = content.width(Length::Fill).height(Length::Fill);

    widget::container(crate::interaction::layer(
        widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        // The compact rail has one width, so its edge is a rule and
        // nothing more: a drag there would silently retune a width the
        // rail does not render.
        widget::container(if compact {
            Element::from(
                widget::container(crate::design::vertical_rule())
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
    shortcuts: &crate::settings::KeyboardShortcuts,
) -> Element<'a, SidebarAction> {
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(4);
    if !model.projects.is_empty() {
        let attention = state.filter == SidebarFilter::Attention;
        actions.push(crate::interaction::tooltip(
            button::icon(Icon::Attention)
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
        let count = state.intervention_count();
        if count > 0 {
            actions.push(crate::interaction::tooltip(
                design::caption(count.to_string()),
                format!("{count} agents need attention"),
            ));
        }
        if attention {
            actions.push(
                button::text("Mark all read")
                    .on_press_maybe(gated(SidebarAction::MarkAllRead, modal_open))
                    .into(),
            );
        }
        let selected = search_toggle_lit(state);
        let search_button = crate::interaction::tooltip(
            button::icon(Icon::Search)
                .class(sidebar_button_style(selected))
                .id(search_toggle_focus_id())
                .name("Search projects")
                .selected(selected)
                .width(ICON_CONTROL_SIZE)
                .height(ICON_CONTROL_SIZE)
                .on_press_maybe(gated(SidebarAction::ToggleControls, modal_open)),
            // Closing the controls also ends the search, so say so.
            if state.controls_expanded && state.search_filters_active() {
                "Close and clear search".to_owned()
            } else {
                crate::workspace_widget::shortcut_tooltip(
                    crate::workspace_widget::GlobalShortcut::FocusProjectSearch,
                    shortcuts,
                )
            },
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
        Icon::Plus,
        "New project",
        gated(SidebarAction::OpenRepositoryPicker, modal_open),
        stable_id("open-repository"),
    ));
    if compact {
        return window_gestures(
            widget::column::with_children(actions)
                .align_x(Alignment::Center)
                .padding([space::XS, 0.0])
                .spacing(space::XS)
                .width(Length::Fill),
        );
    }

    let mut children: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(7);
    children.push(design::heading("Projects").into());
    children.push(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .into(),
    );
    if loading {
        children.push(crate::ui_controls::activity_spinner(
            f32::from(ROW_ICON_SIZE),
            design::warning_color,
        ));
    }
    children.extend(actions);

    let header = widget::row::with_children(children)
        .align_y(Alignment::Center)
        .spacing(space::XXS)
        .padding([0.0, space::XS])
        .height(Length::Fixed(HEADER_HEIGHT))
        .width(Length::Fill);
    window_gestures(header)
}

/// The search toggle is lit whenever the controls narrow the list, not only
/// while they show, so an active search can never pass for the full list.
fn search_toggle_lit(state: &SidebarState) -> bool {
    state.controls_expanded || state.search_filters_active()
}

/// The pane's top strip is part of the titlebar.
///
/// The window is undecorated, so every strip of chrome along the top edge owes
/// the user drag, double-click-maximize and the window menu - `native_header`
/// attaches all three beside this pane, and the compact rail is as much of a
/// titlebar as the expanded header is. These stay live while a dialog is up:
/// moving or maximizing the window is not a pane command, and [`gated`]'s
/// reasoning - that a command the application refuses must not report itself
/// enabled - does not reach them.
fn window_gestures<'a>(
    content: impl Into<Element<'a, SidebarAction>>,
) -> Element<'a, SidebarAction> {
    widget::mouse_area(content)
        .on_drag(SidebarAction::DragWindow)
        .on_double_press(SidebarAction::MaximizeWindow)
        .on_right_press(SidebarAction::WindowMenu)
        .into()
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

pub fn files_scroll_id(key: &ActiveWorktreeKey) -> Id {
    entity_id("panel.files.scroll", key.worktree_id)
}

pub fn files_filter_focus_id(key: &ActiveWorktreeKey) -> Id {
    entity_id("panel.files.query", key.worktree_id)
}

fn controls<'a>(state: &'a SidebarState, modal_open: bool) -> Element<'a, SidebarAction> {
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
            .spacing(space::XS)
            .into(),
    ])
    .spacing(space::XS)
    .width(Length::Fill)
    .into()
}

fn utilities<'a>(
    compact: bool,
    modal_open: bool,
    shortcuts: &crate::settings::KeyboardShortcuts,
) -> Element<'a, SidebarAction> {
    let toggle = crate::ui_controls::icon_button(
        Icon::Sidebar,
        crate::workspace_widget::shortcut_tooltip(
            crate::workspace_widget::GlobalShortcut::ToggleProjects,
            shortcuts,
        ),
    )
    .on_press_maybe(gated(SidebarAction::ToggleProjects, modal_open))
    .id(Id::new("command.toggle-projects"))
    .into();
    let [settings, automations] = utility_controls(modal_open, Some(shortcuts));
    if compact {
        let children: Vec<Element<'a, SidebarAction>> = vec![toggle, settings, automations];
        return widget::column::with_children(children)
            .align_x(Alignment::Center)
            .spacing(space::XS)
            .padding(space::XS)
            .width(Length::Fill)
            .into();
    }
    window_gestures(
        widget::row::with_children(vec![
            settings,
            automations,
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .into(),
            toggle,
        ])
        .align_y(Alignment::Center)
        .spacing(space::XXS)
        .padding([0.0, space::XS])
        .height(Length::Fixed(HEADER_HEIGHT))
        .width(Length::Fill),
    )
}

fn utility_controls<'a>(
    modal_open: bool,
    shortcuts: Option<&crate::settings::KeyboardShortcuts>,
) -> [Element<'a, SidebarAction>; 2] {
    let label = shortcuts.map_or_else(
        || "Settings".to_owned(),
        |shortcuts| {
            crate::workspace_widget::shortcut_tooltip(
                crate::workspace_widget::GlobalShortcut::OpenSettings,
                shortcuts,
            )
        },
    );
    let settings: Element<'a, SidebarAction> =
        crate::ui_controls::icon_button(Icon::Settings, label)
            .on_press_maybe(gated(SidebarAction::OpenSettings, modal_open))
            .id(stable_id("settings"))
            .into();
    let automations = crate::ui_controls::icon_button(Icon::Alarm, "Automations")
        .on_press_maybe(gated(SidebarAction::OpenAutomations, modal_open))
        .id(stable_id("automations"))
        .into();
    [settings, automations]
}

// The standalone editor still mounts its utility controls below its file pane.
fn footer<'a>(compact: bool, modal_open: bool) -> Element<'a, SidebarAction> {
    let [settings, automations] = utility_controls(modal_open, None);
    if compact {
        widget::column::with_children(vec![automations, settings])
            .align_x(Alignment::Center)
            .spacing(space::XS)
            .padding(space::XS)
            .width(Length::Fill)
            .into()
    } else {
        widget::row::with_children(vec![automations, settings])
            .align_y(Alignment::Center)
            .padding(space::XS)
            .width(Length::Fill)
            .into()
    }
}

/// The project filter input.
///
/// A text input with no `on_input` is disabled, which is what standing it down
/// means here: the pane keeps the focus it held when the dialog opened, so a
/// keystroke the overlay does not consume would otherwise still be typed into
/// this box.
fn search<'a>(state: &'a SidebarState, modal_open: bool) -> Element<'a, SidebarAction> {
    let width = Length::Fill;
    let mut input =
        crate::ui_controls::field::search("Search projects, branches, channels", &state.query)
            .width(width);
    if !modal_open {
        input = input
            .on_input(SidebarAction::SearchChanged)
            .on_submit(|_| SidebarAction::OpenFirstSearchResult);
        if !state.query.is_empty() {
            input = input.trailing_icon(crate::ui_controls::field::clear(
                SidebarAction::SearchChanged(String::new()),
            ));
        }
    }
    RowInteraction::search(input.id(search_focus_id()).into(), !modal_open)
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
    let mut row = widget::row::with_capacity(FILTER_CHOICES.len())
        .spacing(space::XS)
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
        SidebarDataState::Loading => loading(),
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
) -> Element<'static, SidebarAction> {
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
    let sticky_viewport = row_interaction::StickyViewport::default();
    let mut column = widget::column::with_capacity(projects.len()).spacing(if compact {
        space::XS
    } else {
        space::S
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
            .padding(RowGrid::list_padding(compact))
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

fn loading() -> Element<'static, SidebarAction> {
    let content = widget::column::with_children(vec![crate::ui_controls::activity_spinner(
        f32::from(STATUS_ICON_SIZE),
        design::accent_color,
    )])
    .align_x(Alignment::Center)
    .spacing(space::S);
    widget::container(content)
        .center(Length::Fill)
        .padding(space::M)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn error<'a>(message: &'a str, compact: bool, modal_open: bool) -> Element<'a, SidebarAction> {
    if compact {
        return widget::container(
            widget::column::with_children(vec![
                Icon::Warning.handle().icon().size(STATUS_ICON_SIZE).into(),
                icon_action(
                    Icon::Refresh,
                    format!("Couldn't load projects. {message}. Try again"),
                    gated(SidebarAction::Refresh, modal_open),
                    stable_id("error.retry"),
                ),
            ])
            .spacing(space::S)
            .align_x(Alignment::Center),
        )
        .center(Length::Fill)
        .height(Length::Fill)
        .into();
    }
    widget::container(
        widget::column::with_children(vec![
            Icon::Warning.handle().icon().size(STATUS_ICON_SIZE).into(),
            design::heading("Couldn’t load projects").into(),
            design::caption(message)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            crate::ui_controls::button::secondary("Try again")
                .id(stable_id("error.retry"))
                .on_press_maybe(gated(SidebarAction::Refresh, modal_open))
                .into(),
        ])
        .align_x(Alignment::Center)
        .spacing(space::S),
    )
    .center(Length::Fill)
    .padding(space::L)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn empty(state: &SidebarState, compact: bool, modal_open: bool) -> Element<'static, SidebarAction> {
    let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
    let (icon_name, title, detail, action_label, action) = if searching {
        (
            Icon::Search,
            "No matches",
            "Try a different project, branch, or path.",
            "Clear search",
            SidebarAction::SearchChanged(String::new()),
        )
    } else {
        match state.filter {
            SidebarFilter::All => (
                Icon::OpenFolder,
                "No projects",
                "Open a folder to add a project.",
                "Open folder",
                SidebarAction::OpenRepositoryPicker,
            ),
            SidebarFilter::Attention => (
                Icon::Check,
                "No unread updates",
                "No panes currently need your attention.",
                "Show all",
                SidebarAction::SetFilter(SidebarFilter::All),
            ),
            SidebarFilter::PullRequests => (
                Icon::PullRequest,
                "No pull requests",
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
    let action_icon = if searching {
        Icon::Clear
    } else if state.filter == SidebarFilter::All {
        Icon::OpenFolder
    } else {
        Icon::Tree
    };
    let action_button =
        crate::ui_controls::icon_button(action_icon, format!("{action_label} · {detail}"))
            .on_press_maybe(gated(action, modal_open))
            .id(stable_id("empty.action"));
    widget::container(
        widget::row::with_children(vec![
            design::caption(title).width(Length::Fill).into(),
            action_button.into(),
        ])
        .spacing(space::S)
        .align_y(Alignment::Center),
    )
    .padding(space::M)
    .width(Length::Fill)
    .into()
}

/// Which of the two grids a navigation row sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A project header. It owns the group's disclosure and "new …" commands,
    /// so it reserves room for those instead of a live indicator.
    Project,
    /// A row inside a project: a channel, a worktree, or one of the rail's
    /// "new …" actions.
    Child,
}

/// The content grid every sidebar navigation row is laid out on.
///
/// Projects, channels, worktrees and "new …" actions used to be four
/// independent layout paths: one boxed its icon in a fixed slot while the next
/// let a glyph size itself, one reserved its trailing controls while the next
/// pushed a badge only while an agent happened to be working. Adjacent labels
/// therefore started at three different x positions, and a rail row moved
/// sideways whenever a spinner appeared. Every row is built from this type, so
/// the grid can only move as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowGrid {
    kind: RowKind,
    /// The rail renders glyphs only: no label, no hierarchy indent, and no
    /// gutter to spend between the two slots it has room for.
    compact: bool,
}

/// One row's content while its slots are being filled.
type RowContent<'a> = widget::Row<'a, SidebarAction, cosmic::Theme, cosmic::Renderer>;

impl RowGrid {
    const fn project(compact: bool) -> Self {
        Self {
            kind: RowKind::Project,
            compact,
        }
    }

    const fn child(compact: bool) -> Self {
        Self {
            kind: RowKind::Child,
            compact,
        }
    }

    fn gutter(self) -> f32 {
        if self.compact { 0.0 } else { ROW_GUTTER }
    }

    fn height(self) -> f32 {
        match self.kind {
            RowKind::Project => PROJECT_HEADER_HEIGHT,
            RowKind::Child => WORKTREE_ROW_HEIGHT,
        }
    }

    /// The scroll viewport's own inset, rail included.
    ///
    /// The rail belongs to the content inside the viewport rather than to a
    /// container around the scrollable: an outer gutter moves the thumb along
    /// with the content and leaves the two touching exactly as before. The
    /// rail's inset is the grid's own gutter, so nothing can squeeze
    /// `COMPACT_ROW_WIDTH` out of the rail.
    fn list_padding(compact: bool) -> Padding {
        design::with_vertical_rail(if compact {
            Padding {
                top: ROW_GUTTER,
                right: 0.0,
                bottom: ROW_GUTTER,
                left: ROW_GUTTER,
            }
        } else {
            Padding::from(space::XS)
        })
    }

    /// Leading icon slot, label slot, and the reserved indicator slot.
    ///
    /// Durable badges - primary, detached, locked, dirty, checks - are pushed
    /// after this and inherit the gutter: they are facts about a worktree
    /// rather than live activity, so they neither compete for the reserved
    /// slot nor move when an agent wakes up.
    fn content<'a>(
        self,
        icon: impl Into<Element<'a, SidebarAction>>,
        label: Option<Element<'a, SidebarAction>>,
        status: Option<Element<'a, SidebarAction>>,
    ) -> RowContent<'a> {
        let mut content = widget::row::with_capacity(3)
            .align_y(Alignment::Center)
            .height(Length::Fill)
            .spacing(self.gutter())
            .push(
                widget::container(icon)
                    .center_x(Length::Fixed(ROW_ICON_WIDTH))
                    .center_y(Length::Fill),
            );
        if let Some(label) = label {
            content = content.push(label);
        }
        if matches!(self.kind, RowKind::Child) {
            content = content.push(
                widget::container(slot(status))
                    .center_x(Length::Fixed(ROW_STATUS_WIDTH))
                    .center_y(Length::Fill),
            );
        }
        content
    }

    /// The button a row is: one height, one inset, one width rule.
    fn control<'a>(
        self,
        content: RowContent<'a>,
        selected: bool,
    ) -> button::Button<'a, SidebarAction> {
        button::custom(content)
            .class(match self.kind {
                RowKind::Project => row_button_style::<true, false>(selected),
                RowKind::Child => row_button_style::<true, true>(selected),
            })
            .selected(selected)
            .padding([0.0, self.gutter()])
            .width(if self.compact {
                Length::Fixed(COMPACT_ROW_WIDTH)
            } else {
                Length::Fill
            })
            .height(Length::Fixed(self.height()))
    }

    /// Navigation rows share the entire list width and the same icon column.
    fn mount<'a>(
        self,
        control: impl Into<Element<'a, SidebarAction>>,
    ) -> Element<'a, SidebarAction> {
        control.into()
    }
}

/// A project header: its identity row, with the hover-revealed commands
/// mounted over the row's own trailing edge.
///
/// Only the identity yields space to hover commands. The disclosure stays at
/// the right edge, so revealing commands cannot move its click target.
fn project_header_row<'a>(
    identity: Element<'a, SidebarAction>,
    commands: Vec<Element<'a, SidebarAction>>,
    disclosure: Element<'a, SidebarAction>,
) -> Element<'a, SidebarAction> {
    let reserve = commands.len() as f32 * ICON_CONTROL_SIZE;
    let identity = RowInteraction::actions(
        identity,
        widget::container(
            widget::row::with_children(commands)
                .align_y(Alignment::Center)
                .height(PROJECT_HEADER_HEIGHT),
        )
        .align_right(Length::Fill)
        .into(),
        reserve,
    );
    widget::row::with_children(vec![identity, disclosure])
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// A project header's identity row: its icon and its name, across the whole
/// row.
///
/// The hover-revealed commands are mounted over this row by
/// `RowInteraction::actions`, which narrows it only while they are on screen,
/// so a long project name reads in full until the moment it would run under
/// them.
fn project_identity_row<'a>(
    project: &'a SidebarProject,
    selected: bool,
    modal_open: bool,
    compact: bool,
    review_mode: bool,
    query: &str,
) -> Element<'static, SidebarAction> {
    let chrome = &project.chrome;
    let grid = RowGrid::project(compact);
    let label = (!compact).then(|| project_name_label(project, query));
    let control = grid
        .control(
            grid.content(project_icon(project, ROW_ICON_SIZE), label, None),
            selected,
        )
        .id(chrome.open_id.clone())
        .name(if review_mode && project.github_ready {
            Cow::Owned(format!("Browse pull requests in {}", project.name))
        } else if compact {
            Cow::Owned(chrome.compact_open_name.to_string())
        } else {
            Cow::Owned(chrome.open_name.to_string())
        })
        .on_press_maybe(
            project_primary_action(
                project,
                if review_mode {
                    SidebarFilter::PullRequests
                } else {
                    SidebarFilter::All
                },
            )
            .and_then(|action| gated(action, modal_open)),
        );
    let tip: Cow<'static, str> = if review_mode && project.github_ready {
        Cow::Owned(format!("Browse pull requests in {}", project.name))
    } else if compact {
        Cow::Owned(chrome.compact_open_name.to_string())
    } else {
        Cow::Owned(project.repository.display().to_string())
    };
    grid.mount(crate::interaction::tooltip(control, tip))
}

/// The primary action for a project row, shared by pointer and keyboard
/// activation. An unreadable project offers retry rather than navigation.
pub(crate) fn project_primary_action(
    project: &SidebarProject,
    filter: SidebarFilter,
) -> Option<SidebarAction> {
    if project.unreachable.is_some() {
        return (!project.reloading).then(|| SidebarAction::RefreshProject {
            project_id: project.id,
            repository: project.repository.clone(),
        });
    }
    Some(
        if filter == SidebarFilter::PullRequests && project.github_ready {
            SidebarAction::BrowsePullRequests {
                project_id: project.id,
            }
        } else if project.kind == ProjectKind::Folder {
            SidebarAction::OpenRepository {
                project_id: project.id,
                repository: project.repository.clone(),
            }
        } else {
            SidebarAction::ToggleProject(project.id)
        },
    )
}

/// The project's name, on one line, ellipsized rather than wrapped so a long
/// name cannot change a header's height.
fn project_name_label(project: &SidebarProject, query: &str) -> Element<'static, SidebarAction> {
    search_label(
        &project.name,
        query,
        design::TEXT_HEADING,
        theme::Text::Default,
        true,
    )
}

/// The rows for a project's worktree creations that have no worktree row yet.
///
/// These do not come from reading the checkout: they are rebuilt from the
/// application's own record of what it is running, so they outlive a failed
/// read and are shown beside the failure. A running creation looks like the
/// worktree row it is about to become, muted and with a spinner, so a slow
/// `git worktree add` shows up the moment it is asked for.
fn operation_rows<'a>(
    project: &'a SidebarProject,
    modal_open: bool,
) -> impl Iterator<Item = Element<'static, SidebarAction>> {
    let grid = RowGrid::child(false);
    project.operations.iter().map(move |operation| {
        let label = design::body(operation.label.to_string())
            .class(theme::Text::Custom(design::muted_text))
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into();
        let content = grid.content(
            operation_icon(operation.phase, false),
            Some(label),
            operation_status(operation.phase),
        );
        // A failure the dialog can take back opens there, beside the input
        // that caused it.
        let control = grid
            .control(content, false)
            .name(operation_accessible_name(operation))
            .on_press_maybe(
                gated(
                    SidebarAction::EditWorktreeOperation(operation.id),
                    modal_open,
                )
                .filter(|_| operation.reopenable),
            )
            .width(Length::Fill);
        let control = crate::interaction::tooltip(control, operation.detail.to_string());
        let retry = operation.reopenable.then(|| {
            icon_action(
                Icon::Refresh,
                format!("Retry creating {}", operation.label),
                gated(
                    SidebarAction::RetryWorktreeOperation(operation.id),
                    modal_open,
                ),
                stable_id(format!("operation.{}.retry", operation.id)),
            )
        });
        let trailing = operation_control(operation, modal_open);
        grid.mount(
            widget::row::with_capacity(3)
                .align_y(Alignment::Center)
                .push(control)
                .push_maybe(retry)
                .push_maybe(trailing),
        )
    })
}

/// The same operation rows as [`operation_rows`], reduced to the icon rail the
/// compact sidebar is made of: the one glyph is also the row's one control.
fn compact_operation_rows<'a>(
    project: &'a SidebarProject,
    modal_open: bool,
) -> impl Iterator<Item = Element<'static, SidebarAction>> {
    let grid = RowGrid::child(true);
    project.operations.iter().map(move |operation| {
        let control = grid
            .control(
                grid.content(operation_icon(operation.phase, true), None, None),
                false,
            )
            .name(operation_accessible_name(operation))
            .on_press_maybe(gated_operation_action(operation, modal_open));
        grid.mount(crate::interaction::tooltip(
            control,
            operation.detail.to_string(),
        ))
    })
}

/// The leading glyph: the worktree icon the row will keep, muted, while the
/// creation runs; a warning once it has failed. The rail has no status slot,
/// so there the spinner takes the glyph's place.
fn operation_icon<'a>(
    phase: WorktreeOperationPhase,
    compact: bool,
) -> Element<'static, SidebarAction> {
    match phase {
        WorktreeOperationPhase::Working if compact => {
            crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::muted_icon_color)
        }
        WorktreeOperationPhase::Working => {
            widget::container(widget::icon(worktree_icon()).size(ROW_ICON_SIZE))
                .class(theme::Container::custom(|theme| {
                    cosmic::iced::widget::container::Style {
                        icon_color: Some(design::muted_icon_color(theme)),
                        ..Default::default()
                    }
                }))
                .into()
        }
        WorktreeOperationPhase::Failed => toned_icon(Icon::Warning, BadgeTone::Warning),
    }
}

fn operation_status<'a>(phase: WorktreeOperationPhase) -> Option<Element<'static, SidebarAction>> {
    match phase {
        WorktreeOperationPhase::Working => Some(crate::ui_controls::activity_spinner(
            f32::from(ROW_ICON_SIZE),
            design::muted_icon_color,
        )),
        WorktreeOperationPhase::Failed => None,
    }
}

/// Cancel while the creation can still be stopped; dismiss once it has failed.
fn gated_operation_action(
    operation: &WorktreeOperationRow,
    modal_open: bool,
) -> Option<SidebarAction> {
    if operation.cancellable {
        gated(
            SidebarAction::CancelWorktreeOperation(operation.id),
            modal_open,
        )
    } else if operation.phase == WorktreeOperationPhase::Failed {
        gated(
            SidebarAction::DismissWorktreeOperation(operation.id),
            modal_open,
        )
    } else {
        None
    }
}

fn operation_control<'a>(
    operation: &'a WorktreeOperationRow,
    modal_open: bool,
) -> Option<Element<'static, SidebarAction>> {
    let action = gated_operation_action(operation, modal_open)?;
    let (icon_name, label) = if operation.cancellable {
        (Icon::Cancel, format!("Cancel creating {}", operation.label))
    } else {
        (Icon::Close, format!("Dismiss {}", operation.label))
    };
    Some(icon_action(
        icon_name,
        label,
        Some(action),
        stable_id(format!("operation.{}.control", operation.id)),
    ))
}

fn operation_accessible_name(operation: &WorktreeOperationRow) -> String {
    match operation.phase {
        WorktreeOperationPhase::Working => {
            format!(
                "Creating worktree {}: {}",
                operation.label, operation.detail
            )
        }
        WorktreeOperationPhase::Failed if operation.reopenable => {
            format!(
                "Worktree {} was not created: {}. Activate to edit and retry.",
                operation.label, operation.detail
            )
        }
        WorktreeOperationPhase::Failed => {
            format!(
                "Worktree {} was not created: {}",
                operation.label, operation.detail
            )
        }
    }
}

/// A project whose checkout could not be read.
///
/// The row keeps the project's name, because the reader's first question is
/// which project is unreadable, states the reason under it, and offers a
/// reload of just this project. The project's worktrees, channels and pull
/// requests are not drawn, because the read which would have listed them is
/// the read that failed and rows left over from an earlier one would claim a
/// structure the project cannot currently be shown to have. Worktree
/// operations are drawn, because a `git worktree add` this application is
/// running is still running, and its cancel is the only way to stop it.
fn unreachable_project<'a>(
    project: &'a SidebarProject,
    state: &SidebarState,
    reason: &'a str,
    compact: bool,
    modal_open: bool,
    sticky_viewport: row_interaction::StickyViewport,
) -> Element<'static, SidebarAction> {
    let chrome = &project.chrome;
    let grid = RowGrid::project(compact);
    let reload =
        project_primary_action(project, state.filter).and_then(|action| gated(action, modal_open));
    // A colon rather than a full stop, because the reason comes from git or
    // the filesystem and those messages begin in lower case.
    let description = if project.reloading {
        format!(
            "Couldn’t read project {}: {reason}. Trying again",
            project.name
        )
    } else {
        format!(
            "Couldn’t read project {}: {reason}. Try again",
            project.name
        )
    };
    let control = grid
        .control(
            grid.content(
                toned_icon(Icon::Warning, BadgeTone::Warning),
                (!compact).then(|| project_name_label(project, &state.query)),
                None,
            ),
            false,
        )
        .id(chrome.open_id.clone())
        .name(description.clone())
        .on_press_maybe(reload.clone());
    let header = RowInteraction::selectable(
        grid.mount(crate::interaction::tooltip(control, description.clone())),
        SelectionRow::Project(project.id),
        state,
        !modal_open,
    );
    if compact {
        // The compact rail has no room for the reason, which the tooltip and
        // the accessible name carry instead, but a running operation's cancel
        // is still the only way to stop it and has to stay reachable.
        let mut column = widget::column::with_capacity(project.operations.len() + 1)
            .width(Length::Fill)
            .push(header);
        for row in compact_operation_rows(project, modal_open) {
            column = column.push(row);
        }
        return widget::container(column)
            .padding([space::XS, 0.0])
            .width(Length::Fill)
            .into();
    }
    let mut details = widget::column::with_capacity(project.operations.len() + 2)
        .spacing(space::XS)
        .push(
            design::caption(format!("Couldn’t read this project: {reason}"))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word),
        );
    for row in operation_rows(project, modal_open) {
        details = details.push(row);
    }
    // The retry is named after its project, because two unreadable projects
    // would otherwise offer a reader two controls announced identically.
    details = details.push(
        crate::ui_controls::button::secondary("Try again")
            .id(chrome.retry_id.clone())
            .name(description)
            .on_press_maybe(reload),
    );
    // Grouped like every other project row, because the enclosing list
    // hit-tests each row's header through `sticky_header_bounds` whether the
    // row pins its header or not, and an ungrouped row would put the drag
    // origin over the reason and the retry once it scrolled past the top.
    RowInteraction::group(header, details.into(), sticky_viewport)
}

fn project_view<'a>(
    project: &FilteredProject<'a>,
    expanded: bool,
    compact: bool,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    sticky_viewport: row_interaction::StickyViewport,
) -> Element<'static, SidebarAction> {
    let entity = project.project;
    let chrome = &entity.chrome;
    let project_id = entity.id;
    if let Some(reason) = entity.unreachable.as_deref() {
        return unreachable_project(
            entity,
            project.state,
            reason,
            compact,
            modal_open,
            sticky_viewport,
        );
    }
    let selection = if entity.channels.iter().any(|channel| channel.selected) {
        None
    } else {
        selection
    };
    if compact {
        return compact_project(project, expanded, selection, modal_open, sticky_viewport);
    }

    let worktree_count = project.worktrees.len();
    let has_children = worktree_count > 0 || !project.channels.is_empty();
    let project_identity = project_identity_row(
        entity,
        selection == Some(SidebarSelection::Project(project_id)),
        modal_open,
        false,
        project.state.filter == SidebarFilter::PullRequests,
        &project.state.query,
    );

    let disclosure: Element<'static, SidebarAction> = if has_children {
        project_icon_action(
            if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            if expanded {
                chrome.collapse_name.to_string()
            } else {
                chrome.expand_name.to_string()
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
    let add_worktree = project_icon_action(
        Icon::WorktreeAdd,
        chrome.new_worktree_name.to_string(),
        gated(
            SidebarAction::NewWorktree {
                project_id,
                repository: entity.repository.clone(),
            },
            modal_open,
        ),
        chrome.new_worktree_id.clone(),
    );
    let add_channel = project_icon_action(
        Icon::ChannelAdd,
        chrome.new_channel_name.to_string(),
        gated(SidebarAction::NewChannel { project_id }, modal_open),
        chrome.new_channel_id.clone(),
    );
    let mut identity = widget::row::with_children(vec![project_identity])
        .align_y(Alignment::Center)
        .width(Length::Fill);
    if !expanded {
        let activity = entity
            .channels
            .iter()
            .filter_map(|channel| channel.activity)
            .chain(
                entity
                    .worktrees
                    .iter()
                    .filter_map(|worktree| project.state.agent_activity(project_id, worktree.id)),
            )
            .max_by_key(|activity| match activity {
                crate::agent::ActivityState::Waiting => 2,
                crate::agent::ActivityState::Working => 1,
                _ => 0,
            });
        let unread = entity.channels.iter().any(|channel| channel.unread)
            || entity
                .worktrees
                .iter()
                .any(|worktree| project.state.has_agent_update(project_id, worktree.id));
        if let Some(indicator) = agent_indicator(activity, unread) {
            identity = identity.push(indicator);
        }
        identity = identity.push(design::caption(
            (entity.channels.len() + entity.worktrees.len()).to_string(),
        ));
    }
    let header = project_header_row(
        identity.into(),
        if entity.kind == ProjectKind::Git {
            vec![add_channel, add_worktree]
        } else {
            vec![add_channel]
        },
        disclosure,
    );
    let header = context_controls(
        header,
        project_id,
        None,
        modal_open,
        entity.kind == ProjectKind::Git,
        true,
        entity.github_ready,
        project.editors,
        project
            .state
            .rows
            .group_size(SelectionRow::Project(project_id)),
    );
    let header = RowInteraction::selectable(
        header,
        SelectionRow::Project(project_id),
        project.state,
        !modal_open,
    );
    let mut column = widget::column::with_capacity(worktree_count + 2);
    for row in operation_rows(entity, modal_open) {
        column = column.push(row);
    }

    if expanded {
        let mut members = Vec::new();
        for channel in &project.channels {
            members.push((
                crate::workspace_model::NavigationItem::Channel(channel.id),
                SelectionRow::Channel(channel.id),
                RowInteraction::selectable(
                    channel_view(
                        channel,
                        modal_open,
                        false,
                        project
                            .state
                            .rows
                            .group_size(SelectionRow::Channel(channel.id)),
                        &project.state.query,
                    ),
                    SelectionRow::Channel(channel.id),
                    project.state,
                    !modal_open && channel.deletion != Some(RowDeletion::Deleting),
                ),
            ));
        }
        for &worktree in &project.worktrees {
            members.push((
                crate::workspace_model::NavigationItem::Worktree(worktree.path.clone()),
                SelectionRow::Worktree(project_id, worktree.id),
                RowInteraction::selectable(
                    worktree_view(
                        project_id,
                        worktree,
                        is_primary_worktree(&entity.repository, &worktree.path),
                        project.state.filter == SidebarFilter::PullRequests,
                        selection,
                        modal_open,
                        entity.github_ready,
                        project.editors,
                        project.state.has_agent_update(project_id, worktree.id),
                        project.state.agent_activity(project_id, worktree.id),
                        project.state.worktree_outcome(project_id, worktree.id),
                        project
                            .state
                            .attention_retained(SelectionRow::Worktree(project_id, worktree.id))
                            && !project.state.has_agent_update(project_id, worktree.id)
                            && !project
                                .state
                                .worktree_attention(project_id, worktree.id)
                                .is_some_and(AttentionState::needs_attention),
                        project.state.worktree_has_panes(project_id, worktree.id),
                        false,
                        project
                            .state
                            .rows
                            .group_size(SelectionRow::Worktree(project_id, worktree.id)),
                        &project.state.query,
                        !project.state.hide_worktree_changes_icons,
                    ),
                    SelectionRow::Worktree(project_id, worktree.id),
                    project.state,
                    !modal_open && worktree.deletion != Some(RowDeletion::Deleting),
                ),
            ));
        }
        if project.state.query.trim().is_empty() {
            apply_navigation_order(&mut members, &entity.navigation_order, |(item, _, _)| {
                item.clone()
            });
        }
        let ids = members.iter().map(|(_, row, _)| *row).collect();
        let content =
            widget::column::with_children(members.into_iter().map(|(_, _, element)| element))
                .width(Length::Fill);
        let disabled = entity
            .channels
            .iter()
            .filter(|row| row.deletion == Some(RowDeletion::Deleting))
            .map(|row| SelectionRow::Channel(row.id))
            .chain(
                entity
                    .worktrees
                    .iter()
                    .filter(|row| row.deletion == Some(RowDeletion::Deleting))
                    .map(|row| SelectionRow::Worktree(project_id, row.id)),
            )
            .collect();
        column = column.push(RowInteraction::members(
            content.into(),
            project_id,
            ids,
            disabled,
            !modal_open && project.state.query.trim().is_empty(),
        ));
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
    expanded: bool,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    sticky_viewport: row_interaction::StickyViewport,
) -> Element<'static, SidebarAction> {
    let entity = project.project;
    let selected = matches!(
        selection,
        Some(SidebarSelection::Project(project_id))
            | Some(SidebarSelection::Worktree { project_id, .. })
            | Some(SidebarSelection::PullRequest { project_id, .. })
            if project_id == entity.id
    );
    let header = context_controls(
        project_identity_row(
            entity,
            selected,
            modal_open,
            true,
            project.state.filter == SidebarFilter::PullRequests,
            &project.state.query,
        ),
        entity.id,
        None,
        modal_open,
        entity.kind == ProjectKind::Git,
        true,
        entity.github_ready,
        project.editors,
        project
            .state
            .rows
            .group_size(SelectionRow::Project(entity.id)),
    );
    let header = RowInteraction::selectable(
        header,
        SelectionRow::Project(entity.id),
        project.state,
        !modal_open,
    );
    let selection = if entity.channels.iter().any(|channel| channel.selected) {
        None
    } else {
        selection
    };
    // The rail's rows are one fixed width, so they start where the header
    // above them starts. Centring them instead would move every row sideways
    // whenever the reserved rail changed the content box's width.
    let mut column = widget::column::with_capacity(project.worktrees.len() + 1).width(Length::Fill);
    for row in compact_operation_rows(entity, modal_open) {
        column = column.push(row);
    }
    for channel in project.channels.iter().filter(|channel| {
        expanded
            && ((!project.state.query.trim().is_empty() && project.state.controls_expanded)
                || channel.selected
                || channel.unread
                || channel.activity.is_some()
                || channel.deletion.is_some())
    }) {
        column = column.push(RowInteraction::selectable(
            channel_view(
                channel,
                modal_open,
                true,
                project
                    .state
                    .rows
                    .group_size(SelectionRow::Channel(channel.id)),
                &project.state.query,
            ),
            SelectionRow::Channel(channel.id),
            project.state,
            !modal_open && channel.deletion != Some(RowDeletion::Deleting),
        ));
    }
    for &worktree in project.worktrees.iter().filter(|worktree| {
        expanded
            && ((!project.state.query.trim().is_empty() && project.state.controls_expanded)
                || selection
                    == Some(SidebarSelection::Worktree {
                        project_id: entity.id,
                        worktree_id: worktree.id,
                    })
                || project.state.has_agent_update(entity.id, worktree.id)
                || project.state.worktree_has_panes(entity.id, worktree.id)
                || worktree.deletion.is_some()
                || project
                    .state
                    .agent_activity(entity.id, worktree.id)
                    .is_some())
    }) {
        column = column.push(RowInteraction::selectable(
            worktree_view(
                entity.id,
                worktree,
                is_primary_worktree(&entity.repository, &worktree.path),
                project.state.filter == SidebarFilter::PullRequests,
                selection,
                modal_open,
                entity.github_ready,
                project.editors,
                project.state.has_agent_update(entity.id, worktree.id),
                project.state.agent_activity(entity.id, worktree.id),
                project.state.worktree_outcome(entity.id, worktree.id),
                project
                    .state
                    .attention_retained(SelectionRow::Worktree(entity.id, worktree.id))
                    && !project.state.has_agent_update(entity.id, worktree.id)
                    && !project
                        .state
                        .worktree_attention(entity.id, worktree.id)
                        .is_some_and(AttentionState::needs_attention),
                project.state.worktree_has_panes(entity.id, worktree.id),
                true,
                project
                    .state
                    .rows
                    .group_size(SelectionRow::Worktree(entity.id, worktree.id)),
                &project.state.query,
                !project.state.hide_worktree_changes_icons,
            ),
            SelectionRow::Worktree(entity.id, worktree.id),
            project.state,
            !modal_open && worktree.deletion != Some(RowDeletion::Deleting),
        ));
    }
    widget::container(RowInteraction::group(
        header,
        column.into(),
        sticky_viewport,
    ))
    .padding([space::XS, 0.0])
    .width(Length::Fill)
    .into()
}

/// How strongly a navigation row draws where no state color claims it.
///
/// A worktree or channel with an open pane is somewhere the user has work in
/// front of them, so it keeps full strength; one with nothing open recedes,
/// which is what lets the rows that deserve focus stand out in a long list.
/// Unread news keeps a row at full strength too: it is a reason to go back
/// even when nothing is open there. The glyph and the label follow the same
/// rule, so a row reads as one strength rather than two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowInk {
    Full,
    Muted,
}

impl RowInk {
    fn of(has_panes: bool, unread: bool) -> Self {
        if has_panes || unread {
            Self::Full
        } else {
            Self::Muted
        }
    }

    fn icon_color(self, theme: &cosmic::Theme) -> cosmic::iced::Color {
        match self {
            Self::Full => theme.current_container().on.into(),
            Self::Muted => design::muted_icon_color(theme),
        }
    }

    fn text_class(self) -> theme::Text {
        match self {
            Self::Full => theme::Text::default(),
            Self::Muted => theme::Text::Custom(design::muted_text),
        }
    }
}

/// The color a navigation row's leading glyph takes.
///
/// Worktree pull requests retain their state colors; channel identity stays
/// neutral, with activity shown separately in the indicator slot. Small and
/// `Copy` so the style closure resolves it against the drawing theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlyphTone {
    Ink(RowInk),
    /// A draft pull request. Its cue is being quieter than its neighbours,
    /// which no fixed color can say beside rows that are themselves either
    /// full or muted, so it stays one step below whatever the row's ink is.
    Draft(RowInk),
    PullRequest(PullRequestTone),
}

impl GlyphTone {
    fn color(self, theme: &cosmic::Theme) -> cosmic::iced::Color {
        match self {
            Self::Ink(ink) => ink.icon_color(theme),
            Self::Draft(RowInk::Full) => design::muted_icon_color(theme),
            Self::Draft(RowInk::Muted) => PullRequestTone::Draft.color(theme),
            Self::PullRequest(tone) => tone.color(theme),
        }
    }
}

fn channel_ink(channel: &SidebarChannel) -> RowInk {
    deletion_ink(
        if channel.acknowledged_attention {
            RowInk::Muted
        } else {
            RowInk::of(channel.has_panes, channel.unread)
        },
        channel.deletion.as_ref(),
    )
}

fn channel_glyph_tone(channel: &SidebarChannel) -> GlyphTone {
    GlyphTone::Ink(channel_ink(channel))
}

/// A neutral pull request carries no news, so its glyph follows the row's ink
/// like the plain worktree glyph it stands in for; a draft follows it one step
/// quieter.
fn worktree_glyph_tone(
    pull_request: Option<PullRequestTone>,
    update: bool,
    has_panes: bool,
) -> GlyphTone {
    let ink = RowInk::of(has_panes, update);
    match pull_request {
        None | Some(PullRequestTone::Neutral) => GlyphTone::Ink(ink),
        Some(PullRequestTone::Draft) => GlyphTone::Draft(ink),
        Some(tone) => GlyphTone::PullRequest(tone),
    }
}

/// The one live indicator a channel row shows in its reserved slot.
///
/// Attention outranks the chat count: a channel whose agent is working or
/// waiting is the reason to look at the sidebar at all, and the count stays in
/// the row's accessible name and tooltip either way.
fn channel_indicator(channel: &SidebarChannel) -> Option<Element<'static, SidebarAction>> {
    outcome_indicator(channel.activity, channel.unread, channel.outcome.as_ref())
        .or_else(|| (channel.chats > 1).then(|| design::caption(channel.chats.to_string()).into()))
}

/// Waiting is a live state, independent of whether its notification was read.
/// Channels and worktrees share the same vocabulary and priority.
pub(crate) fn attention_indicator(
    state: Option<AttentionState>,
) -> Option<Element<'static, SidebarAction>> {
    match state {
        Some(AttentionState::Failed) => Some(status_icon(
            Icon::Warning,
            "Agent failed",
            BadgeTone::Danger,
        )),
        Some(AttentionState::Cancelled) => Some(status_icon(
            Icon::Cancel,
            "Agent cancelled",
            BadgeTone::Neutral,
        )),
        Some(AttentionState::Waiting) => {
            agent_indicator(Some(crate::agent::ActivityState::Waiting), false)
        }
        Some(AttentionState::Working) => {
            agent_indicator(Some(crate::agent::ActivityState::Working), false)
        }
        Some(AttentionState::Unread) => agent_indicator(None, true),
        None => None,
    }
}

fn with_unread_indicator<Message: Clone + 'static>(
    base: Element<'static, Message>,
    label: &'static str,
) -> Element<'static, Message> {
    let unread = widget::container(Icon::Attention.handle().icon().size(8))
        .align_right(Length::Fill)
        .align_bottom(Length::Fill);
    crate::interaction::tooltip(
        cosmic::iced::widget::Stack::with_children(vec![base, unread.into()])
            .width(f32::from(ROW_ICON_SIZE))
            .height(f32::from(ROW_ICON_SIZE)),
        label,
    )
}

pub(crate) fn outcome_indicator<Message: Clone + 'static>(
    activity: Option<crate::agent::ActivityState>,
    unread: bool,
    outcome: Option<&crate::agent::ActivityOutcome>,
) -> Option<Element<'static, Message>> {
    if matches!(outcome, Some(crate::agent::ActivityOutcome::Cancelled))
        && unread
        && !matches!(
            activity,
            Some(crate::agent::ActivityState::Working | crate::agent::ActivityState::Waiting)
        )
    {
        return Some(with_unread_indicator(
            status_icon(Icon::Cancel, "Agent cancelled", BadgeTone::Neutral),
            "Cancelled · unread agent update",
        ));
    }
    match attention_state(activity, unread, outcome) {
        Some(AttentionState::Failed) => {
            let reason = outcome
                .map(|outcome| {
                    format!(
                        "{}: {}",
                        outcome.label(),
                        outcome.reason().unwrap_or("No reason reported")
                    )
                })
                .unwrap_or_else(|| "Agent failed".into());
            Some(status_icon(Icon::Warning, reason, BadgeTone::Danger))
        }
        Some(AttentionState::Cancelled) => Some(status_icon(
            Icon::Cancel,
            "Agent cancelled",
            BadgeTone::Neutral,
        )),
        _ => agent_indicator(activity, unread),
    }
}

fn agent_indicator<Message: Clone + 'static>(
    activity: Option<crate::agent::ActivityState>,
    unread: bool,
) -> Option<Element<'static, Message>> {
    if activity == Some(crate::agent::ActivityState::Waiting) {
        return Some(status_icon(
            Icon::Warning,
            "An agent is waiting for input",
            BadgeTone::Warning,
        ));
    }
    if activity == Some(crate::agent::ActivityState::Working) {
        let spinner = crate::interaction::tooltip(
            crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::warning_color),
            "An agent is actively working",
        );
        return Some(if unread {
            with_unread_indicator(spinner, "Working · unread agent update")
        } else {
            spinner
        });
    }
    if unread {
        return Some(status_icon(
            Icon::Attention,
            "Unread agent update",
            BadgeTone::Neutral,
        ));
    }
    None
}

/// A deletion under way takes the reserved slot: it is the one live thing
/// about the row, and outranks whatever the row was doing before.
fn deleting_indicator<'a>() -> Element<'static, SidebarAction> {
    crate::interaction::tooltip(
        crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::muted_icon_color),
        "Deleting…",
    )
}

/// A failed deletion is a fact about the row until it is retried, so it is a
/// badge after the reserved slot and leaves live activity where it was. The
/// rail has room for nothing after the slot, so there it takes the slot.
fn deletion_failed_badge(reason: &str) -> Element<'static, SidebarAction> {
    status_icon(Icon::Warning, reason.to_owned(), BadgeTone::Warning)
}

/// A row being deleted reads as muted as a creation's row does, glyph and
/// label alike; the name stays so the user can tell which row it is.
fn deletion_ink(ink: RowInk, deletion: Option<&RowDeletion>) -> RowInk {
    if deletion == Some(&RowDeletion::Deleting) {
        RowInk::Muted
    } else {
        ink
    }
}

/// Mounts a row with the trailing retry a failed deletion offers.
fn with_deletion_retry<'a>(
    row: Element<'static, SidebarAction>,
    deletion: Option<&RowDeletion>,
    compact: bool,
    label: &str,
    retry: Option<SidebarAction>,
    id: Id,
) -> Element<'static, SidebarAction> {
    if compact || !matches!(deletion, Some(RowDeletion::Failed(_))) {
        return row;
    }
    widget::row::with_capacity(2)
        .align_y(Alignment::Center)
        .push(row)
        .push(icon_action(
            Icon::Refresh,
            format!("Retry deleting {label}"),
            retry,
            id,
        ))
        .into()
}

/// A row's accessible name with the deletion it is part of.
fn with_deletion_name<'a>(
    name: Cow<'static, str>,
    kind: &str,
    label: &str,
    deletion: Option<&RowDeletion>,
) -> Cow<'static, str> {
    match deletion {
        None => name,
        Some(RowDeletion::Deleting) => Cow::Owned(format!("Deleting {kind} {label}")),
        Some(RowDeletion::Failed(reason)) => {
            Cow::Owned(format!("{name}, could not be deleted: {reason}"))
        }
    }
}

fn channel_view(
    channel: &SidebarChannel,
    modal_open: bool,
    compact: bool,
    selection_count: Option<usize>,
    query: &str,
) -> Element<'static, SidebarAction> {
    let grid = RowGrid::child(compact);
    let deletion = channel.deletion.as_ref();
    let deleting = deletion == Some(&RowDeletion::Deleting);
    let tone = channel_glyph_tone(channel);
    let glyph = widget::container(widget::icon(Icon::Channel.handle()).size(CHILD_ROW_ICON_SIZE))
        .class(theme::Container::custom(move |theme| {
            cosmic::iced::widget::container::Style {
                icon_color: Some(tone.color(theme)),
                ..Default::default()
            }
        }));
    let label = (!compact).then(|| {
        let label = search_label(
            &channel.title,
            query,
            CHILD_ROW_TEXT_SIZE,
            channel_ink(channel).text_class(),
            false,
        );
        if let Some((_, branch)) = &channel.worktree {
            widget::row::with_children(vec![label, design::caption(branch.clone()).into()])
                .spacing(space::XS)
                .into()
        } else {
            label
        }
    });
    let indicator = match deletion {
        Some(RowDeletion::Deleting) => Some(deleting_indicator()),
        Some(RowDeletion::Failed(reason)) if compact => Some(deletion_failed_badge(reason)),
        _ => channel_indicator(channel),
    };
    let mut content = grid.content(glyph, label, indicator);
    if !compact {
        match deletion {
            Some(RowDeletion::Deleting) => content = content.push(design::caption("Deleting…")),
            Some(RowDeletion::Failed(reason)) => {
                content = content.push(deletion_failed_badge(reason))
            }
            None => {}
        }
    }
    // A channel on its way out is not somewhere to go, nor something to
    // delete twice.
    let inert = modal_open || deleting;
    let action = grid
        .control(content, channel.selected)
        .id(channel.open_id.clone())
        .name(with_deletion_name(
            Cow::Owned(channel.accessible_name.to_string()),
            "channel",
            &channel.title,
            deletion,
        ))
        .on_press_maybe(gated(
            SidebarAction::OpenChannel {
                channel_id: channel.id,
            },
            inert,
        ));
    let action = row_quick_delete(
        crate::interaction::tooltip(action, channel.tooltip.to_string()),
        &channel.delete_name,
        gated(
            SidebarAction::DeleteChannel {
                channel_id: channel.id,
            },
            inert,
        ),
        channel.delete_id.clone(),
        compact,
    );
    grid.mount(with_deletion_retry(
        channel_context_controls(action, channel.id, inert, selection_count),
        deletion,
        compact,
        &channel.title,
        gated(
            SidebarAction::RetryChannelDeletion {
                channel_id: channel.id,
            },
            modal_open,
        ),
        entity_id("channel.retry-delete", channel.id),
    ))
}

/// Keep the row's primary button and its destructive quick action separate.
/// The compact rail reuses its indicator slot while hovered; an expanded row
/// reserves the button width so the label continues to ellipsize correctly.
fn row_quick_delete<'a>(
    content: Element<'static, SidebarAction>,
    label: &str,
    action: Option<SidebarAction>,
    id: Id,
    compact: bool,
) -> Element<'static, SidebarAction> {
    let width = if compact {
        ROW_STATUS_WIDTH
    } else {
        ICON_CONTROL_SIZE
    };
    let control = button::icon(Icon::Delete)
        .class(sidebar_button_style(false))
        .id(id)
        .name(label.to_owned())
        .width(width)
        .height(WORKTREE_ROW_HEIGHT)
        .padding(0)
        .on_press_maybe(action);
    let controls = widget::container(
        widget::container(crate::interaction::tooltip(control, label.to_owned()))
            .class(theme::Container::Primary)
            .width(width)
            .height(WORKTREE_ROW_HEIGHT),
    )
    .align_right(Length::Fill)
    .into();
    RowInteraction::actions(content, controls, if compact { 0.0 } else { width })
}

/// The commands a channel row offers on right-click.
///
/// A channel is the only navigation row whose contents exist nowhere else, so
/// deleting one is destructive and the entry says so with an ellipsis: the
/// application answers it with a confirmation rather than a removal.
fn channel_context_controls<'a>(
    content: impl Into<Element<'static, SidebarAction>>,
    channel_id: ChannelId,
    modal_open: bool,
    selection_count: Option<usize>,
) -> Element<'static, SidebarAction> {
    use widget::menu::{Item, items};
    let Some(menu) = (!modal_open && selection_count.is_some()).then(|| {
        let mut entries = vec![Item::Button(
            match selection_count {
                Some(count) if count > 1 => format!(
                    "{}…",
                    SelectionRow::Channel(channel_id).deletion_label(count)
                ),
                _ => "Delete channel…".to_owned(),
            },
            None,
            ChannelContextCommand(
                channel_id,
                selection_count.unwrap_or(1) > 1,
                ChannelMenuCommand::Delete,
            ),
        )];
        if selection_count == Some(1) {
            entries.insert(
                0,
                Item::Button(
                    "Rename channel…".into(),
                    None,
                    ChannelContextCommand(channel_id, false, ChannelMenuCommand::Rename),
                ),
            );
            entries.insert(
                1,
                Item::Button(
                    "Mark as read".into(),
                    None,
                    ChannelContextCommand(channel_id, false, ChannelMenuCommand::MarkRead),
                ),
            );
        }
        items(&std::collections::HashMap::new(), entries)
    }) else {
        return content.into();
    };
    crate::interaction::context_menu(content, menu)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChannelContextCommand(ChannelId, bool, ChannelMenuCommand);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChannelMenuCommand {
    Delete,
    Rename,
    MarkRead,
}

impl widget::menu::Action for ChannelContextCommand {
    type Message = SidebarAction;

    fn message(&self) -> SidebarAction {
        match self.2 {
            ChannelMenuCommand::Rename => return SidebarAction::RenameChannel(self.0),
            ChannelMenuCommand::MarkRead => return SidebarAction::MarkChannelRead(self.0),
            ChannelMenuCommand::Delete => {}
        }
        if self.1 {
            SidebarAction::DeleteSelectedRows
        } else {
            SidebarAction::DeleteChannel { channel_id: self.0 }
        }
    }
}

fn worktree_accessible_name(
    worktree: &SidebarWorktree,
    unread: bool,
    activity: Option<crate::agent::ActivityState>,
    has_panes: bool,
) -> Cow<'static, str> {
    let mut name: Cow<'static, str> = Cow::Owned(if has_panes {
        worktree.chrome.accessible_name.to_string()
    } else {
        worktree.chrome.paneless_accessible_name.to_string()
    });
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

/// What a worktree's own state contributes to an indicator, as inputs rather
/// than widgets.
///
/// The navigation rail's row and the context rail's tabs both resolve their
/// badges through this, so the two rails cannot report the same worktree
/// differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeIndicators<'a> {
    /// The dirty badge's label, which is also its change count.
    pub dirty: Option<&'a str>,
    pub pull_request: Option<&'a SidebarPullRequest>,
    pub pending_checks: bool,
}

pub fn worktree_indicators(worktree: &SidebarWorktree) -> WorktreeIndicators<'_> {
    let pull_request = worktree.chrome.pull_request.as_ref();
    WorktreeIndicators {
        dirty: worktree.chrome.dirty_badge.as_deref(),
        pull_request,
        pending_checks: pull_request.is_some_and(SidebarPullRequest::has_pending_checks),
    }
}

/// The one live indicator a worktree row shows in its reserved slot.
///
/// The rail folds pending checks into the same spinner: it has room for one
/// indicator, and "something is running here" is what the rail is for. The
/// expanded row keeps checks as a durable badge of their own, after the
/// reserved slot.
fn worktree_indicator(
    worktree: &SidebarWorktree,
    agent_update: bool,
    activity: Option<crate::agent::ActivityState>,
    outcome: Option<&crate::agent::ActivityOutcome>,
    compact: bool,
    show_changes_icons: bool,
) -> Option<Element<'static, SidebarAction>> {
    let indicators = worktree_indicators(worktree);
    if let Some(indicator) = outcome_indicator(activity, agent_update, outcome) {
        return Some(indicator);
    }
    let checks = indicators.pending_checks;
    if compact && checks {
        return Some(crate::interaction::tooltip(
            crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::warning_color),
            "CI checks pending or running",
        ));
    }
    if compact && show_changes_icons {
        return indicators
            .dirty
            .map(|label| status_icon(Icon::Changes, label.to_owned(), BadgeTone::Warning));
    }
    None
}

fn worktree_view<'a>(
    project_id: ProjectId,
    worktree: &'a SidebarWorktree,
    primary: bool,
    review_mode: bool,
    selection: Option<SidebarSelection>,
    modal_open: bool,
    github_ready: bool,
    editors: &'a [SidebarEditor],
    agent_update: bool,
    activity: Option<crate::agent::ActivityState>,
    outcome: Option<&crate::agent::ActivityOutcome>,
    acknowledged_attention: bool,
    has_panes: bool,
    compact: bool,
    selection_count: Option<usize>,
    query: &str,
    show_changes_icons: bool,
) -> Element<'static, SidebarAction> {
    let chrome = &worktree.chrome;
    let grid = RowGrid::child(compact);
    let deletion = worktree.deletion.as_ref();
    let deleting = deletion == Some(&RowDeletion::Deleting);
    let label = (!compact).then(|| {
        search_label(
            &chrome.label,
            query,
            CHILD_ROW_TEXT_SIZE,
            deletion_ink(
                if acknowledged_attention {
                    RowInk::Muted
                } else {
                    RowInk::of(has_panes, agent_update)
                },
                deletion,
            )
            .text_class(),
            false,
        )
    });
    let indicator = match deletion {
        Some(RowDeletion::Deleting) => Some(deleting_indicator()),
        Some(RowDeletion::Failed(reason)) if compact => Some(deletion_failed_badge(reason)),
        _ => worktree_indicator(
            worktree,
            agent_update,
            activity,
            outcome,
            compact,
            show_changes_icons,
        ),
    };
    let glyph = if deleting {
        widget::container(widget::icon(worktree_icon()).size(CHILD_ROW_ICON_SIZE))
            .class(theme::Container::custom(|theme| {
                cosmic::iced::widget::container::Style {
                    icon_color: Some(design::muted_icon_color(theme)),
                    ..Default::default()
                }
            }))
            .into()
    } else {
        worktree_status_icon(
            worktree,
            false,
            !acknowledged_attention && (has_panes || agent_update),
        )
    };
    let mut content = grid.content(glyph, label, indicator);
    if !compact {
        match deletion {
            Some(RowDeletion::Deleting) => content = content.push(design::caption("Deleting…")),
            Some(RowDeletion::Failed(reason)) => {
                content = content.push(deletion_failed_badge(reason))
            }
            None => {}
        }
    }
    // Durable attributes, after the reserved slot: each is a fact about the
    // worktree rather than live activity, so none of them moves while an agent
    // starts or stops working. The rail has no room for any of them, and a
    // worktree on its way out has no use for them.
    if !compact && !deleting {
        let indicators = worktree_indicators(worktree);
        if show_changes_icons && let Some(label) = indicators.dirty {
            content = content.push(status_icon(
                Icon::Changes,
                label.to_owned(),
                BadgeTone::Warning,
            ));
        }
        content = content.push(slot(indicators.pull_request.and_then(|pr| {
            pr.review_requested().then(|| {
                crate::interaction::tooltip(
                    design::caption(if pr.review_requested_again {
                        "Review again"
                    } else {
                        "Review"
                    }),
                    if pr.review_requested_again {
                        "Your review was requested again"
                    } else {
                        "Your review is requested"
                    },
                )
            })
        })));
        if let Some(checks) = indicators
            .pull_request
            .and_then(SidebarPullRequest::active_checks)
            && matches!(
                checks,
                CheckStatus::Failing | CheckStatus::Pending | CheckStatus::Passing
            )
        {
            content = content.push(check_indicator(checks));
        }
        if primary {
            content = content.push(status_icon(
                Icon::Home,
                "Primary worktree",
                BadgeTone::Neutral,
            ));
        }
        if worktree.detached {
            content = content.push(status_icon(Icon::Info, "Detached HEAD", BadgeTone::Neutral));
        }
        if worktree.locked {
            content = content.push(status_icon(
                Icon::Lock,
                "Locked worktree",
                BadgeTone::Neutral,
            ));
        }
    }

    let selected = selection
        == Some(SidebarSelection::Worktree {
            project_id,
            worktree_id: worktree.id,
        })
        || (review_mode
            && worktree.pull_request().is_some_and(|pr| {
                selection
                    == Some(SidebarSelection::PullRequest {
                        project_id,
                        number: pr.number,
                    })
            }));
    // A worktree on its way out is not somewhere to go: opening it would start
    // sessions in a directory Git is about to remove.
    let inert = modal_open || deleting;
    let action = grid
        .control(content, selected)
        .id(chrome.open_id.clone())
        .name(with_deletion_name(
            worktree_accessible_name(worktree, agent_update, activity, has_panes),
            "worktree",
            &chrome.label,
            deletion,
        ))
        .on_press_maybe(gated(
            worktree_primary_action(project_id, worktree, review_mode),
            inert,
        ));
    let action: Element<'static, SidebarAction> = if compact {
        action.into()
    } else {
        crate::interaction::hover(action)
    };
    let tip = match outcome {
        Some(outcome) if outcome.reason().is_some() => format!(
            "{} · {}: {}",
            chrome.tooltip,
            outcome.label(),
            outcome.reason().unwrap()
        ),
        Some(crate::agent::ActivityOutcome::Cancelled) => format!("{} · Cancelled", chrome.tooltip),
        _ => chrome.tooltip.to_string(),
    };
    let action = crate::interaction::tooltip(action, tip);
    let action = if !primary
        && worktree
            .pull_request()
            .is_some_and(|pr| pr.state == Some(WorktreePullRequestState::Merged))
    {
        row_quick_delete(
            action,
            &chrome.delete_name,
            gated(
                SidebarAction::ProjectCommand {
                    project_id,
                    worktree_id: Some(worktree.id),
                    command: ProjectCommand::Delete,
                },
                inert,
            ),
            chrome.delete_id.clone(),
            compact,
        )
    } else {
        action
    };
    grid.mount(with_deletion_retry(
        context_controls(
            action,
            project_id,
            Some(worktree.id),
            inert,
            true,
            worktree.branch.is_some(),
            github_ready,
            editors,
            selection_count,
        ),
        deletion,
        compact,
        &chrome.label,
        gated(
            SidebarAction::RetryWorktreeDeletion {
                project_id,
                worktree_id: worktree.id,
            },
            modal_open,
        ),
        entity_id("worktree.retry-delete", worktree.id),
    ))
}

fn worktree_primary_action(
    project_id: ProjectId,
    worktree: &SidebarWorktree,
    review_mode: bool,
) -> SidebarAction {
    if review_mode && let Some(pr) = worktree.pull_request() {
        SidebarAction::OpenPullRequest {
            project_id,
            number: pr.number,
            url: pr.url.clone(),
        }
    } else {
        SidebarAction::OpenWorktree {
            project_id,
            worktree_id: worktree.id,
            path: worktree.path.clone(),
        }
    }
}

fn project_icon(project: &SidebarProject, size: u16) -> Element<'static, SidebarAction> {
    match &project.avatar {
        Some(handle) => cosmic::iced::widget::image(handle.clone())
            .width(size)
            .height(size)
            .into(),
        None => widget::container(Icon::Folder.handle().icon().size(size))
            .class(theme::Container::custom(move |theme| {
                cosmic::iced::widget::container::Style {
                    icon_color: Some(design::muted_icon_color(theme)),
                    ..Default::default()
                }
            }))
            .into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectCommand {
    CopyPath,
    CopyBranch,
    PullRequest,
    NewWorktreeFromBranch,
    MarkRead,
    MoveUp,
    MoveDown,
    ResetSidebarOrder,
    Reveal,
    NewTerminal,
    NewChannel,
    NewWorktree,
    RepositoryUrl,
    OpenEditor(String),
    Delete,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextCommand<'a>(ProjectId, Option<WorktreeId>, &'a ProjectCommand, bool);

impl widget::menu::Action for ContextCommand<'_> {
    type Message = SidebarAction;

    fn message(&self) -> SidebarAction {
        if self.3 {
            return SidebarAction::DeleteSelectedRows;
        }
        if *self.2 == ProjectCommand::MarkRead
            && let Some(worktree) = self.1
        {
            return SidebarAction::MarkWorktreeRead(self.0, worktree);
        }
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
    git: bool,
    has_branch: bool,
    github_ready: bool,
    editors: &[SidebarEditor],
    selection_count: Option<usize>,
) -> Element<'a, SidebarAction> {
    use widget::menu::{Item, items};
    let editors = editors.to_vec();
    if modal_open || selection_count.is_none() {
        return content.into();
    }
    crate::interaction::context_menu_lazy(content, move || {
        let mut commands = vec![
            ("Open folder".to_owned(), None, ProjectCommand::Reveal),
            ("New terminal".to_owned(), None, ProjectCommand::NewTerminal),
            ("New thread".to_owned(), None, ProjectCommand::NewChannel),
        ];
        if git {
            commands.push(("New worktree".to_owned(), None, ProjectCommand::NewWorktree));
        }
        if worktree_id.is_some() {
            commands.push(("Mark worktree read".into(), None, ProjectCommand::MarkRead));
            commands.push(("Copy path".into(), None, ProjectCommand::CopyPath));
            if git {
                commands.push(("Copy branch".into(), None, ProjectCommand::CopyBranch));
                commands.push((
                    "New worktree from this branch…".into(),
                    None,
                    ProjectCommand::NewWorktreeFromBranch,
                ));
                if github_ready {
                    commands.push((
                        "Open or create pull request…".into(),
                        None,
                        ProjectCommand::PullRequest,
                    ));
                }
            }
        } else {
            commands.push(("Move project up".into(), None, ProjectCommand::MoveUp));
            commands.push(("Move project down".into(), None, ProjectCommand::MoveDown));
            commands.push((
                "Reset sidebar order".into(),
                None,
                ProjectCommand::ResetSidebarOrder,
            ));
        }
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
        if selection_count != Some(1) {
            commands.clear();
        }
        if let Some(count) = selection_count {
            // Every row names its own kind, so one label covers both counts.
            let (row, command) = match worktree_id {
                Some(worktree_id) => (
                    SelectionRow::Worktree(project_id, worktree_id),
                    ProjectCommand::Delete,
                ),
                None => (SelectionRow::Project(project_id), ProjectCommand::Remove),
            };
            let label = if count > 1 {
                row.deletion_label(count)
            } else {
                format!("{} {}", row.deletion_verb(), row.noun(1))
            };
            commands.push((format!("{label}…"), None, command));
        }
        let entries = commands
            .iter()
            .map(|(label, icon, command)| {
                let item = if !has_branch
                    && matches!(
                        command,
                        ProjectCommand::CopyBranch
                            | ProjectCommand::NewWorktreeFromBranch
                            | ProjectCommand::PullRequest
                    ) {
                    Item::ButtonDisabled
                } else {
                    Item::Button
                };
                item(
                    if !has_branch
                        && matches!(
                            command,
                            ProjectCommand::CopyBranch
                                | ProjectCommand::NewWorktreeFromBranch
                                | ProjectCommand::PullRequest
                        )
                    {
                        format!("{label} (detached HEAD)")
                    } else {
                        label.clone()
                    },
                    icon.clone(),
                    ContextCommand(
                        project_id,
                        worktree_id,
                        command,
                        selection_count.is_some_and(|count| count > 1)
                            && matches!(command, ProjectCommand::Delete | ProjectCommand::Remove),
                    ),
                )
            })
            .collect();
        items(&std::collections::HashMap::new(), entries)
    })
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

/// The file commands a row offers, in the order its menu draws them.
///
/// `file_management` is the host's willingness to mutate the working tree, so
/// it gates Rename and Duplicate and nothing else: copying a path is a
/// clipboard write, and a row that offers no command at all reads as dead
/// chrome. Directories are copyable for the same reason, and only the two
/// mutations have nothing to do on one.
fn file_commands(
    directory: bool,
    file_management: bool,
) -> impl Iterator<Item = (&'static str, Icon, FileCommand)> {
    [
        ("Rename…", Icon::Rename, FileCommand::Rename),
        ("Duplicate file…", Icon::Duplicate, FileCommand::CopyFile),
        ("Copy path", Icon::CopyPath, FileCommand::CopyPath),
        (
            "Copy relative path",
            Icon::CopyRelativePath,
            FileCommand::CopyRelativePath,
        ),
        (
            "Reveal in file manager",
            Icon::OpenFolder,
            FileCommand::RevealInFileManager,
        ),
    ]
    .into_iter()
    .filter(move |(_, _, command)| {
        !matches!(command, FileCommand::Rename | FileCommand::CopyFile)
            || (file_management && !directory)
    })
}

/// The right-click menu for one file-tree row. Every row has one: see
/// [`file_commands`].
fn file_context_controls<'a>(
    content: Element<'a, SidebarAction>,
    key: &ActiveWorktreeKey,
    row: &FileRow,
    editors: &'a [SidebarEditor],
    file_management: bool,
    blocked: bool,
) -> Element<'a, SidebarAction> {
    if blocked {
        return content;
    }
    let path = &row.path;
    let mut entries = Vec::with_capacity(5 + editors.len());
    for (label, icon, command) in file_commands(row.directory, file_management) {
        entries.push(widget::menu::Item::Button(
            label.to_owned(),
            Some(icon.handle()),
            FileContextCommand::Manage { key, path, command },
        ));
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

fn worktree_status_icon(
    worktree: &SidebarWorktree,
    update: bool,
    has_panes: bool,
) -> Element<'static, SidebarAction> {
    let (handle, pull_request) = worktree
        .chrome
        .pull_request
        .as_ref()
        .map(|pr| (pull_request_icon(pr), Some(pull_request_tone(pr))))
        .unwrap_or_else(|| (worktree_icon(), None));
    let tone = worktree_glyph_tone(pull_request, update, has_panes);
    let glyph = widget::container(widget::icon(handle).size(CHILD_ROW_ICON_SIZE)).class(
        theme::Container::custom(move |theme| cosmic::iced::widget::container::Style {
            // Resolved here, against the theme this frame is drawing with: this
            // builder runs once per visible worktree row, and reaching for
            // `cosmic::theme::active()` locked the process-global theme and
            // cloned it that many times per frame.
            icon_color: Some(tone.color(theme)),
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
        widget::row::with_children(vec![glyph.into(), dot.into()]).align_y(Alignment::Center),
        if update {
            "Unread agent update"
        } else if let Some(pr) = &worktree.chrome.pull_request {
            match pr.state {
                Some(WorktreePullRequestState::Merged) => "Merged pull request",
                Some(WorktreePullRequestState::Closed) => "Closed pull request",
                Some(WorktreePullRequestState::Draft) => "Draft pull request",
                Some(WorktreePullRequestState::Open) => "Open pull request",
                None => "Pull request status",
            }
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
        PanelNavigation::GitViews,
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
    worktree_panel(
        model,
        state,
        width,
        modal_open,
        WorktreePanel::Files,
        PanelNavigation::None,
        false,
    )
}

/// Files, Git changes, or pull requests, with one tab row between them. Used
/// by the standalone editor, which has no context-inspector tab strip.
///
/// `git` is the host's own knowledge that the folder is a Git worktree. It
/// decides the tab row up front rather than from a snapshot that is still
/// loading: a plain folder has only Files, and one tab is not a choice, so it
/// draws no row at all instead of one that vanishes when the read lands.
pub fn worktree_sidebar<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
    git: bool,
) -> Element<'a, SidebarAction> {
    let (panel, navigation) = standalone_navigation(panel, git);
    worktree_panel(model, state, width, modal_open, panel, navigation, true)
}

/// Standalone panel with application-owned footer actions.
pub fn worktree_sidebar_with_footer<'a, M: 'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
    git: bool,
    footer: Element<'a, M>,
    map: impl Fn(SidebarAction) -> M + 'a,
) -> Element<'a, M> {
    let (panel, navigation) = standalone_navigation(panel, git);
    widget::column::with_children(vec![
        worktree_panel(model, state, width, modal_open, panel, navigation, false).map(map),
        crate::design::horizontal_rule(),
        footer,
    ])
    .width(Length::Fixed(sidebar_pane_width(width)))
    .height(Length::Fill)
    .into()
}

/// The panel and tab row the standalone sidebar shows for a folder that is,
/// or is not, a Git worktree.
fn standalone_navigation(panel: WorktreePanel, git: bool) -> (WorktreePanel, PanelNavigation) {
    if git {
        (panel, PanelNavigation::Panels)
    } else {
        (WorktreePanel::Files, PanelNavigation::None)
    }
}

/// [`worktree_sidebar`] for a panel whose body the shell renders itself: the
/// sidebar keeps the worktree header, its panel switch, and the loading and
/// error notices, and `body` fills the space beneath them once the worktree
/// is readable. Every panel the sidebar owns renders as usual.
pub fn worktree_sidebar_with_body<'a, M: 'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
    git: bool,
    body: Element<'a, M>,
    footer: Element<'a, M>,
    map: impl Fn(SidebarAction) -> M + 'a,
) -> Element<'a, M> {
    let ready = bound_source_control(model, state)
        .filter(|source| matches!(source.data_state, SidebarDataState::Ready));
    let (panel, navigation) = standalone_navigation(panel, git);
    let (Some(source), WorktreePanel::PullRequests) = (ready, panel) else {
        return worktree_sidebar_with_footer(
            model, state, width, modal_open, panel, git, footer, map,
        );
    };
    let worktree = bound_worktree(model, source);
    let view = PanelView::new(panel, navigation, state);
    let header = source_control_header(state, source, worktree, modal_open, view);
    let navigation = panel_navigation(
        source,
        modal_open,
        view,
        navigation,
        sidebar_pane_width(width),
    );
    let notices: Element<'a, SidebarAction> = source_control_notices(source).into();
    // The body sits between the sidebar's own header and footer, so each of
    // those maps on its own; the mapper is shared rather than cloned.
    let map = std::rc::Rc::new(map);
    let map_header = map.clone();
    let map_navigation = map.clone();
    let map_notices = map.clone();
    let pane = widget::column::with_children(vec![
        widget::container(worktree_panel_chrome(
            header.map(move |action| map_header(action)),
            navigation.map(|navigation| navigation.map(move |action| map_navigation(action))),
            notices.map(move |action| map_notices(action)),
            Some(body),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        crate::design::horizontal_rule(),
        footer,
    ])
    .width(Length::Fill)
    .height(Length::Fill);
    widget::container(pane)
        .width(Length::Fixed(sidebar_pane_width(width)))
        .height(Length::Fill)
        .into()
}

/// The pane width the worktree sidebar renders at for a requested `width`.
fn sidebar_pane_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(0.0, design::INSPECTOR_MAX_WIDTH)
    } else {
        design::INSPECTOR_DEFAULT_WIDTH
    }
}

/// The sidebar row for the worktree a source-control model describes.
fn bound_worktree<'a>(
    model: &'a SidebarModel,
    source: &SourceControlModel,
) -> Option<&'a SidebarWorktree> {
    model
        .projects
        .iter()
        .find(|project| project.id == source.key.project_id)
        .and_then(|project| {
            project
                .worktrees
                .iter()
                .find(|worktree| worktree.id == source.key.worktree_id)
        })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorktreePanel {
    #[default]
    Files,
    Changes,
    /// The shell's pull request review. The sidebar draws this panel's
    /// header and notices; the shell supplies the body through
    /// [`worktree_sidebar_with_body`], because the review's actions are not
    /// sidebar actions.
    PullRequests,
}

impl WorktreePanel {
    const ALL: [Self; 3] = [Self::Files, Self::Changes, Self::PullRequests];

    /// The tab label. Pull requests has a short form for a narrow pane,
    /// where three full words do not fit on one row.
    fn label(self, narrow: bool) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Changes => "Changes",
            Self::PullRequests if narrow => "PRs",
            Self::PullRequests => "Pull requests",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Files => "Files in this worktree",
            Self::Changes => "Uncommitted changes in this worktree",
            Self::PullRequests => "Pull requests for this repository",
        }
    }
}

/// A count as a tab states it. Past three digits the exact figure stops
/// telling the reader anything the section headers do not, and a wider figure
/// would push the tab row past a narrow pane.
fn tab_count_label(count: usize) -> Box<str> {
    if count > 999 {
        "999+".into()
    } else {
        count.to_string().into_boxed_str()
    }
}

/// Below this pane width the panel tabs use their short labels and compact
/// insets. At it, "Files", "Changes 999+", and "Pull requests" fit with their
/// full insets; below it, the short forms fit down to the narrowest sidebar.
const FULL_PANEL_LABELS_MIN_WIDTH: f32 = 320.0;

/// What the tab row beneath the worktree header switches between.
///
/// A pane gets exactly one level of navigation. Where the host's own chrome
/// already picked the panel, the row switches that panel's views; where the
/// host has no such chrome, the row switches the panels themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelNavigation {
    /// No row: the host's chrome chose a panel with a single view.
    None,
    /// The Git panel's Changes, Branch, and History views, for the cagents
    /// context inspector, whose own tab strip chose the Git panel.
    GitViews,
    /// Files, Changes, and Pull requests, for the standalone editor. It loads
    /// no history and no base comparison, so its Git panel is Changes alone
    /// rather than two views that could only ever say "not loaded".
    Panels,
}

/// The one view on screen, whichever level of navigation chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelView {
    Files,
    Git(ChangesTab),
    PullRequests,
}

impl PanelView {
    fn new(panel: WorktreePanel, navigation: PanelNavigation, state: &SidebarState) -> Self {
        match panel {
            WorktreePanel::Files => Self::Files,
            WorktreePanel::PullRequests => Self::PullRequests,
            WorktreePanel::Changes => Self::Git(match navigation {
                PanelNavigation::GitViews => state.changes_tab,
                PanelNavigation::None | PanelNavigation::Panels => ChangesTab::Changes,
            }),
        }
    }

    fn panel(self) -> WorktreePanel {
        match self {
            Self::Files => WorktreePanel::Files,
            Self::Git(_) => WorktreePanel::Changes,
            Self::PullRequests => WorktreePanel::PullRequests,
        }
    }
}

fn worktree_panel<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    width: f32,
    modal_open: bool,
    panel: WorktreePanel,
    navigation: PanelNavigation,
    settings_footer: bool,
) -> Element<'a, SidebarAction> {
    let width = sidebar_pane_width(width);
    let content = match bound_source_control(model, state) {
        Some(source) => source_control_panel(
            state,
            source,
            bound_worktree(model, source),
            &model.external_editors,
            model.file_management && matches!(model.data_state, SidebarDataState::Ready),
            modal_open,
            panel,
            navigation,
            width,
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
                crate::ui_controls::button::secondary("Open folder")
                    .on_press_maybe(gated(SidebarAction::OpenRepositoryPicker, modal_open))
                    .into(),
            ])
            .spacing(space::S)
            .width(Length::Fill),
        )
        .padding(space::M)
        .width(Length::Fill)
        .into(),
    };
    let pane = if settings_footer {
        widget::column::with_children(vec![
            widget::container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            crate::design::horizontal_rule().into(),
            footer(false, modal_open),
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        content
    };
    widget::container(pane)
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .into()
}

pub(crate) fn bound_source_control<'a>(
    model: &'a SidebarModel,
    state: &SidebarState,
) -> Option<&'a SourceControlModel> {
    let key = state.active_worktree()?;
    model
        .source_control
        .as_ref()
        .filter(|source| &source.key == key)
}

/// The worktree title row: the worktree's identity, then the options of the
/// view on screen - the file filter for Files, the tree/flat switch for a
/// list of changed files - and nothing else. Navigation lives in its own row
/// beneath, so an option never sits beside a peer it does not belong to.
fn source_control_header<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    worktree: Option<&'a SidebarWorktree>,
    modal_open: bool,
    view: PanelView,
) -> Element<'a, SidebarAction> {
    let chrome = &source.chrome;
    let key = &source.key;
    let activity = source.progress_label().or_else(|| {
        if matches!(source.data_state, SidebarDataState::Loading) {
            Some("Loading worktree…")
        } else if matches!(view, PanelView::Files)
            && matches!(source.file_search, FileSearch::Pending { .. })
            && !state.files_query().trim().is_empty()
        {
            Some("Searching files…")
        } else if matches!(view, PanelView::Git(ChangesTab::History)) && source.history_loading {
            Some("Loading commits…")
        } else {
            None
        }
    });
    // Replace the identity glyph in place: neither the title nor the body
    // gives up room when a background read or mutation starts.
    let mut header = widget::row::with_children(vec![
        crate::interaction::tooltip(
            widget::container(if activity.is_some() {
                crate::ui_controls::activity_spinner(14.0, design::warning_color)
            } else {
                worktree_icon().icon().size(ROW_ICON_SIZE).into()
            })
            .center(Length::Fixed(ROW_ICON_WIDTH)),
            activity.unwrap_or("Worktree"),
        ),
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
    match view {
        PanelView::Files => {
            header = header.push(
                crate::ui_controls::icon_button(
                    Icon::Search,
                    if state.files_filter_open {
                        "Hide file filter"
                    } else {
                        "Filter files"
                    },
                )
                .selected(state.files_filter_open)
                .on_press_maybe(gated(
                    SidebarAction::ToggleFilesFilter { key: key.clone() },
                    modal_open,
                ))
                .id(chrome.files_filter_id.clone()),
            );
        }
        // File browsing is always a tree, so only a list of changed files
        // offers the tree/flat switch: the flat list exists for reading
        // diffs, not for browsing.
        PanelView::Git(tab) if tab.lists_files() => {
            header = header.push(list_mode_toggle(
                key,
                chrome,
                state.change_list_mode,
                modal_open,
            ));
        }
        PanelView::Git(_) | PanelView::PullRequests => {}
    }
    header
        .align_y(Alignment::Center)
        .spacing(space::XS)
        .height(design::HEADER_CONTENT_HEIGHT)
        .width(Length::Fill)
        .into()
}

/// Error notices beneath the header. Activity stays in the header's fixed
/// icon slot so a background read or mutation cannot move the body.
fn source_control_notices<'a>(
    source: &'a SourceControlModel,
) -> cosmic::iced::widget::Column<'a, SidebarAction, cosmic::Theme, cosmic::Renderer> {
    widget::column::with_capacity(3)
        .spacing(space::XS)
        .push(slot(source.error.as_deref().map(|error| {
            design::caption(error)
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into()
        })))
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
    navigation: PanelNavigation,
    width: f32,
) -> Element<'a, SidebarAction> {
    let chrome = &source.chrome;
    let key = &source.key;
    let view = PanelView::new(panel, navigation, state);
    let header = source_control_header(state, source, worktree, modal_open, view);
    let tabs = panel_navigation(source, modal_open, view, navigation, width);
    let mut notices = source_control_notices(source);

    match &source.data_state {
        SidebarDataState::Loading => {
            // A read usually finishes within a frame or two, and a caption
            // that says "loading" for that long is a flash of text where the
            // files are about to be. The panel stays blank under its header;
            // the header's own busy spinner is what says a read is running.
            return worktree_panel_chrome(header, tabs, notices.into(), None);
        }
        SidebarDataState::Error(message) => {
            notices = notices
                .push(
                    design::caption(format!("Couldn’t read this worktree. {message}"))
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                )
                .push(
                    crate::ui_controls::button::secondary("Try again")
                        .id(chrome.retry_id.clone())
                        .on_press_maybe(gated(
                            SidebarAction::RefreshWorktree { key: key.clone() },
                            modal_open,
                        )),
                );
            return worktree_panel_chrome(header, tabs, notices.into(), None);
        }
        SidebarDataState::Ready => {}
    }

    // The shell renders this panel's body; see `worktree_sidebar_with_body`.
    if matches!(view, PanelView::PullRequests) {
        return worktree_panel_chrome(header, tabs, notices.into(), None);
    }
    let PanelView::Git(tab) = view else {
        return worktree_panel_chrome(
            header,
            tabs,
            notices.into(),
            Some(files_section(
                state,
                source,
                modal_open,
                editors,
                file_management,
            )),
        );
    };

    let body = match tab {
        ChangesTab::Changes => changes_section(state, source, modal_open),
        ChangesTab::Branch => branch_changes_section(state, source, modal_open),
        ChangesTab::History => history_section(state, source, modal_open),
    };
    // Committing is the panel's terminal action, so it sits at the panel's
    // bottom edge rather than after however many change rows the scroll
    // region happens to hold. It belongs to the working tree, so only the
    // working-tree view carries it.
    let footer = matches!(tab, ChangesTab::Changes).then(|| {
        widget::container(changes_footer(state, source, modal_open))
            .padding(Padding::from([space::S, space::S]))
            .width(Length::Fill)
            .into()
    });

    worktree_panel_chrome_with_footer(
        header,
        tabs,
        notices.into(),
        Some(crate::interaction::keyboard_list(
            widget::scrollable(
                widget::container(body)
                    .padding(design::with_vertical_rail(Padding::from([0.0, space::S])))
                    .width(Length::Fill),
            )
            .direction(sidebar_scroll_direction())
            .class(cosmic::style::iced::Scrollable::Minimal)
            .width(Length::Fill)
            .height(Length::Fill),
            !modal_open,
        )),
        footer,
    )
}

/// The pane's one row of navigation, or `None` where there is nothing to
/// switch between.
///
/// Every tab is a word, never a bare glyph, and each keeps a stable id so
/// focus and the accessibility tree survive a switch. A view that holds a
/// countable list states its count beside its name, so what is behind a tab
/// is readable before it is opened; the working tree's count takes the same
/// warning tone the shell uses for a dirty worktree.
fn panel_navigation<'a>(
    source: &'a SourceControlModel,
    modal_open: bool,
    view: PanelView,
    navigation: PanelNavigation,
    width: f32,
) -> Option<Element<'a, SidebarAction>> {
    let key = &source.key;
    let chrome = &source.chrome;
    let changes_count = || {
        source
            .change_count_label
            .as_deref()
            .map(|count| (Cow::Borrowed(count), design::warning_text as fn(&_) -> _))
    };
    let tabs = match navigation {
        PanelNavigation::None => return None,
        PanelNavigation::GitViews => ChangesTab::ALL
            .into_iter()
            .map(|tab| crate::ui_controls::ViewTab {
                label: Cow::Borrowed(tab.label()),
                description: Cow::Borrowed(tab.description()),
                count: match tab {
                    ChangesTab::Changes => changes_count(),
                    ChangesTab::Branch => match &source.branch_changes {
                        BranchChanges::Ready(branch) if !branch.rows.is_empty() => Some((
                            Cow::Borrowed(&*branch.count_label),
                            design::muted_text as fn(&_) -> _,
                        )),
                        _ => None,
                    },
                    ChangesTab::History => None,
                },
                selected: view == PanelView::Git(tab),
                id: Some(chrome.changes_tab_id(tab).clone()),
                on_press: gated(
                    SidebarAction::SetChangesTab {
                        key: key.clone(),
                        tab,
                    },
                    modal_open,
                ),
            })
            .collect(),
        // The host said this is a Git worktree; see `worktree_sidebar`.
        PanelNavigation::Panels => {
            let narrow = width < FULL_PANEL_LABELS_MIN_WIDTH;
            WorktreePanel::ALL
                .into_iter()
                .map(|panel| crate::ui_controls::ViewTab {
                    label: Cow::Borrowed(panel.label(narrow)),
                    description: Cow::Borrowed(panel.description()),
                    count: match panel {
                        WorktreePanel::Changes => changes_count(),
                        WorktreePanel::Files | WorktreePanel::PullRequests => None,
                    },
                    selected: view.panel() == panel,
                    id: Some(chrome.panel_tab_id(panel).clone()),
                    on_press: gated(SidebarAction::SetWorktreePanel { panel }, modal_open),
                })
                .collect()
        }
    };
    Some(
        widget::container(crate::ui_controls::view_tabs(
            tabs,
            width < FULL_PANEL_LABELS_MIN_WIDTH,
        ))
        .padding(Padding::from([space::XS, space::S]))
        .width(Length::Fill)
        .into(),
    )
}

/// Worktree title, full-width rule, the pane's tab row when it has one, then
/// padded notices and the panel body.
///
/// The projects sidebar already pads its header without padding the rule
/// beneath it. Wrapping both in one inset container left a gap between the
/// rule and the pane's vertical borders, so the files and Git headers looked
/// disconnected from the splitter they share with the editor tab strip.
fn worktree_panel_chrome<'a, M: 'a>(
    header: Element<'a, M>,
    tabs: Option<Element<'a, M>>,
    notices: Element<'a, M>,
    body: Option<Element<'a, M>>,
) -> Element<'a, M> {
    worktree_panel_chrome_with_footer(header, tabs, notices, body, None)
}

/// The same chrome with a region pinned below the scrolling body, for the
/// commit box: a terminal action the user must not have to scroll to reach.
fn worktree_panel_chrome_with_footer<'a, M: 'a>(
    header: Element<'a, M>,
    tabs: Option<Element<'a, M>>,
    notices: Element<'a, M>,
    body: Option<Element<'a, M>>,
    footer: Option<Element<'a, M>>,
) -> Element<'a, M> {
    // The divider is the header's bottom edge; spacing would move it below
    // the tab strip's matching rule.
    let mut chrome = widget::column::with_capacity(4);
    chrome = chrome.push(
        widget::container(header)
            .padding([0.0, space::S])
            .width(Length::Fill),
    );
    chrome = chrome.push(crate::design::horizontal_rule());
    // Always a child, so the notices below keep their index as the row
    // comes and goes; see `source_control_notices`.
    chrome = chrome.push(widget::column::with_children(tabs).width(Length::Fill));
    chrome = chrome.push(
        widget::container(notices)
            .padding([0.0, space::S])
            .width(Length::Fill),
    );
    match body {
        Some(body) => {
            let mut column = widget::column::with_capacity(4).push(chrome).push(body);
            if let Some(footer) = footer {
                column = column.push(crate::design::horizontal_rule()).push(footer);
            }
            column.width(Length::Fill).height(Length::Fill).into()
        }
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
    icon: Icon,
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
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
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
    actions: Option<Element<'a, SidebarAction>>,
    body: impl FnOnce() -> Element<'a, SidebarAction>,
) -> Element<'a, SidebarAction> {
    let parts = SectionHeader::new(key, section, count, expanded, modal_open);
    let row = button::custom(
        widget::row::with_children(vec![
            widget::container(parts.icon.handle().icon().size(ROW_ICON_SIZE))
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
        .spacing(space::XS)
        .height(Length::Fill),
    )
    .class(sidebar_button_style(false))
    .id(id)
    .name(parts.action_name.clone())
    .padding([0.0, space::XS])
    .height(Length::Fixed(PANEL_ROW_HEIGHT))
    .width(Length::Fill)
    .on_press_maybe(parts.toggle);
    let header = crate::interaction::tooltip(crate::interaction::hover(row), parts.action_name);
    let header: Element<'a, SidebarAction> = if let Some(actions) = actions {
        RowInteraction::actions(header.into(), actions, 0.0).into()
    } else {
        header.into()
    };
    let mut column = widget::column::with_capacity(2).push(header);
    if expanded {
        column = column.push(widget::container(body()).padding([0.0, 0.0, space::S, 0.0]));
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
    let key = &source.key;
    let filter_key = key.clone();
    // A text input with no `on_input` is disabled, which is what standing the
    // filter down means here: the pane keeps the focus it held when the dialog
    // opened, so a keystroke the overlay does not consume would otherwise still
    // be typed into this box.
    let mut filter = crate::ui_controls::field::search("Filter files", state.files_query())
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
            let (visible, hidden) = file_row_page(
                &source.files,
                &state.expanded_directories,
                &folded,
                true,
                false,
                state.files_page,
            );
            let hidden = hidden.saturating_sub(visible.len());
            (visible, hidden, None)
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
            let (visible, hidden) = file_row_page(
                &source.files,
                &state.expanded_directories,
                &folded,
                flat,
                false,
                state.files_page,
            );
            let notice = if source.files.is_empty() {
                Some("This worktree has no tracked files.")
            } else if visible.is_empty() {
                Some("No files match this filter.")
            } else {
                None
            };
            let hidden = if flat {
                hidden.saturating_sub(visible.len())
            } else {
                hidden
            };
            (visible, hidden, notice)
        }
    };
    let page = if flat {
        0
    } else {
        state
            .files_page
            .min(hidden.saturating_sub(1) / MAX_VISIBLE_FILE_ROWS)
    };
    let mut rows = widget::column::with_capacity(visible.len());
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
        }
        .opacity(if row.ignored { 0.5 } else { 1.0 });
        let selected = matches!(
            state.panel_selection.as_ref(),
            Some(PanelSelection::File(path)) if *path == row.path
        );
        let identity = panel_row(
            &*chrome.accessible_name,
            if flat || page > 0 { 0 } else { row.depth },
            widget::row::with_children(vec![
                widget::container(file_icon)
                    .center_x(Length::Fixed(ROW_ICON_WIDTH))
                    .into(),
                design::body(if flat || page > 0 {
                    row.path.display()
                } else {
                    Cow::Borrowed(&*chrome.label)
                })
                .class(if row.ignored {
                    theme::Text::Custom(design::muted_text)
                } else {
                    theme::Text::Default
                })
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(space::XS),
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
        if expanded && let Some(error) = source.ignored_directory_errors.get(&row.path) {
            rows = rows.push(
                widget::container(
                    design::caption(format!(
                        "Could not list {}: {error} Collapse and expand the folder to retry.",
                        row.path.display()
                    ))
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word),
                )
                .padding([0.0, space::S])
                .width(Length::Fill),
            );
        }
    }
    let list = widget::column::with_children(vec![
        rows.into(),
        slot((flat && hidden > 0).then(|| {
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
            .spacing(space::XS),
        )
        .padding([0.0, space::S])
        .width(Length::Fill)
        .into(),
        widget::scrollable(
            widget::container(list)
                .padding(design::with_vertical_rail(Padding::from([
                    space::XS,
                    space::S,
                ])))
                .width(Length::Fill),
        )
        .id(files_scroll_id(key))
        .direction(sidebar_scroll_direction())
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        if flat {
            cosmic::iced::widget::Space::new().height(0).into()
        } else {
            file_pager(state, key, hidden, modal_open)
        },
    ])
    .height(Length::Fill)
    .into()
}

fn file_pager<'a>(
    state: &SidebarState,
    key: &ActiveWorktreeKey,
    total: usize,
    disabled: bool,
) -> Element<'a, SidebarAction> {
    if total <= MAX_VISIBLE_FILE_ROWS {
        return cosmic::iced::widget::Space::new().height(0).into();
    }
    let last = total.saturating_sub(1) / MAX_VISIBLE_FILE_ROWS;
    let page = state.files_page.min(last);
    let action = |page| {
        gated(
            SidebarAction::FilesPage {
                key: key.clone(),
                page,
            },
            disabled,
        )
    };
    widget::column::with_children(vec![
        design::caption(format!(
            "{}–{} of {total}",
            page * MAX_VISIBLE_FILE_ROWS + 1,
            ((page + 1) * MAX_VISIBLE_FILE_ROWS).min(total)
        ))
        .into(),
        widget::row::with_children(vec![
            crate::ui_controls::button::secondary("Previous")
                .width(Length::Fill)
                .on_press_maybe((page > 0).then(|| action(page - 1)).flatten())
                .into(),
            crate::ui_controls::button::secondary("Next")
                .width(Length::Fill)
                .on_press_maybe((page < last).then(|| action(page + 1)).flatten())
                .into(),
        ])
        .spacing(space::XS)
        .into(),
    ])
    .spacing(space::XS)
    .padding(space::XS)
    .into()
}

/// One bounded page of browsable file rows and the total visible row count.
///
/// Flat mode and queries show file paths without directory rows. Tree mode
/// shows only the children of expanded directories: everything starts
/// collapsed, so a whole project tree renders as its top level until the user
/// opens folders. Change review uses its own collapsed state below.
///
/// At most [`MAX_VISIBLE_FILE_ROWS`] references are ever collected. This runs
/// on every view rebuild, including rebuilds caused by terminal output the
/// panel has nothing to do with, so a worktree of a hundred thousand tracked
/// paths must not make a frame fill - and drop - a hundred-thousand-entry
/// vector to render four hundred rows. Counting the remainder allocates
/// nothing.
#[cfg(test)]
fn visible_file_rows<'a>(
    rows: &'a [FileRow],
    expanded: &BTreeSet<RepoPath>,
    folded_query: &str,
    flat: bool,
    expand_all: bool,
) -> (Vec<&'a FileRow>, usize) {
    file_row_page(rows, expanded, folded_query, flat, expand_all, 0)
}

fn file_row_page<'a>(
    rows: &'a [FileRow],
    expanded: &BTreeSet<RepoPath>,
    folded_query: &str,
    flat: bool,
    expand_all: bool,
    page: usize,
) -> (Vec<&'a FileRow>, usize) {
    let mut visible: Vec<&'a FileRow> = Vec::with_capacity(rows.len().min(MAX_VISIBLE_FILE_ROWS));
    let start = page.saturating_mul(MAX_VISIBLE_FILE_ROWS);
    let mut count = 0usize;
    if flat {
        for row in rows {
            if row.directory || !row.chrome.folded.contains(folded_query) {
                continue;
            }
            if count >= start && visible.len() < MAX_VISIBLE_FILE_ROWS {
                visible.push(row);
            }
            count += 1;
        }
        return (visible, count);
    }
    let mut skip_below: Option<usize> = None;
    for row in rows {
        if let Some(depth) = skip_below {
            if row.depth > depth {
                continue;
            }
            skip_below = None;
        }
        if count >= start && visible.len() < MAX_VISIBLE_FILE_ROWS {
            visible.push(row);
        }
        count += 1;
        if row.directory && !expand_all && !expanded.contains(&row.path) {
            skip_below = Some(row.depth);
        }
    }
    if visible.is_empty() && count > 0 && page > 0 {
        return file_row_page(
            rows,
            expanded,
            folded_query,
            flat,
            expand_all,
            (count - 1) / MAX_VISIBLE_FILE_ROWS,
        );
    }
    (visible, count)
}

/// Change trees start expanded. A folded directory hides its descendants
/// without walking a second copy of the tree or allocating hidden rows.
#[cfg(test)]
fn visible_change_rows<'a>(
    rows: &'a [FileRow],
    collapsed: Option<&BTreeSet<RepoPath>>,
    flat: bool,
) -> (Vec<&'a FileRow>, usize) {
    change_row_page(rows, collapsed, flat, 0)
}

/// Mount one bounded page, while retaining the complete change model.
fn change_row_page<'a>(
    rows: &'a [FileRow],
    collapsed: Option<&BTreeSet<RepoPath>>,
    flat: bool,
    page: usize,
) -> (Vec<&'a FileRow>, usize) {
    let mut visible = Vec::with_capacity(rows.len().min(MAX_VISIBLE_CHANGE_ROWS));
    let mut count = 0;
    let mut skip_below = None;
    for row in rows {
        if flat && row.directory {
            continue;
        }
        if let Some(depth) = skip_below {
            if row.depth > depth {
                continue;
            }
            skip_below = None;
        }
        if count >= page.saturating_mul(MAX_VISIBLE_CHANGE_ROWS)
            && visible.len() < MAX_VISIBLE_CHANGE_ROWS
        {
            visible.push(row);
        }
        count += 1;
        if !flat && row.directory && collapsed.is_some_and(|paths| paths.contains(&row.path)) {
            skip_below = Some(row.depth);
        }
    }
    if visible.is_empty() && count > 0 && page > 0 {
        return change_row_page(rows, collapsed, flat, (count - 1) / MAX_VISIBLE_CHANGE_ROWS);
    }
    (visible, count)
}

fn change_pager<'a>(
    state: &SidebarState,
    key: &ActiveWorktreeKey,
    scope: ChangeTreeScope,
    total: usize,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    if total <= MAX_VISIBLE_CHANGE_ROWS {
        return cosmic::iced::widget::Space::new().height(0).into();
    }
    let last = total.saturating_sub(1) / MAX_VISIBLE_CHANGE_ROWS;
    let page = state
        .change_pages
        .get(&scope)
        .copied()
        .unwrap_or(0)
        .min(last);
    let action = |target| {
        gated(
            SidebarAction::ChangePage {
                key: key.clone(),
                scope,
                page: target,
            },
            modal_open,
        )
    };
    widget::row::with_children(vec![
        crate::ui_controls::button::secondary("Previous")
            .on_press_maybe((page > 0).then(|| action(page - 1)).flatten())
            .into(),
        design::caption(format!(
            "{}–{} of {total}",
            page * MAX_VISIBLE_CHANGE_ROWS + 1,
            ((page + 1) * MAX_VISIBLE_CHANGE_ROWS).min(total)
        ))
        .into(),
        crate::ui_controls::button::secondary("Next")
            .on_press_maybe((page < last).then(|| action(page + 1)).flatten())
            .into(),
    ])
    .spacing(space::XS)
    .align_y(Alignment::Center)
    .into()
}

/// The tree/flat switch for a list of changed files, as one action that
/// names the arrangement it switches to. Two segments for a two-state choice
/// drew a second selector beside the panel's navigation.
fn list_mode_toggle<'a>(
    key: &ActiveWorktreeKey,
    chrome: &SourceControlChrome,
    current: FileListMode,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let (label, icon, mode) = match current {
        FileListMode::Tree => ("View as list", Icon::List, FileListMode::Flat),
        FileListMode::Flat => ("View as tree", Icon::Tree, FileListMode::Tree),
    };
    crate::ui_controls::icon_button(icon, label)
        .on_press_maybe(gated(
            SidebarAction::SetChangeListMode {
                key: key.clone(),
                mode,
            },
            modal_open,
        ))
        .id(chrome.list_mode_id.clone())
        .into()
}

fn changes_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let mut column = widget::column::with_capacity(source.groups().len() + 1).spacing(space::XS);
    if source.groups().iter().all(ChangeGroup::is_empty) {
        column = column.push(design::caption(
            if state.section_expanded(PanelSection::Untracked) {
                "Working tree clean."
            } else {
                "No tracked changes."
            },
        ));
    }
    for group in source.groups() {
        // A collapsed untracked group remains discoverable because its
        // snapshot omits the walk. Hide a measured empty group instead of
        // spending two rows announcing that there are no files.
        if group.is_empty()
            && (group.area != ChangeArea::Untracked
                || state.section_expanded(PanelSection::Untracked))
        {
            continue;
        }
        let section = PanelSection::for_area(group.area);
        let expanded = state.section_expanded(section);
        let actions = expanded.then(|| {
            // Bulk commands materialize the complete group when activated.
            widget::container(widget::row::with_children(
                group_commands(source, group, modal_open)
                    .into_iter()
                    .map(IconCommand::into_element),
            ))
            .align_right(Length::Fill)
            .height(PANEL_ROW_HEIGHT)
            .into()
        });
        column = column.push(panel_section(
            &source.key,
            section,
            source.chrome.section_id(section).clone(),
            // A count matters most while the rows are not drawn: a bare
            // "Unstaged" does not say it is folding forty files away.
            Some(RowCount::exact(group.rows().len())),
            expanded,
            modal_open,
            actions,
            || {
                if group.is_empty() {
                    let noun = match group.area {
                        ChangeArea::Conflict => "conflicted files",
                        ChangeArea::Staged => "staged changes",
                        ChangeArea::Unstaged => "unstaged changes",
                        ChangeArea::Untracked => "untracked files",
                    };
                    design::caption(format!("No {noun}.")).into()
                } else {
                    change_group_view(state, source, group, modal_open)
                }
            },
        ));
    }
    // The publish form is several rows tall and grows a wrapping refusal, so
    // it stays in the scroll region: pinned beside the commit box it could
    // push its own Cancel and Publish past the bottom of a short panel.
    if let Some(publish) = state
        .publish_draft()
        .filter(|draft| draft.key == source.key)
    {
        column = column.push(crate::design::horizontal_rule());
        column = column.push(publish_picker(source, publish, modal_open));
    }
    column.into()
}

fn branch_changes_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let mut column = widget::column::with_capacity(4).spacing(space::XS);
    let branch = match &source.branch_changes {
        BranchChanges::Ready(branch) => branch,
        other => {
            return match other {
                BranchChanges::Unavailable(reason) => column.push(design::caption(*reason)).into(),
                BranchChanges::Loading(base_ref) => column
                    .push(
                        widget::row::with_children(vec![
                            crate::ui_controls::activity_spinner(14.0, design::warning_color),
                            design::caption(format!("Comparing with {}…", base_ref.short_name()))
                                .into(),
                        ])
                        .spacing(space::S)
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
                    .push(
                        crate::ui_controls::button::secondary("Try again").on_press_maybe(gated(
                            SidebarAction::RefreshWorktree {
                                key: source.key.clone(),
                            },
                            modal_open,
                        )),
                    )
                    .into(),
                BranchChanges::Ready(_) => unreachable!(),
            };
        }
    };
    // The tab above already names the view and counts its rows; the body
    // says only what the rows were measured against.
    column = column.push(design::caption(&*branch.base_label));
    if branch.rows.is_empty() {
        return column
            .push(design::caption(
                "No committed changes relative to this base.",
            ))
            .into();
    }
    let flat = state.change_list_mode == FileListMode::Flat;
    let scope = ChangeTreeScope::Branch;
    let collapsed = state.collapsed_change_directories.get(&scope);
    let (tree, total) = change_row_page(
        &branch.tree,
        collapsed,
        flat,
        state.change_pages.get(&scope).copied().unwrap_or(0),
    );
    let mut rows = Vec::with_capacity(tree.len());
    for file in tree {
        if file.directory {
            let folded = collapsed.is_some_and(|paths| paths.contains(&file.path));
            rows.push(crate::sticky_tree::Row {
                depth: file.depth,
                directory: true,
                element: panel_row(
                    format!(
                        "{} folder {}",
                        if folded { "Expand" } else { "Collapse" },
                        file.path.display()
                    ),
                    file.depth,
                    widget::row::with_children(vec![
                        crate::file_icon::folder(!folded).into(),
                        design::body(&*file.chrome.label).into(),
                    ])
                    .spacing(space::XS)
                    .align_y(Alignment::Center),
                    Some(SidebarAction::ToggleChangeDirectory {
                        key: source.key.clone(),
                        scope,
                        path: file.path.clone(),
                    }),
                    false,
                    modal_open,
                ),
            });
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
        rows.push(crate::sticky_tree::Row { depth: file.depth, directory: false, element: panel_row(
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
            ]).spacing(space::XS).align_y(Alignment::Center),
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
        ) });
    }
    column = column.push(crate::sticky_tree::rows(rows, PANEL_ROW_HEIGHT));
    column
        .push(change_pager(state, &source.key, scope, total, modal_open))
        .into()
}

fn change_group_view<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    group: &'a ChangeGroup,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let flat = state.change_list_mode == FileListMode::Flat;
    let scope = ChangeTreeScope::Area(group.area);
    let collapsed = state.collapsed_change_directories.get(&scope);
    let (tree, total) = change_row_page(
        &group.tree,
        collapsed,
        flat,
        state.change_pages.get(&scope).copied().unwrap_or(0),
    );
    // The disclosure header above this view already carries the group's name,
    // its count, and its commands; the body is only the rows.
    let mut rows = Vec::with_capacity(tree.len());
    for file in tree {
        if file.directory {
            let folded = collapsed.is_some_and(|paths| paths.contains(&file.path));
            rows.push(crate::sticky_tree::Row {
                depth: file.depth,
                directory: true,
                element: panel_row(
                    format!(
                        "{} folder {}",
                        if folded { "Expand" } else { "Collapse" },
                        file.path.display()
                    ),
                    file.depth,
                    widget::row::with_children(vec![
                        crate::file_icon::folder(!folded).into(),
                        design::body(&*file.chrome.label).into(),
                    ])
                    .spacing(space::XS)
                    .align_y(Alignment::Center),
                    Some(SidebarAction::ToggleChangeDirectory {
                        key: source.key.clone(),
                        scope,
                        path: file.path.clone(),
                    }),
                    false,
                    modal_open,
                ),
            });
        } else if let Some(row) = group.visible_path(&file.path) {
            rows.push(crate::sticky_tree::Row {
                depth: file.depth,
                directory: false,
                element: change_row_view(
                    state,
                    source,
                    group.area,
                    row,
                    modal_open,
                    if flat { 0 } else { file.depth },
                    !flat,
                ),
            });
        }
    }
    let mut column =
        widget::column::with_children(vec![crate::sticky_tree::rows(rows, PANEL_ROW_HEIGHT)]);
    column = column.push(change_pager(state, &source.key, scope, total, modal_open));
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
        .spacing(space::XS);
    status = status.push(
        if row.entry.conflict.is_none()
            && matches!(row.entry.kind, ChangeKind::Added | ChangeKind::Untracked)
        {
            status_icon(Icon::AddedFile, "Added file", tone)
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
        .spacing(space::XS),
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
            .align_y(Alignment::Center),
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
            .padding([0.0, space::XS])
            .align_right(Length::Fill)
            .into(),
        0.0,
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
    icon: Icon,
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
/// have nothing to unstage from. An in-flight Git command disables the actions
/// in place, because a second mutation against the same index would race the first.
fn group_commands<'a>(
    source: &'a SourceControlModel,
    group: &'a ChangeGroup,
    modal_open: bool,
) -> Vec<IconCommand<'a>> {
    let key = &source.key;
    let chrome = &group.chrome;
    if group.is_empty() {
        return Vec::new();
    }
    let mut commands = Vec::with_capacity(2);
    if group.area == ChangeArea::Staged {
        commands.push(IconCommand {
            icon: Icon::Minus,
            name: &chrome.unstage_all_name,
            action: gated(
                SidebarAction::ChangeGroup {
                    key: key.clone(),
                    area: group.area,
                    command: ChangeGroupCommand::Unstage,
                },
                modal_open || source.busy(),
            ),
            id: chrome.unstage_all_id.clone(),
        });
    } else {
        commands.push(IconCommand {
            icon: Icon::Plus,
            name: &chrome.stage_all_name,
            action: gated(
                SidebarAction::ChangeGroup {
                    key: key.clone(),
                    area: group.area,
                    command: ChangeGroupCommand::Stage,
                },
                modal_open || source.busy(),
            ),
            id: chrome.stage_all_id.clone(),
        });
    }
    if matches!(group.area, ChangeArea::Unstaged | ChangeArea::Untracked) {
        commands.push(IconCommand {
            icon: Icon::Undo,
            name: &chrome.discard_all_name,
            action: gated(
                SidebarAction::ChangeGroup {
                    key: key.clone(),
                    area: group.area,
                    command: ChangeGroupCommand::Discard,
                },
                modal_open || source.busy(),
            ),
            id: chrome.discard_all_id.clone(),
        });
    }
    commands
}

/// Resolve against the current fenced model, rather than cloning every path
/// into every redraw's button message.
pub fn resolve_group_command(
    source: &SourceControlModel,
    action: &SidebarAction,
) -> Option<SidebarAction> {
    let SidebarAction::ChangeGroup { key, area, command } = action else {
        return None;
    };
    if key != &source.key || source.busy() {
        return None;
    }
    let group = source.group(*area)?;
    if group.is_empty() {
        return None;
    }
    let selection = || group.rows().iter().map(ChangeRow::selection).collect();
    match command {
        ChangeGroupCommand::Stage if *area != ChangeArea::Staged => Some(SidebarAction::Stage {
            key: key.clone(),
            selection: selection(),
        }),
        ChangeGroupCommand::Unstage if *area == ChangeArea::Staged => {
            Some(SidebarAction::Unstage {
                key: key.clone(),
                selection: selection(),
            })
        }
        ChangeGroupCommand::Discard
            if matches!(area, ChangeArea::Unstaged | ChangeArea::Untracked) =>
        {
            Some(SidebarAction::RequestDiscard {
                key: key.clone(),
                request: discard_request(key, *area, selection(), group),
            })
        }
        _ => None,
    }
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
    let mut commands = Vec::with_capacity(2);
    if area == ChangeArea::Staged {
        commands.push(IconCommand {
            icon: Icon::Minus,
            name: &chrome.unstage_name,
            action: gated(
                SidebarAction::Unstage {
                    key: key.clone(),
                    selection: vec![row.selection()],
                },
                modal_open || source.busy(),
            ),
            id: chrome.unstage_id.clone(),
        });
    } else {
        commands.push(IconCommand {
            icon: Icon::Plus,
            name: &chrome.stage_name,
            action: gated(
                SidebarAction::Stage {
                    key: key.clone(),
                    selection: vec![row.selection()],
                },
                modal_open || source.busy(),
            ),
            id: chrome.stage_id.clone(),
        });
    }
    if matches!(area, ChangeArea::Unstaged | ChangeArea::Untracked) {
        commands.push(IconCommand {
            icon: Icon::Undo,
            name: &chrome.discard_name,
            action: gated(
                SidebarAction::RequestDiscard {
                    key: key.clone(),
                    request: single_discard_request(key, area, row),
                },
                modal_open || source.busy(),
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

fn compose_commit_message(subject: &str, description: &str) -> String {
    if description.trim().is_empty() {
        subject.to_owned()
    } else {
        format!("{subject}\n\n{description}")
    }
}

fn commit_input_border(theme: &cosmic::Theme, focused: bool) -> cosmic::iced::Border {
    cosmic::iced::Border {
        radius: design::RADIUS_S.into(),
        width: design::BORDER_WIDTH,
        color: if focused {
            design::accent_color(theme)
        } else {
            design::hairline(theme)
        },
    }
}

fn commit_description_style(
    theme: &cosmic::Theme,
    status: cosmic::iced::widget::text_editor::Status,
) -> cosmic::iced::widget::text_editor::Style {
    cosmic::iced::widget::text_editor::Style {
        background: design::sunken(theme).into(),
        border: commit_input_border(
            theme,
            matches!(
                status,
                cosmic::iced::widget::text_editor::Status::Focused { .. }
            ),
        ),
        placeholder: design::muted_color(theme),
        value: theme.cosmic().palette.neutral_9.into(),
        selection: design::accent_color(theme),
    }
}

fn changes_footer<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let key = &source.key;
    let chrome = &source.chrome;
    let message = state.commit_message();
    let input_key = key.clone();
    let can_commit = !modal_open
        && !source.busy()
        && !state.commit_draft.subject.trim().is_empty()
        && (source.staged_count > 0 || (state.commit_draft.amend && source.head_oid.is_some()));
    let commit_action = SidebarAction::Commit {
        key: key.clone(),
        message: message.to_owned(),
        amend: state.commit_draft.amend,
    };
    let submit = commit_action.clone();
    let subject_submit = commit_action.clone();
    let mut subject_input =
        crate::ui_controls::field::editor("Commit subject", &state.commit_draft.subject_composer.0)
            .placeholder("Commit subject")
            .id(chrome.commit_input_id.clone())
            .font(cosmic::iced::Font::DEFAULT)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .height(Length::Fixed(design::TEXT_BODY * 1.3 + 16.0))
            .size(design::TEXT_BODY)
            .padding([space::S, 10.0])
            .style(commit_description_style)
            .key_binding(move |press| {
                if !matches!(
                    press.status,
                    cosmic::iced::widget::text_editor::Status::Focused { .. }
                ) {
                    return None;
                }
                if matches!(
                    press.key,
                    cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Enter)
                ) {
                    if press.modifiers.control() && can_commit {
                        Some(cosmic::iced::widget::text_editor::Binding::Custom(
                            subject_submit.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    cosmic::iced::widget::text_editor::Binding::from_key_press(press)
                }
            });
    if !modal_open {
        let key = key.clone();
        let cursor_key = key.clone();
        subject_input = subject_input
            .on_action(move |action| SidebarAction::CommitSubjectEditorAction {
                key: key.clone(),
                action,
            })
            .on_cursor(move |cursor| SidebarAction::CommitSubjectEditorCursor {
                key: cursor_key.clone(),
                cursor,
            });
    }
    let mut column = widget::column::with_capacity(5)
        .spacing(space::S)
        .push(subject_input);
    let has_subject = !state.commit_draft.subject.trim().is_empty();

    let mut commit_input =
        crate::ui_controls::field::editor("Commit description", &state.commit_draft.composer.0)
            .placeholder("Description (optional)")
            .id(chrome.commit_description_id.clone())
            .height(Length::Fixed(
                (state.commit_draft.composer.0.line_count().clamp(2, 4) as f32)
                    * design::TEXT_BODY
                    * 1.3
                    + 16.0,
            ))
            .font(cosmic::iced::Font::DEFAULT)
            .style(commit_description_style)
            .size(design::TEXT_BODY)
            .padding([space::XS, space::S])
            .key_binding(move |press| {
                if !matches!(
                    press.status,
                    cosmic::iced::widget::text_editor::Status::Focused { .. }
                ) {
                    return None;
                }
                if press.modifiers.control()
                    && matches!(
                        press.key,
                        cosmic::iced::keyboard::Key::Named(
                            cosmic::iced::keyboard::key::Named::Enter
                        )
                    )
                {
                    can_commit
                        .then(|| cosmic::iced::widget::text_editor::Binding::Custom(submit.clone()))
                } else {
                    cosmic::iced::widget::text_editor::Binding::from_key_press(press)
                }
            });
    if !modal_open {
        let cursor_key = input_key.clone();
        commit_input = commit_input
            .on_action(move |action| SidebarAction::CommitEditorAction {
                key: input_key.clone(),
                action,
            })
            .on_cursor(move |cursor| SidebarAction::CommitEditorCursor {
                key: cursor_key.clone(),
                cursor,
            });
    }

    let commit_label = if state.commit_draft.amend {
        "Amend commit"
    } else {
        "Commit"
    };
    let commit_hint = if !has_subject {
        "Enter a commit summary"
    } else if source.busy() {
        "Git operation in progress"
    } else if source.staged_count == 0 && !state.commit_draft.amend {
        "Stage changes to commit"
    } else if !can_commit && state.commit_draft.amend && source.head_oid.is_none() {
        "No commit to amend"
    } else {
        commit_label
    };
    let commit = crate::interaction::tooltip(
        crate::ui_controls::button::primary(commit_label)
            .icon(Icon::Commit)
            .name(format!("{commit_label} (Ctrl+Enter)"))
            .id(chrome.commit_id.clone())
            .on_press_maybe(can_commit.then_some(commit_action)),
        format!("{commit_hint} (Ctrl+Enter)"),
    );

    if has_subject {
        column = column.push(commit_input);
    }
    let amend = crate::ui_controls::field::checkbox(state.commit_draft.amend)
        .label("Amend")
        .size(14)
        .text_size(design::TEXT_CAPTION)
        .spacing(6)
        .on_toggle_maybe(
            (!modal_open && !source.busy() && source.head_oid.is_some()).then(|| {
                let key = key.clone();
                move |amend| SidebarAction::AmendChanged {
                    key: key.clone(),
                    amend,
                }
            }),
        );
    let mut buttons = widget::row::with_capacity(4)
        .spacing(space::S)
        .align_y(cosmic::iced::Alignment::Center);
    buttons = buttons.push(commit);
    match (&source.branch, &source.upstream) {
        (Some(branch), Some(target)) => {
            let counts = match &source.tracking {
                TrackingState::Compared { ahead, behind, .. } if *behind > 0 => {
                    Some(format!("↑{ahead} ↓{behind}"))
                }
                TrackingState::Compared { ahead, .. } if *ahead > 0 => Some(format!("↑{ahead}")),
                _ => None,
            };
            let detail = counts
                .as_ref()
                .map_or_else(String::new, |counts| format!(" · {counts}"));
            let push = crate::ui_controls::icon_button(
                Icon::Push,
                format!(
                    "Push {} to {}{detail}",
                    branch.as_str(),
                    target
                        .remote_side_ref()
                        .map(|reference| format!("{}:{}", target.remote(), reference.as_str()))
                        .unwrap_or_else(|_| target.remote().to_owned())
                ),
            )
            .on_press_maybe(
                (source.can_push() && !modal_open).then(|| SidebarAction::Push {
                    key: key.clone(),
                    branch: branch.clone(),
                    target: target.clone(),
                }),
            )
            .id(chrome.push_id.clone());
            let mut upload = widget::row::with_children(vec![push.into()])
                .spacing(space::XS)
                .align_y(cosmic::iced::Alignment::Center);
            if let Some(counts) = counts {
                upload = upload.push(design::caption(counts));
            }
            buttons = buttons.push(upload);
        }
        (Some(branch), None) if !source.upstream_reported() => {
            buttons = buttons.push(
                crate::ui_controls::icon_button(
                    Icon::Push,
                    format!("Publish {}… Choose a remote and upstream", branch.as_str()),
                )
                .on_press_maybe((source.can_publish() && !modal_open).then(|| {
                    SidebarAction::OpenPublish {
                        key: key.clone(),
                        branch: branch.clone(),
                        remotes: source.remotes.clone(),
                    }
                }))
                .id(chrome.push_id.clone()),
            );
        }
        (Some(_), None) => {
            // `git status` reported an upstream this panel could not name, so
            // publishing would re-point the branch at whatever remote the
            // picker offered rather than push to the upstream it has.
            column = column.push(design::caption(
                "Upstream could not be read — nothing to push",
            ));
        }
        (None, _) => {
            // A branch this panel cannot name is not one it may invent a
            // mutation for, and each reason is stated as itself.
            column = column.push(design::caption(source.no_branch_reason()));
        }
    }
    column = column.push(
        buttons
            .push(widget::Space::new().width(Length::Fill))
            .push(amend),
    );
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
    let chrome = &source.chrome;
    let key = &publish.key;
    let mut remotes = widget::row::with_capacity(source.remotes.len()).spacing(space::XS);
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
    let mut branch_input = crate::ui_controls::field::text("Remote branch", &publish.remote_branch)
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
    let mut confirm =
        crate::ui_controls::button::secondary("Publish").id(chrome.publish_id.clone());
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
            crate::ui_controls::button::secondary("Cancel")
                .id(chrome.publish_cancel_id.clone())
                .on_press_maybe(gated(SidebarAction::CancelPublish, modal_open))
                .into(),
            confirm.into(),
        ])
        .spacing(space::S)
        .into(),
    ])
    .spacing(space::XS)
    .into()
}

/// This branch's own commits.
///
/// The list is a walk from `HEAD` — the application loads it with
/// [`crate::repository::HistoryRevision::Head`] — so it is the branch's
/// history. Selecting a row drills into its immutable changed-file list.
fn commit_changes_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    changes: &'a CommitChanges,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let key = &source.key;
    let commit = &changes.commit;
    let mut column = widget::column::with_capacity(5)
        .spacing(space::XS)
        .push(
            button::custom(
                widget::row::with_children(vec![
                    Icon::ChevronLeft.handle().icon().size(ROW_ICON_SIZE).into(),
                    design::body("History").into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center),
            )
            .class(sidebar_button_style(false))
            .on_press_maybe(gated(
                SidebarAction::CloseCommit { key: key.clone() },
                modal_open,
            )),
        )
        .push(design::heading(&*commit.chrome.subject).width(Length::Fill))
        .push(design::caption(&*commit.chrome.metadata).width(Length::Fill))
        .push(design::caption(if commit.parent.is_some() {
            "Changes against first parent"
        } else {
            "Initial commit"
        }));
    match &changes.result {
        None => column = column.push(design::caption("Loading changed files…")),
        Some(Err(error)) => {
            column = column
                .push(design::caption(error).width(Length::Fill))
                .push(
                    button::custom(design::body("Try again"))
                        .class(sidebar_button_style(false))
                        .on_press_maybe(gated(
                            SidebarAction::OpenCommit {
                                key: key.clone(),
                                oid: commit.oid.clone(),
                            },
                            modal_open,
                        )),
                );
        }
        Some(Ok(entries)) => {
            column = column.push(design::caption(format!("{} changed files", entries.len())));
            for entry in entries.iter().take(changes.visible_files) {
                let label = entry.old_path.as_ref().map_or_else(
                    || entry.path.display().into_owned(),
                    |old| format!("{} → {}", old.display(), entry.path.display()),
                );
                let tone = match entry.kind {
                    ChangeKind::Added => BadgeTone::Success,
                    ChangeKind::Deleted => BadgeTone::Danger,
                    _ => BadgeTone::Warning,
                };
                column = column.push(panel_row(
                    label.clone(),
                    0,
                    widget::row::with_children(vec![
                        crate::file_icon::icon(entry.path.as_path()).into(),
                        design::body(label)
                            .width(Length::Fill)
                            .wrapping(cosmic::iced::widget::text::Wrapping::None)
                            .ellipsize(cosmic::iced::widget::text::Ellipsize::Start(
                                EllipsizeHeightLimit::Lines(1),
                            ))
                            .into(),
                        semantic_caption(change_kind_label(entry.kind), tone),
                        design::caption(crate::repository::line_change_counts(
                            entry.added_lines,
                            entry.removed_lines,
                        ))
                        .into(),
                    ])
                    .spacing(space::XS)
                    .align_y(Alignment::Center),
                    Some(SidebarAction::OpenCommitFile {
                        key: key.clone(),
                        oid: commit.oid.clone(),
                        path: entry.path.clone(),
                    }),
                    matches!(&state.panel_selection, Some(PanelSelection::Commit { oid, path }) if oid == &commit.oid && path == &entry.path),
                    modal_open,
                ));
            }
            if entries.len() > changes.visible_files {
                column = column.push(
                    button::custom(design::caption(format!(
                        "Show more files ({} remaining)",
                        entries.len() - changes.visible_files
                    )))
                    .class(sidebar_button_style(false))
                    .on_press_maybe(gated(
                        SidebarAction::ShowMoreCommitFiles {
                            key: key.clone(),
                            oid: commit.oid.clone(),
                        },
                        modal_open,
                    )),
                );
            }
        }
    }
    column.into()
}

fn history_section<'a>(
    state: &'a SidebarState,
    source: &'a SourceControlModel,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let key = &source.key;
    if let Some(changes) = &source.commit_changes {
        return commit_changes_section(state, source, changes, modal_open);
    }
    // The notice, the row list, and the pager. The notice and the pager are
    // slots because both come and go — the notice the moment the first page
    // lands, the pager when the walk runs out — and the pager is a named
    // button.
    let mut column = widget::column::with_capacity(3).spacing(space::XS);
    column = column.push(slot(
        source
            .history_error
            .as_deref()
            .or_else(|| history_notice(source))
            .map(|notice| {
                design::caption(notice)
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into()
            }),
    ));
    let mut rows = widget::column::with_capacity(source.history.len());
    for row in &source.history {
        let chrome = &row.chrome;
        rows = rows.push(crate::interaction::keyboard_list_row(
            button::custom(
                widget::row::with_children(vec![
                    widget::container(Icon::Commit.handle().icon().size(ROW_ICON_SIZE))
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
                    .width(Length::Fill)
                    .into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center),
            )
            .padding([space::XS, space::XS])
            .class(sidebar_button_style(false))
            .on_press_maybe(gated(
                SidebarAction::OpenCommit {
                    key: key.clone(),
                    oid: row.oid.clone(),
                },
                modal_open,
            ))
            .width(Length::Fill),
        ));
    }
    column = column.push(rows);
    column = column.push(slot((!source.history_complete && !source.unborn).then(
        || {
            // The first page is not "more": labelling it that way while the list is
            // empty hides that nothing has been asked for yet.
            button::custom(design::caption(if source.history.is_empty() {
                format!("Load {HISTORY_PAGE_SIZE} commits")
            } else {
                format!("Load {HISTORY_PAGE_SIZE} more commits")
            }))
            .class(sidebar_button_style(false))
            .padding([0.0, space::XS])
            .height(PANEL_ROW_HEIGHT)
            .id(source.chrome.more_history_id.clone())
            .on_press_maybe(gated(
                SidebarAction::LoadMoreHistory {
                    key: key.clone(),
                    page: source.next_history_page,
                },
                modal_open || !source.can_load_history(),
            ))
            .into()
        },
    )));
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
    if source.history_loading && source.history.is_empty() {
        return Some("Loading commits…");
    }
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
/// Rows use positional widget state. Their accessible name labels them, and
/// no command addresses an individual row through a widget [`Id`]. Keeping
/// these rows nameless also avoids retaining state for every filtered entry.

fn panel_row<'a>(
    name: impl Into<Cow<'a, str>>,
    depth: usize,
    content: impl Into<Element<'a, SidebarAction>>,
    // `None` renders an inert hierarchy label.
    action: Option<SidebarAction>,
    selected: bool,
    modal_open: bool,
) -> Element<'a, SidebarAction> {
    let indent = u16::try_from(depth).unwrap_or(u16::MAX).saturating_mul(12);
    crate::interaction::keyboard_list_row(crate::interaction::hover(
        button::custom(
            widget::row::with_children(vec![
                cosmic::iced::widget::Space::new()
                    .width(Length::Fixed(f32::from(indent)))
                    .into(),
                content.into(),
            ])
            .align_y(Alignment::Center)
            .height(Length::Fill),
        )
        .class(sidebar_button_style(selected))
        .name(name)
        .selected(selected)
        .padding([0.0, space::XS])
        .height(Length::Fixed(PANEL_ROW_HEIGHT))
        .width(Length::Fill)
        .on_press_maybe(action.and_then(|action| gated(action, modal_open))),
    ))
}

/// The confirmation the application mounts before any discard runs.
///
/// It lives here so the copy, the counted paths, and the confirmed request all
/// come from the surface that raised the intent; `app.rs` only decides where
/// to put it.
pub fn discard_confirmation_dialog(pending: &DiscardConfirmation) -> Element<'_, SidebarAction> {
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
            design::title("Discard changes?").into(),
            design::body(detail.join(". "))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            design::caption("Discarded work is not recoverable from Git.").into(),
            widget::row::with_children(vec![
                crate::ui_controls::button::secondary("Cancel")
                    .id(discard_cancel_id())
                    .on_press(SidebarAction::CancelDiscard)
                    .into(),
                crate::ui_controls::button::destructive("Discard")
                    .id(discard_confirm_id())
                    .on_press(pending.confirm_action())
                    .into(),
            ])
            .spacing(space::S)
            .into(),
        ])
        .spacing(space::M),
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
        .spacing(space::XS)
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
pub fn sidebar_button_style(selected: bool) -> theme::Button {
    row_button_style::<false, true>(selected)
}

/// Navigation fills span their complete row; project labels remain plain on hover.
fn row_button_style<const FLAT: bool, const HOVER: bool>(selected: bool) -> theme::Button {
    fn appearance<const FLAT: bool>(
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
        style.icon_color = Some(if selected || focused {
            text
        } else {
            design::muted_icon_color(theme)
        });
        style.border_radius = if FLAT {
            [0.0; 4].into()
        } else {
            theme.cosmic().corner_radii.radius_s.into()
        };
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
    fn class<const SELECTED: bool, const FLAT: bool, const HOVER: bool>() -> theme::Button {
        theme::Button::Custom {
            active: Box::new(|focused, theme| {
                appearance::<FLAT>(theme, focused, SELECTED, 0.0, false)
            }),
            disabled: Box::new(|theme| appearance::<FLAT>(theme, false, SELECTED, 0.0, true)),
            hovered: Box::new(|focused, theme| {
                appearance::<FLAT>(
                    theme,
                    focused,
                    SELECTED,
                    if HOVER { 0.06 } else { 0.0 },
                    false,
                )
            }),
            pressed: Box::new(|focused, theme| {
                appearance::<FLAT>(
                    theme,
                    focused,
                    SELECTED,
                    if HOVER { 0.1 } else { 0.0 },
                    false,
                )
            }),
        }
    }
    if selected {
        class::<true, FLAT, HOVER>()
    } else {
        class::<false, FLAT, HOVER>()
    }
}

/// Compact source-control commands share the row height and hover geometry.
fn panel_icon_action<'a>(
    icon_name: Icon,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    crate::interaction::tooltip(
        button::custom(
            widget::container(icon::icon(icon_name.handle()).size(ROW_ICON_SIZE))
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
fn project_icon_action<'a>(
    icon_name: Icon,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    let label = label.into();
    crate::interaction::tooltip(
        button::custom(
            widget::container(widget::icon(icon_name.handle()).size(ROW_ICON_SIZE))
                .class(theme::Container::custom(|_| {
                    cosmic::iced::widget::container::Style::default()
                }))
                .center(Length::Fill),
        )
        .class(sidebar_button_style(false))
        .id(id)
        .name(label.clone())
        .padding(0)
        .width(ICON_CONTROL_SIZE)
        .height(ICON_CONTROL_SIZE)
        .on_press_maybe(action),
        label,
    )
}

/// One icon command in shared workspace chrome.
fn icon_action<'a>(
    icon_name: Icon,
    label: impl Into<Cow<'a, str>>,
    action: Option<SidebarAction>,
    id: Id,
) -> Element<'a, SidebarAction> {
    crate::ui_controls::icon_button(icon_name, label)
        .on_press_maybe(action)
        .id(id)
        .into()
}

/// A "new …" command that is a row of its own, on the grid of the rows it
/// creates.
///
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
pub enum BadgeTone {
    Neutral,
    Success,
    Warning,
    Danger,
}

fn status_summary(
    detached: bool,
    locked: bool,
    dirty: Option<&DirtyState>,
    untracked_omitted: bool,
    ahead: Option<u64>,
    behind: Option<u64>,
) -> String {
    let mut parts = Vec::with_capacity(4);
    match dirty {
        Some(dirty) if dirty.is_dirty() => parts.push(format!(
            "{} {}changes",
            dirty_count(dirty),
            if untracked_omitted { "tracked " } else { "" }
        )),
        Some(_) if untracked_omitted => parts.push("no tracked changes".to_owned()),
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

/// A row icon in one of the semantic tones.
///
/// The tone is resolved by the drawing theme rather than read from the
/// process-global one while the view is built, so a row that carries a tone
/// costs no theme lock per frame.
fn toned_icon<'a, Message: 'a>(icon_name: Icon, tone: BadgeTone) -> Element<'a, Message> {
    widget::container(widget::icon(icon_name.handle()).size(ROW_ICON_SIZE))
        .class(theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            cosmic::iced::widget::container::Style {
                icon_color: Some(match tone {
                    BadgeTone::Success => cosmic.success.base.into(),
                    BadgeTone::Warning => cosmic.warning.base.into(),
                    BadgeTone::Danger => cosmic.destructive.base.into(),
                    _ => design::muted_icon_color(theme),
                }),
                ..Default::default()
            }
        }))
        .into()
}

/// A badge: a toned glyph that explains itself on hover.
///
/// Public and generic over the message because the context rail draws the very
/// same badges as the navigation rail, and two rails describing one worktree
/// differently is the defect this shares its way out of.
pub fn status_icon<'a, Message: Clone + 'a>(
    icon_name: Icon,
    label: impl Into<Cow<'a, str>> + 'a,
    tone: BadgeTone,
) -> Element<'a, Message> {
    crate::interaction::tooltip(toned_icon(icon_name, tone), label)
}

pub fn check_indicator<Message: Clone + 'static>(status: CheckStatus) -> Element<'static, Message> {
    if status == CheckStatus::Pending {
        return crate::interaction::tooltip(
            crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::warning_color),
            "CI checks pending or running",
        );
    }
    status_icon(
        match status {
            CheckStatus::Passing => Icon::Check,
            CheckStatus::Failing => Icon::Error,
            _ => Icon::Info,
        },
        check_description(status),
        check_tone(status),
    )
}

#[derive(Debug)]
struct FilteredProject<'a> {
    project: &'a SidebarProject,
    worktrees: Vec<&'a SidebarWorktree>,
    channels: Vec<&'a SidebarChannel>,
    editors: &'a [SidebarEditor],
    state: &'a SidebarState,
}

/// Cache each visible row's rank once; terminal redraws must not scan the
/// complete durable order for every comparison.
pub(crate) fn apply_navigation_order<T>(
    rows: &mut [T],
    order: &[crate::workspace_model::NavigationItem],
    item: impl Fn(&T) -> crate::workspace_model::NavigationItem,
) {
    if order.is_empty() || rows.len() < 2 {
        return;
    }
    let ranks: std::collections::HashMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(rank, item)| (item, rank))
        .collect();
    rows.sort_by_cached_key(|row| ranks.get(&item(row)).copied().unwrap_or(usize::MAX));
}

fn navigation_item(
    project: &SidebarProject,
    row: SelectionRow,
) -> Option<crate::workspace_model::NavigationItem> {
    use crate::workspace_model::NavigationItem;
    match row {
        SelectionRow::Channel(id) => Some(NavigationItem::Channel(id)),
        SelectionRow::Worktree(_, id) => project
            .worktrees
            .iter()
            .find(|worktree| worktree.id == id)
            .map(|worktree| NavigationItem::Worktree(worktree.path.clone())),
        _ => None,
    }
}

/// The selectable rows actually drawn, including disclosure and filter state.
pub fn visible_selection_rows(
    model: &SidebarModel,
    state: &SidebarState,
    compact: bool,
) -> Vec<SelectionRow> {
    let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
    let mut rows = Vec::new();
    for project in filtered_projects(model, state, compact) {
        rows.push(SelectionRow::Project(project.project.id));
        if searching || !state.collapsed_projects.contains(&project.project.id) {
            let first = rows.len();
            rows.extend(
                project
                    .channels
                    .iter()
                    .filter(|channel| channel.deletion != Some(RowDeletion::Deleting))
                    .filter(|channel| {
                        !compact
                            || searching
                            || channel.selected
                            || channel.unread
                            || channel.activity.is_some()
                            || channel.deletion.is_some()
                    })
                    .map(|channel| SelectionRow::Channel(channel.id)),
            );
            rows.extend(
                project
                    .worktrees
                    .iter()
                    .filter(|worktree| worktree.deletion != Some(RowDeletion::Deleting))
                    .filter(|worktree| {
                        !compact
                            || searching
                            || state.selection
                                == Some(SidebarSelection::Worktree {
                                    project_id: project.project.id,
                                    worktree_id: worktree.id,
                                })
                            || state.has_agent_update(project.project.id, worktree.id)
                            || state.worktree_has_panes(project.project.id, worktree.id)
                            || state
                                .agent_activity(project.project.id, worktree.id)
                                .is_some()
                            || worktree.deletion.is_some()
                    })
                    .map(|worktree| SelectionRow::Worktree(project.project.id, worktree.id)),
            );
            if !compact && !searching {
                apply_navigation_order(
                    &mut rows[first..],
                    &project.project.navigation_order,
                    |row| navigation_item(project.project, *row).expect("project member"),
                );
            }
        }
    }
    rows
}

/// A compact query applies only while its search popover is available.
/// Closing the controls clears the query anyway; this keeps a query restored
/// from an older saved view from filtering the rail invisibly.
fn filtered_projects<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Vec<FilteredProject<'a>> {
    let query = if compact && !state.controls_expanded {
        fold("")
    } else {
        fold(state.query.trim())
    };
    let mut filtered: Vec<_> = model
        .projects
        .iter()
        .filter_map(|project| {
            let project_matches = query.is_empty() || project.matches(query.as_ref());
            // An unreachable project draws none of its children, so it must
            // not be matched or counted through them either: an attention or
            // pull-request filter asks which worktrees need looking at, and
            // rows from a read that has since failed cannot answer that. The
            // project itself survives every filter, because a project that
            // cannot be read is exactly something needing attention, and
            // hiding it is the complaint this row exists to answer.
            if project.unreachable.is_some() {
                return project_matches.then_some(FilteredProject {
                    project,
                    worktrees: Vec::new(),
                    channels: Vec::new(),
                    editors: &model.external_editors,
                    state,
                });
            }
            let mut channels =
                channels_in_display_order(project.channels.iter().filter(|channel| {
                    (project_matches
                        || search_score(&channel.folded_title, query.as_ref()).is_some())
                        && match state.filter {
                            SidebarFilter::All => true,
                            SidebarFilter::Attention => {
                                state
                                    .retained_attention
                                    .contains(&SelectionRow::Channel(channel.id))
                                    || channel.unread
                                    || attention_state(
                                        channel.activity,
                                        channel.unread,
                                        channel.outcome.as_ref(),
                                    )
                                    .is_some_and(AttentionState::needs_attention)
                                    || channel.activity
                                        == Some(crate::agent::ActivityState::Waiting)
                            }
                            SidebarFilter::PullRequests => false,
                        }
                }));
            let mut worktrees = worktrees_in_display_order(
                &project.repository,
                state.worktree_sort,
                project
                    .worktrees
                    .iter()
                    .filter(|worktree| match state.filter {
                        SidebarFilter::All => true,
                        SidebarFilter::PullRequests => worktree.pull_request().is_some(),
                        SidebarFilter::Attention => {
                            state
                                .retained_attention
                                .contains(&SelectionRow::Worktree(project.id, worktree.id))
                                || state.has_agent_update(project.id, worktree.id)
                                || state
                                    .worktree_attention(project.id, worktree.id)
                                    .is_some_and(AttentionState::needs_attention)
                        }
                    })
                    .filter(|worktree| project_matches || worktree.matches(query.as_ref())),
            );
            if !query.is_empty() {
                channels.sort_by_key(|channel| {
                    search_score(&channel.folded_title, &query).unwrap_or(usize::MAX)
                });
                worktrees
                    .sort_by_key(|worktree| worktree.search_rank(&query).unwrap_or(usize::MAX));
            }
            let visible = match state.filter {
                SidebarFilter::All => {
                    project_matches || !worktrees.is_empty() || !channels.is_empty()
                }
                SidebarFilter::PullRequests => {
                    (project_matches && project.github_ready) || !worktrees.is_empty()
                }
                SidebarFilter::Attention => !worktrees.is_empty() || !channels.is_empty(),
            };
            visible.then_some(FilteredProject {
                project,
                worktrees,
                channels,
                editors: &model.external_editors,
                state,
            })
        })
        .collect();
    if !query.is_empty() {
        filtered.sort_by_key(|project| {
            project
                .project
                .search_rank(&query)
                .into_iter()
                .chain(
                    project
                        .channels
                        .iter()
                        .filter_map(|channel| search_score(&channel.folded_title, &query)),
                )
                .chain(
                    project
                        .worktrees
                        .iter()
                        .filter_map(|worktree| worktree.search_rank(&query)),
                )
                .min()
                .unwrap_or(usize::MAX)
        });
    }
    filtered
}

/// The one place a project's worktree rows are ordered for display.
///
/// The primary worktree is pinned first whatever order is asked for: it is the
/// repository itself rather than one of its branches and it is never the
/// newest, so letting the order move it would hide the row every other row is
/// read relative to.
///
/// A row with no creation time sorts after every row that has one, under both
/// time orders. Creation time is the only evidence a time order can use, and
/// the creation path stamps the instant it created a worktree, so a row
/// without one is a worktree Git listed on a filesystem that keeps no birth
/// time - an import this cannot place in time at all. Sorting those last
/// leaves them out of an answer they cannot contribute to; sorting them first
/// would lift every row of such a filesystem above the worktree the user just
/// made, which is the complaint this ordering exists to answer. Among
/// themselves they fall back to path, which is the same across frames,
/// reloads and restarts.
pub(crate) fn worktrees_in_display_order<'a>(
    repository: &Path,
    sort: crate::settings::WorktreeSort,
    rows: impl IntoIterator<Item = &'a SidebarWorktree>,
) -> Vec<&'a SidebarWorktree> {
    use crate::settings::WorktreeSort;
    // The primary-worktree test compares paths, and the comparator ran it for
    // both sides of every comparison: a fact that is one per row cost
    // O(n log n) path comparisons per project per frame, on a list rebuilt by
    // every terminal wake. Resolve it once per row instead.
    let mut keyed = rows
        .into_iter()
        .map(|worktree| (is_primary_worktree(repository, &worktree.path), worktree))
        .collect::<Vec<_>>();
    keyed.sort_by(|(left_primary, left), (right_primary, right)| {
        right_primary
            .cmp(left_primary)
            .then_with(|| match sort {
                WorktreeSort::Alphabetical => left.chrome.label.cmp(&right.chrome.label),
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
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    keyed.into_iter().map(|(_, worktree)| worktree).collect()
}

/// The one place a project's channel rows are ordered for display.
///
/// Newest first, by the creation rank the shared registry assigned. A channel
/// is created to be used now, so it belongs where the pointer already is; and
/// the rank is the registry's, so two windows showing one project show it in
/// one order whichever of them created which channel.
///
/// A channel created here holds a provisional rank one above the highest this
/// window knows until the registry answers, so a channel created a moment ago
/// sorts where its real rank will put it. `sort_by_key` is stable, so two rows
/// that do share a rank keep the order the window built them in rather than
/// swapping between frames.
pub(crate) fn channels_in_display_order<'a>(
    rows: impl IntoIterator<Item = &'a SidebarChannel>,
) -> Vec<&'a SidebarChannel> {
    let mut rows = rows.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|channel| std::cmp::Reverse(channel.ordinal));
    rows
}

/// Every top-level destination the sidebar lists -- each project's worktrees
/// and then its channels -- in the order the reader sees them.
///
/// This supplies candidates for workspace history. Unvisited destinations use
/// the same fallback ordering as the rows rather than model storage order.
/// The active filter is deliberately not applied: a transient search must not
/// silently shrink what a keyboard shortcut can reach. A row being deleted is
/// passed over: it is on its way out and cannot be opened meanwhile.
pub fn workspace_cycle(model: &SidebarModel, state: &SidebarState) -> Vec<SidebarAction> {
    let mut destinations = Vec::new();
    for project in &model.projects {
        if project.kind == ProjectKind::Folder {
            destinations.push(SidebarAction::OpenRepository {
                project_id: project.id,
                repository: project.repository.clone(),
            });
        }
        let first = destinations.len();
        destinations.extend(
            channels_in_display_order(&project.channels)
                .into_iter()
                .filter(|channel| channel.deletion != Some(RowDeletion::Deleting))
                .map(|channel| SidebarAction::OpenChannel {
                    channel_id: channel.id,
                }),
        );
        destinations.extend(
            worktrees_in_display_order(
                &project.repository,
                state.worktree_sort,
                &project.worktrees,
            )
            .into_iter()
            .filter(|worktree| worktree.deletion != Some(RowDeletion::Deleting))
            .map(|worktree| SidebarAction::OpenWorktree {
                project_id: project.id,
                worktree_id: worktree.id,
                path: worktree.path.clone(),
            }),
        );
        apply_navigation_order(
            &mut destinations[first..],
            &project.navigation_order,
            |action| match action {
                SidebarAction::OpenChannel { channel_id } => {
                    crate::workspace_model::NavigationItem::Channel(*channel_id)
                }
                SidebarAction::OpenWorktree { path, .. } => {
                    crate::workspace_model::NavigationItem::Worktree(path.clone())
                }
                _ => unreachable!(),
            },
        );
    }
    destinations
}

/// Case-folds a filter key. Entities store the result so the free-text filter
/// never folds a candidate while rendering.
fn fold(value: &str) -> Box<str> {
    value.case_fold().collect::<String>().into_boxed_str()
}

/// Match names first; ordered abbreviations allow narrow searches without
/// allocating another folded candidate on each frame.
fn search_score(candidate: &str, query: &str) -> Option<usize> {
    if let Some(position) = candidate.find(query) {
        return Some(if position == 0 { 0 } else { 100 + position });
    }
    let mut remaining = candidate.chars();
    let mut skipped = candidate.len();
    for expected in query.chars() {
        loop {
            let actual = remaining.next()?;
            skipped += 1;
            if actual == expected {
                break;
            }
        }
    }
    Some(1_000 + skipped)
}

/// Ranges in the displayed spelling, including full Unicode case-fold
/// expansions. For example, `ss` highlights the single character `ß`.
fn search_highlight_ranges(label: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    let query: Vec<_> = query.trim().case_fold().collect();
    if query.is_empty() {
        return Vec::new();
    }
    let folded: Vec<_> = label
        .char_indices()
        .flat_map(|(start, ch)| {
            let range = start..start + ch.len_utf8();
            ch.to_string()
                .case_fold()
                .map(move |folded| (folded, range.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let contiguous = folded
        .windows(query.len())
        .position(|window| window.iter().map(|(ch, _)| ch).eq(query.iter()));
    let mut ranges = Vec::new();
    if let Some(start) = contiguous {
        ranges.extend(
            folded[start..start + query.len()]
                .iter()
                .map(|(_, range)| range.clone()),
        );
    } else {
        let mut offset = 0;
        for (query_index, expected) in query.iter().enumerate() {
            // Prefer an intact suffix to scattering it through an earlier
            // word: `fauth` reads as F + auth in "Feature authentication".
            let suffix = &query[query_index..];
            if let Some(position) = folded[offset..]
                .windows(suffix.len())
                .position(|window| window.iter().map(|(ch, _)| ch).eq(suffix.iter()))
            {
                let start = offset + position;
                ranges.extend(
                    folded[start..start + suffix.len()]
                        .iter()
                        .map(|(_, range)| range.clone()),
                );
                break;
            }
            let Some(position) = folded[offset..].iter().position(|(ch, _)| ch == expected) else {
                return Vec::new();
            };
            offset += position;
            ranges.push(folded[offset].1.clone());
            offset += 1;
        }
    }
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && previous.end >= range.start
        {
            previous.end = previous.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn search_label(
    label: &str,
    query: &str,
    size: f32,
    class: theme::Text,
    heading: bool,
) -> Element<'static, SidebarAction> {
    use cosmic::iced::widget::text::Span;
    let font = cosmic::font::Font {
        weight: if heading {
            cosmic::iced::font::Weight::Medium
        } else {
            cosmic::iced::font::Weight::Normal
        },
        ..cosmic::font::default()
    };
    let emphasis = cosmic::font::Font {
        weight: cosmic::iced::font::Weight::Bold,
        ..font
    };
    let highlights = search_highlight_ranges(label, query);
    if highlights.is_empty() {
        return widget::text::body(label.to_owned())
            .size(size)
            .font(font)
            .class(class)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into();
    }
    let mut spans: Vec<Span<'_, ()>> = Vec::new();
    let mut cursor = 0;
    for range in highlights {
        if cursor < range.start {
            spans.push(Span::new(label[cursor..range.start].to_owned()));
        }
        spans.push(
            Span::new(label[range.clone()].to_owned())
                .font(emphasis)
                .color(design::accent_color(&cosmic::theme::active())),
        );
        cursor = range.end;
    }
    if cursor < label.len() {
        spans.push(Span::new(label[cursor..].to_owned()));
    }
    let label = cosmic::iced::widget::rich_text(spans)
        .size(size)
        .font(font)
        .class(class)
        .wrapping(cosmic::iced::widget::text::Wrapping::None)
        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
            EllipsizeHeightLimit::Lines(1),
        ))
        .width(Length::Fill);
    // The pinned rich paragraph renderer does not apply ellipsizing to spans.
    // Its ink must still stay inside the label slot, clear of row indicators.
    widget::container(label)
        .width(Length::Fill)
        .clip(true)
        .class(theme::Container::custom(|_| {
            cosmic::iced::widget::container::Style::default()
        }))
        .into()
}

pub(crate) fn is_check_failure(value: &str) -> bool {
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

pub(crate) fn is_check_pending(value: &str) -> bool {
    matches!(
        value,
        "pending" | "queued" | "in_progress" | "waiting" | "expected"
    )
}

pub(crate) fn is_check_success(value: &str) -> bool {
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
        HEADER_ACTION_WIDTH, PROJECT_HEADER_HEIGHT, ProjectId, SidebarAction, SidebarFilter,
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
        Selectable {
            row: super::SelectionRow,
            /// This row is in the explicit multi-selection.
            selected: bool,
            /// Some row is, so Escape has a selection to clear.
            any_selected: bool,
            enabled: bool,
        },
        FilteredScroll(SidebarFilter),
        Search {
            enabled: bool,
        },
        Reorder {
            ids: Vec<super::SelectionRow>,
            disabled: Vec<super::SelectionRow>,
            project: Option<ProjectId>,
            enabled: bool,
            viewport: StickyViewport,
        },
        Group {
            viewport: StickyViewport,
        },
        /// `reserve` is the width the overlay controls cover at the row's
        /// trailing edge. It is subtracted from the base row only while those
        /// controls are on screen, which is the whole point of drawing them
        /// over the row instead of beside it.
        Actions {
            reserve: f32,
        },
    }

    #[derive(Default)]
    struct State {
        modifiers: keyboard::Modifiers,
        selection_bounds: Option<Rectangle>,
        origin: Option<(super::SelectionRow, Point)>,
        dragging: bool,
        drop: Option<(Option<super::SelectionRow>, f32)>,
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
    struct FocusWithin(bool, Option<Id>, Option<Rectangle>);

    impl Operation for FocusWithin {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }

        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if self.1.as_ref().is_none_or(|target| Some(target) == id) {
                self.0 |= state.is_focused();
                self.2 = Some(bounds);
            }
        }
    }

    impl<'a> RowInteraction<'a> {
        pub(super) fn selectable(
            content: Element<'a, SidebarAction>,
            row: super::SelectionRow,
            state: &super::SidebarState,
            enabled: bool,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::Selectable {
                    row,
                    selected: state.rows.contains(row),
                    any_selected: !state.rows.is_empty(),
                    enabled,
                },
            })
        }
        pub(super) fn filtered_scroll(
            content: Element<'a, SidebarAction>,
            filter: SidebarFilter,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::FilteredScroll(filter),
            })
        }

        pub(super) fn search(
            content: Element<'a, SidebarAction>,
            enabled: bool,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::Search { enabled },
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
                kind: Kind::Reorder {
                    ids: ids.into_iter().map(super::SelectionRow::Project).collect(),
                    disabled: Vec::new(),
                    project: None,
                    enabled,
                    viewport,
                },
            })
        }

        pub(super) fn members(
            content: Element<'a, SidebarAction>,
            project: ProjectId,
            ids: Vec<super::SelectionRow>,
            disabled: Vec<super::SelectionRow>,
            enabled: bool,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::Reorder {
                    ids,
                    disabled,
                    project: Some(project),
                    enabled,
                    viewport: StickyViewport::default(),
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

        /// `reserve` is how much of the row's trailing edge `actions` covers
        /// once it is visible; pass `0.0` when the strip paints its own
        /// background and the base row may run underneath it.
        pub(super) fn actions(
            base: Element<'a, SidebarAction>,
            actions: Element<'a, SidebarAction>,
            reserve: f32,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![base, actions],
                kind: Kind::Actions { reserve },
            })
        }

        fn focus(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
            let mut focus = FocusWithin::default();
            match self.kind {
                Kind::Selectable { row, .. } => focus.1 = Some(super::selection_row_focus_id(row)),
                Kind::Actions { .. } => {}
                Kind::Search { .. } => focus.1 = Some(super::search_focus_id()),
                _ => return,
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
            let state = tree.state.downcast_mut::<State>();
            state.focused = focus.0;
            state.selection_bounds = focus.2;
        }

        fn header_at(&self, layout: Layout<'_>, point: Point) -> Option<super::SelectionRow> {
            let Kind::Reorder {
                ids,
                enabled: true,
                viewport,
                project,
                disabled,
            } = &self.kind
            else {
                return None;
            };
            ids.iter()
                .zip(layout.children().next()?.children())
                .find_map(|(id, row)| {
                    let bounds = if project.is_some() {
                        row.bounds()
                    } else {
                        sticky_header_bounds(row.bounds(), PROJECT_HEADER_HEIGHT, viewport.0.get())
                    };
                    // Leave expansion and new-worktree buttons as ordinary controls.
                    let header = Rectangle {
                        x: bounds.x,
                        y: bounds.y,
                        width: (bounds.width - HEADER_ACTION_WIDTH).max(0.0),
                        height: bounds.height,
                    };
                    (header.contains(point) && !disabled.contains(id)).then_some(*id)
                })
        }

        fn drop_at(&self, layout: Layout<'_>, point: Point) -> (Option<super::SelectionRow>, f32) {
            let Kind::Reorder {
                ids,
                viewport,
                project,
                ..
            } = &self.kind
            else {
                unreachable!();
            };
            for (id, row) in ids.iter().zip(layout.children().next().unwrap().children()) {
                let header = if project.is_some() {
                    row.bounds()
                } else {
                    sticky_header_bounds(row.bounds(), PROJECT_HEADER_HEIGHT, viewport.0.get())
                };
                if point.y < header.y + header.height / 2.0 {
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
            if let Kind::Reorder {
                ids,
                disabled,
                enabled,
                ..
            } = &self.kind
            {
                let state = tree.state.downcast_mut::<State>();
                if !enabled
                    || state
                        .origin
                        .is_some_and(|(id, _)| !ids.contains(&id) || disabled.contains(&id))
                {
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
            // Overlay controls are on screen only while the row is hovered or
            // focused, so only then does the base row lose the width they
            // cover. Every other frame the title is measured against the whole
            // row, which is what keeps a reservation out of the resting state.
            let reserve = match self.kind {
                Kind::Actions { reserve } if reserve > 0.0 => {
                    let state = tree.state.downcast_ref::<State>();
                    if state.hovered || state.focused {
                        reserve
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };
            let base = self.children[0].as_widget_mut().layout(
                &mut tree.children[0],
                renderer,
                &limits.shrink(Size::new(reserve, 0.0)),
            );
            // The row itself never changes width: only what the title may use
            // does, so the strip lands on the same trailing edge either way.
            let size = Size::new(base.size().width + reserve, base.size().height);
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
            if let Kind::Reorder { viewport, .. } = &self.kind {
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
            self.focus(tree, layout, renderer);
            if let Kind::Reorder {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y);
            }
            let reserves = matches!(self.kind, Kind::Actions { reserve } if reserve > 0.0);
            let state = tree.state.downcast_mut::<State>();
            match event {
                Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                    state.modifiers = *modifiers
                }
                Event::Window(window::Event::Unfocused) => {
                    state.modifiers = keyboard::Modifiers::empty()
                }
                _ => {}
            }
            let hovered = cursor.is_over(layout.bounds()) && cursor.is_over(*viewport);
            // Handle the search's own Escape before the input captures it
            // merely to blur. Other focused controls and modal dialogs retain
            // their own dismissal because this requires enabled search focus.
            if matches!(self.kind, Kind::Search { enabled: true })
                && state.focused
                && let Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    modifiers,
                    ..
                }) = event
                && modifiers.is_empty()
            {
                let action = match key {
                    keyboard::key::Named::ArrowDown => Some(SidebarAction::FocusFirstSearchResult),
                    keyboard::key::Named::Escape => Some(SidebarAction::ToggleControls),
                    _ => None,
                };
                if let Some(action) = action {
                    shell.publish(action);
                    shell.capture_event();
                    return;
                }
            }
            if let Kind::Selectable {
                row,
                selected,
                any_selected,
                enabled: true,
            } = self.kind
            {
                let additive = state.modifiers.control() || state.modifiers.logo();
                match event {
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                        if hovered
                            && state
                                .selection_bounds
                                .is_some_and(|bounds| cursor.is_over(bounds)) =>
                    {
                        // A plain click is navigation: the row's own button
                        // opens it, and the selection only learns the anchor.
                        let intent = if state.modifiers.shift() {
                            super::SelectionIntent::Range { extend: additive }
                        } else if additive {
                            super::SelectionIntent::Toggle
                        } else {
                            super::SelectionIntent::Navigate
                        };
                        shell.publish(SidebarAction::SelectRows { row, intent });
                        if additive || state.modifiers.shift() {
                            shell.capture_event();
                            return;
                        }
                    }
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(key),
                        modifiers,
                        ..
                    }) if state.focused && modifiers.is_empty() => match key {
                        keyboard::key::Named::ArrowUp
                        | keyboard::key::Named::ArrowDown
                        | keyboard::key::Named::ArrowLeft
                        | keyboard::key::Named::ArrowRight
                        | keyboard::key::Named::Home
                        | keyboard::key::Named::End => {
                            shell.publish(SidebarAction::NavigateRow { row, key: *key });
                            shell.capture_event();
                            return;
                        }
                        keyboard::key::Named::Delete | keyboard::key::Named::Backspace => {
                            // The focused row is the target unless it belongs
                            // to the explicit selection, as in VS Code: focus
                            // moved off a selection, or a Ctrl-click that
                            // deselected this row, must not aim at other rows.
                            shell.publish(if selected {
                                SidebarAction::DeleteSelectedRows
                            } else {
                                SidebarAction::DeleteRow(row)
                            });
                            shell.capture_event();
                            return;
                        }
                        // Escape belongs to whatever else wants it unless there
                        // is a selection for it to end.
                        keyboard::key::Named::Escape if any_selected => {
                            shell.publish(SidebarAction::ClearRowSelection);
                            shell.capture_event();
                            return;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if hovered != state.hovered {
                state.hovered = hovered;
                shell.request_redraw();
                // The base row is measured against a narrower space while the
                // controls show, so revealing them is a relayout, not a repaint.
                if reserves {
                    shell.invalidate_layout();
                }
            }
            if matches!(self.kind, Kind::Reorder { enabled: true, .. }) {
                match event {
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                        if hovered && state.modifiers.is_empty() =>
                    {
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
                                    shell.publish(match (&self.kind, project) {
                                        (
                                            Kind::Reorder {
                                                project: Some(owner),
                                                ..
                                            },
                                            source,
                                        ) => SidebarAction::ReorderMember {
                                            project: *owner,
                                            source,
                                            before,
                                        },
                                        (_, super::SelectionRow::Project(project)) => {
                                            SidebarAction::ReorderProject {
                                                project,
                                                before: before.and_then(|row| {
                                                    if let super::SelectionRow::Project(id) = row {
                                                        Some(id)
                                                    } else {
                                                        None
                                                    }
                                                }),
                                            }
                                        }
                                        _ => unreachable!(),
                                    });
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
                    if let Some(position) = cursor.position() {
                        state.drop = hovered.then(|| self.drop_at(layout, position));
                        let margin = 32.0;
                        let direction = if position.y < viewport.y + margin {
                            -1.0
                        } else if position.y > viewport.y + viewport.height - margin {
                            1.0
                        } else {
                            0.0
                        };
                        if direction != 0.0 && hovered {
                            if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
                                shell.publish(SidebarAction::ScrollProjects(direction * 8.0));
                            }
                            shell.request_redraw();
                        }
                    }
                    if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })) {
                        return;
                    }
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
                    && matches!(self.kind, Kind::Actions { .. })
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
            let focused = tree.state.downcast_ref::<State>().focused;
            self.focus(tree, layout, renderer);
            if reserves && tree.state.downcast_ref::<State>().focused != focused {
                shell.invalidate_layout();
            }
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
            if let Kind::Reorder {
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
                return mouse::Interaction::Grab;
            }
            self.children
                .iter()
                .zip(&tree.children)
                .zip(child_layouts(&self.kind, state, layout))
                .enumerate()
                .rev()
                .map(|(index, ((child, tree), child_layout))| {
                    let hidden_actions = index == 1
                        && matches!(self.kind, Kind::Actions { .. })
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
            if let Kind::Reorder {
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
            // Multi-selection shares the neutral row fill without a leading accent.
            if matches!(self.kind, Kind::Selectable { selected: true, .. }) {
                // The row's own button, not the indented slot around it, so
                // the mark lines up with the active row's highlight.
                let bounds = state.selection_bounds.unwrap_or(layout.bounds());
                if let Some(clip) = bounds.intersection(viewport) {
                    renderer.with_layer(clip, |renderer| {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                border: cosmic::iced::Border {
                                    radius: 0.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                            cosmic::iced::Color::from(theme.current_container().on)
                                .scale_alpha(0.08),
                        );
                    });
                }
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
            if let Kind::Reorder {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y - translation.y);
            }
            let state = tree.state.downcast_ref::<State>();
            let hide_actions =
                matches!(self.kind, Kind::Actions { .. }) && !(state.hovered || state.focused);
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
            if let Kind::Reorder { viewport, .. } = &self.kind {
                viewport
                    .0
                    .set(layout.bounds().y + layout.virtual_offset().y);
            }
            // The outer list translates every descendant together. Strip only
            // its ancestor offset here; nested scrollables add their own offsets.
            let child_layout = if matches!(self.kind, Kind::Reorder { project: None, .. }) {
                layout.with_virtual_offset(cosmic::iced::Vector::ZERO)
            } else {
                layout
            };
            let mut accessible = iced_accessibility::A11yTree::join(
                self.children
                    .iter()
                    .zip(&tree.children)
                    .zip(child_layouts(
                        &self.kind,
                        tree.state.downcast_ref::<State>(),
                        child_layout,
                    ))
                    .map(|((child, tree), layout)| {
                        child.as_widget().a11y_nodes(layout, tree, cursor)
                    }),
            );
            // Membership adds to the button's own selected state, which marks
            // the active row, rather than replacing it.
            if let Kind::Selectable { selected: true, .. } = self.kind {
                let mut applied = false;
                let mut select = |node: &mut iced_accessibility::A11yNode| {
                    if !applied && node.node().role() == iced_accessibility::accesskit::Role::Button
                    {
                        node.node_mut().set_selected(true);
                        applied = true;
                    }
                };
                accessible.root_mut().iter_mut().for_each(&mut select);
                accessible.children_mut().iter_mut().for_each(select);
            }
            // libcosmic buttons report content-coordinate bounds; its scrollable
            // forwards a virtual offset without translating their AccessKit nodes.
            // Translate once at the list boundary, not per nested decorator.
            if matches!(self.kind, Kind::Reorder { project: None, .. }) {
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
            if let Kind::Reorder { viewport, .. } = &self.kind {
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
pub(crate) mod tests {
    #[test]
    fn required_review_policy_does_not_claim_the_viewer_was_requested() {
        let mut pull_request = crate::pull_request_review::tests::pull_request(51);
        for decision in ["REVIEW_REQUIRED", "CHANGES_REQUESTED"] {
            pull_request.review_decision = Some(crate::github::RemoteValue(decision.into()));
            let row = super::SidebarPullRequest::from_github(
                crate::model::ProjectId::new(),
                &pull_request,
                &[],
            );
            assert!(!row.needs_review);
            assert!(!row.review_requested_again);
        }
    }

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

    #[test]
    fn navigation_cache_invalidates_attention_query_and_disclosure_but_reuses_identical_views() {
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        let cache = NavigationCache::default();
        let initial = cache.generation(&model, &state);
        assert_eq!(initial, cache.generation(&model.clone(), &state.clone()));
        state.apply(&SidebarAction::ToggleProject(project_id(1)));
        let collapsed = cache.generation(&model, &state);
        assert_ne!(initial, collapsed);
        state.query = "fix".into();
        let searched = cache.generation(&model, &state);
        assert_ne!(collapsed, searched);
        state.observe_agent_update(
            CardId::new(),
            project_id(1),
            WorktreeId::from_uuid(Uuid::from_u128(2)),
        );
        let attention = cache.generation(&model, &state);
        assert_ne!(searched, attention);
        assert_eq!(attention, cache.generation(&model, &state));
    }

    #[test]
    fn navigation_cache_ignores_large_source_control_and_file_panel_interactions() {
        let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        let cache = NavigationCache::default();
        let initial = cache.generation(&model, &state);
        let paths: Vec<_> = (0..10_000)
            .map(|index| RepoPath::parse(format!("src/file_{index}.rs")).unwrap())
            .collect();
        let mut source = SourceControlModel::loading(main_key());
        source.files = file_rows(&paths);
        model.source_control = Some(source);
        assert_eq!(initial, cache.generation(&model, &state));

        state.files_query = "file".into();
        state
            .expanded_directories
            .insert(RepoPath::parse("src").unwrap());
        state.commit_draft.message = "message".into();
        state
            .commit_draft
            .composer
            .0
            .perform(cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Insert('x'),
            ));
        state.changes_tab = ChangesTab::History;
        assert_eq!(initial, cache.generation(&model, &state));
        model.source_control.as_mut().unwrap().files.clear();
        assert_eq!(initial, cache.generation(&model, &state));
    }

    #[test]
    fn navigation_cache_tracks_active_worktree_open_panes_editors_and_project_changes() {
        let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        let cache = NavigationCache::default();
        let mut previous = cache.generation(&model, &state);
        state.active_worktree = Some(main_key());
        let next = cache.generation(&model, &state);
        assert_ne!(previous, next);
        previous = next;
        state
            .worktrees_with_panes
            .insert((main_key().project_id, main_key().worktree_id));
        let next = cache.generation(&model, &state);
        assert_ne!(previous, next);
        previous = next;
        model.external_editors.push(SidebarEditor {
            id: "test".into(),
            name: "Test editor".into(),
            icon: "editor".into(),
        });
        let next = cache.generation(&model, &state);
        assert_ne!(previous, next);
        previous = next;
        model.projects[0].reloading = true;
        assert_ne!(previous, cache.generation(&model, &state));
    }

    #[test]
    fn search_ignores_common_ancestor_paths_and_accepts_name_abbreviations() {
        let entry = project();
        assert!(!entry.matches("repo"));
        assert!(!entry.worktrees[0].matches("repo"));
        assert!(entry.worktrees[0].matches("/repo/"));
        assert!(search_score("feature/authentication", "fauth").is_some());
        assert!(search_score("feature/authentication", "fhz").is_none());
    }

    #[test]
    fn compact_search_mounts_matching_rows_without_open_panes_or_attention() {
        let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
        let mut state = SidebarState::default();
        let id = selection_row_focus_id(SelectionRow::Worktree(
            project_id(1),
            WorktreeId::from_uuid(Uuid::from_u128(3)),
        ));
        let quiet = project_list(&model, &state, true, false);
        assert!(Tree::new(&quiet).find(&id).is_none());
        state.controls_expanded = true;
        state.query = "fix".into();
        let result = project_list(&model, &state, true, false);
        assert!(
            Tree::new(&result).find(&id).is_some(),
            "a matching inactive worktree must be mounted so Search Down can focus it"
        );
    }

    #[test]
    fn ranked_search_prefers_prefixes_then_substrings_then_abbreviations_and_paths() {
        let mut entry = project();
        entry.worktrees = vec![
            worktree(20, "x-fix-y", false),
            worktree(21, "fancy-index", false),
            worktree(22, "fix", false),
        ];
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        state.query = "fix".into();
        let filtered = filtered_projects(&model, &state, false);
        assert_eq!(
            filtered[0]
                .worktrees
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [22, 20, 21].map(|id| WorktreeId::from_uuid(Uuid::from_u128(id)))
        );
        assert!(search_score("fix", "fix") < search_score("prefix-fix", "fix"));
        assert!(search_score("prefix-fix", "fix") < search_score("fancy-index", "fix"));
        assert!(
            model.projects[0].worktrees[0]
                .search_rank("/repo/")
                .unwrap()
                >= 10_000
        );
    }

    #[test]
    fn fuzzy_highlights_preserve_utf8_boundaries_and_casefold_expansions() {
        assert_eq!(
            search_highlight_ranges("Feature authentication", "fauth"),
            vec![0..1, 8..12]
        );
        assert_eq!(search_highlight_ranges("Straße", "ss"), vec![4..6]);
        assert_eq!(search_highlight_ranges("Éclair", "écl"), vec![0..4]);
        assert!(search_highlight_ranges("Feature", "missing").is_empty());
        assert!(search_highlight_ranges("Feature", "").is_empty());
    }

    #[test]
    fn search_navigation_and_escape_require_enabled_input_focus() {
        use cosmic::iced::{
            Event, Rectangle,
            advanced::{Shell, clipboard, widget::operation},
            keyboard, mouse,
        };
        for key in [
            keyboard::key::Named::ArrowDown,
            keyboard::key::Named::Escape,
        ] {
            for (focused, modal) in [(false, false), (true, false), (true, true)] {
                let state = SidebarState::default();
                let renderer = headless_renderer();
                let mut element = search(&state, modal);
                let mut tree = Tree::new(&element);
                let viewport = Rectangle::with_size(Size::new(240.0, 48.0));
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, viewport.size()),
                );
                if focused {
                    element.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut operation::focusable::focus::<()>(search_focus_id()),
                    );
                }
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(key),
                        modified_key: keyboard::Key::Named(key),
                        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::ArrowDown),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert_eq!(
                    messages,
                    if focused && !modal {
                        vec![if key == keyboard::key::Named::Escape {
                            SidebarAction::ToggleControls
                        } else {
                            SidebarAction::FocusFirstSearchResult
                        }]
                    } else {
                        vec![]
                    }
                );
            }
        }
    }

    #[test]
    fn waiting_indicator_survives_read_acknowledgement_and_ci_does_not_recolor_pr_state() {
        assert!(
            agent_indicator::<SidebarAction>(Some(crate::agent::ActivityState::Waiting), false)
                .is_some()
        );
        assert!(agent_indicator::<SidebarAction>(None, true).is_some());
        assert!(agent_indicator::<SidebarAction>(None, false).is_none());
        let mut pr = project().pull_requests[0].clone();
        pr.needs_review = false;
        pr.draft = false;
        pr.state = Some(WorktreePullRequestState::Open);
        for checks in [
            CheckStatus::Failing,
            CheckStatus::Pending,
            CheckStatus::Passing,
        ] {
            pr.checks = checks;
            assert_eq!(pull_request_tone(&pr), PullRequestTone::Neutral);
        }
        pr.state = Some(WorktreePullRequestState::Closed);
        assert_eq!(pull_request_tone(&pr), PullRequestTone::Danger);
    }

    fn worktree(id: u128, branch: &str, dirty: bool) -> SidebarWorktree {
        let mut row = SidebarWorktree::from_repository(
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
                untracked_omitted: false,
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
        );
        row.head_repository = Some(
            crate::github::GitHubRepoKey::new("github.com", None, "example", "cagents").unwrap(),
        );
        row
    }

    fn project() -> SidebarProject {
        let mut project = SidebarProject::new(
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
        );
        project.pull_requests[0].head_repository = project.worktrees[0].head_repository.clone();
        let pr = project.pull_requests[0].clone();
        project.worktrees[1].set_pull_request(pr);
        project
    }

    #[test]
    fn selection_order_matches_visible_rows_including_channels_and_collapses() {
        let mut entry = project();
        let channel = ChannelId::new();
        entry.channels.push(SidebarChannel::new(
            channel,
            "Conversation".into(),
            false,
            None,
            false,
            0,
            false,
            1,
        ));
        let id = entry.id;
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        let rows = visible_selection_rows(&model, &state, false);
        assert_eq!(rows[0], SelectionRow::Project(id));
        assert_eq!(rows[1], SelectionRow::Channel(channel));
        assert!(matches!(rows[2], SelectionRow::Worktree(..)));
        state.apply(&SidebarAction::ToggleProject(id));
        assert_eq!(
            visible_selection_rows(&model, &state, false),
            vec![SelectionRow::Project(id)]
        );
        assert_eq!(
            visible_selection_rows(&model, &state, true),
            vec![SelectionRow::Project(id)],
            "compact rail respects disclosure"
        );
        state.apply(&SidebarAction::SearchChanged("not-present".into()));
        assert!(visible_selection_rows(&model, &state, false).is_empty());
    }

    #[test]
    fn channel_hover_delete_targets_its_row_without_opening_it_in_both_densities() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let channel = SidebarChannel::new(
            ChannelId::new(),
            "Investigation".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        );
        for (compact, modal) in [(false, false), (true, false), (false, true), (true, true)] {
            let renderer = headless_renderer();
            let mut element = channel_view(&channel, modal, compact, Some(1), "");
            let mut tree = Tree::new(&element);
            let width = if compact { COMPACT_ROW_WIDTH } else { 240.0 };
            let viewport = Rectangle::with_size(Size::new(width, WORKTREE_ROW_HEIGHT));
            let limits = layout::Limits::new(Size::ZERO, viewport.size());
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let cursor = mouse::Cursor::Available(Point::new(width - 8.0, 16.0));
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: cursor.position().unwrap(),
                }),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert_eq!(
                messages,
                if modal {
                    Vec::new()
                } else {
                    vec![SidebarAction::DeleteChannel {
                        channel_id: channel.id,
                    }]
                },
                "compact={compact}, modal={modal}"
            );
        }
    }

    #[test]
    fn worktree_changes_icons_can_be_hidden_without_hiding_activity() {
        let renderer = headless_renderer();
        let mut worktree = worktree(3, "feature", false);
        worktree.chrome.dirty_badge = Some("3 changed files".into());
        fn badge_count(node: &layout::Node) -> usize {
            usize::from(
                node.children().is_empty()
                    && node.bounds().size()
                        == Size::new(f32::from(ROW_ICON_SIZE), f32::from(ROW_ICON_SIZE)),
            ) + node.children().iter().map(badge_count).sum::<usize>()
        }
        for compact in [false, true] {
            let row = |show| {
                worktree_view(
                    project_id(1),
                    &worktree,
                    false,
                    false,
                    None,
                    false,
                    false,
                    &[],
                    false,
                    None,
                    None,
                    false,
                    false,
                    compact,
                    None,
                    "",
                    show,
                )
            };
            let counts = [true, false].map(|show| {
                let mut element = row(show);
                let mut tree = Tree::new(&element);
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(380.0, WORKTREE_ROW_HEIGHT)),
                );
                badge_count(&node)
            });
            assert_eq!(
                counts[0],
                counts[1] + 1,
                "only the changes badge is removed"
            );
            assert_eq!(
                accessible_names(&renderer, row(true)),
                accessible_names(&renderer, row(false)),
                "dirty status remains available to assistive technology"
            );
            assert!(worktree_indicator(&worktree, true, None, None, compact, false).is_some());
        }
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        let cache = NavigationCache::default();
        let before = cache.generation(&model, &state);
        state.set_show_worktree_changes_icons(false);
        assert_ne!(before, cache.generation(&model, &state));
    }

    #[test]
    fn only_merged_nonprimary_worktree_rows_expose_delete_quick_action() {
        let renderer = headless_renderer();
        let mut worktree = worktree(3, "feature", false);
        let mut pr = project().pull_requests.remove(0);
        for (state, primary, expected) in [
            (WorktreePullRequestState::Open, false, false),
            (WorktreePullRequestState::Closed, false, false),
            (WorktreePullRequestState::Merged, true, false),
            (WorktreePullRequestState::Merged, false, true),
        ] {
            pr.state = Some(state);
            worktree.set_pull_request(pr.clone());
            let mut element = worktree_view(
                project_id(1),
                &worktree,
                primary,
                false,
                None,
                false,
                true,
                &[],
                false,
                None,
                None,
                false,
                false,
                false,
                Some(1),
                "",
                true,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(240.0, WORKTREE_ROW_HEIGHT)),
            );
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                cosmic::iced::mouse::Cursor::Unavailable,
            );
            let has_delete = nodes.root().iter().chain(nodes.children()).any(|node| {
                node.node()
                    .label()
                    .is_some_and(|name| name == "Delete merged worktree Polish sidebar")
            });
            assert_eq!(has_delete, expected, "{state:?}, primary={primary}");
        }
    }

    #[test]
    fn row_selection_is_explicit_and_delete_targets_the_focused_row_unless_selected() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard, widget::operation},
            keyboard, mouse,
        };
        let renderer = headless_renderer();
        let row = SelectionRow::Project(project_id(1));
        let state = SidebarState::default();
        let mut element = RowInteraction::selectable(
            button::custom(design::body("Project"))
                .id(selection_row_focus_id(row))
                .name("Project")
                .width(120)
                .height(32)
                .on_press(SidebarAction::OpenRepositoryPicker)
                .into(),
            row,
            &state,
            true,
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(120.0, 32.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let key = || {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Delete),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Delete),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Delete),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            })
        };
        let dispatch = |element: &mut Element<'_, SidebarAction>, tree: &mut Tree, event: Event| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(30.0, 16.0)),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        assert!(
            dispatch(&mut element, &mut tree, key()).is_empty(),
            "editor focus must not delete rows"
        );
        dispatch(
            &mut element,
            &mut tree,
            Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::CTRL)),
        );
        assert_eq!(
            dispatch(
                &mut element,
                &mut tree,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            ),
            vec![SidebarAction::SelectRows {
                row,
                intent: SelectionIntent::Toggle
            }]
        );
        assert!(
            dispatch(
                &mut element,
                &mut tree,
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            )
            .is_empty()
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(selection_row_focus_id(row)),
        );
        assert_eq!(
            dispatch(&mut element, &mut tree, key()),
            vec![SidebarAction::DeleteRow(row)],
            "a focused row outside the selection is its own target"
        );
        assert!(
            dispatch(
                &mut element,
                &mut tree,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                })
            )
            .is_empty(),
            "Escape with nothing selected is left for the rest of the window"
        );
        dispatch(
            &mut element,
            &mut tree,
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::empty(),
            )),
        );
        assert_eq!(
            dispatch(
                &mut element,
                &mut tree,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            ),
            vec![SidebarAction::SelectRows {
                row,
                intent: SelectionIntent::Navigate
            }],
            "a plain click navigates and selects nothing"
        );

        // The same focused row, now a member of the selection, deletes the
        // whole selection instead.
        let mut selected = SidebarState::default();
        selected.select_rows(row, SelectionIntent::Toggle, &[row]);
        let mut element = RowInteraction::selectable(
            button::custom(design::body("Project"))
                .id(selection_row_focus_id(row))
                .width(120)
                .height(32)
                .on_press(SidebarAction::OpenRepositoryPicker)
                .into(),
            row,
            &selected,
            true,
        );
        let mut tree = Tree::new(&element);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(selection_row_focus_id(row)),
        );
        assert_eq!(
            dispatch(&mut element, &mut tree, key()),
            vec![SidebarAction::DeleteSelectedRows]
        );
    }

    /// Unvisited history candidates follow sidebar order, worktrees before
    /// channels, and are not narrowed by a transient filter.
    #[test]
    fn the_workspace_cycle_follows_the_order_the_sidebar_lists() {
        let mut entry = project();
        let mut root = worktree(10, "main", false);
        root.path = entry.repository.clone();
        let mut older = worktree(11, "z-old", false);
        older.created_at_unix_ms = Some(100);
        let mut newer = worktree(12, "a-new", false);
        newer.created_at_unix_ms = Some(200);
        entry.worktrees = vec![older, root, newer];
        let first = ChannelId::new();
        let second = ChannelId::new();
        entry.channels = vec![
            SidebarChannel::new(first, "First".into(), false, None, false, 1, true, 1),
            SidebarChannel::new(second, "Second".into(), false, None, false, 1, true, 2),
        ];
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        state.filter = SidebarFilter::Attention;

        let cycle = workspace_cycle(&model, &state);
        let id = |id| WorktreeId::from_uuid(Uuid::from_u128(id));
        let worktrees = cycle
            .iter()
            .filter_map(|action| match action {
                SidebarAction::OpenWorktree { worktree_id, .. } => Some(*worktree_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        // The primary worktree pins to the top, then newest first.
        assert_eq!(worktrees, vec![id(10), id(12), id(11)]);
        let channels = cycle
            .iter()
            .filter_map(|action| match action {
                SidebarAction::OpenChannel { channel_id } => Some(*channel_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(channels, vec![second, first]);
        // Channel rows precede worktrees in the sidebar.
        assert!(matches!(
            cycle.first(),
            Some(SidebarAction::OpenChannel { .. })
        ));
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
    fn channel_attention_keeps_its_project_visible_without_unread_worktrees() {
        let mut entry = project();
        let channel_id = ChannelId::from_uuid(Uuid::from_u128(20));
        entry.channels.push(SidebarChannel::new(
            channel_id,
            "Investigate login".into(),
            true,
            Some(crate::agent::ActivityState::Waiting),
            true,
            2,
            true,
            1,
        ));
        let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.apply(&SidebarAction::SearchChanged("login".into()));
        let projects = filtered_projects(&model, &state, false);
        assert_eq!(projects.len(), 1);
        assert!(projects[0].worktrees.is_empty());
        assert_eq!(projects[0].channels[0].id, channel_id);
        model.projects[0].channels[0].unread = false;
        assert_eq!(filtered_projects(&model, &state, false).len(), 1);
        assert_eq!(
            model.projects[0].channels[0].activity,
            Some(crate::agent::ActivityState::Waiting),
        );
    }

    /// Every navigation row laid out in `layout`: its content row's bounds and
    /// the bounds of the slots inside it, outermost row first.
    ///
    /// A row is recognised by the grid's own leading icon slot - the only node
    /// whose first child is laid out to exactly `ROW_ICON_WIDTH` - so these are
    /// the positions the renderer draws at rather than the numbers the builders
    /// were handed.
    fn navigation_rows(
        layout: Layout<'_>,
        rows: &mut Vec<(cosmic::iced::Rectangle, Vec<cosmic::iced::Rectangle>)>,
    ) {
        let slots: Vec<cosmic::iced::Rectangle> =
            layout.children().map(|child| child.bounds()).collect();
        if slots
            .first()
            .is_some_and(|slot| (slot.width - ROW_ICON_WIDTH).abs() < 0.5)
        {
            rows.push((layout.bounds(), slots));
            return;
        }
        for child in layout.children() {
            navigation_rows(child, rows);
        }
    }

    fn grid_fixture() -> SidebarModel {
        let mut entry = project();
        // The first worktree is the repository itself, so `main` is the
        // primary row the reported screenshot compares against.
        entry.worktrees[0].path = entry.repository.clone();
        entry.channels.push(SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(20)),
            "Investigate login".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        ));
        SidebarModel::ready(SidebarMode::Expanded, vec![entry])
    }

    #[test]
    fn long_worktree_labels_cannot_paint_over_status_or_badges() {
        use cosmic::iced::{
            Color, Rectangle,
            advanced::{
                mouse,
                renderer::{Headless, Style},
            },
        };

        let mut row = project().worktrees[1].clone();
        row.chrome.label =
            "feat(claws): add JMR Media and Kari with a very long worktree description".into();
        row.detached = true;
        row.locked = true;
        row.chrome.dirty_badge = Some("3 changed files".into());
        row.chrome.pull_request.as_mut().unwrap().checks = CheckStatus::Passing;

        let frame = |worktree: &SidebarWorktree, query: &str, width: f32| {
            let mut renderer = headless_renderer();
            let mut element = worktree_view(
                project_id(1),
                worktree,
                true,
                false,
                None,
                false,
                false,
                &[],
                false,
                None,
                None,
                false,
                true,
                false,
                None,
                query,
                true,
            );
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(width, WORKTREE_ROW_HEIGHT));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut rows = Vec::new();
            navigation_rows(Layout::new(&node), &mut rows);
            assert_eq!(rows.len(), 1);
            let (bounds, slots) = &rows[0];
            assert!(slots[1].width > 0.0);
            for pair in slots.windows(2) {
                assert!(pair[0].x + pair[0].width <= pair[1].x);
            }
            for slot in slots.iter().skip(2) {
                assert!(slot.x + slot.width <= bounds.x + bounds.width);
            }
            let status_start = slots[2].x.ceil() as usize;
            element.as_widget().draw(
                &tree,
                &mut renderer,
                &cosmic::Theme::dark(),
                &Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            let size = Size::new(width as u32, WORKTREE_ROW_HEIGHT as u32);
            (renderer.screenshot(size, 1.0, Color::BLACK), status_start)
        };

        let mut empty = row.clone();
        empty.chrome.label = "".into();
        for width in [220.0, 320.0] {
            let (reference, status_start) = frame(&empty, "", width);
            for query in ["", "claws"] {
                let (painted, actual_start) = frame(&row, query, width);
                assert_eq!(actual_start, status_start);
                let stride = width as usize * 4;
                for y in 0..WORKTREE_ROW_HEIGHT as usize {
                    let region = y * stride + status_start * 4..(y + 1) * stride;
                    assert_eq!(
                        painted[region.clone()],
                        reference[region],
                        "label ink must stay clear of trailing indicators (width={width}, query={query:?}, y={y})",
                    );
                }
            }
        }
    }

    #[test]
    fn navigation_rows_share_one_content_grid() {
        let renderer = headless_renderer();
        let model = grid_fixture();
        let state = SidebarState::default();
        let laid_out = |element: &mut Element<'_, SidebarAction>, width: f32| {
            let mut tree = Tree::new(&*element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 512.0)),
            );
            let mut rows = Vec::new();
            navigation_rows(Layout::new(&node), &mut rows);
            rows
        };
        // `RowInteraction::group` stashes its header's layout for sticky
        // positioning and leaves a childless node in its place, so the header
        // is measured through the builder the group mounts, under the limits
        // the group hands it.
        let header = |compact: bool, width: f32| {
            laid_out(
                &mut project_identity_row(&model.projects[0], false, false, compact, false, ""),
                width,
            )
        };
        let members = |compact: bool, width: f32| {
            let projects = filtered_projects(&model, &state, compact);
            laid_out(
                &mut project_view(
                    &projects[0],
                    true,
                    compact,
                    None,
                    false,
                    row_interaction::StickyViewport::default(),
                ),
                width,
            )
        };

        let width = design::NAVIGATION_DEFAULT_WIDTH;
        let expanded_header = header(false, width);
        let expanded = members(false, width);
        // The channel, then both worktrees.
        assert_eq!((expanded_header.len(), expanded.len()), (1, 3));
        let (head, channel, primary) = (&expanded_header[0].1, &expanded[0].1, &expanded[1].1);
        assert_eq!(
            (channel[0].x, channel[1].x),
            (head[0].x, head[1].x),
            "project, channel and worktree glyphs and labels share columns",
        );
        assert_eq!(expanded[0].0.x, expanded_header[0].0.x);
        assert_eq!(expanded[0].0.width, expanded_header[0].0.width);
        assert_eq!(
            (channel[0].x, channel[1].x),
            (primary[0].x, primary[1].x),
            "channel and worktree rows share one icon and one label column",
        );
        for (row, slots) in expanded_header.iter().chain(&expanded) {
            assert_eq!(slots[0].height, design::ROW_HEIGHT, "row icon slot height");
            assert_eq!(
                slots[0].center_y(),
                slots[1].center_y(),
                "icon and label sit on one baseline grid",
            );
            assert_eq!(row.height, design::ROW_HEIGHT);
        }

        let compact_header = header(true, COMPACT_ROW_WIDTH);
        let compact = members(true, COMPACT_ROW_WIDTH);
        // Quiet children and creation controls do not fill the compact rail.
        assert_eq!((compact_header.len(), compact.len()), (1, 0));
        for (row, slots) in compact_header.iter().chain(&compact) {
            assert_eq!(slots[0].x, compact_header[0].1[0].x, "one rail column");
            assert_eq!(row.height, design::ROW_HEIGHT);
        }
        for (row, _) in &compact {
            assert_eq!(
                row.x + row.width,
                COMPACT_ROW_WIDTH,
                "every rail row reserves the same indicator slot",
            );
        }
    }

    #[test]
    fn project_drag_label_has_no_hover_fill_and_navigation_selection_spans_a_flat_row() {
        let theme = cosmic::Theme::dark();
        let theme::Button::Custom {
            active,
            hovered,
            pressed,
            ..
        } = row_button_style::<true, false>(false)
        else {
            unreachable!();
        };
        assert!(active(false, &theme).background.is_none());
        assert!(hovered(false, &theme).background.is_none());
        assert!(pressed(false, &theme).background.is_none());
        assert!(
            hovered(true, &theme).background.is_some(),
            "keyboard focus remains visible"
        );
        let theme::Button::Custom { active, .. } = row_button_style::<true, true>(true) else {
            unreachable!();
        };
        let selected = active(false, &theme);
        assert!(selected.background.is_some());
        assert_eq!(selected.border_radius, [0.0; 4].into());
    }

    /// The whole point of drawing the header's commands over the row is that
    /// they cost the title nothing until they appear. A reservation that is
    /// always there is the same as no overlay at all.
    #[test]
    fn project_hover_commands_preserve_disclosure_position_and_reserve_only_visible_commands() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let model = grid_fixture();
        let width = design::NAVIGATION_DEFAULT_WIDTH;
        let limits = layout::Limits::new(Size::ZERO, Size::new(width, PROJECT_HEADER_HEIGHT));
        let mut element = project_header_row(
            project_identity_row(&model.projects[0], false, false, false, false, ""),
            vec![
                icon_action(
                    Icon::ChannelAdd,
                    "New channel",
                    None,
                    Id::new("test.channel"),
                ),
                icon_action(
                    Icon::WorktreeAdd,
                    "New worktree",
                    None,
                    Id::new("test.worktree"),
                ),
            ],
            project_icon_action(
                Icon::ChevronDown,
                "Collapse",
                None,
                Id::new("test.collapse"),
            ),
        );
        let mut tree = Tree::new(&element);
        let label_edge = |element: &mut Element<'_, SidebarAction>, tree: &mut Tree| {
            let node = element.as_widget_mut().layout(tree, &renderer, &limits);
            let mut rows = Vec::new();
            navigation_rows(Layout::new(&node), &mut rows);
            assert_eq!(rows.len(), 1, "one header row");
            let label = rows[0].1[1];
            let disclosure = Layout::new(&node).children().last().unwrap().bounds();
            (label.x + label.width, disclosure)
        };

        let resting = label_edge(&mut element, &mut tree);
        let content_edge = width - ICON_CONTROL_SIZE - RowGrid::project(false).gutter();
        assert_eq!(
            resting.0, content_edge,
            "a hidden control reserves nothing: the title runs to the row's own content edge",
        );

        let viewport = Rectangle::with_size(Size::new(width, PROJECT_HEADER_HEIGHT));
        let over = Point::new(width / 2.0, PROJECT_HEADER_HEIGHT / 2.0);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorMoved { position: over }),
            Layout::new(&node),
            mouse::Cursor::Available(over),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut Vec::new()),
            &viewport,
        );
        let hovered = label_edge(&mut element, &mut tree);
        assert_eq!(
            hovered.0,
            content_edge - 2.0 * ICON_CONTROL_SIZE,
            "once the commands are drawn the title stops exactly where they start, so nothing \
             visible runs under them",
        );
        assert_eq!(resting.1, hovered.1, "the disclosure target stays in place");
        assert_eq!(hovered.1.x + hovered.1.width, width);
    }

    /// Rows with open panes keep full strength and the rest recede. Channel
    /// activity never colors the identity glyph; pull requests retain their
    /// meaningful state colors.
    #[test]
    fn rows_without_panes_mute_and_channel_glyphs_stay_neutral() {
        use crate::agent::ActivityState;
        assert_eq!(RowInk::of(true, false), RowInk::Full);
        assert_eq!(RowInk::of(false, true), RowInk::Full);
        assert_eq!(RowInk::of(false, false), RowInk::Muted);

        let channel = |activity, unread, has_panes| {
            SidebarChannel::new(
                ChannelId::from_uuid(Uuid::from_u128(20)),
                "Investigate login".into(),
                false,
                activity,
                unread,
                0,
                has_panes,
                1,
            )
        };
        assert_eq!(
            channel_glyph_tone(&channel(None, false, true)),
            GlyphTone::Ink(RowInk::Full)
        );
        assert_eq!(
            channel_glyph_tone(&channel(None, false, false)),
            GlyphTone::Ink(RowInk::Muted)
        );
        assert_eq!(
            channel_glyph_tone(&channel(Some(ActivityState::Waiting), false, true)),
            GlyphTone::Ink(RowInk::Full),
            "waiting is shown by the indicator, not the glyph"
        );
        assert_eq!(
            channel_glyph_tone(&channel(Some(ActivityState::Working), false, true)),
            GlyphTone::Ink(RowInk::Full),
            "working is shown by the indicator, not the glyph"
        );
        assert_eq!(
            channel_glyph_tone(&channel(None, true, false)),
            GlyphTone::Ink(RowInk::Full),
            "unread channels keep neutral identity glyphs"
        );
        assert!(
            channel(None, false, false)
                .accessible_name
                .contains("no open panes")
        );
        assert!(
            !channel(None, false, true)
                .accessible_name
                .contains("no open panes")
        );

        assert_eq!(
            worktree_glyph_tone(None, false, true),
            GlyphTone::Ink(RowInk::Full)
        );
        assert_eq!(
            worktree_glyph_tone(None, false, false),
            GlyphTone::Ink(RowInk::Muted)
        );
        assert_eq!(
            worktree_glyph_tone(None, true, false),
            GlyphTone::Ink(RowInk::Full),
            "an unread update keeps a paneless worktree at full strength"
        );
        assert_eq!(
            worktree_glyph_tone(Some(PullRequestTone::Neutral), false, false),
            GlyphTone::Ink(RowInk::Muted),
            "a neutral pull request carries no news"
        );
        assert_eq!(
            worktree_glyph_tone(Some(PullRequestTone::Draft), false, true),
            GlyphTone::Draft(RowInk::Full),
            "a draft stays a step quieter than its row, whichever the row is"
        );
        assert_eq!(
            worktree_glyph_tone(Some(PullRequestTone::Draft), false, false),
            GlyphTone::Draft(RowInk::Muted)
        );
        for tone in [PullRequestTone::Merged, PullRequestTone::Danger] {
            assert_eq!(
                worktree_glyph_tone(Some(tone), false, false),
                GlyphTone::PullRequest(tone)
            );
        }
    }

    #[test]
    fn worktree_pane_occupancy_is_keyed_by_project_and_worktree() {
        let project = ProjectId::from_uuid(Uuid::from_u128(1));
        let other_project = ProjectId::from_uuid(Uuid::from_u128(2));
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(3));
        let other = WorktreeId::from_uuid(Uuid::from_u128(4));
        let mut state = SidebarState::default();
        assert!(!state.worktree_has_panes(project, worktree));

        state.set_worktrees_with_panes(BTreeSet::from([(project, worktree)]));
        assert!(state.worktree_has_panes(project, worktree));
        assert!(!state.worktree_has_panes(project, other));
        assert!(!state.worktree_has_panes(other_project, worktree));

        state.set_worktrees_with_panes(BTreeSet::new());
        assert!(!state.worktree_has_panes(project, worktree));
    }

    #[test]
    fn a_paneless_worktree_says_so_to_assistive_technology() {
        let row = worktree(1, "feature", false);
        assert!(worktree_accessible_name(&row, false, None, false).ends_with(", no open panes"));
        assert!(!worktree_accessible_name(&row, false, None, true).contains("no open panes"));
    }

    #[test]
    fn row_indicators_share_one_reserved_slot() {
        let renderer = headless_renderer();
        let channel = |activity, unread, chats| {
            SidebarChannel::new(
                ChannelId::from_uuid(Uuid::from_u128(20)),
                "Investigate login".into(),
                false,
                activity,
                unread,
                chats,
                chats > 0,
                1,
            )
        };
        let label_slot = |channel: &SidebarChannel| {
            let mut element = channel_view(channel, false, false, Some(1), "");
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(
                    Size::ZERO,
                    Size::new(design::NAVIGATION_DEFAULT_WIDTH, design::ROW_HEIGHT),
                ),
            );
            let mut rows = Vec::new();
            navigation_rows(Layout::new(&node), &mut rows);
            assert_eq!(rows.len(), 1);
            rows[0].1[1]
        };
        let plain = label_slot(&channel(None, false, 1));
        for (state, row) in [
            (
                "a working spinner",
                channel(Some(crate::agent::ActivityState::Working), false, 1),
            ),
            ("an attention badge", channel(None, true, 1)),
            ("a chat count", channel(None, false, 4)),
        ] {
            let slot = label_slot(&row);
            assert_eq!(
                (slot.x, slot.width),
                (plain.x, plain.width),
                "{state} reflowed the label",
            );
        }
    }

    /// How far right anything inside the scroll viewport's padded content
    /// reaches. The `filtered_scroll` decorator, the scrollable and its content
    /// container all span the pane deliberately; the first node inside that
    /// container's padding is at depth three.
    fn content_right_edge(layout: Layout<'_>, depth: usize) -> f32 {
        let own = if depth >= 3 {
            layout.bounds().x + layout.bounds().width
        } else {
            f32::MIN
        };
        layout.children().fold(own, |edge, child| {
            edge.max(content_right_edge(child, depth + 1))
        })
    }

    #[test]
    fn sidebar_content_clears_the_scrollbar_rail_inside_the_viewport() {
        let renderer = headless_renderer();
        let model = grid_fixture();
        let state = SidebarState::default();
        for (compact, width) in [
            (false, design::NAVIGATION_DEFAULT_WIDTH),
            (false, EXPANDED_WIDTH_MIN),
            // A fractional logical width is what a 1.25 display scale hands a
            // pane whose device width is odd.
            (false, 259.5),
            (true, COMPACT_WIDTH),
        ] {
            let mut element = project_list(&model, &state, compact, false);
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 240.0)),
            );
            assert_eq!(
                Layout::new(&node).bounds().width,
                width,
                "the scrollable still spans the pane: the rail is inset inside \
                 its viewport rather than taken off a container around it",
            );
            let edge = content_right_edge(Layout::new(&node), 0);
            assert!(
                edge <= width - design::SCROLLBAR_GUTTER,
                "content reaches {edge} of {width}, under the rail",
            );
            assert!(
                edge > width - 2.0 * design::SCROLLBAR_GUTTER,
                "content stops {} short of {width}: more than the rail, so the \
                 reservation is not what is holding it back",
                width - edge,
            );
        }
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
    fn shared_attention_ranking_and_cross_workspace_cycle() {
        use crate::agent::{Activity, ActivityOutcome, ActivityState, Provider};
        let mut state = SidebarState::default();
        let first = CardId::from_uuid(Uuid::from_u128(101));
        let waiting = CardId::from_uuid(Uuid::from_u128(102));
        let failed = CardId::from_uuid(Uuid::from_u128(103));
        let other = WorktreeId::from_uuid(Uuid::from_u128(100));
        let project = project_id(1);
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
        let activity = |state| Activity {
            provider: Provider::Omp,
            state,
        };
        state.set_agent_activity(first, project, worktree, activity(ActivityState::Working));
        state.set_agent_activity(waiting, project, worktree, activity(ActivityState::Waiting));
        state.set_agent_activity(failed, project_id(2), other, activity(ActivityState::Idle));
        state.set_card_outcome(
            failed,
            Some(ActivityOutcome::Failed {
                reason: "exit 3".into(),
            }),
        );
        state.observe_agent_update(first, project, worktree);
        assert_eq!(
            state.agent_activity(project, worktree),
            Some(ActivityState::Waiting)
        );
        assert_eq!(
            state.worktree_attention(project, worktree),
            Some(AttentionState::Waiting)
        );
        assert_eq!(
            state.worktree_attention(project_id(2), other),
            Some(AttentionState::Failed)
        );
        // Unread work remains navigable even while it works on a subsequent turn.
        assert_eq!(state.attention_cards(), vec![waiting, failed, first]);
        let cards = [waiting, failed, first];
        assert_eq!(cycle_attention(&cards, Some(waiting), false), Some(failed));
        assert_eq!(cycle_attention(&cards, Some(first), false), Some(waiting));
        assert_eq!(cycle_attention(&cards, Some(waiting), true), Some(first));
        assert_eq!(cycle_attention(&cards, None, false), Some(waiting));
        assert_eq!(cycle_attention(&[], Some(first), true), None);
        assert_eq!(state.waiting_count(), 1);
        assert_eq!(state.intervention_count(), 2);
        for live in [
            None,
            Some(ActivityState::Idle),
            Some(ActivityState::Working),
            Some(ActivityState::Waiting),
        ] {
            for unread in [false, true] {
                for outcome in [
                    None,
                    Some(ActivityOutcome::Succeeded),
                    Some(ActivityOutcome::Cancelled),
                    Some(ActivityOutcome::Failed {
                        reason: "boom".into(),
                    }),
                    Some(ActivityOutcome::Interrupted {
                        reason: "missing final result".into(),
                    }),
                ] {
                    let expected = attention_state(live, unread, outcome.as_ref());
                    let mut pane = SidebarState::default();
                    pane.set_agent_activity(
                        first,
                        project,
                        worktree,
                        activity(live.unwrap_or(ActivityState::Unknown)),
                    );
                    pane.set_card_outcome(first, outcome);
                    if unread {
                        pane.observe_agent_update(first, project, worktree);
                    }
                    assert_eq!(pane.card_attention(first), expected);
                    assert_eq!(
                        pane.worktree_attention(project, worktree),
                        expected.max(unread.then_some(AttentionState::Unread))
                    );
                }
            }
        }
    }

    #[test]
    fn unread_round_trip_and_attention_acknowledgement_are_stable() {
        let mut state = SidebarState::default();
        let card = CardId::from_uuid(Uuid::from_u128(9));
        let project = project_id(1);
        let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
        state.observe_agent_update(card, project, worktree);
        let saved = serde_json::to_string(&state.saved_view()).unwrap();
        let mut restored = SidebarState::default();
        restored.restore_view(serde_json::from_str(&saved).unwrap());
        assert!(restored.has_card_update(card));
        assert!(restored.has_agent_update(project, worktree));
        restored.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        restored.acknowledge_agent_update(card);
        assert!(!restored.has_card_update(card));
        assert!(
            restored
                .retained_attention
                .contains(&SelectionRow::Worktree(project, worktree))
        );
        restored.apply(&SidebarAction::SetFilter(SidebarFilter::All));
        restored.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(restored.retained_attention.is_empty());
    }

    #[test]
    fn channel_attribution_uses_the_longest_known_launch_root() {
        let root =
            SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into()));
        let linked = SidebarWorktree::from_durable(
            WorktreeId::new(),
            "/repo/.worktrees/topic".into(),
            Some("topic".into()),
        );
        let roots = [root.clone(), linked.clone()];
        assert_eq!(
            channel_worktree(&roots, [Path::new("/repo/src")]),
            Some((root.id, "main".into()))
        );
        assert_eq!(
            channel_worktree(&roots, [Path::new("/repo/.worktrees/topic/src")]),
            Some((linked.id, "topic".into()))
        );
        assert_eq!(channel_worktree(&roots, [Path::new("/unknown")]), None);
        assert_eq!(
            channel_worktree(
                &roots,
                [Path::new("/repo"), Path::new("/repo/.worktrees/topic")]
            ),
            None
        );
        assert_eq!(
            channel_worktree(&roots, [Path::new("/repo"), Path::new("/unknown")]),
            None
        );
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
            Some(ActivityState::Waiting)
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
    fn group_mutations_cover_folded_files_and_preserve_rename_halves() {
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
        let stage = group_commands(&source, group, false)
            .into_iter()
            .filter_map(|command| command.action)
            .find_map(|action| match resolve_group_command(&source, &action) {
                Some(SidebarAction::Stage { selection, .. }) => Some(selection),
                _ => None,
            })
            .unwrap();
        assert_eq!(stage.len(), 2);
        assert!(stage.contains(&PathSelection::from_changed_path(&renamed)));
        assert!(
            stage
                .iter()
                .any(|selection| selection.path == repo_path("src/hidden.rs"))
        );
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
    fn file_menu_opens_for_every_row_unless_blocked() {
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
                            !blocked,
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
    fn copying_a_path_needs_neither_file_management_nor_a_file() {
        let commands = |directory, file_management| {
            file_commands(directory, file_management)
                .map(|(_, _, command)| command)
                .collect::<Vec<_>>()
        };
        for directory in [false, true] {
            assert_eq!(
                commands(directory, false),
                vec![
                    FileCommand::CopyPath,
                    FileCommand::CopyRelativePath,
                    FileCommand::RevealInFileManager
                ],
                "a clipboard write is not a working-tree mutation (directory={directory})"
            );
        }
        assert_eq!(
            commands(true, true),
            vec![
                FileCommand::CopyPath,
                FileCommand::CopyRelativePath,
                FileCommand::RevealInFileManager
            ],
            "a directory has nothing to rename or duplicate"
        );
        assert_eq!(
            commands(false, true),
            vec![
                FileCommand::Rename,
                FileCommand::CopyFile,
                FileCommand::CopyPath,
                FileCommand::CopyRelativePath,
                FileCommand::RevealInFileManager,
            ],
        );
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
            button::custom(design::body("Open"))
                .width(80)
                .height(32)
                .on_press(SidebarAction::OpenRepositoryPicker)
                .into(),
            button::custom(design::body("Disabled"))
                .width(80)
                .height(32)
                .into(),
            cosmic::iced::widget::Space::new()
                .width(80)
                .height(32)
                .into(),
        ]);
        let mut element = context_controls(
            content,
            project_id(1),
            None,
            false,
            true,
            true,
            false,
            &[],
            Some(1),
        );
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

    /// One frame's worth of the real sidebar, laid out the way the runtime
    /// lays it out.
    fn interface<'a>(
        model: &'a SidebarModel,
        state: &'a SidebarState,
        navigation: &'a NavigationCache,
        bounds: Size,
        cache: cosmic::iced::runtime::user_interface::Cache,
        renderer: &mut cosmic::Renderer,
    ) -> cosmic::iced::runtime::UserInterface<'a, SidebarAction, cosmic::Theme, cosmic::Renderer>
    {
        cosmic::iced::runtime::UserInterface::build(
            view_cached(
                model,
                state,
                SidebarMode::Expanded,
                design::NAVIGATION_DEFAULT_WIDTH,
                false,
                navigation,
                &crate::settings::KeyboardShortcuts::default(),
            ),
            bounds,
            cache,
            renderer,
        )
    }

    /// Drives the real sidebar the way the runtime does, so that a gesture can
    /// be judged by the messages it publishes and the frame it paints. A
    /// window shorter than the project list is what makes a scroll offset, and
    /// the menu geometry that depends on it, observable at all.
    struct Sidebar {
        navigation: NavigationCache,
        model: SidebarModel,
        state: SidebarState,
        renderer: cosmic::Renderer,
        cache: cosmic::iced::runtime::user_interface::Cache,
        bounds: Size,
    }

    impl Sidebar {
        /// A window shorter than a row menu, so the menu has to choose its
        /// side and slice itself the way it would on screen.
        fn scrolling() -> Self {
            Self::sized(320.0)
        }

        /// A window with room below the anchor for the whole menu, so its
        /// placement is fixed by the anchor alone. Nothing then moves the menu
        /// if a font or a theme makes it a few pixels taller, which is what
        /// makes measuring it in pixels reliable.
        fn roomy() -> Self {
            Self::sized(640.0)
        }

        /// The same roomy rail, with a channel row above the project's
        /// worktrees.
        fn with_channel(channel: ChannelId) -> Self {
            let mut sidebar = Self::roomy();
            sidebar.model.projects[0].channels.push(SidebarChannel::new(
                channel,
                "Investigate login".into(),
                false,
                None,
                false,
                1,
                true,
                1,
            ));
            sidebar
        }

        fn sized(height: f32) -> Self {
            let mut entry = project();
            entry.worktrees = (2..30)
                .map(|id| worktree(id, &format!("branch-{id}"), false))
                .collect();
            let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
            // Enough editors that the menu is taller than the short window.
            model.external_editors = (0..6)
                .map(|index| SidebarEditor {
                    id: format!("editor-{index}"),
                    name: format!("Editor {index}"),
                    icon: "text-editor-symbolic".to_owned(),
                })
                .collect();
            Self {
                navigation: NavigationCache::default(),
                model,
                state: SidebarState::default(),
                renderer: headless_renderer(),
                cache: Default::default(),
                bounds: Size::new(design::NAVIGATION_DEFAULT_WIDTH, height),
            }
        }

        fn feed(
            &mut self,
            events: &[cosmic::iced::Event],
            cursor: cosmic::iced::Point,
        ) -> Vec<SidebarAction> {
            use cosmic::iced::advanced::{clipboard, mouse};
            let mut messages = Vec::new();
            let mut interface = interface(
                &self.model,
                &self.state,
                &self.navigation,
                self.bounds,
                std::mem::take(&mut self.cache),
                &mut self.renderer,
            );
            interface.update(
                events,
                mouse::Cursor::Available(cursor),
                &mut self.renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            self.cache = interface.into_cache();
            messages
        }

        fn scroll(&mut self, pixels: f32, at: cosmic::iced::Point) {
            use cosmic::iced::{Event, advanced::mouse};
            self.feed(
                &[Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: pixels },
                })],
                at,
            );
        }

        fn click(
            &mut self,
            button: cosmic::iced::advanced::mouse::Button,
            at: cosmic::iced::Point,
        ) -> Vec<SidebarAction> {
            use cosmic::iced::{Event, advanced::mouse};
            self.feed(
                &[
                    Event::Mouse(mouse::Event::ButtonPressed(button)),
                    Event::Mouse(mouse::Event::ButtonReleased(button)),
                ],
                at,
            )
        }

        fn move_to(&mut self, at: cosmic::iced::Point) {
            use cosmic::iced::{Event, advanced::mouse};
            self.feed(
                &[Event::Mouse(mouse::Event::CursorMoved { position: at })],
                at,
            );
        }

        /// The window rows an open row menu paints on, measured against the
        /// same frame with the menu closed and the pointer absent from both,
        /// so that neither the list's scroll position nor hover feedback can
        /// be mistaken for the menu.
        fn menu_rows(scroll: f32) -> Vec<(usize, usize)> {
            Self::menu_rows_for(Self::roomy, MENU_ROW, scroll)
        }

        fn menu_rows_for(
            make: impl Fn() -> Self,
            anchor: cosmic::iced::Point,
            scroll: f32,
        ) -> Vec<(usize, usize)> {
            use cosmic::iced::{
                Color,
                advanced::{
                    clipboard, mouse,
                    renderer::{Headless, Style},
                },
            };
            let frame = |open: bool| {
                let mut sidebar = make();
                sidebar.scroll(scroll, anchor);
                if open {
                    sidebar.click(mouse::Button::Right, anchor);
                }
                sidebar.move_to(anchor);
                // The frame that draws has to be the frame that updated: the
                // runtime keeps an overlay's layout from its own update pass.
                let mut interface = interface(
                    &sidebar.model,
                    &sidebar.state,
                    &sidebar.navigation,
                    sidebar.bounds,
                    std::mem::take(&mut sidebar.cache),
                    &mut sidebar.renderer,
                );
                interface.update(
                    &[],
                    mouse::Cursor::Unavailable,
                    &mut sidebar.renderer,
                    &mut clipboard::Null,
                    &mut Vec::new(),
                );
                interface.draw(
                    &mut sidebar.renderer,
                    &cosmic::Theme::dark(),
                    &Style::default(),
                    mouse::Cursor::Unavailable,
                );
                let size = Size::new(sidebar.bounds.width as u32, sidebar.bounds.height as u32);
                (sidebar.renderer.screenshot(size, 1.0, Color::BLACK), size)
            };
            let (closed, size) = frame(false);
            let (opened, _) = frame(true);
            let width = 4 * size.width as usize;
            let mut rows: Vec<(usize, usize)> = Vec::new();
            for y in 0..closed.len() / width {
                let row = y * width..(y + 1) * width;
                if closed[row.clone()] == opened[row] {
                    continue;
                }
                match rows.last_mut() {
                    Some((_, last)) if *last + 1 == y => *last = y,
                    _ => rows.push((y, y)),
                }
            }
            rows
        }
    }

    /// A worktree row under the pointer whether the list is scrolled or not.
    /// The short window slices its menu to the available vertical space.
    const MENU_ROW: cosmic::iced::Point = cosmic::iced::Point::new(56.0, 136.0);
    /// How far the wheel moves the list in the scrolled half of each case.
    const MENU_SCROLL: f32 = -80.0;

    #[test]
    fn a_row_menu_in_a_scrolled_list_runs_the_entry_for_its_row() {
        use cosmic::iced::{Point, advanced::mouse};
        // The anchor is above the window's center, so the native popup picks
        // the larger space below it. A short reach selects its first entry.
        let entry_cursor = Point::new(86.0, MENU_ROW.y + 8.0);
        let mut outcomes = Vec::new();
        for scroll in [0.0, MENU_SCROLL] {
            let mut sidebar = Sidebar::scrolling();
            sidebar.scroll(scroll, MENU_ROW);
            let activated = sidebar.click(mouse::Button::Left, MENU_ROW);
            let Some(SidebarAction::OpenWorktree { worktree_id, .. }) = activated
                .iter()
                .find(|action| matches!(action, SidebarAction::OpenWorktree { .. }))
            else {
                panic!("a worktree row sits under the pointer (scroll={scroll}): {activated:?}");
            };
            let under_pointer = *worktree_id;
            sidebar.click(mouse::Button::Right, MENU_ROW);
            sidebar.feed(
                &[cosmic::iced::Event::Window(
                    cosmic::iced::window::Event::RedrawRequested(std::time::Instant::now()),
                )],
                MENU_ROW,
            );
            sidebar.move_to(entry_cursor);
            let chosen = sidebar.click(mouse::Button::Left, entry_cursor);
            let (worktree, command) = match chosen.first() {
                Some(SidebarAction::ProjectCommand {
                    worktree_id,
                    command,
                    ..
                }) => (*worktree_id, command.clone()),
                other => panic!(
                    "choosing a menu entry must run a row command, not fall through to the row \
                     underneath (scroll={scroll}): {other:?}"
                ),
            };
            assert_eq!(
                worktree,
                Some(under_pointer),
                "the menu belongs to the row the pointer opened it over (scroll={scroll})"
            );
            outcomes.push((under_pointer, command));
        }
        let [
            (unscrolled_row, unscrolled_entry),
            (scrolled_row, scrolled_entry),
        ] = outcomes.as_slice()
        else {
            unreachable!("one outcome per scroll state");
        };
        assert_ne!(
            unscrolled_row, scrolled_row,
            "the wheel has to move the list, or the scrolled case tests nothing"
        );
        assert_eq!(
            unscrolled_entry, scrolled_entry,
            "the same reach below the anchor has to land on the same entry, so the menu keeps \
             measuring itself against the window rather than against the scrolled-away content"
        );
    }

    /// A channel is deletable from the row that shows it, like every other
    /// navigation row, and the entry that says so has to reach the
    /// application rather than fall through to the row underneath.
    #[test]
    fn a_channel_row_offers_deleting_the_channel_it_shows() {
        use cosmic::iced::{Point, advanced::mouse};
        let channel_id = ChannelId::from_uuid(Uuid::from_u128(20));
        let mut sidebar = Sidebar::with_channel(channel_id);
        // Find the channel row the way the user does, by clicking until the
        // channel opens: its y depends on the rail's own chrome.
        let row = (0..40)
            .map(|step| Point::new(56.0, 8.0 * step as f32))
            .find(|point| {
                sidebar
                    .click(mouse::Button::Left, *point)
                    .iter()
                    .any(|action| matches!(action, SidebarAction::OpenChannel { .. }))
            })
            .expect("the rail draws the project's channel row");

        // The third entry's position follows the native menu's measured
        // height, including fonts and theme padding. Use its painted bounds
        // rather than a fixed distance from the right-click anchor.
        let painted = Sidebar::menu_rows_for(|| Sidebar::with_channel(channel_id), row, 0.0);
        let (top, bottom) = painted
            .iter()
            .find(|(from, to)| *from <= row.y as usize && *to > row.y as usize)
            .expect("the channel menu paints over its anchor");
        // Menus include a shadow and theme padding. Probe only the measured
        // band, remounting each time so another command cannot close the menu
        // before the Delete entry is reached. Assert the entire event result
        // so a menu command cannot hide click-through to the row underneath.
        let attempted: Vec<_> = (*top..=*bottom)
            .step_by(4)
            .map(|y| {
                let mut candidate = Sidebar::with_channel(channel_id);
                candidate.click(mouse::Button::Right, row);
                // The redraw requested by opening a native menu initializes
                // its geometry at the anchor before the pointer leaves it.
                // Feed that frame just as the application event loop does.
                candidate.feed(
                    &[cosmic::iced::Event::Window(
                        cosmic::iced::window::Event::RedrawRequested(std::time::Instant::now()),
                    )],
                    row,
                );
                let entry = Point::new(row.x + 30.0, y as f32);
                candidate.move_to(entry);
                candidate.click(mouse::Button::Left, entry)
            })
            .collect();
        let selected = attempted
            .iter()
            .find(|actions| actions.contains(&SidebarAction::DeleteChannel { channel_id }));
        assert_eq!(
            selected,
            Some(&vec![SidebarAction::DeleteChannel { channel_id }]),
            "Delete must target the channel without opening a row underneath: {attempted:?}",
        );
    }

    #[test]
    fn a_row_menu_paints_where_it_answers_the_pointer() {
        // The band a menu paints on starts at the pointer and runs down the
        // window. Opening it also repaints the row it was opened on, which
        // borders that band from above, so the two cases are compared by how
        // far the menu reaches rather than by where the repainting starts.
        let band = |painted: Vec<(usize, usize)>, list: &str| {
            painted
                .iter()
                .copied()
                .find(|(from, to)| *from <= MENU_ROW.y as usize && *to > MENU_ROW.y as usize)
                .unwrap_or_else(|| {
                    panic!("{list} row's menu paints where the pointer opened it: {painted:?}")
                })
        };
        let (_, unscrolled) = band(Sidebar::menu_rows(0.0), "an unscrolled");
        let (_, scrolled) = band(Sidebar::menu_rows(MENU_SCROLL), "a scrolled");
        assert_eq!(
            scrolled, unscrolled,
            "a scrolled row's menu has to reach as far down the window as an unscrolled one, or \
             it is drawn somewhere other than where it takes clicks"
        );
    }

    #[test]
    fn clicking_beside_an_open_row_menu_only_dismisses_it() {
        use cosmic::iced::{Point, advanced::mouse};
        // A header control above the scrolled list, inside the band a menu
        // anchored in window space leaves behind if it stops covering the
        // whole window.
        let beside = Point::new(24.0, 20.0);
        for scroll in [0.0, MENU_SCROLL] {
            let mut sidebar = Sidebar::scrolling();
            sidebar.scroll(scroll, MENU_ROW);
            sidebar.click(mouse::Button::Right, MENU_ROW);
            sidebar.move_to(MENU_ROW);
            let dismissing = sidebar.click(mouse::Button::Left, beside);
            assert_eq!(
                dismissing,
                Vec::new(),
                "a click beside an open menu dismisses it rather than reaching the control it \
                 covers (scroll={scroll})"
            );
            let reachable = sidebar.click(mouse::Button::Left, beside);
            assert!(
                !reachable.is_empty(),
                "the dismissed menu leaves that control usable again (scroll={scroll})"
            );
        }
    }

    #[test]
    fn full_sidebar_hover_reveals_project_commands_without_moving_disclosure() {
        use cosmic::iced::{
            Point, Rectangle,
            advanced::{
                mouse,
                widget::{Operation, operation},
            },
        };

        struct ControlBounds {
            target: Id,
            bounds: Option<Rectangle>,
        }
        impl Operation for ControlBounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }

            fn focusable(
                &mut self,
                id: Option<&Id>,
                bounds: Rectangle,
                _state: &mut dyn operation::Focusable,
            ) {
                if id == Some(&self.target) {
                    self.bounds = Some(bounds);
                }
            }
        }

        let control_bounds = |sidebar: &mut Sidebar, target: Id| {
            let mut bounds = ControlBounds {
                target,
                bounds: None,
            };
            let mut interface = interface(
                &sidebar.model,
                &sidebar.state,
                &sidebar.navigation,
                sidebar.bounds,
                std::mem::take(&mut sidebar.cache),
                &mut sidebar.renderer,
            );
            interface.operate(&sidebar.renderer, &mut bounds);
            sidebar.cache = interface.into_cache();
            bounds.bounds.expect("the project control is mounted")
        };

        for expanded in [true, false] {
            let mut sidebar = Sidebar::roomy();
            let project = sidebar.model.projects[0].id;
            let chrome = sidebar.model.projects[0].chrome.clone();
            if !expanded {
                sidebar.state.apply(&SidebarAction::ToggleProject(project));
            }
            sidebar.move_to(Point::new(400.0, 400.0));
            let resting_identity = control_bounds(&mut sidebar, chrome.open_id.clone());
            let resting_disclosure = control_bounds(&mut sidebar, chrome.toggle_id.clone());

            sidebar.move_to(resting_identity.center());
            let hovered_identity = control_bounds(&mut sidebar, chrome.open_id.clone());
            let hovered_disclosure = control_bounds(&mut sidebar, chrome.toggle_id.clone());
            assert_eq!(
                hovered_identity.width,
                resting_identity.width - 2.0 * ICON_CONTROL_SIZE,
                "pointer movement alone reveals creation commands (expanded={expanded})",
            );
            assert_eq!(
                hovered_disclosure, resting_disclosure,
                "hover cannot move the disclosure target (expanded={expanded})",
            );

            let channel = control_bounds(&mut sidebar, chrome.new_channel_id.clone());
            let actions = sidebar.click(mouse::Button::Left, channel.center());
            assert!(
                actions.contains(&SidebarAction::NewChannel {
                    project_id: project
                }),
                "the revealed command responds without opening the project: {actions:?}",
            );
            assert!(!actions.contains(&SidebarAction::ToggleProject(project)));
        }
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
            button::custom(design::body("Project"))
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
        assert_eq!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(origin),
                &viewport,
                &renderer,
            ),
            mouse::Interaction::Grab,
            "project titles expose their existing drag gesture",
        );
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
    fn manual_navigation_interleaves_channels_and_worktrees_for_keyboard_navigation() {
        use crate::workspace_model::NavigationItem;
        let mut entry = project();
        entry.worktrees = vec![worktree(20, "main", false), worktree(21, "feature", false)];
        let channel = ChannelId::new();
        entry.channels.push(SidebarChannel::new(
            channel,
            "Discussion".into(),
            false,
            None,
            false,
            0,
            false,
            1,
        ));
        entry.navigation_order = vec![
            NavigationItem::Worktree(entry.worktrees[1].path.clone()),
            NavigationItem::Channel(channel),
            NavigationItem::Worktree(entry.worktrees[0].path.clone()),
        ];
        let owner = entry.id;
        let first = entry.worktrees[1].id;
        let last = entry.worktrees[0].id;
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        assert_eq!(
            visible_selection_rows(&model, &state, false),
            vec![
                SelectionRow::Project(owner),
                SelectionRow::Worktree(owner, first),
                SelectionRow::Channel(channel),
                SelectionRow::Worktree(owner, last)
            ]
        );
        let cycle = workspace_cycle(&model, &state);
        assert!(
            matches!(&cycle[0], SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == first)
        );
        assert!(
            matches!(&cycle[1], SidebarAction::OpenChannel { channel_id } if *channel_id == channel)
        );
        assert!(
            matches!(&cycle[2], SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == last)
        );
        // Search keeps its existing relevance order instead of a manual override.
        let mut search = state;
        search.query = "feature".into();
        assert_eq!(
            visible_selection_rows(&model, &search, false),
            vec![
                SelectionRow::Project(owner),
                SelectionRow::Worktree(owner, first)
            ]
        );
    }

    #[test]
    fn nested_member_drag_translates_accessibility_bounds_once_when_scrolled() {
        use cosmic::iced::{Vector, mouse};
        let renderer = headless_renderer();
        let row = SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1)));
        let button = button::custom(design::body("Worktree"))
            .name("Drag accessibility worktree")
            .width(240)
            .height(PROJECT_HEADER_HEIGHT)
            .on_press(SidebarAction::OpenRepositoryPicker);
        let members = RowInteraction::members(
            widget::column::with_children(vec![
                button.into(),
                crate::ui_controls::field::TextInput::new("Rename worktree", "nonce")
                    .on_input(|_| SidebarAction::OpenRepositoryPicker)
                    .into(),
            ])
            .into(),
            project_id(9),
            vec![row],
            Vec::new(),
            true,
        );
        let mut element = RowInteraction::projects(
            widget::column::with_children(vec![members]).into(),
            vec![project_id(9)],
            true,
            row_interaction::StickyViewport::default(),
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 512.0)),
        );
        let bounds = |offset, label| {
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node).with_virtual_offset(Vector::new(0.0, offset)),
                &tree,
                mouse::Cursor::Unavailable,
            );
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
        for label in ["Drag accessibility worktree", "Rename worktree"] {
            let normal = bounds(0.0, label);
            let scrolled = bounds(40.0, label);
            assert_eq!(scrolled.y0, normal.y0 - 40.0);
            assert_eq!(scrolled.y1, normal.y1 - 40.0);
            assert_eq!(scrolled.x0, normal.x0);
            assert_eq!(scrolled.x1, normal.x1);
        }
    }

    #[test]
    fn deleting_member_cannot_initiate_a_reorder() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let row = SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1)));
        let content = widget::column::with_children(vec![
            button::custom(design::body("Deleting worktree"))
                .width(240)
                .height(PROJECT_HEADER_HEIGHT)
                .into(),
        ]);
        let mut element =
            RowInteraction::members(content.into(), project_id(9), vec![row], vec![row], true);
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(240.0, PROJECT_HEADER_HEIGHT));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let origin = Point::new(100.0, PROJECT_HEADER_HEIGHT / 2.0);
        let end = Point::new(100.0, PROJECT_HEADER_HEIGHT - 1.0);
        assert_ne!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(origin),
                &viewport,
                &renderer
            ),
            mouse::Interaction::Grab
        );
        let mut messages = Vec::new();
        for (event, point) in [
            (
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                origin,
            ),
            (
                Event::Mouse(mouse::Event::CursorMoved { position: end }),
                end,
            ),
            (
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                end,
            ),
        ] {
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
        }
        assert!(messages.is_empty(), "disabled member emitted {messages:?}");
    }

    #[test]
    fn member_drag_does_not_activate_the_pressed_row() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let content = widget::column::with_children((1..=2).map(|id| {
            button::custom(design::body("Project"))
                .height(PROJECT_HEADER_HEIGHT)
                .width(240)
                .on_press(SidebarAction::ToggleProject(project_id(id)))
                .into()
        }));
        let mut element = RowInteraction::members(
            content.into(),
            project_id(9),
            vec![
                SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1))),
                SelectionRow::Channel(ChannelId::new()),
            ],
            Vec::new(),
            true,
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
        assert_eq!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(origin),
                &viewport,
                &renderer,
            ),
            mouse::Interaction::Grab,
            "project titles expose their existing drag gesture",
        );
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
            vec![SidebarAction::ReorderMember {
                project: project_id(9),
                source: SelectionRow::Worktree(
                    project_id(9),
                    WorktreeId::from_uuid(Uuid::from_u128(1))
                ),
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
            let header = button::custom(design::body("Project"))
                .id(if id == 1 {
                    header_id.clone()
                } else {
                    Id::unique()
                })
                .name(format!("Sticky project {id}"))
                .width(240)
                .height(PROJECT_HEADER_HEIGHT)
                .on_press(SidebarAction::ToggleProject(project_id(id)));
            let member = button::custom(design::body("Worktree"))
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
                widget::button::custom(design::body("Worktree"))
                    .name("Worktree")
                    .width(120)
                    .height(32)
                    .on_press_maybe((!modal).then_some(SidebarAction::OpenRepositoryPicker)),
                project_id(1),
                Some(WorktreeId::from_uuid(Uuid::from_u128(2))),
                modal,
                true,
                true,
                true,
                &[],
                Some(1),
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
    fn hiding_the_search_controls_ends_the_search_they_held() {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleControls);
        state.apply(&SidebarAction::SearchChanged("sidebar".into()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
        assert!(state.search_filters_active());
        assert_eq!(state.saved_view().query, "sidebar");

        state.apply(&SidebarAction::ToggleControls);
        assert!(!state.controls_expanded());
        assert_eq!(state.query(), "");
        assert_eq!(
            state.filter(),
            SidebarFilter::All,
            "a hidden pull request filter would silently change row clicks"
        );
        assert!(!state.search_filters_active());

        // Attention keeps its own always-visible header toggle.
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.apply(&SidebarAction::ToggleControls);
        state.apply(&SidebarAction::ToggleControls);
        assert_eq!(state.filter(), SidebarFilter::Attention);
        assert!(!state.search_filters_active());
    }

    #[test]
    fn a_hidden_query_is_neither_saved_nor_restored() {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleControls);
        state.apply(&SidebarAction::SearchChanged("kept".into()));
        let mut restored = SidebarState::default();
        restored.restore_view(state.saved_view());
        assert_eq!(restored.query(), "kept", "a visible search survives");

        // A view saved before hiding cleared the query.
        let legacy: SidebarView = serde_json::from_value(serde_json::json!({
            "query": "stale",
            "controls_expanded": false,
            "collapsed_projects": [],
            "selection": null,
        }))
        .unwrap();
        let mut restored = SidebarState::default();
        restored.restore_view(legacy);
        assert_eq!(restored.query(), "");
        assert_eq!(restored.saved_view().query, "");
    }

    #[test]
    fn the_search_toggle_is_lit_while_any_search_filter_applies() {
        for (controls_expanded, query, filter, lit) in [
            (false, "", SidebarFilter::All, false),
            (false, "", SidebarFilter::Attention, false),
            (false, "  ", SidebarFilter::All, false),
            (true, "", SidebarFilter::All, true),
            // Neither can arise through the pane any more, but a state that
            // holds one must still show it.
            (false, "q", SidebarFilter::All, true),
            (false, "", SidebarFilter::PullRequests, true),
        ] {
            let state = SidebarState {
                controls_expanded,
                query: query.into(),
                filter,
                ..SidebarState::default()
            };
            assert_eq!(
                search_toggle_lit(&state),
                lit,
                "controls={controls_expanded} query={query:?} filter={filter:?}"
            );
        }
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
        assert_eq!(
            filtered_projects(&model, &state, false)[0].worktrees.len(),
            1,
            "read rows stay in the attention inbox"
        );
        state.apply(&SidebarAction::SetFilter(SidebarFilter::All));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        assert!(filtered_projects(&model, &state, false).is_empty());
    }

    #[test]
    fn sidebar_search_matches_unicode_case_in_each_row_type() {
        let worktree = SidebarWorktree::new(
            WorktreeId::new(),
            "/repo/equipe".into(),
            Some("Équipe".into()),
            false,
            false,
            None,
            None,
            None,
        );
        let mut project = SidebarProject::new(
            project_id(1),
            RepositoryId(Uuid::new_v4()),
            "Überblick".into(),
            "/repo/plain".into(),
            vec![worktree],
            Vec::new(),
        );
        project.channels.push(SidebarChannel::new(
            ChannelId::new(),
            "München".into(),
            false,
            None,
            false,
            0,
            false,
            1,
        ));
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project]);
        let mut state = SidebarState::default();

        state.apply(&SidebarAction::SearchChanged("überblick".into()));
        assert_eq!(filtered_projects(&model, &state, false).len(), 1);
        state.apply(&SidebarAction::SearchChanged("équipe".into()));
        assert_eq!(
            filtered_projects(&model, &state, false)[0].worktrees.len(),
            1
        );
        state.apply(&SidebarAction::SearchChanged("münchen".into()));
        assert_eq!(
            filtered_projects(&model, &state, false)[0].channels.len(),
            1
        );
        assert_eq!(fold("ΟΣ"), fold("ος"), "final sigma must case-fold");
        assert_eq!(fold("Straße"), fold("STRASSE"), "full folds can expand");
    }

    #[test]
    fn project_header_actions_are_independent_and_attention_can_be_cleared() {
        let renderer = headless_renderer();
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged("main".into()));
        for compact in [false, true] {
            let actions = frame_enter_presses(
                &renderer,
                header(
                    &model,
                    &state,
                    compact,
                    false,
                    false,
                    &crate::settings::KeyboardShortcuts::default(),
                ),
            );
            assert_eq!(
                actions,
                vec![
                    SidebarAction::SetFilter(SidebarFilter::Attention),
                    SidebarAction::ToggleControls,
                    SidebarAction::OpenRepositoryPicker,
                ]
            );
            state.apply(&actions[0]);
            let actions = frame_enter_presses(
                &renderer,
                header(
                    &model,
                    &state,
                    compact,
                    false,
                    false,
                    &crate::settings::KeyboardShortcuts::default(),
                ),
            );
            assert_eq!(actions[0], SidebarAction::SetFilter(SidebarFilter::All));
            state.apply(&actions[0]);
            assert_eq!(state.query(), "main", "attention does not overwrite search");
            assert!(
                frame_enter_presses(
                    &renderer,
                    header(
                        &model,
                        &state,
                        compact,
                        false,
                        true,
                        &crate::settings::KeyboardShortcuts::default()
                    )
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn projects_header_keeps_its_title_and_controls_within_the_minimum_width() {
        let renderer = headless_renderer();
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let state = SidebarState::default();
        let mut shortcuts = crate::settings::KeyboardShortcuts::default();
        shortcuts.toggle_projects = vec!["Alt+B".into()];
        for loading in [false, true] {
            let mut element = header(
                &model,
                &state,
                false,
                loading,
                false,
                &crate::settings::KeyboardShortcuts::default(),
            );
            let mut tree = Tree::new(&element);
            // The sidebar adds another four pixels of padding on each side.
            let width = EXPANDED_WIDTH_MIN - 8.0;
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, HEADER_HEIGHT)),
            );
            assert!(node.children()[0].bounds().height <= HEADER_HEIGHT / 2.0);
            for child in node.children() {
                assert!(child.bounds().x + child.bounds().width <= width);
            }
            assert!(
                accessible_names(&renderer, utilities(false, false, &shortcuts))
                    .contains(&"Toggle Projects sidebar — Alt+B".to_owned())
            );
        }
    }

    /// The accessible names an element publishes, in tree order.
    fn accessible_names(
        renderer: &cosmic::Renderer,
        mut element: Element<'_, SidebarAction>,
    ) -> Vec<String> {
        use cosmic::iced::advanced::mouse;

        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1280.0, 4_000.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().label().map(ToOwned::to_owned))
            .collect()
    }

    /// A project whose checkout could not be read is still a row, and that row
    /// offers exactly one thing: reading it again. Its worktrees, channels and
    /// pull requests came from the read that failed, so nothing built from
    /// them may be pressable.
    #[test]
    fn folder_projects_remain_selectable_without_git_actions_in_both_sidebar_modes() {
        let renderer = headless_renderer();
        let mut entry = project().with_kind(ProjectKind::Folder);
        entry.worktrees.clear();
        entry.revoke_github();
        let open = SidebarAction::OpenRepository {
            project_id: entry.id,
            repository: entry.repository.clone(),
        };
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        for compact in [false, true] {
            let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
            assert!(actions.contains(&open));
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, SidebarAction::NewWorktree { .. }))
            );
            let names = accessible_names(&renderer, body(&model, &state, compact, false));
            assert!(!names.iter().any(|name| name.starts_with("New worktree")));
        }
    }

    #[test]
    fn an_unreadable_project_keeps_its_row_and_offers_only_a_reload() {
        let renderer = headless_renderer();
        let mut entry = project();
        entry.channels.push(SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(20)),
            "Investigate login".into(),
            false,
            None,
            true,
            1,
            true,
            1,
        ));
        entry.unreachable = Some("fatal: not a git repository".into());
        let reload = SidebarAction::RefreshProject {
            project_id: entry.id,
            repository: entry.repository.clone(),
        };
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        for compact in [false, true] {
            let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
            assert!(
                actions.contains(&reload),
                "compact {compact}: the row cannot be read again: {actions:?}"
            );
            assert!(
                actions.iter().all(|action| *action == reload),
                "compact {compact}: an unreadable project offered more than a reload: {actions:?}"
            );
            assert!(
                frame_enter_presses(&renderer, body(&model, &state, compact, true)).is_empty(),
                "compact {compact}: the reload stays withheld while a modal is up"
            );
            let names = accessible_names(&renderer, body(&model, &state, compact, false));
            assert!(
                names.iter().any(|name| name
                    == "Couldn’t read project Cagents: fatal: not a git repository. Try again"),
                "compact {compact}: no accessible name reports the failure: {names:?}"
            );
        }
    }

    #[test]
    fn a_read_already_in_flight_withholds_the_retry_and_says_so() {
        let renderer = headless_renderer();
        let mut entry = project();
        entry.unreachable = Some("fatal: not a git repository".into());
        entry.reloading = true;
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        for compact in [false, true] {
            assert!(
                frame_enter_presses(&renderer, body(&model, &state, compact, false)).is_empty(),
                "compact {compact}: a retry was offered while a read was already running"
            );
            let names = accessible_names(&renderer, body(&model, &state, compact, false));
            assert!(
                names.iter().any(|name| name
                    == "Couldn’t read project Cagents: fatal: not a git repository. Trying again"),
                "compact {compact}: nothing reports that the read is running: {names:?}"
            );
        }
    }

    #[test]
    fn a_filter_cannot_hide_a_project_that_cannot_be_read() {
        let mut entry = project();
        entry.unreachable = Some("fatal: not a git repository".into());
        let id = entry.id;
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        for filter in [
            SidebarFilter::All,
            SidebarFilter::Attention,
            SidebarFilter::PullRequests,
        ] {
            let state = SidebarState {
                filter,
                ..SidebarState::default()
            };
            assert_eq!(
                filtered_projects(&model, &state, false)
                    .iter()
                    .map(|project| project.project.id)
                    .collect::<Vec<_>>(),
                vec![id],
                "{filter:?} hid the project that cannot be read"
            );
        }
    }

    #[test]
    fn a_running_worktree_operation_stays_cancellable_on_a_project_that_cannot_be_read() {
        let renderer = headless_renderer();
        let mut entry = project();
        let operation = crate::model::OperationId::new();
        entry.unreachable = Some("fatal: not a git repository".into());
        entry.operations.push(WorktreeOperationRow {
            id: operation,
            label: "review".into(),
            detail: "Running git".into(),
            phase: WorktreeOperationPhase::Working,
            cancellable: true,
            reopenable: false,
        });
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        for compact in [false, true] {
            let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
            assert!(
                actions.contains(&SidebarAction::CancelWorktreeOperation(operation)),
                "compact {compact}: a running operation lost its cancel: {actions:?}"
            );
        }
    }

    /// A deletion runs behind its row (#264): the row being deleted says so
    /// and offers nothing, and one whose deletion failed says why, stays
    /// navigable, and offers to run it again.
    #[test]
    fn a_row_being_deleted_is_inert_and_a_failed_one_offers_a_retry() {
        let renderer = headless_renderer();
        let mut entry = project();
        let project_id = entry.id;
        let (kept, doomed) = (entry.worktrees[0].id, entry.worktrees[1].id);
        let channel = ChannelId::new();
        entry.channels = vec![SidebarChannel::new(
            channel,
            "Investigation".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        )];
        let opens = |actions: &[SidebarAction], id: WorktreeId| {
            actions.iter().any(|action| {
                matches!(action, SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == id)
            })
        };

        entry.worktrees[1].deletion = Some(RowDeletion::Deleting);
        entry.channels[0].deletion = Some(RowDeletion::Deleting);
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry.clone()]);
        let mut state = SidebarState::default();
        state.set_worktrees_with_panes(BTreeSet::from([(project_id, kept)]));
        for compact in [false, true] {
            let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
            assert!(opens(&actions, kept), "compact {compact}: {actions:?}");
            assert!(
                !opens(&actions, doomed),
                "compact {compact}: a worktree being deleted can still be opened"
            );
            assert!(
                !actions.contains(&SidebarAction::OpenChannel {
                    channel_id: channel
                }),
                "compact {compact}: a channel being deleted can still be opened"
            );
            let names = accessible_names(&renderer, body(&model, &state, compact, false));
            for expected in [
                // The row is labelled by its pull request.
                "Deleting worktree Polish sidebar",
                "Deleting channel Investigation",
            ] {
                assert!(
                    names.iter().any(|name| name == expected),
                    "compact {compact}: {expected:?} missing from {names:?}"
                );
            }
        }

        entry.worktrees[1].deletion = Some(RowDeletion::Failed("The worktree has changes".into()));
        entry.channels[0].deletion = Some(RowDeletion::Failed("Daemon unreachable".into()));
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let actions = frame_enter_presses(&renderer, body(&model, &state, false, false));
        assert!(
            opens(&actions, doomed),
            "a failed deletion leaves the row usable"
        );
        assert!(
            actions.contains(&SidebarAction::RetryWorktreeDeletion {
                project_id,
                worktree_id: doomed,
            }),
            "{actions:?}"
        );
        assert!(
            actions.contains(&SidebarAction::RetryChannelDeletion {
                channel_id: channel
            }),
            "{actions:?}"
        );
        let names = accessible_names(&renderer, body(&model, &state, false, false));
        assert!(
            names
                .iter()
                .any(|name| name.ends_with("could not be deleted: The worktree has changes")),
            "{names:?}"
        );
    }

    #[test]
    fn a_failed_worktree_operation_offers_dismiss_and_nothing_else() {
        let renderer = headless_renderer();
        let mut entry = project();
        let operation = crate::model::OperationId::new();
        entry.operations.push(WorktreeOperationRow {
            id: operation,
            label: "review".into(),
            detail: "Cancelled before git ran".into(),
            phase: WorktreeOperationPhase::Failed,
            cancellable: false,
            reopenable: false,
        });
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        for compact in [false, true] {
            let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
            assert!(
                actions.contains(&SidebarAction::DismissWorktreeOperation(operation)),
                "compact {compact}: a failed operation cannot be dismissed: {actions:?}"
            );
            assert!(
                !actions.contains(&SidebarAction::CancelWorktreeOperation(operation)),
                "compact {compact}: a finished operation still offers cancel: {actions:?}"
            );
        }
    }

    /// A failure this session started keeps its input: the row reopens it in
    /// the dialog to edit, or runs it again, beside dismissing it (#499).
    #[test]
    fn a_reopenable_failed_worktree_operation_offers_edit_and_retry() {
        let renderer = headless_renderer();
        let mut entry = project();
        let operation = crate::model::OperationId::new();
        entry.operations.push(WorktreeOperationRow {
            id: operation,
            label: "review".into(),
            detail: "destination already exists".into(),
            phase: WorktreeOperationPhase::Failed,
            cancellable: false,
            reopenable: true,
        });
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState::default();
        let actions = frame_enter_presses(&renderer, body(&model, &state, false, false));
        for action in [
            SidebarAction::EditWorktreeOperation(operation),
            SidebarAction::RetryWorktreeOperation(operation),
            SidebarAction::DismissWorktreeOperation(operation),
        ] {
            assert!(actions.contains(&action), "{action:?} missing: {actions:?}");
        }
        let behind_a_dialog = frame_enter_presses(&renderer, body(&model, &state, false, true));
        assert!(
            !behind_a_dialog.iter().any(|action| matches!(
                action,
                SidebarAction::EditWorktreeOperation(_) | SidebarAction::RetryWorktreeOperation(_)
            )),
            "{behind_a_dialog:?}"
        );
    }

    #[test]
    fn the_sidebar_header_keeps_its_titlebar_gestures_in_every_state() {
        use cosmic::iced::{
            Event, Point, Rectangle,
            advanced::{Shell, clipboard},
            mouse,
        };
        let renderer = headless_renderer();
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
        let state = SidebarState::default();
        // The window is undecorated, so the compact rail is as much of a
        // titlebar as the expanded header, and a dialog does not stop the user
        // moving the window.
        for compact in [false, true] {
            for modal_open in [false, true] {
                let mut element = header(
                    &model,
                    &state,
                    compact,
                    false,
                    modal_open,
                    &crate::settings::KeyboardShortcuts::default(),
                );
                let mut tree = Tree::new(&element);
                let viewport = Rectangle::with_size(Size::new(260.0, 44.0));
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
                        mouse::Cursor::Available(Point::new(2.0, 2.0)),
                        &renderer,
                        &mut clipboard::Null,
                        &mut Shell::new(&mut messages),
                        &viewport,
                    );
                }
                assert!(
                    messages.contains(&SidebarAction::WindowMenu),
                    "compact={compact}, modal_open={modal_open}"
                );
            }
        }
    }

    #[test]
    fn pr_browser_keeps_authenticated_projects_without_matching_worktrees() {
        let mut entry = project();
        entry.github_ready = true;
        entry.pull_requests.clear();
        entry.worktrees.clear();
        entry.channels.clear();
        let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        let state = SidebarState {
            filter: SidebarFilter::PullRequests,
            ..SidebarState::default()
        };
        for compact in [false, true] {
            assert_eq!(filtered_projects(&model, &state, compact).len(), 1);
        }
    }

    #[test]
    fn compact_search_and_pr_filter_apply_while_the_popover_is_open() {
        let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::ToggleControls);
        state.apply(&SidebarAction::SearchChanged("fix-sidebar".into()));
        assert_eq!(
            filtered_projects(&model, &state, true)[0].worktrees.len(),
            1
        );
        state.apply(&SidebarAction::SearchChanged(String::new()));
        state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
        let filtered = filtered_projects(&model, &state, true);
        assert_eq!(
            filtered[0].worktrees[0].branch.as_deref(),
            Some("fix-sidebar")
        );
        assert_eq!(filtered[0].worktrees.len(), 1);
        // Dismissing the popover ends both.
        state.apply(&SidebarAction::ToggleControls);
        assert_eq!(
            filtered_projects(&model, &state, true)[0].worktrees.len(),
            2
        );
    }

    #[test]
    fn the_search_input_carries_the_focus_target_the_application_reveals() {
        let mut state = SidebarState::default();
        assert!(
            Tree::new(&search(&state, false))
                .find(&search_focus_id())
                .is_some()
        );

        // A query adds the clear action, which must not cost the input the id
        // the application focuses.
        state.apply(&SidebarAction::SearchChanged("cagents".into()));
        assert!(
            Tree::new(&search(&state, false))
                .find(&search_focus_id())
                .is_some()
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
    fn terminal_pull_request_rows_hide_retained_review_and_check_badges() {
        use cosmic::iced::{
            Color, Rectangle,
            advanced::{
                mouse,
                renderer::{Headless, Style},
            },
        };

        // Draw the actual expanded row and compact rail. Terminal rows with
        // stale remote flags must look exactly like those with no live flags.
        let frame = |row: &SidebarWorktree, compact: bool| {
            let mut renderer = headless_renderer();
            let mut element = worktree_view(
                project_id(1),
                row,
                false,
                false,
                None,
                false,
                true,
                &[],
                false,
                None,
                None,
                false,
                true,
                compact,
                None,
                "",
                true,
            );
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(460.0, WORKTREE_ROW_HEIGHT));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            element.as_widget().draw(
                &tree,
                &mut renderer,
                &cosmic::Theme::dark(),
                &Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            renderer.screenshot(
                Size::new(460, WORKTREE_ROW_HEIGHT as u32),
                1.0,
                Color::BLACK,
            )
        };

        for state in [
            WorktreePullRequestState::Merged,
            WorktreePullRequestState::Closed,
        ] {
            let mut row = project().worktrees.remove(1);
            let mut pr = project().pull_requests.remove(0);
            pr.state = Some(state);
            pr.needs_review = true;
            pr.review_requested_again = true;
            for checks in [
                CheckStatus::Pending,
                CheckStatus::Passing,
                CheckStatus::Failing,
            ] {
                pr.checks = checks;
                row.set_pull_request(pr.clone());
                let terminal = row.pull_request().unwrap();
                assert!(terminal.needs_review);
                assert!(terminal.review_requested_again);
                assert_eq!(terminal.checks, checks, "remote metadata is retained");
                assert!(!worktree_indicators(&row).pending_checks);
                for description in [&row.chrome.accessible_name, &row.chrome.tooltip] {
                    assert!(description.contains(match state {
                        WorktreePullRequestState::Merged => "merged",
                        _ => "closed",
                    }));
                    assert!(!description.contains(", checks "), "{description}");
                    assert!(!description.contains("needs your review"), "{description}");
                    assert!(
                        !description.contains("review re-requested"),
                        "{description}"
                    );
                }
                assert_eq!(
                    pull_request_tone(terminal),
                    match state {
                        WorktreePullRequestState::Merged => PullRequestTone::Merged,
                        _ => PullRequestTone::Danger,
                    }
                );

                let mut cleared = row.clone();
                let mut no_flags = pr.clone();
                no_flags.needs_review = false;
                no_flags.review_requested_again = false;
                no_flags.checks = CheckStatus::Unknown;
                cleared.set_pull_request(no_flags);
                for compact in [false, true] {
                    assert_eq!(
                        frame(&row, compact),
                        frame(&cleared, compact),
                        "terminal badges leaked: state={state:?}, checks={checks:?}, compact={compact}"
                    );
                }
            }
        }
    }

    #[test]
    fn live_and_reopened_pull_request_rows_keep_review_and_check_status() {
        let mut row = project().worktrees.remove(1);
        let mut pr = project().pull_requests.remove(0);
        pr.needs_review = true;
        pr.review_requested_again = true;
        pr.checks = CheckStatus::Pending;
        for state in [
            None,
            Some(WorktreePullRequestState::Open),
            Some(WorktreePullRequestState::Draft),
        ] {
            pr.state = state;
            row.set_pull_request(pr.clone());
            assert!(worktree_indicators(&row).pending_checks);
            assert!(row.pull_request().unwrap().review_requested());
            assert_eq!(
                row.pull_request().unwrap().active_checks(),
                Some(CheckStatus::Pending)
            );
            assert!(row.chrome.accessible_name.contains(", checks pending"));
            assert!(row.chrome.tooltip.contains(", checks pending"));
            assert!(
                row.chrome
                    .accessible_name
                    .contains("review re-requested from you")
            );
            assert!(row.chrome.tooltip.contains("review re-requested from you"));
        }

        pr.state = Some(WorktreePullRequestState::Closed);
        row.set_pull_request(pr);
        let mut reopened = row.pull_request().unwrap().clone();
        reopened.state = Some(WorktreePullRequestState::Open);
        reopened.review_requested_again = false;
        row.set_pull_request(reopened);
        assert!(worktree_indicators(&row).pending_checks);
        assert!(row.chrome.accessible_name.contains("needs your review"));
        assert!(row.chrome.tooltip.contains("needs your review"));
        assert!(!row.chrome.accessible_name.contains("review re-requested"));
    }

    #[test]
    fn pull_request_filter_rows_open_reviews_and_other_rows_open_worktrees() {
        let project = project();
        let mut worktree = project.worktrees[0].clone();
        let pr = project.pull_requests[0].clone();
        worktree.set_pull_request(pr.clone());
        assert!(
            matches!(worktree_primary_action(project.id, &worktree, true),
            SidebarAction::OpenPullRequest { project_id, number, url }
            if project_id == project.id && number == pr.number && url == pr.url)
        );
        assert!(
            matches!(worktree_primary_action(project.id, &worktree, false),
            SidebarAction::OpenWorktree { worktree_id, .. } if worktree_id == worktree.id)
        );
        worktree.clear_pull_request();
        assert!(
            matches!(worktree_primary_action(project.id, &worktree, true),
            SidebarAction::OpenWorktree { worktree_id, .. } if worktree_id == worktree.id)
        );
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
    fn fork_branch_collisions_and_missing_identity_never_supply_a_title() {
        let mut project = project();
        let mut row = worktree(7, "fix-sidebar", false);
        let mut foreign = project.pull_requests[0].clone();
        foreign.head_repository.as_mut().unwrap().owner = "another-fork".into();
        assert!(matching_pull_request(&row, &[foreign.clone()]).is_none());
        assert!(matching_pull_request(&row, &project.pull_requests).is_some());
        project.pull_requests[0].head_repository = None;
        assert!(matching_pull_request(&row, &project.pull_requests).is_none());
        // Rebuilding from refreshed data must drop an old incorrect title.
        row.set_pull_request(foreign.clone());
        let rebuilt = SidebarProject::new(
            project.id,
            project.repository_id,
            project.name,
            project.repository,
            vec![row],
            vec![foreign],
        );
        assert!(rebuilt.worktrees[0].pull_request().is_none());
    }

    #[test]
    fn a_unique_open_pr_survives_same_repository_closed_branch_history() {
        let project = project();
        let row = worktree(7, "fix-sidebar", false);
        let mut open = project.pull_requests[0].clone();
        open.state = Some(WorktreePullRequestState::Open);
        let mut merged = open.clone();
        merged.number += 1;
        merged.state = Some(WorktreePullRequestState::Merged);
        assert_eq!(
            matching_pull_request(&row, &[merged, open.clone()])
                .unwrap()
                .number,
            open.number
        );
        let mut other = open.clone();
        other.number += 2;
        assert!(matching_pull_request(&row, &[other, open]).is_none());
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
        fn button_labels(mut element: Element<'_, SidebarAction>) -> Vec<String> {
            let renderer = headless_renderer();
            let mut tree = Tree::new(element.as_widget());
            let root = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(240.0, 320.0)),
            );
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
            button_labels(error("disk on fire", false, false))
                .iter()
                .any(|label| label == "Try again"),
            "the error state's retry button must carry its own accessible name"
        );

        for (query, filter, expected) in [
            (String::new(), SidebarFilter::All, "Open folder"),
            (String::new(), SidebarFilter::Attention, "Show all"),
            ("nothing".to_owned(), SidebarFilter::All, "Clear search"),
        ] {
            let mut state = SidebarState::default();
            state.apply(&SidebarAction::SearchChanged(query));
            state.apply(&SidebarAction::SetFilter(filter));
            let empty = empty(&state, false, false);
            assert!(
                button_labels(empty)
                    .iter()
                    .any(|label| label == expected || label.starts_with(&format!("{expected} · "))),
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
                untracked_omitted: false,
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
                ..WorkingFiles::default()
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
    fn ignored_files_and_folders_are_muted_without_affecting_change_review() {
        let mut snapshot = snapshot();
        snapshot.files.ignored = vec![repo_path("build/nested/output.log"), repo_path("debug.log")];
        snapshot.files.ignored_directories = vec![repo_path("build"), repo_path("empty")];
        snapshot.files.paths.push(repo_path("build/tracked.rs"));
        snapshot.files.paths.sort();
        snapshot
            .files
            .ignored_directory_errors
            .insert(repo_path("empty"), "Folder is not readable".into());
        let original_changes = snapshot.status.entries.clone();
        let model = SourceControlModel::ready(source_control().key, &snapshot);
        assert_eq!(
            model.ignored_directory_errors,
            snapshot.files.ignored_directory_errors
        );
        for name in [
            "build",
            "build/nested",
            "build/nested/output.log",
            "debug.log",
            "empty",
        ] {
            assert!(
                model
                    .files
                    .iter()
                    .find(|row| row.path == repo_path(name))
                    .unwrap()
                    .ignored,
                "{name}"
            );
        }
        assert!(
            !model
                .files
                .iter()
                .find(|row| row.path == repo_path("build/tracked.rs"))
                .unwrap()
                .ignored
        );
        assert!(
            !model
                .files
                .iter()
                .find(|row| row.path == repo_path("README.md"))
                .unwrap()
                .ignored
        );
        assert_eq!(
            model
                .groups
                .iter()
                .map(|group| group.rows.len())
                .sum::<usize>(),
            original_changes.len()
        );
    }

    #[test]
    fn change_pages_reach_every_file_and_bulk_commands_cover_the_entire_group() {
        let paths: Vec<_> = (0..501)
            .map(|index| repo_path(&format!("file-{index:04}.rs")))
            .collect();
        let tree = file_rows(&paths);
        let mut visited = Vec::new();
        for page in 0..3 {
            let (rows, total) = change_row_page(&tree, None, true, page);
            assert_eq!(total, 501);
            assert!(rows.len() <= MAX_VISIBLE_CHANGE_ROWS);
            visited.extend(rows.into_iter().map(|row| row.path.clone()));
        }
        assert_eq!(visited, paths);
        let mut source = source_control();
        let group = ChangeGroup::new(
            source.key.worktree_id,
            ChangeArea::Unstaged,
            paths
                .iter()
                .map(|path| {
                    ChangeRow::new(
                        source.key.worktree_id,
                        &changed(
                            path.clone(),
                            ChangeArea::Unstaged,
                            ChangeKind::Modified,
                            None,
                        ),
                    )
                })
                .collect(),
        );
        source.groups = vec![group];
        let command = group_commands(&source, &source.groups[0], false)
            .into_iter()
            .find_map(|command| command.action)
            .unwrap();
        let command = resolve_group_command(&source, &command).unwrap();
        let SidebarAction::Stage { selection, .. } = command else {
            panic!("expected stage-all");
        };
        assert_eq!(selection.len(), 501);
        assert_eq!(selection.last().unwrap().path, paths[500]);
    }

    #[test]
    fn partial_status_does_not_claim_untracked_files_are_clean() {
        let mut worktree =
            SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into()));
        let mut status = StatusSnapshot {
            untracked_omitted: true,
            ..StatusSnapshot::default()
        };
        worktree.apply_status(&status);
        assert!(worktree.chrome.tooltip.contains("no tracked changes"));
        assert!(!worktree.chrome.tooltip.contains("clean"));
        status.dirty.unstaged = 1;
        worktree.apply_status(&status);
        assert_eq!(
            worktree.chrome.dirty_badge.as_deref(),
            Some("1 tracked changes")
        );
        status.untracked_omitted = false;
        worktree.apply_status(&status);
        assert_eq!(worktree.chrome.dirty_badge.as_deref(), Some("1 changes"));
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
    fn change_folders_fold_independently_and_survive_view_restore() {
        let source = source_control();
        let mut state = SidebarState::default();
        let scope = ChangeTreeScope::Area(ChangeArea::Unstaged);
        let paths = |state: &SidebarState, scope, flat| {
            visible_change_rows(
                &source.files,
                state.collapsed_change_directories.get(&scope),
                flat,
            )
            .0
            .iter()
            .map(|row| row.path.display().into_owned())
            .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&state, scope, false),
            ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
        );
        state.apply(&SidebarAction::ToggleChangeDirectory {
            key: main_key(),
            scope,
            path: repo_path("src"),
        });
        assert_eq!(paths(&state, scope, false), ["src", "README.md"]);
        assert_eq!(
            paths(&state, ChangeTreeScope::Branch, false),
            ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
        );
        assert_eq!(
            paths(&state, scope, true),
            ["src/app.rs", "src/sidebar.rs", "README.md"]
        );
        let saved = state.saved_view();
        let serialized = serde_json::to_string(&saved).unwrap();
        let mut restored = SidebarState::default();
        restored.restore_view(serde_json::from_str(&serialized).unwrap());
        assert_eq!(paths(&restored, scope, false), ["src", "README.md"]);
    }

    #[test]
    fn file_pages_reach_later_siblings_without_mounting_the_entire_tree() {
        let paths = (0..450)
            .map(|i| repo_path(&format!("early/file-{i:03}.txt")))
            .chain(std::iter::once(repo_path("later/file.txt")))
            .collect::<Vec<_>>();
        let rows = file_rows(&paths);
        let expanded = BTreeSet::from([repo_path("early")]);
        let (first, total) = file_row_page(&rows, &expanded, "", false, false, 0);
        assert_eq!(first.len(), MAX_VISIBLE_FILE_ROWS);
        assert_eq!(total, 452);
        let (second, _) = file_row_page(&rows, &expanded, "", false, false, 1);
        assert_eq!(second.len(), 52);
        assert_eq!(second.last().unwrap().path, repo_path("later"));
        let (clamped, total) = file_row_page(&rows, &BTreeSet::new(), "", false, false, 1);
        assert_eq!(total, 2);
        assert_eq!(clamped.len(), 2);
        let top_level = file_rows(
            &(0..450)
                .map(|i| repo_path(&format!("file-{i:03}.txt")))
                .collect::<Vec<_>>(),
        );
        let (last, total) = file_row_page(&top_level, &BTreeSet::new(), "", false, false, 1);
        assert_eq!(total, 450);
        assert_eq!(last.len(), 50);
        assert_eq!(last.last().unwrap().path, repo_path("file-449.txt"));
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
    pub(crate) fn panel_model() -> SidebarModel {
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
    pub(crate) fn panel_state(model: &SidebarModel) -> SidebarState {
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
        element: Element<'_, SidebarAction>,
    ) -> Vec<SidebarAction> {
        frame_enter_presses_at_width(renderer, element, 1280.0)
    }

    fn frame_enter_presses_at_width(
        renderer: &cosmic::Renderer,
        mut element: Element<'_, SidebarAction>,
        width: f32,
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
        let viewport = Rectangle::with_size(Size::new(width, 4_000.0));
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
    fn separate_commit_fields_preserve_description_and_clear_together() {
        let mut state = SidebarState::default();
        let key = main_key();
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "Original subject\n\nDescription paragraph\nAnother line".into(),
        });
        assert_eq!(state.commit_draft.subject, "Original subject");
        let description = state.commit_draft.composer.0.text();
        assert!(description.starts_with("Description paragraph\nAnother line"));
        state.apply(&SidebarAction::CommitSubjectEditorAction {
            key: key.clone(),
            action: cosmic::iced::widget::text_editor::Action::SelectAll,
        });
        state.apply(&SidebarAction::CommitSubjectEditorAction {
            key: key.clone(),
            action: cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                    "Revised subject".into(),
                )),
            ),
        });
        assert_eq!(state.commit_draft.composer.0.text(), description);
        assert_eq!(
            state.commit_message(),
            compose_commit_message("Revised subject", &description)
        );
        state.apply(&SidebarAction::CommitEditorAction {
            key,
            action: cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                    "Added detail ".into(),
                )),
            ),
        });
        assert!(state.commit_message().starts_with("Revised subject\n\n"));
        assert!(state.commit_message().contains("Added detail "));
        state.clear_commit_message();
        assert!(state.commit_draft.subject.is_empty());
        assert!(state.commit_message().is_empty());
        assert!(state.commit_draft.composer.0.text().trim().is_empty());
        assert_eq!(compose_commit_message("Subject only", "\n"), "Subject only");
        state.apply(&SidebarAction::CommitSubjectEditorAction {
            key: main_key(),
            action: cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                    "First line\nSecond line\nThird line".into(),
                )),
            ),
        });
        assert_eq!(
            state.commit_draft.subject,
            "First line Second line Third line"
        );
        assert!(
            !state
                .commit_draft
                .subject_composer
                .0
                .text()
                .trim_end_matches('\n')
                .contains('\n')
        );
        assert_eq!(state.commit_message(), "First line Second line Third line");
    }

    #[test]
    fn rebinding_scopes_panel_drafts_and_restores_the_worktrees_commit_draft() {
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
        state.apply(&SidebarAction::CommitMessageChanged {
            key: fix_key(),
            message: "Sibling subject\n\nSibling body".into(),
        });
        assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));
        assert_eq!(
            state.commit_message(),
            "wip",
            "returning to the owner restores its input"
        );
        assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
        assert_eq!(state.commit_message(), "Sibling subject\n\nSibling body");
    }

    #[test]
    fn submitted_commit_clears_only_its_revision_even_while_parked() {
        let model = panel_model();
        let mut state = panel_state(&model);
        let key = main_key();
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "First subject\n\nFirst body".into(),
        });
        state.apply(&SidebarAction::AmendChanged {
            key: key.clone(),
            amend: true,
        });
        let submitted = state
            .submitted_commit_revision(&key, state.commit_message(), true)
            .unwrap();
        assert_eq!(
            state.submitted_commit_revision(&key, "Stale frame", true),
            None
        );
        state.sync_active_worktree(&model, &focus_on(&fix_key()));
        state.apply(&SidebarAction::CommitMessageChanged {
            key: fix_key(),
            message: "Other draft".into(),
        });
        state.clear_submitted_commit(&key, submitted);
        assert_eq!(state.commit_message(), "Other draft");
        state.sync_active_worktree(&model, &focus_on(&key));
        assert!(state.commit_message().is_empty());
        assert!(!state.commit_draft.amend);
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "New subject\n\nNew body".into(),
        });
        let next = state
            .submitted_commit_revision(&key, state.commit_message(), false)
            .unwrap();
        state.apply(&SidebarAction::CommitEditorAction {
            key: key.clone(),
            action: cosmic::iced::widget::text_editor::Action::Edit(
                cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("Later edit ".into())),
            ),
        });
        let edited = state.commit_message().to_owned();
        state.sync_active_worktree(&model, &focus_on(&fix_key()));
        state.clear_submitted_commit(&key, next);
        state.sync_active_worktree(&model, &focus_on(&key));
        assert_eq!(
            state.commit_message(),
            edited,
            "later edits survive the older commit's completion"
        );
        // Even replacing text with the same text is a newer editing intent.
        state.apply(&SidebarAction::CommitMessageChanged {
            key: key.clone(),
            message: "New subject\n\nNew body".into(),
        });
        state.clear_submitted_commit(&key, next);
        assert_eq!(state.commit_message(), "New subject\n\nNew body");
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
            group_commands(&source, unstaged, false)
                .into_iter()
                .next()
                .and_then(|command| command.action)
                .and_then(|action| resolve_group_command(&source, &action)),
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
            group_commands(&source, group, false)
                .iter()
                .all(|command| command.action.is_none())
                && row_commands(&source, ChangeArea::Unstaged, &group.rows()[0], false)
                    .iter()
                    .all(|command| command.action.is_none()),
            "two `git add` runs against one index corrupt each other's lock, so the second is not \
             enabled"
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
            .filter_map(|command| command.action)
            .find_map(|action| match resolve_group_command(&source, &action) {
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
    fn sidebar_worktree_and_tab_headers_share_the_same_separator_baseline() {
        use cosmic::iced::Point;
        let renderer = headless_renderer();
        let model = panel_model();
        let state = panel_state(&model);
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let width = 380.0;
        let tabs = widget::column::with_children(vec![
            crate::tab_strip::horizontal(
                widget::Id::new("header.geometry.tabs"),
                vec![],
                design::PRIMARY_CONTROL_SIZE,
                design::HEADER_HEIGHT,
            ),
            design::horizontal_rule(),
        ])
        .into();
        let surfaces = [
            view_inner(
                &model,
                &state,
                SidebarMode::Expanded,
                width,
                false,
                None,
                &shortcuts,
            ),
            file_panel(&model, &state, width, false),
            git_panel(&model, &state, width, false),
            tabs,
        ];
        fn first_rule(node: &layout::Node, parent: Point, width: f32) -> Option<f32> {
            let bounds = node.bounds();
            let origin = Point::new(parent.x + bounds.x, parent.y + bounds.y);
            let own = (node.children().is_empty()
                && (bounds.width - width).abs() < 0.1
                && (bounds.height - design::BORDER_WIDTH).abs() < 0.1)
                .then_some(origin.y);
            node.children()
                .iter()
                .filter_map(|child| first_rule(child, origin, width))
                .chain(own)
                .min_by(f32::total_cmp)
        }
        for mut surface in surfaces {
            let mut tree = Tree::new(&surface);
            let node = surface.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 300.0)),
            );
            assert_eq!(
                first_rule(&node, Point::ORIGIN, width),
                Some(design::HEADER_CONTENT_HEIGHT)
            );
        }
    }

    #[test]
    fn the_git_tab_strip_offers_every_view_it_is_not_already_showing() {
        let renderer = headless_renderer();
        let model = panel_model();
        let mut state = panel_state(&model);
        for tab in ChangesTab::ALL {
            state.changes_tab = tab;
            let published = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
            let offered: Vec<ChangesTab> = published
                .iter()
                .filter_map(|action| match action {
                    SidebarAction::SetChangesTab { tab, .. } => Some(*tab),
                    _ => None,
                })
                .collect();
            let expected: Vec<ChangesTab> = ChangesTab::ALL
                .into_iter()
                .filter(|it| *it != tab)
                .collect();
            assert_eq!(
                offered, expected,
                "the strip must offer the other two views and refuse the one already showing; \
                 from {tab:?} it published {offered:?}"
            );
        }
    }

    /// The standalone editor's sidebar has one row of navigation: the three
    /// panels. It never also offers the Git views, which that host does not
    /// load, and a view's option appears only on the view it arranges.
    #[test]
    fn the_standalone_sidebar_offers_one_level_of_navigation() {
        let renderer = headless_renderer();
        let model = panel_model();
        let state = panel_state(&model);
        for panel in WorktreePanel::ALL {
            let published = frame_enter_presses(
                &renderer,
                worktree_sidebar(&model, &state, 380.0, false, panel, true),
            );
            let offered: Vec<WorktreePanel> = published
                .iter()
                .filter_map(|action| match action {
                    SidebarAction::SetWorktreePanel { panel } => Some(*panel),
                    _ => None,
                })
                .collect();
            let expected: Vec<WorktreePanel> = WorktreePanel::ALL
                .into_iter()
                .filter(|it| *it != panel)
                .collect();
            assert_eq!(
                offered, expected,
                "from {panel:?} it published {published:?}"
            );
            assert!(
                !published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::SetChangesTab { .. })),
                "a second row of Git views is a second level of navigation; {panel:?} \
                 published {published:?}"
            );
            assert_eq!(
                published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::ToggleFilesFilter { .. })),
                panel == WorktreePanel::Files,
                "only Files offers the file filter; {panel:?} published {published:?}"
            );
            assert_eq!(
                published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::SetChangeListMode { .. })),
                panel == WorktreePanel::Changes,
                "only a list of changed files offers tree/list; {panel:?} published \
                 {published:?}"
            );
        }
    }

    /// A Git view the standalone editor never loads cannot be left selected
    /// behind its back: its Changes panel is the working tree, whatever the
    /// shared state last recorded.
    #[test]
    fn the_standalone_changes_panel_is_always_the_working_tree() {
        let renderer = headless_renderer();
        let model = panel_model();
        let mut state = panel_state(&model);
        for tab in ChangesTab::ALL {
            state.changes_tab = tab;
            let published = frame_enter_presses(
                &renderer,
                worktree_sidebar(&model, &state, 380.0, false, WorktreePanel::Changes, true),
            );
            assert!(
                published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::Stage { .. })),
                "with {tab:?} recorded the panel must still draw the working tree; it \
                 published {published:?}"
            );
        }
    }

    /// A plain folder has only files, and one tab is not a choice. The host
    /// says so up front, so the row never depends on whether the read has
    /// landed yet, and a panel remembered from a Git tab falls back to Files.
    #[test]
    fn a_plain_folder_draws_no_panel_tabs() {
        let renderer = headless_renderer();
        let model = panel_model();
        let state = panel_state(&model);
        for panel in WorktreePanel::ALL {
            let published = frame_enter_presses(
                &renderer,
                worktree_sidebar(&model, &state, 380.0, false, panel, false),
            );
            assert!(
                !published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::SetWorktreePanel { .. })),
                "{panel:?} published {published:?}"
            );
            assert!(
                published
                    .iter()
                    .any(|action| matches!(action, SidebarAction::ToggleFilesFilter { .. })),
                "{panel:?} must fall back to Files; it published {published:?}"
            );
        }
    }

    /// The tree/flat choice arranges changed files, so History, a list of
    /// commits, does not offer it; and the one control names the arrangement
    /// it switches to.
    #[test]
    fn the_list_mode_toggle_follows_the_view_it_arranges() {
        let renderer = headless_renderer();
        let model = panel_model();
        let mut state = panel_state(&model);
        for tab in ChangesTab::ALL {
            state.changes_tab = tab;
            let modes: Vec<FileListMode> =
                frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false))
                    .into_iter()
                    .filter_map(|action| match action {
                        SidebarAction::SetChangeListMode { mode, .. } => Some(mode),
                        _ => None,
                    })
                    .collect();
            let expected = if tab == ChangesTab::History {
                Vec::new()
            } else {
                vec![FileListMode::Flat]
            };
            assert_eq!(modes, expected, "{tab:?}");
        }
        state.changes_tab = ChangesTab::Changes;
        state.apply(&SidebarAction::SetChangeListMode {
            key: main_key(),
            mode: FileListMode::Flat,
        });
        let names = accessible_names(&renderer, git_panel(&model, &state, 380.0, false));
        assert!(names.iter().any(|name| name == "View as tree"), "{names:?}");
        assert!(
            !names.iter().any(|name| name == "View as list"),
            "{names:?}"
        );
    }

    /// Every tab is named in words; none is an unlabeled glyph.
    #[test]
    fn every_panel_tab_is_named() {
        let renderer = headless_renderer();
        let model = panel_model();
        let state = panel_state(&model);
        let names = accessible_names(&renderer, git_panel(&model, &state, 380.0, false));
        for tab in ChangesTab::ALL {
            assert!(
                names.iter().any(|name| name.starts_with(tab.description())),
                "{tab:?} is missing from {names:?}"
            );
        }
        // The count the tab draws is part of what it is called.
        assert!(
            names
                .iter()
                .any(|name| name == "Uncommitted changes in this worktree: 4"),
            "{names:?}"
        );
        let names = accessible_names(
            &renderer,
            worktree_sidebar(&model, &state, 380.0, false, WorktreePanel::Files, true),
        );
        for panel in WorktreePanel::ALL {
            assert!(
                names
                    .iter()
                    .any(|name| name.starts_with(panel.description())),
                "{panel:?} is missing from {names:?}"
            );
        }
    }

    #[test]
    fn tab_counts_stop_growing_past_three_digits() {
        assert_eq!(&*tab_count_label(7), "7");
        assert_eq!(&*tab_count_label(999), "999");
        assert_eq!(&*tab_count_label(1_000), "999+");
    }

    /// The Changes tab counts changed files, not group rows: a file with
    /// both staged and unstaged edits is one change.
    #[test]
    fn the_changes_count_is_distinct_paths() {
        assert_eq!(source_control().change_count_label.as_deref(), Some("4"));
        let mut snapshot = snapshot();
        snapshot.status.entries.push(changed(
            repo_path("src/app.rs"),
            ChangeArea::Unstaged,
            ChangeKind::Modified,
            Some((1, 0)),
        ));
        let source = SourceControlModel::ready(main_key(), &snapshot);
        assert_eq!(source.change_count_label.as_deref(), Some("4"));
        let mut clean = self::snapshot();
        clean.status.entries.clear();
        assert_eq!(
            SourceControlModel::ready(main_key(), &clean).change_count_label,
            None
        );
    }

    #[test]
    fn uploads_do_not_require_a_commit_subject_and_keep_their_guards() {
        let renderer = headless_renderer();
        let state = SidebarState::default();
        for width in [240.0, 380.0] {
            let mut source = source_control();
            assert!(source.can_push());
            let actions = frame_enter_presses_at_width(
                &renderer,
                changes_footer(&state, &source, false),
                width,
            );
            assert!(
                actions
                    .iter()
                    .any(|action| matches!(action, SidebarAction::Push { .. }))
            );
            assert!(
                !actions
                    .iter()
                    .any(|action| matches!(action, SidebarAction::Commit { .. }))
            );
            assert!(
                !frame_enter_presses_at_width(
                    &renderer,
                    changes_footer(&state, &source, true),
                    width
                )
                .iter()
                .any(|action| matches!(action, SidebarAction::Push { .. }))
            );
            source.set_mutation(Some(MutationKind::Stage));
            assert!(
                !frame_enter_presses_at_width(
                    &renderer,
                    changes_footer(&state, &source, false),
                    width
                )
                .iter()
                .any(|action| matches!(action, SidebarAction::Push { .. }))
            );
            source.set_mutation(None);
            source.upstream = None;
            source.tracking = TrackingState::NoUpstream;
            assert!(source.can_publish());
            let actions = frame_enter_presses_at_width(
                &renderer,
                changes_footer(&state, &source, false),
                width,
            );
            assert!(
                actions
                    .iter()
                    .any(|action| matches!(action, SidebarAction::OpenPublish { .. }))
            );
            assert!(!actions.iter().any(|action| matches!(
                action,
                SidebarAction::Push { .. } | SidebarAction::Commit { .. }
            )));
        }
    }

    /// Committing belongs to the working tree, and the working tree is one of
    /// three views. Leaving the commit box under History or branch review
    /// would offer a mutation of state that view is not showing.
    #[test]
    fn committing_is_offered_only_by_the_working_tree_view() {
        let renderer = headless_renderer();
        let model = panel_model();
        let mut state = panel_state(&model);
        state.apply(&SidebarAction::CommitMessageChanged {
            key: main_key(),
            message: "a message".into(),
        });
        for tab in ChangesTab::ALL {
            state.changes_tab = tab;
            let commits = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false))
                .iter()
                .any(|action| matches!(action, SidebarAction::Commit { .. }));
            assert_eq!(
                commits,
                tab == ChangesTab::Changes,
                "{tab:?} offered Commit: {commits}"
            );
        }
    }

    /// Every change group discloses the same way. `Unstaged` used to be the
    /// one group with no way to fold a long list away.
    #[test]
    fn every_change_group_folds_away_its_rows() {
        let renderer = headless_renderer();
        let model = panel_model();
        let mut state = panel_state(&model);
        state.changes_tab = ChangesTab::Changes;
        for section in PanelSection::ALL {
            assert!(
                state.section_expanded(section),
                "the fixture opens every group; {section:?} was closed"
            );
        }
        let unstaged_row = |action: &SidebarAction| {
            matches!(
                action,
                SidebarAction::OpenChange {
                    area: ChangeArea::Unstaged,
                    ..
                }
            )
        };
        let open = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
        assert!(
            open.iter().any(unstaged_row),
            "the fixture must draw unstaged rows for this test to mean anything; it published \
             {open:?}"
        );
        state.apply(&SidebarAction::ToggleSection {
            key: main_key(),
            section: PanelSection::Unstaged,
        });
        assert!(!state.section_expanded(PanelSection::Unstaged));
        let folded = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
        assert!(
            !folded.iter().any(unstaged_row),
            "a folded group must draw none of its rows; it published {folded:?}"
        );
        assert!(
            folded.iter().any(|action| matches!(
                action,
                SidebarAction::ToggleSection {
                    section: PanelSection::Unstaged,
                    ..
                }
            )),
            "the folded group must keep the control that reopens it; it published {folded:?}"
        );
    }

    #[test]
    fn no_panel_command_is_pressable_while_a_dialog_is_open() {
        let renderer = headless_renderer();
        let mut model = panel_model();
        for project in &mut model.projects {
            project.github_ready = true;
        }
        let mut state = panel_state(&model);
        state.changes_tab = ChangesTab::Changes;

        // The Changes view contains index mutations; History and Branch are
        // separate views with their own commands, and each must stand down.
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
        for tab in ChangesTab::ALL {
            state.changes_tab = tab;
            let gated = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, true));
            assert!(
                gated.is_empty(),
                "{tab:?} published {gated:?} behind a modal"
            );
        }
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
    fn background_activity_keeps_panel_content_in_place() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
        let renderer = headless_renderer();
        let bounds = |mut element: Element<'_, SidebarAction>, width| {
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 700.0)),
            );
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter_map(|node| {
                    let label = node.node().label()?;
                    let rect = node.node().bounds()?;
                    Some((label.to_owned(), [rect.x0, rect.y0, rect.x1, rect.y1]))
                })
                .collect::<BTreeMap<_, _>>()
        };
        for width in [220.0, 380.0] {
            for panel in [WorktreePanel::Files, WorktreePanel::Changes] {
                let model = panel_model();
                let state = panel_state(&model);
                let before = bounds(
                    worktree_sidebar(&model, &state, width, false, panel, true),
                    width,
                );
                assert!(before.len() > 3, "fixture must contain visible controls");
                for mutation in [None, Some(MutationKind::Stage)] {
                    let mut busy = model.clone();
                    let source = busy.source_control.as_mut().unwrap();
                    source.reloading = true;
                    source.mutation = mutation;
                    let after = bounds(
                        worktree_sidebar(&busy, &state, width, false, panel, true),
                        width,
                    );
                    for (label, rect) in &before {
                        assert_eq!(after.get(label), Some(rect), "{panel:?}: {label} moved");
                    }
                }
            }
        }
    }

    #[test]
    fn loading_more_history_keeps_commits_and_pager_in_place() {
        let mut source = source_control();
        let identity = CommitIdentity {
            name: "Author".into(),
            email: "author@example.com".into(),
            timestamp: 0,
            offset_minutes: 0,
        };
        source.history = vec![CommitRow::new(&CommitSummary {
            oid: oid_of(1),
            parents: vec![],
            author: identity.clone(),
            committer: identity,
            subject: "Existing commit".into(),
            body: String::new(),
        })];
        source.history_loaded = true;
        assert_eq!(history_notice(&source), None);
        let renderer = headless_renderer();
        let state = SidebarState::default();
        let before = accessible_names(&renderer, history_section(&state, &source, false));
        assert!(
            before
                .iter()
                .any(|name| name.contains(&format!("Load {HISTORY_PAGE_SIZE} more commits")))
        );
        source.mark_history_requested();
        assert_eq!(history_notice(&source), None);
        let after = accessible_names(&renderer, history_section(&state, &source, false));
        assert_eq!(before, after);
        assert!(
            frame_enter_presses(&renderer, history_section(&state, &source, false))
                .iter()
                .all(|action| !matches!(action, SidebarAction::LoadMoreHistory { .. }))
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
    #[test]
    fn untracked_starts_expanded_and_history_reloads_when_head_moves() {
        assert!(SidebarState::default().section_expanded(PanelSection::Untracked));
        let mut previous = SourceControlModel::ready(main_key(), &snapshot());
        previous.head_oid = Some(oid_of(1).as_str().into());
        previous.mark_history_requested();
        let mut same = SourceControlModel::ready(main_key(), &snapshot());
        same.head_oid = previous.head_oid.clone();
        previous.carry_mutation_into(&mut same);
        assert!(same.history_loading);
        let mut moved = SourceControlModel::ready(main_key(), &snapshot());
        moved.head_oid = Some(oid_of(2).as_str().into());
        previous.carry_mutation_into(&mut moved);
        assert!(!moved.history_requested());
        assert!(moved.wants_history());
        previous.history_failed("Failed".into());
        assert!(!previous.history_loading);
        assert!(
            previous.history_requested(),
            "failure requires explicit retry, not an automatic loop"
        );
        assert!(previous.can_load_history());
    }
}

/// Focus a navigation row and reveal it within the Projects scroll viewport.
pub fn focus_and_reveal<T: Send + 'static>(id: Id) -> cosmic::iced::Task<T> {
    use cosmic::iced::advanced::widget::{Operation, operation};
    struct FocusRow {
        id: Id,
        bounds: Option<cosmic::iced::Rectangle>,
    }
    impl<T: 'static> Operation<T> for FocusRow {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: cosmic::iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.id) {
                state.focus();
                self.bounds = Some(bounds);
            } else {
                state.unfocus();
            }
        }
        fn finish(&self) -> operation::Outcome<T> {
            self.bounds.map_or(operation::Outcome::None, |bounds| {
                operation::Outcome::Chain(Box::new(RevealRow { bounds }))
            })
        }
    }
    struct RevealRow {
        bounds: cosmic::iced::Rectangle,
    }
    impl<T> Operation<T> for RevealRow {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            bounds: cosmic::iced::Rectangle,
            content: cosmic::iced::Rectangle,
            translation: cosmic::iced::Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if id != Some(&stable_id("sidebar.projects.scroll")) {
                return;
            }
            let row = self.bounds - translation;
            let delta = if row.y < bounds.y {
                row.y - bounds.y
            } else if row.y + row.height > bounds.y + bounds.height {
                row.y + row.height - bounds.y - bounds.height
            } else {
                0.0
            };
            if delta != 0.0 {
                state.scroll_by(
                    operation::scrollable::AbsoluteOffset { x: 0.0, y: delta },
                    bounds,
                    content,
                );
            }
        }
    }
    cosmic::iced::advanced::widget::operate(FocusRow { id, bounds: None })
}
