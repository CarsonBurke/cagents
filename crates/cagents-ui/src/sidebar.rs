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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
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

mod attention;
pub use attention::{State as AttentionState, Summary as AttentionSummary};

mod members;
pub use members::{MemberState as ChannelMemberState, Members as ChannelMembers};

mod rail;

mod revisioned;
pub use revisioned::Revisioned;

mod selection;
pub use selection::{Intent as SelectionIntent, Row as SelectionRow};

pub fn selection_row_focus_id(row: SelectionRow) -> Id {
    match row {
        SelectionRow::Project(id) => entity_id("project.open", id),
        SelectionRow::Worktree(_, id) => entity_id("worktree.open", id),
        SelectionRow::Channel(id) => entity_id("channel.open", id),
        SelectionRow::Card(id) => card_open_id(id),
    }
}

fn card_open_id(card: CardId) -> Id {
    entity_id("card.open", card)
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
// Sidebar identities lead their rows' labels, so they take the leading glyph
// size rather than the panel rows' while retaining the shared row grid.
const ROW_ICON_SIZE: u16 = design::LEADING_ICON_SIZE;
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
/// The status slot of a channel row, whose hover reveals a delete control
/// over its end, where the slot is. The control is laid over the row
/// without moving anything, so the slot is reserved at rest as wide as the
/// control reaches past the row's inset and the gutter before the slot: the
/// control then covers the slot and never the title beside it. The
/// indicator keeps its own column at the slot's end, in line with every
/// other row's.
const ROW_DELETE_STATUS_WIDTH: f32 = ICON_CONTROL_SIZE - 2.0 * ROW_GUTTER;
const _: () = assert!(ROW_DELETE_STATUS_WIDTH >= ROW_STATUS_WIDTH);
/// The project header's hover-revealed commands: disclose, new channel, new
/// worktree. Reserved inside the header row so a long project name ellipsizes
/// instead of running under them, and read by `row_interaction::header_at` so
/// the same three controls stay out of the group's drag region.
const HEADER_ACTION_WIDTH: f32 = 3.0 * ICON_CONTROL_SIZE;

/// The glyph for "a worktree on a branch": [`Icon::Worktree`], wherever a
/// worktree is shown, so the same idea is not drawn two ways.
pub(crate) fn worktree_icon() -> widget::icon::Handle {
    Icon::Worktree.handle()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SidebarFilter {
    #[default]
    All,
    Attention,
    PullRequests,
}

/// Where a card's attention shows, as only the application knows it. The
/// Attention filter keeps the rows a card was listed in when it is dealt
/// with, and needs both halves to find them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttentionPlacement<'a> {
    /// The channels holding the card.
    pub channels: Vec<ChannelId>,
    /// The rows the list draws now. Only these are kept: a row hidden by a
    /// search or a collapsed project was never under the pointer. Shared, so
    /// a batch of reads gathers them once.
    pub listed: &'a [SelectionRow],
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
    /// Revisioned so the navigation list can see in one comparison, on every
    /// frame, that no project changed; see [`NavigationCache`].
    pub projects: Revisioned<Vec<SidebarProject>>,
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

    /// Known projects follow the durable order; unknown projects use a stable
    /// path/identity fallback, independent of asynchronous load completion.
    pub fn apply_project_order(&mut self, order: &[ProjectId]) {
        self.projects.sort_by_key(|project| {
            (
                order
                    .iter()
                    .position(|id| *id == project.id)
                    .unwrap_or(usize::MAX),
                project.repository.clone(),
                project.id,
            )
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
            projects: projects.into(),
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
            projects: Revisioned::default(),
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

    /// The projects whose [`SidebarProject::panes`] no longer match the
    /// state's last attention projection, or name a pane other than `title`
    /// does now. A title report usually changes no row, so this compares in
    /// place and allocates nothing per row; only the projects it returns
    /// need [`Self::set_panes`].
    pub fn stale_panes<'t>(
        &self,
        state: &SidebarState,
        mut title: impl FnMut(CardId) -> Option<&'t str>,
    ) -> Vec<ProjectId> {
        // Each project's rows, and how many of them the projection matched.
        let mut matched: HashMap<ProjectId, (&[SidebarCard], usize)> = self
            .projects
            .iter()
            .map(|project| (project.id, (project.panes.as_slice(), 0)))
            .collect();
        let mut stale = BTreeSet::new();
        for card in state.projected_cards() {
            let Some(title) = title(card.card) else {
                continue;
            };
            let Some((rows, index)) = matched.get_mut(&card.project) else {
                continue;
            };
            if !rows.get(*index).is_some_and(|row| row.shows(card, title)) {
                stale.insert(card.project);
            }
            *index += 1;
        }
        stale.extend(
            matched
                .into_iter()
                .filter(|(_, (rows, index))| rows.len() != *index)
                .map(|(project, _)| project),
        );
        stale.into_iter().collect()
    }

    /// Rebuilds the [`SidebarProject::panes`] of `projects` from the state's
    /// last attention projection; see [`Self::stale_panes`]. `title` names a
    /// card as its pane does; a card it cannot name gets no row. Unchanged
    /// rows are kept.
    pub fn set_panes(
        &mut self,
        state: &SidebarState,
        projects: &[ProjectId],
        mut title: impl FnMut(CardId) -> Option<String>,
    ) {
        // Every open pane has a row, so gather each project's once rather
        // than scanning them all for every project.
        let mut projected: HashMap<ProjectId, Vec<&ProjectedCard>> = HashMap::new();
        for card in state
            .projected_cards()
            .iter()
            .filter(|card| projects.contains(&card.project))
        {
            projected.entry(card.project).or_default().push(card);
        }
        for project in self
            .projects
            .iter_mut()
            .filter(|project| projects.contains(&project.id))
        {
            let mut previous: HashMap<_, _> = std::mem::take(&mut project.panes)
                .into_iter()
                .map(|row| (row.id, row))
                .collect();
            project.panes = projected
                .remove(&project.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|card| {
                    let title = title(card.card)?;
                    Some(match previous.remove(&card.card) {
                        Some(row) if row.shows(card, &title) => row,
                        _ => SidebarCard::new(
                            card.card,
                            title,
                            card.terminal,
                            card.worktree,
                            card.summary,
                        ),
                    })
                })
                .collect();
        }
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
    /// The open panes outside every channel, one per conversation, in
    /// workspace order: the rows the Attention list names, and the rows that
    /// stand in for checkouts while they are hidden. The application fills
    /// these with [`SidebarModel::set_panes`] after each attention projection.
    pub panes: Vec<SidebarCard>,
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
    /// Identifies the pull requests this project displays, so an index built
    /// from them can tell it is current without comparing them. See
    /// [`Self::pull_requests_revision`].
    pull_requests_revision: Revision,
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
    pub attention: AttentionSummary,
    /// Bounded derived context from the shared notification journal.
    pub notification_preview: Option<String>,
    /// The live agents and plain terminals in the channel, which its glyph
    /// column draws as the lead's mark and a count. A fixed-size tally, so comparing rows stays
    /// as cheap as comparing their other fields.
    pub members: ChannelMembers,
    pub chats: usize,
    pub has_panes: bool,
    /// The creation rank the shared registry assigned. Carried on the row
    /// because the row is what gets ordered, and the registry's rank is the
    /// only order two windows can agree on.
    pub ordinal: u32,
    pub deletion: Option<RowDeletion>,
    folded_title: Box<str>,
    open_id: Id,
    delete_id: Id,
    delete_name: Box<str>,
    accessible_name: Box<str>,
    tooltip: Box<str>,
}

impl SidebarChannel {
    #[cfg(test)]
    pub(crate) fn tooltip_for_test(&self) -> &str {
        &self.tooltip
    }

    #[cfg(test)]
    pub(crate) fn accessible_name_for_test(&self) -> &str {
        &self.accessible_name
    }

    pub fn with_attention(mut self, attention: AttentionSummary) -> Self {
        self.attention = attention;
        self.describe();
        self
    }

    pub fn with_notification_preview(mut self, preview: Option<String>) -> Self {
        self.notification_preview = preview;
        self.describe();
        self
    }

    /// The live agents and plain terminals the row's glyph column shows.
    pub fn with_members(mut self, members: ChannelMembers) -> Self {
        self.members = members;
        self.describe();
        self
    }

    /// What the row's tooltip and accessible name say, rebuilt from the row's
    /// own fields whenever one of them is set, so no builder's text depends
    /// on which ran first.
    fn describe(&mut self) {
        let contents = match self.chats {
            1 => "1 chat".to_owned(),
            other => format!("{other} chats"),
        };
        let mut tooltip = format!("{} · {contents}", self.title);
        let mut name = format!(
            "Open channel {}, {contents}{}{}",
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
        );
        let members = self.members.description();
        // The members already say who is working, waiting or stopped by a
        // usage limit, so the attention line adds only what they don't:
        // unread news, a failure, or an agent the members leave out.
        let described = AttentionSummary {
            working: self.members.agents_in(ChannelMemberState::Working),
            waiting: self.members.agents_in(ChannelMemberState::Waiting),
            limited: self.members.agents_in(ChannelMemberState::Limited),
            ..AttentionSummary::default()
        };
        let attention = self.attention.without(described).description();
        for part in [Some(members.as_str()), Some(attention.as_str())]
            .into_iter()
            .chain(std::iter::once(self.notification_preview.as_deref()))
            .flatten()
            .filter(|part| !part.is_empty())
        {
            tooltip.push('\n');
            tooltip.push_str(part);
            name.push_str(", ");
            name.push_str(part);
        }
        self.tooltip = tooltip.into_boxed_str();
        self.accessible_name = name.into_boxed_str();
    }

    /// What the Attention filter lists a channel for: an unread update, an
    /// agent waiting for input, a failure or a usage limit.
    fn needs_attention(&self) -> bool {
        self.unread || self.attention.needs_attention()
    }

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
        let mut row = Self {
            id,
            selected,
            activity,
            unread,
            attention: AttentionSummary::card(activity, unread, false),
            notification_preview: None,
            members: ChannelMembers::default(),
            chats,
            has_panes,
            ordinal,
            deletion: None,
            folded_title: fold(&title),
            open_id: entity_id("channel.open", id),
            delete_id: entity_id("channel.delete", id),
            delete_name: format!("Delete channel {title}").into_boxed_str(),
            accessible_name: Box::default(),
            tooltip: Box::default(),
            title,
        };
        row.describe();
        row
    }
}

/// One conversation or terminal the Attention list names on its own row.
///
/// Only a pane outside every channel gets one: a channel's panes are reached,
/// read and cleared through the channel's row. The worktree is where the
/// pane's agent runs, which the row names after its title so two panes with
/// the same title in different checkouts read apart. `attention` is the whole
/// conversation's, counted once however many cards show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarCard {
    pub id: CardId,
    pub title: String,
    pub terminal: bool,
    pub worktree: Option<WorktreeId>,
    pub attention: AttentionSummary,
    folded_title: Box<str>,
    open_id: Id,
}

impl SidebarCard {
    pub fn new(
        id: CardId,
        title: String,
        terminal: bool,
        worktree: Option<WorktreeId>,
        attention: AttentionSummary,
    ) -> Self {
        Self {
            id,
            terminal,
            worktree,
            attention,
            folded_title: fold(&title),
            open_id: card_open_id(id),
            title,
        }
    }

    fn icon(&self) -> Icon {
        if self.terminal {
            Icon::Terminal
        } else {
            Icon::Chat
        }
    }

    /// Whether the row already draws `card` as titled `title`.
    fn shows(&self, card: &ProjectedCard, title: &str) -> bool {
        self.id == card.card
            && self.title == title
            && self.terminal == card.terminal
            && self.worktree == card.worktree
            && self.attention == card.summary
    }

    /// How well the row matches a search: by its title, or by the checkout
    /// it runs in, as that checkout's own row would.
    fn search_rank(&self, project: &SidebarProject, query: &str) -> Option<usize> {
        search_score(&self.folded_title, query)
            .into_iter()
            .chain(
                self.checkout_row(project)
                    .and_then(|checkout| checkout.search_rank(query)),
            )
            .min()
    }

    /// The checkout the pane runs in, as that checkout's own row names it. A
    /// Folder project's root is the project itself, whose header is the row
    /// above, so it adds nothing.
    fn checkout<'a>(&self, project: &'a SidebarProject) -> Option<&'a str> {
        self.checkout_row(project).map(SidebarWorktree::label)
    }

    fn checkout_row<'a>(&self, project: &'a SidebarProject) -> Option<&'a SidebarWorktree> {
        let worktree = self.worktree?;
        project.worktrees.iter().find(|row| row.id == worktree)
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
            panes: Vec::new(),
            pull_requests,
            github_status: "GitHub has not been refreshed.".into(),
            github_ready: false,
            cached_display: None,
            operations: Vec::new(),
            avatar: None,
            unreachable: None,
            reloading: false,
            pull_requests_revision: Revision::next(),
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
        let flags = |number: u64| {
            let request = requested.get(&number);
            (
                request.is_some(),
                request.is_some_and(|request| {
                    request.history == crate::github::review_requests::scan::History::ReRequested
                }),
            )
        };
        for row in &mut self.pull_requests {
            (row.needs_review, row.review_requested_again) = flags(row.number);
        }
        if let Some(cached) = &mut self.cached_display {
            for row in &mut cached.rows {
                (row.needs_review, row.review_requested_again) = flags(row.number);
            }
        }
        // Discovery reapplies the same requests on nearly every pass, and a
        // worktree's row can only be replaced whole, so it is cloned only
        // when its flags actually differ.
        for worktree in &mut self.worktrees {
            let Some(row) = worktree.pull_request() else {
                continue;
            };
            let flags = flags(row.number);
            if flags != (row.needs_review, row.review_requested_again) {
                let mut row = row.clone();
                (row.needs_review, row.review_requested_again) = flags;
                worktree.set_pull_request(row);
            }
        }
    }

    pub fn replace_pull_requests(&mut self, pull_requests: Vec<SidebarPullRequest>) {
        if self.display_pull_requests() != pull_requests.as_slice() {
            self.pull_requests_revision = Revision::next();
        }
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

    /// Changes when [`Self::replace_pull_requests`] changes which pull
    /// requests, or what of them, the project displays. Revisions come from
    /// one count for the whole process, so a project rebuilt from a fresh
    /// load never repeats the revision of the one it replaces. Review
    /// request flags, which no index reads, leave it alone.
    pub fn pull_requests_revision(&self) -> u64 {
        self.pull_requests_revision.0
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

    /// Whether a query, folded by [`fold_query`], finds this row as the
    /// sidebar search does.
    pub(crate) fn matches(&self, folded_query: &str) -> bool {
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

/// A revision of a project's displayed pull requests, unique in the process.
///
/// It is bookkeeping for indexes built from the project, not part of what the
/// project is: two projects that show the same thing are equal whatever their
/// revisions.
#[derive(Debug, Clone, Copy)]
struct Revision(u64);

impl Revision {
    fn next() -> Self {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self(COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1)
    }
}

impl PartialEq for Revision {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Revision {}

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
            folded_pr_title: None,
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
                .map(|dirty| dirty_label(dirty, false).into_boxed_str()),
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

    /// The literal branch name, or the directory for a detached worktree.
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

    /// Whether a query, folded by [`fold_query`], finds this row as the
    /// sidebar search does.
    pub(crate) fn matches(&self, folded_query: &str) -> bool {
        self.search_rank(folded_query).is_some()
    }

    /// How well a folded query finds this row, lower first; `None` when it
    /// does not.
    pub(crate) fn search_rank(&self, query: &str) -> Option<usize> {
        search_score(&self.chrome.folded_label, query)
            .into_iter()
            .chain(
                self.chrome
                    .folded_branch
                    .as_deref()
                    .and_then(|branch| search_score(branch, query)),
            )
            .chain(
                self.chrome
                    .folded_pr_title
                    .as_deref()
                    .and_then(|title| search_score(title, query)),
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
        self.chrome.folded_pr_title = Some(fold(&pull_request.title));
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
        self.chrome.folded_pr_title = None;
        if changed {
            self.refresh_status_chrome();
        }
    }

    fn refresh_status_chrome(&mut self) {
        let label = worktree_display_name(&self.path, self.branch.as_deref(), self.detached);
        if label != self.chrome.label.as_ref() {
            self.chrome.folded_label = fold(&label);
            self.chrome.delete_name = format!("Delete merged worktree {label}").into_boxed_str();
            self.chrome.label = label.into();
        }
        self.chrome.dirty_badge = self
            .dirty
            .as_ref()
            .filter(|dirty| dirty.is_dirty())
            .map(|dirty| dirty_label(dirty, self.untracked_omitted).into_boxed_str());
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
        let mut tooltip = format!(
            "{}\nBranch: {branch}\n{}\n{status}",
            self.chrome.label,
            self.path.display()
        );
        if let Some(pr) = &self.chrome.pull_request {
            use std::fmt::Write as _;
            let _ = write!(tooltip, "\nPull request #{}: {}", pr.number, pr.title);
        }
        self.chrome.tooltip = tooltip.into_boxed_str();
    }
}

fn paneless_name(accessible_name: &str) -> Box<str> {
    format!("{accessible_name}, no open panes").into_boxed_str()
}

pub(crate) fn worktree_display_name(path: &Path, branch: Option<&str>, detached: bool) -> String {
    branch
        .filter(|_| !detached)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            path.file_name()
                .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
                .into_owned()
        })
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
    folded_pr_title: Option<Box<str>>,
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

pub use crate::ui_controls::WorktreePullRequestState;

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
    pub(crate) fn review_requested(&self) -> bool {
        self.needs_review && !self.is_terminal()
    }

    pub(crate) fn active_checks(&self) -> Option<CheckStatus> {
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
    pr.state
        .map_or(Icon::PullRequest, WorktreePullRequestState::icon)
        .handle()
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
        match self {
            Self::Merged => WorktreePullRequestState::Merged.color(theme),
            Self::Review => design::accent_color(theme),
            Self::Danger => WorktreePullRequestState::Closed.color(theme),
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
    /// A repository card, identified by the worktree it is open on, or a
    /// pull request card, by its repository.
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
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
/// Local to the surface and restored with its window; see [`SidebarState`].
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
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
    ignored_directory_errors: BTreeMap<RepoPath, String>,
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
    /// The snapshot these rows were built from, while they still show
    /// exactly what it builds. A reload that reads an equal snapshot then
    /// keeps the model instead of rebuilding it; anything that edits
    /// snapshot-derived state afterwards must drop it, so the next read is
    /// applied rather than skipped.
    source_snapshot: Option<Arc<RepositorySnapshot>>,
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
            source_snapshot: None,
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
            source_snapshot: None,
        }
    }

    /// Records that this model is exactly what [`Self::ready`] builds from
    /// `snapshot`, so an identical read can leave it in place.
    pub fn built_from(mut self, snapshot: Arc<RepositorySnapshot>) -> Self {
        self.source_snapshot = Some(snapshot);
        self
    }

    /// The snapshot these rows still match, if any; see
    /// [`Self::built_from`].
    pub fn source_snapshot(&self) -> Option<&Arc<RepositorySnapshot>> {
        self.source_snapshot.as_ref()
    }

    /// The expanded ignored folders whose listing failed, with the reason.
    pub fn ignored_directory_errors(&self) -> &BTreeMap<RepoPath, String> {
        &self.ignored_directory_errors
    }

    /// Forgets the failed read of one expanded ignored folder so the next
    /// reload lists it again. The error came from the snapshot, so the
    /// model no longer matches it, and that reload must be applied even if
    /// it fails the same way.
    pub fn retry_ignored_directory(&mut self, path: &RepoPath) {
        self.ignored_directory_errors.remove(path);
        self.source_snapshot = None;
    }

    /// Forgets every failed ignored-folder read; see
    /// [`Self::retry_ignored_directory`].
    pub fn retry_ignored_directories(&mut self) {
        self.ignored_directory_errors.clear();
        self.source_snapshot = None;
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
    pub(crate) fn progress_label(&self) -> Option<&'static str> {
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// A saved presentation value, or its default when this build does not know
/// it: a variant renamed later, or one a newer build saved, must not cost
/// the rest of the window's view.
pub(crate) fn known_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged, bound = "T: serde::Deserialize<'de>")]
    enum Saved<T> {
        Known(T),
        Unknown(serde::de::IgnoredAny),
    }
    Ok(
        match <Saved<T> as serde::Deserialize>::deserialize(deserializer)? {
            Saved::Known(value) => value,
            Saved::Unknown(_) => T::default(),
        },
    )
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SidebarView {
    query: String,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    #[serde(default)]
    expanded_worktree_groups: BTreeSet<ProjectId>,
    selection: Option<SidebarSelection>,
    #[serde(default)]
    expanded_directories: BTreeSet<RepoPath>,
    #[serde(default)]
    collapsed_change_directories: Vec<(ChangeTreeScope, Vec<RepoPath>)>,
    #[serde(default)]
    in_progress_collapsed: bool,
    #[serde(default, deserialize_with = "known_or_default")]
    filter: SidebarFilter,
    #[serde(default, deserialize_with = "known_or_default")]
    toggled_sections: BTreeSet<PanelSection>,
    #[serde(default)]
    panel_open: bool,
    #[serde(default)]
    files_query: String,
    #[serde(default)]
    files_filter_open: bool,
    #[serde(default, deserialize_with = "known_or_default")]
    change_list_mode: FileListMode,
    #[serde(default, deserialize_with = "known_or_default")]
    changes_tab: ChangesTab,
}

impl SidebarView {
    /// This view without the panel state that belonged to the worktree it
    /// was saved on, as binding another worktree would clear it.
    pub fn without_worktree_panel(self) -> Self {
        Self {
            files_query: String::new(),
            files_filter_open: false,
            expanded_directories: BTreeSet::new(),
            collapsed_change_directories: Vec::new(),
            toggled_sections: BTreeSet::new(),
            changes_tab: ChangesTab::default(),
            ..self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneActivity {
    owner: Option<(ProjectId, WorktreeId)>,
    activity: crate::agent::Activity,
    /// A provider usage limit stops the pane's agent. It outlives activity
    /// reports, which say nothing of it, until the conversation says the
    /// limit no longer holds or the pane stops reporting.
    limited: bool,
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

/// The exact ordered roots the projection owns, borrowed on a cache hit.
fn attention_roots(project: &SidebarProject) -> impl Iterator<Item = (WorktreeId, &Path)> {
    project
        .worktrees
        .iter()
        .map(|row| (row.id, row.path.as_path()))
        .chain((project.kind == ProjectKind::Folder).then_some((
            WorktreeId::from_uuid(project.id.as_uuid()),
            project.repository.as_path(),
        )))
}

/// A pane with a row of its own: the first open chat or terminal card of one
/// conversation that no channel holds. The Attention list names those with
/// something to show; hidden checkouts give way to every one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectedCard {
    card: CardId,
    project: ProjectId,
    worktree: Option<WorktreeId>,
    terminal: bool,
    summary: AttentionSummary,
}

#[derive(Debug, Clone)]
struct AttentionProjection {
    dirty: bool,
    owners: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    /// Panes with a row of their own, in workspace order.
    cards: Vec<ProjectedCard>,
    /// The row each card's attention shows on, for every card of a
    /// conversation in [`Self::cards`]: aliases of one conversation share
    /// their first card's row.
    card_rows: BTreeMap<CardId, CardId>,
    source: std::sync::Weak<crate::model::WorkspaceState>,
    inventory: Vec<(ProjectId, Vec<(WorktreeId, PathBuf)>)>,
    folders: BTreeSet<ProjectId>,
    projects: BTreeMap<ProjectId, AttentionSummary>,
    worktrees: BTreeMap<(ProjectId, WorktreeId), AttentionSummary>,
}
impl PartialEq for AttentionProjection {
    fn eq(&self, other: &Self) -> bool {
        self.dirty == other.dirty
            && self.owners == other.owners
            && self.cards == other.cards
            && self.card_rows == other.card_rows
            && self.inventory == other.inventory
            && self.folders == other.folders
            && self.projects == other.projects
            && self.worktrees == other.worktrees
    }
}
impl Eq for AttentionProjection {}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidebarState {
    rows: selection::Selection,
    query: String,
    filter: SidebarFilter,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    sidebar_worktrees: crate::settings::SidebarWorktrees,
    worktree_display_overrides: BTreeMap<ProjectId, crate::settings::SidebarWorktrees>,
    expanded_worktree_groups: BTreeSet<ProjectId>,
    /// Whether the Attention list's "In progress" section is folded. It
    /// starts open.
    in_progress_collapsed: bool,
    /// A press asked to fold In progress, and the fold waits on a probe of
    /// where keyboard focus is; see [`Self::toggle_in_progress`].
    in_progress_fold_pending: bool,
    /// The pane workspace focus names, as the application last reported it.
    focused_card: Option<CardId>,
    /// The pane row the Attention list draws as current: the row of the
    /// focused pane's conversation, when the last projection gave it one.
    followed_card: Option<CardId>,
    selection: Option<SidebarSelection>,
    active_worktree: Option<ActiveWorktreeKey>,
    /// The worktree focus names, which the list draws as current. It is the
    /// panel's worktree unless [`Self::pin_panel_worktree`] binds the panel
    /// to another one.
    followed_worktree: Option<ActiveWorktreeKey>,
    /// A worktree the panel shows in place of the one focus names, such as
    /// the one chosen in the context inspector.
    panel_pin: Option<PathBuf>,
    channel_selected: bool,
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
    failed_updates: BTreeSet<CardId>,
    worktree_attention: BTreeMap<(ProjectId, WorktreeId), AttentionSummary>,
    raw_project_attention: BTreeMap<ProjectId, AttentionSummary>,
    attention_projection: Option<AttentionProjection>,
    #[cfg(test)]
    attention_projection_rebuilds: usize,
    /// Rows read, or whose waiting agent carried on, while the Attention
    /// filter listed them, with how each was dealt with. They stay listed,
    /// dimmed, until the user hides them or changes the filter, so dealing
    /// with a row does not pull it, and often its whole project, out from
    /// under the pointer. Never holds [`Listing::Live`].
    retained_attention: BTreeMap<SelectionRow, Listing>,
    agent_activity: BTreeMap<CardId, PaneActivity>,
    /// Worktrees holding at least one open pane. Derived by the application
    /// after its updates, never while drawing, so each row's read is one
    /// lookup however many cards are open.
    worktrees_with_panes: BTreeSet<(ProjectId, WorktreeId)>,
    /// The checkout row the workspace files each open pane outside every
    /// channel under: its recorded scope, or else the checkout holding its
    /// launch directory. Derived by the application after its updates, it
    /// places a pane with nothing to show and no recorded scope, such as a
    /// plain terminal, while checkouts are hidden.
    pane_checkouts: BTreeMap<CardId, (ProjectId, WorktreeId)>,
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
    /// Clears the count of attention rebuilds, so two states compare by
    /// what they hold rather than by how often they were rebuilt.
    #[cfg(test)]
    pub(crate) fn forget_attention_rebuilds(&mut self) {
        self.attention_projection_rebuilds = 0;
    }

    /// The pane the Attention list draws as current.
    #[cfg(test)]
    pub(crate) fn focused_card(&self) -> Option<CardId> {
        self.focused_card
    }

    pub fn reveal_worktree(
        &mut self,
        project: &SidebarProject,
        worktree: WorktreeId,
        navigation: bool,
    ) {
        let selection = Some(SidebarSelection::Worktree {
            project_id: project.id,
            worktree_id: worktree,
        });
        if navigation || self.channel_selected || self.selection != selection {
            self.collapsed_projects.remove(&project.id);
            if project.worktrees.iter().any(|row| {
                row.id == worktree && !is_primary_worktree(&project.repository, &row.path)
            }) {
                self.expanded_worktree_groups.insert(project.id);
            }
        }
    }

    pub fn set_channel_selected(&mut self, selected: bool) {
        self.channel_selected = selected;
    }
    pub fn set_sidebar_worktrees(&mut self, mode: crate::settings::SidebarWorktrees) {
        self.sidebar_worktrees = mode;
    }

    pub fn set_worktree_overrides(
        &mut self,
        overrides: BTreeMap<ProjectId, crate::settings::SidebarWorktrees>,
    ) {
        self.worktree_display_overrides = overrides;
    }

    /// How `project` shows its checkouts. Hiding them by default hides them
    /// in every project: the default is what the header's checkouts toggle
    /// flips, and a project's own mode applies again once they show.
    pub fn worktree_display(&self, project: ProjectId) -> crate::settings::SidebarWorktrees {
        if !self.sidebar_worktrees.shows_checkouts() {
            return self.sidebar_worktrees;
        }
        self.worktree_display_overrides
            .get(&project)
            .copied()
            .unwrap_or(self.sidebar_worktrees)
    }

    /// Whether the list draws a row for `card`, a pane of `project`: every
    /// pane while the project's checkouts are hidden, and in the Attention
    /// list one needing the user, kept there, or working under In progress.
    fn draws_pane(&self, project: &SidebarProject, card: &SidebarCard) -> bool {
        self.lists_panes(project)
            || (self.filter == SidebarFilter::Attention
                && (self.lists_card(card) || card.attention.only_working()))
    }

    /// Whether the list gives `project`'s open panes rows of their own in
    /// place of its checkouts: the unfiltered list, with its checkouts hidden.
    /// The Attention list names its panes in any case. A folder has no
    /// checkouts to hide: its header is its root.
    pub fn lists_panes(&self, project: &SidebarProject) -> bool {
        self.filter == SidebarFilter::All
            && project.kind == ProjectKind::Git
            && !self.worktree_display(project.id).shows_checkouts()
    }

    /// Whether the checkout rows the filter asks for are drawn. The pull
    /// request filter lists pull requests by their checkouts, so hiding
    /// checkouts would empty it; it draws them whatever the display.
    fn shows_checkouts(&self, project: ProjectId) -> bool {
        self.filter == SidebarFilter::PullRequests
            || self.worktree_display(project).shows_checkouts()
    }

    /// Whether any project's pane rows may be drawn, and so which of them
    /// is current matters to the list.
    fn draws_pane_rows(&self) -> bool {
        match self.filter {
            SidebarFilter::Attention => true,
            SidebarFilter::PullRequests => false,
            SidebarFilter::All => {
                !self.sidebar_worktrees.shows_checkouts()
                    || self
                        .worktree_display_overrides
                        .values()
                        .any(|mode| !mode.shows_checkouts())
            }
        }
    }

    fn selected_worktree(&self, project: &SidebarProject) -> Option<WorktreeId> {
        if self.channel_selected {
            return None;
        }
        match self.drawn_selection() {
            Some(SidebarSelection::Worktree {
                project_id,
                worktree_id,
            }) if project_id == project.id => Some(worktree_id),
            _ => None,
        }
    }

    fn worktree_group_expanded(&self, project: &SidebarProject) -> bool {
        self.expanded_worktree_groups.contains(&project.id) || !self.query.trim().is_empty()
    }

    /// Whether `worktree`'s row is drawn. Hidden checkouts stay hidden even
    /// when selected: the row of the pane in focus, or else the project's,
    /// says where the user is.
    fn shows_worktree(&self, project: &SidebarProject, worktree: &SidebarWorktree) -> bool {
        match self.worktree_display(project.id) {
            crate::settings::SidebarWorktrees::Hidden => self.shows_checkouts(project.id),
            crate::settings::SidebarWorktrees::Expanded => true,
            crate::settings::SidebarWorktrees::Grouped => {
                is_primary_worktree(&project.repository, &worktree.path)
                    || self.selected_worktree(project) == Some(worktree.id)
                    || self.worktree_group_expanded(project)
            }
        }
    }

    fn grouped_attention(&self, project: &SidebarProject) -> AttentionSummary {
        let mut summary = AttentionSummary::default();
        for worktree in &project.worktrees {
            if !is_primary_worktree(&project.repository, &worktree.path) {
                summary.add(self.worktree_attention(project.id, worktree.id));
            }
        }
        summary
    }

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
            self.set_filter(SidebarFilter::All);
        }
    }

    /// Choosing a filter starts a fresh list, so rows kept only because they
    /// were dealt with under Attention go.
    fn set_filter(&mut self, filter: SidebarFilter) {
        self.retained_attention.clear();
        self.filter = filter;
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
            expanded_worktree_groups: self.expanded_worktree_groups.clone(),
            selection: self.selection,
            expanded_directories: self.expanded_directories.clone(),
            collapsed_change_directories: self
                .collapsed_change_directories
                .iter()
                .map(|(scope, paths)| (*scope, paths.iter().cloned().collect()))
                .collect(),
            in_progress_collapsed: self.in_progress_collapsed,
            filter: self.filter,
            toggled_sections: self.toggled_sections.clone(),
            panel_open: self.panel_open,
            files_query: self.files_query.clone(),
            files_filter_open: self.files_filter_open,
            change_list_mode: self.change_list_mode,
            changes_tab: self.changes_tab,
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
        // Like the query, the pull request filter belongs to the search
        // controls and ends with them; see `clear_search_filters`.
        self.filter = if !view.controls_expanded && view.filter == SidebarFilter::PullRequests {
            SidebarFilter::All
        } else {
            view.filter
        };
        self.toggled_sections = view.toggled_sections;
        self.panel_open = view.panel_open;
        self.files_query = view.files_query;
        self.files_filter_open = view.files_filter_open;
        self.change_list_mode = view.change_list_mode;
        self.changes_tab = view.changes_tab;
        self.collapsed_projects = view.collapsed_projects;
        self.expanded_worktree_groups = view.expanded_worktree_groups;
        self.selection = view.selection;
        self.expanded_directories = view.expanded_directories;
        self.collapsed_change_directories = view
            .collapsed_change_directories
            .into_iter()
            .map(|(scope, paths)| (scope, paths.into_iter().collect()))
            .collect();
        self.in_progress_collapsed = view.in_progress_collapsed;
        self.in_progress_fold_pending = false;
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

    /// Unfolds `project`, leaving the search and filter as they are.
    pub fn expand_project(&mut self, project: ProjectId) {
        self.collapsed_projects.remove(&project);
    }

    pub fn reveal_project(&mut self, project: ProjectId) {
        self.collapsed_projects.remove(&project);
        self.query.clear();
        self.set_filter(SidebarFilter::All);
    }

    /// Records unread attention without requiring a repository-owned pane.
    /// Ordinary terminal output and activity alone never call this.
    pub fn observe_card_update(&mut self, card: CardId) {
        self.change_attention(card, |state| {
            state.agent_updates.entry(card).or_insert(None);
        });
    }

    pub fn observe_agent_update(&mut self, card: CardId, project: ProjectId, worktree: WorktreeId) {
        self.change_attention(card, |state| {
            state.agent_updates.insert(card, Some((project, worktree)));
        });
    }

    /// The user has read `card`'s update. While the Attention filter is
    /// showing, the rows that update kept listed stay listed, dimmed, until
    /// the user hides read rows or changes the filter. Closing or removing a
    /// surface is not reading it; use [`Self::forget_card_attention`].
    pub fn acknowledge_agent_update(&mut self, card: CardId, placement: &AttentionPlacement<'_>) {
        let row = self.listed_attention_card(card);
        self.change_attention(card, |state| {
            if state.agent_updates.remove(&card).is_some() {
                state.retain_attention_rows(row, placement, Listing::Read);
            }
            state.failed_updates.remove(&card);
        });
    }

    /// Call before recording that `card`'s agent, waiting for input, carries
    /// on. Its rows were listed for that wait, so, as with a read, the
    /// Attention filter keeps them listed, dimmed. The wait may have been
    /// answered here, elsewhere, or by a policy, so they are kept as handled
    /// rather than read. An agent that stops, disconnects or closes while
    /// waiting did not carry on and must not be settled.
    pub fn settle_waiting_agent(&mut self, card: CardId, placement: &AttentionPlacement<'_>) {
        if self
            .agent_activity
            .get(&card)
            .is_some_and(|entry| entry.activity.state == crate::agent::ActivityState::Waiting)
        {
            let row = self.listed_attention_card(card);
            self.retain_attention_rows(row, placement, Listing::Handled);
        }
    }

    /// Keeps the rows a card's attention showed in: its own pane row and the
    /// channels holding it, of those the list draws now. A read outranks a
    /// handled wait: the user has seen the row.
    fn retain_attention_rows(
        &mut self,
        card_row: Option<CardId>,
        placement: &AttentionPlacement<'_>,
        listing: Listing,
    ) {
        if self.filter != SidebarFilter::Attention {
            return;
        }
        let rows = card_row
            .map(SelectionRow::Card)
            .into_iter()
            .chain(
                placement
                    .channels
                    .iter()
                    .copied()
                    .map(SelectionRow::Channel),
            )
            .filter(|row| placement.listed.contains(row));
        for row in rows {
            self.retained_attention
                .entry(row)
                .and_modify(|kept| {
                    if listing == Listing::Read {
                        *kept = listing;
                    }
                })
                .or_insert(listing);
        }
    }

    /// Drops kept rows whose channel or pane no longer has one. A pane that
    /// closed, or joined a channel, is not listed again as though it had
    /// been read.
    pub fn prune_retained_attention(&mut self, model: &SidebarModel) {
        if self.retained_attention.is_empty() {
            return;
        }
        // Projection can run on every terminal wake. Index live identities
        // once, rather than searching the entire model for every kept row.
        let live: HashSet<SelectionRow> = model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .channels
                    .iter()
                    .map(|channel| SelectionRow::Channel(channel.id))
                    .chain(project.panes.iter().map(|card| SelectionRow::Card(card.id)))
            })
            .collect();
        self.retained_attention.retain(|row, _| live.contains(row));
    }

    /// How the Attention filter keeps `row` listed because it was dealt with
    /// there, whether or not it also needs attention again since.
    fn kept_in_attention(&self, row: SelectionRow) -> Option<Listing> {
        if self.filter != SidebarFilter::Attention {
            return None;
        }
        self.retained_attention.get(&row).copied()
    }

    fn retained_in_attention(&self, row: SelectionRow) -> bool {
        self.kept_in_attention(row).is_some()
    }

    /// Everything a worktree row draws about the agents and panes in it.
    fn worktree_row_state(&self, project: ProjectId, worktree: WorktreeId) -> WorktreeRowState {
        WorktreeRowState {
            attention: self.worktree_attention(project, worktree),
            unread: self.has_agent_update(project, worktree),
            activity: self.worktree_row_activity(project, worktree),
            has_panes: self.worktree_has_panes(project, worktree),
        }
    }

    /// Rows and collapsed project headers describe the same outstanding wait.
    fn worktree_row_activity(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<crate::agent::ActivityState> {
        self.agent_activity(project, worktree)
    }

    /// How the Attention filter lists a channel row.
    fn channel_listing(&self, channel: &SidebarChannel) -> Listing {
        match self.kept_in_attention(SelectionRow::Channel(channel.id)) {
            Some(kept) if !channel.needs_attention() => kept,
            _ => Listing::Live,
        }
    }

    /// How the Attention filter lists a pane row.
    fn card_listing(&self, card: &SidebarCard) -> Listing {
        match self.kept_in_attention(SelectionRow::Card(card.id)) {
            Some(kept) if !card.attention.needs_attention() => kept,
            _ => Listing::Live,
        }
    }

    /// The news a row draws. The Attention list's main section lists a row
    /// for what needs the user, and its indicator says which; the work
    /// spinner there would claim the row is listed for its work, which In
    /// progress is for. A row kept there after the user dealt with it has no
    /// news to show, so its work still shows: it never moves to In progress.
    /// Rows elsewhere draw everything.
    fn shown_attention(&self, attention: AttentionSummary) -> AttentionSummary {
        if self.filter == SidebarFilter::Attention && attention.needs_attention() {
            attention.without_work()
        } else {
            attention
        }
    }

    /// Whether the Attention list's main section lists `card`: it needs the
    /// user, or was dealt with there and is kept until hidden.
    fn lists_card(&self, card: &SidebarCard) -> bool {
        card.attention.needs_attention() || self.retained_in_attention(SelectionRow::Card(card.id))
    }

    /// Whether the Attention list's main section lists `channel`.
    fn lists_channel(&self, channel: &SidebarChannel) -> bool {
        channel.needs_attention() || self.retained_in_attention(SelectionRow::Channel(channel.id))
    }

    pub(crate) fn unread_updates(&self) -> &BTreeMap<CardId, Option<(ProjectId, WorktreeId)>> {
        &self.agent_updates
    }

    pub(crate) fn restore_unread_updates(
        &mut self,
        updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    ) {
        let cards: BTreeSet<_> = self
            .agent_updates
            .keys()
            .chain(updates.keys())
            .copied()
            .collect();
        for card in cards {
            self.change_attention(card, |state| {
                if let Some(owner) = updates.get(&card) {
                    state.agent_updates.insert(card, *owner);
                } else {
                    state.agent_updates.remove(&card);
                    state.failed_updates.remove(&card);
                }
            });
        }
    }

    /// A removed surface is no longer an attention target. This is not a
    /// read acknowledgement: the Attention filter does not keep its rows
    /// listed, and no journal notice is acknowledged.
    pub fn forget_card_attention(&mut self, card: CardId) {
        self.change_attention(card, |state| {
            state.agent_updates.remove(&card);
            state.failed_updates.remove(&card);
            state.agent_activity.remove(&card);
        });
    }

    /// The caller has proved alias and canonical are the same conversation.
    /// Keep existing canonical attribution; otherwise await its current filing
    /// instead of carrying the alias's historical worktree into another view.
    pub fn transfer_card_update(&mut self, alias: CardId, canonical: CardId) {
        if alias != canonical && self.has_card_update(alias) {
            let failed = self.failed_updates.contains(&alias);
            self.change_attention(alias, |state| {
                state.agent_updates.remove(&alias);
                state.failed_updates.remove(&alias);
            });
            self.change_attention(canonical, |state| {
                state.agent_updates.entry(canonical).or_insert(None);
                if failed {
                    state.failed_updates.insert(canonical);
                }
            });
        }
    }

    pub fn has_card_update(&self, card: CardId) -> bool {
        self.agent_updates.contains_key(&card)
    }

    pub fn has_agent_update(&self, project: ProjectId, worktree: WorktreeId) -> bool {
        self.worktree_attention(project, worktree).unread != 0
    }

    /// Replaces the worktrees that hold an open pane.
    pub fn set_worktrees_with_panes(&mut self, worktrees: BTreeSet<(ProjectId, WorktreeId)>) {
        self.worktrees_with_panes = worktrees;
    }

    /// Replace where open panes are filed. A change rebuilds the pane rows
    /// at the next projection; an unchanged map costs one comparison.
    pub fn set_pane_checkouts(&mut self, checkouts: BTreeMap<CardId, (ProjectId, WorktreeId)>) {
        if self.pane_checkouts == checkouts {
            return;
        }
        self.pane_checkouts = checkouts;
        if let Some(projection) = &mut self.attention_projection {
            projection.dirty = true;
        }
    }

    pub fn worktree_has_panes(&self, project: ProjectId, worktree: WorktreeId) -> bool {
        self.worktrees_with_panes.contains(&(project, worktree))
    }

    /// Existing attention owners, for rebinding after workspace topology changes.
    /// Ordinary grid output does not need to rediscover these owners.
    pub fn attention_cards(&self) -> impl Iterator<Item = CardId> + '_ {
        self.agent_activity
            .keys()
            .chain(self.agent_updates.keys())
            .copied()
    }

    /// Move cached activity and unread attention without synthesizing a new turn.
    pub fn reattribute_card_attention(
        &mut self,
        card: CardId,
        owner: Option<(ProjectId, WorktreeId)>,
    ) {
        self.change_attention(card, |state| {
            if let Some(activity) = state.agent_activity.get_mut(&card) {
                activity.owner = owner;
            }
            if let Some(unread) = state.agent_updates.get_mut(&card) {
                *unread = owner;
            }
        });
    }

    pub fn card_activity(&self, card: CardId) -> Option<crate::agent::Activity> {
        self.agent_activity.get(&card).map(|entry| entry.activity)
    }

    pub fn clear_agent_activity(&mut self, card: CardId) {
        self.change_attention(card, |state| {
            state.agent_activity.remove(&card);
        });
    }

    pub fn set_card_activity(&mut self, card: CardId, activity: crate::agent::Activity) {
        self.change_attention(card, |state| {
            let limited = state
                .agent_activity
                .get(&card)
                .is_some_and(|entry| entry.limited);
            state.agent_activity.insert(
                card,
                PaneActivity {
                    owner: None,
                    activity,
                    limited,
                },
            );
            if let Some(owner) = state.agent_updates.get_mut(&card) {
                *owner = None;
            }
        });
    }

    pub fn set_agent_activity(
        &mut self,
        card: CardId,
        project: ProjectId,
        worktree: WorktreeId,
        activity: crate::agent::Activity,
    ) {
        self.change_attention(card, |state| {
            let owner = Some((project, worktree));
            let limited = state
                .agent_activity
                .get(&card)
                .is_some_and(|entry| entry.limited);
            state.agent_activity.insert(
                card,
                PaneActivity {
                    owner,
                    activity,
                    limited,
                },
            );
            if let Some(attribution) = state.agent_updates.get_mut(&card) {
                *attribution = owner;
            }
        });
    }

    pub fn agent_activity(
        &self,
        project: ProjectId,
        worktree: WorktreeId,
    ) -> Option<crate::agent::ActivityState> {
        self.worktree_attention(project, worktree).activity()
    }

    pub fn card_attention(&self, card: CardId) -> AttentionSummary {
        let activity = self.agent_activity.get(&card);
        AttentionSummary::card(
            activity.map(|entry| entry.activity.state),
            self.has_card_update(card),
            self.failed_updates.contains(&card),
        )
        .limited(activity.is_some_and(|entry| entry.limited))
    }

    /// Whether a usage limit stops `card`'s agent. A pane reporting no
    /// activity has no agent for a limit to stop.
    pub fn set_card_limited(&mut self, card: CardId, limited: bool) {
        if self
            .agent_activity
            .get(&card)
            .is_none_or(|entry| entry.limited == limited)
        {
            return;
        }
        self.change_attention(card, |state| {
            if let Some(entry) = state.agent_activity.get_mut(&card) {
                entry.limited = limited;
            }
        });
    }

    /// Replaces explicit unacknowledged Failed journal news. This never
    /// changes live activity or treats cancellation/observation loss as failure.
    pub fn set_failed_updates(&mut self, cards: BTreeSet<CardId>) -> bool {
        let changed: Vec<_> = self
            .failed_updates
            .symmetric_difference(&cards)
            .copied()
            .collect();
        let did_change = !changed.is_empty();
        for card in changed {
            self.change_attention(card, |state| {
                if cards.contains(&card) {
                    state.failed_updates.insert(card);
                } else {
                    state.failed_updates.remove(&card);
                }
            });
        }
        did_change
    }

    // The projection describes the rows the user saw. Preserve its rows
    // through a batch of reads even while the summary counters are changing.
    fn listed_attention_card(&self, card: CardId) -> Option<CardId> {
        match &self.attention_projection {
            Some(projection) => projection.card_rows.get(&card).copied(),
            None => Some(card),
        }
    }

    /// The panes the last attention projection gave rows of their own.
    fn projected_cards(&self) -> &[ProjectedCard] {
        self.attention_projection
            .as_ref()
            .map_or(&[], |projection| projection.cards.as_slice())
    }

    /// The pane row `card`'s attention shows on, if the last projection gave
    /// its conversation one.
    pub fn attention_card_row(&self, card: CardId) -> Option<CardId> {
        self.attention_projection
            .as_ref()?
            .card_rows
            .get(&card)
            .copied()
    }

    /// Follows workspace focus to the pane `card`, so the Attention list
    /// draws its row as current. Cheap and idempotent: the application calls
    /// it after every update, and only a change of row redraws the list.
    pub fn follow_card(&mut self, card: Option<CardId>) {
        self.focused_card = card;
        self.follow_projected_card();
    }

    fn follow_projected_card(&mut self) {
        let followed = self
            .focused_card
            .and_then(|card| self.attention_card_row(card));
        if self.followed_card != followed {
            self.followed_card = followed;
        }
    }

    /// The current pane row, while the list may draw one: a list without
    /// pane rows redraws nothing when focus moves between panes.
    fn drawn_followed_card(&self) -> Option<CardId> {
        self.followed_card.filter(|_| self.draws_pane_rows())
    }

    /// Whether the list draws `card`'s row as the current one.
    fn follows_card(&self, card: &SidebarCard) -> bool {
        self.followed_card == Some(card.id)
    }

    /// Every card whose attention the pane row of `row` shows: the cards of
    /// one conversation, which opening that row reads together.
    pub fn attention_cards_shown_on(&self, row: CardId) -> Vec<CardId> {
        self.attention_projection
            .as_ref()
            .map(|projection| {
                projection
                    .card_rows
                    .iter()
                    .filter(|(_, shown_on)| **shown_on == row)
                    .map(|(card, _)| *card)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The cards [`SidebarModel::set_panes`] will ask to name.
    pub fn pane_cards(&self) -> impl Iterator<Item = CardId> + '_ {
        self.projected_cards().iter().map(|card| card.card)
    }

    fn attention_owner(&self, card: CardId) -> Option<(ProjectId, WorktreeId)> {
        self.agent_activity
            .get(&card)
            .and_then(|entry| entry.owner)
            .or_else(|| self.agent_updates.get(&card).copied().flatten())
    }

    fn change_attention(&mut self, card: CardId, change: impl FnOnce(&mut Self)) {
        let before = (self.attention_owner(card), self.card_attention(card));
        change(self);
        let after = (self.attention_owner(card), self.card_attention(card));
        if before == after {
            return;
        }
        if let Some(owner) = before.0 {
            self.worktree_attention
                .entry(owner)
                .or_default()
                .remove(before.1);
            self.raw_project_attention
                .entry(owner.0)
                .or_default()
                .remove(before.1);
            if self.worktree_attention.get(&owner) == Some(&AttentionSummary::default()) {
                self.worktree_attention.remove(&owner);
            }
            if self.raw_project_attention.get(&owner.0) == Some(&AttentionSummary::default()) {
                self.raw_project_attention.remove(&owner.0);
            }
        }
        if let Some(owner) = after.0.filter(|_| after.1 != AttentionSummary::default()) {
            self.worktree_attention
                .entry(owner)
                .or_default()
                .add(after.1);
            self.raw_project_attention
                .entry(owner.0)
                .or_default()
                .add(after.1);
        }
        if let Some(projection) = &mut self.attention_projection {
            projection.dirty = true;
        }
    }

    /// Whether an attention change since the last projection moved a count
    /// between rows, so the listed rows await a refresh.
    pub fn attention_projection_stale(&self) -> bool {
        self.attention_projection
            .as_ref()
            .is_some_and(|projection| projection.dirty)
    }

    fn worktree_attention_map(&self) -> &BTreeMap<(ProjectId, WorktreeId), AttentionSummary> {
        self.attention_projection
            .as_ref()
            .filter(|projection| !projection.dirty)
            .map(|projection| &projection.worktrees)
            .unwrap_or(&self.worktree_attention)
    }

    pub fn worktree_attention(&self, project: ProjectId, worktree: WorktreeId) -> AttentionSummary {
        self.worktree_attention_map()
            .get(&(project, worktree))
            .copied()
            .unwrap_or_default()
    }

    fn project_attention_map(&self) -> &BTreeMap<ProjectId, AttentionSummary> {
        self.attention_projection
            .as_ref()
            .filter(|projection| !projection.dirty)
            .map(|projection| &projection.projects)
            .unwrap_or(&self.raw_project_attention)
    }

    pub fn project_attention(&self, project: ProjectId) -> AttentionSummary {
        self.project_attention_map()
            .get(&project)
            .copied()
            .unwrap_or_default()
    }

    /// Fold open conversation metadata when attention or filing changes.
    /// Drawing reads counters; it never scans transcripts or saved history.
    pub fn refresh_attention_projection(
        &mut self,
        durable: &Arc<crate::model::WorkspaceState>,
        projects: &[SidebarProject],
    ) {
        if self.attention_projection.as_ref().is_some_and(|cache| {
            !cache.dirty
                && cache
                    .source
                    .upgrade()
                    .is_some_and(|old| Arc::ptr_eq(&old, durable))
                && cache.inventory.len() == projects.len()
                && cache
                    .inventory
                    .iter()
                    .zip(projects)
                    .all(|((id, roots), project)| {
                        *id == project.id
                            && cache.folders.contains(id) == (project.kind == ProjectKind::Folder)
                            && roots
                                .iter()
                                .map(|(id, path)| (*id, path.as_path()))
                                .eq(attention_roots(project))
                    })
        }) {
            return;
        }
        let inventory: Vec<_> = projects
            .iter()
            .map(|project| {
                (
                    project.id,
                    attention_roots(project)
                        .map(|(id, path)| (id, path.to_path_buf()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        #[cfg(test)]
        {
            self.attention_projection_rebuilds += 1;
        }
        let loaded: BTreeSet<_> = projects.iter().map(|project| project.id).collect();
        let loaded_worktrees: BTreeSet<_> = inventory
            .iter()
            .flat_map(|(project, paths)| paths.iter().map(move |(id, _)| (*project, *id)))
            .collect();
        let mut roots = BTreeMap::new();
        for (project, paths) in &inventory {
            for (worktree, path) in paths {
                let owner = (*project, *worktree);
                roots
                    .entry(path.as_path())
                    .and_modify(|old| {
                        if *old != Some(owner) {
                            *old = None;
                        }
                    })
                    .or_insert(Some(owner));
            }
        }
        // A pane row kept after its read has no attention left to say where
        // it is, and a pane with no recorded scope has nothing else; it stays
        // where it was listed.
        let kept: BTreeMap<CardId, (ProjectId, Option<WorktreeId>)> = self
            .projected_cards()
            .iter()
            .filter(|card| {
                self.retained_attention
                    .contains_key(&SelectionRow::Card(card.card))
            })
            .map(|card| (card.card, (card.project, card.worktree)))
            .collect();
        let mut channels = BTreeMap::new();
        for channel in &durable.channels {
            for card in &channel.cards {
                let owner = (channel.project_id, channel.id);
                channels
                    .entry(*card)
                    .and_modify(|old| {
                        if *old != Some(owner) {
                            *old = None;
                        }
                    })
                    .or_insert(Some(owner));
            }
        }
        // Shared native/console recipes can have one logical Session UUID.
        // Count it once, and refuse conflicting project attribution.
        let mut conversations: BTreeMap<
            _,
            Option<(ProjectId, Option<WorktreeId>, AttentionSummary)>,
        > = BTreeMap::new();
        let mut card_resources = BTreeMap::new();
        // Each conversation's first open chat or terminal card outside every
        // channel, by workspace position, with whether any of its cards is in
        // a channel: the channel's row speaks for those.
        let mut panes: Vec<(usize, _, CardId, bool)> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut channeled = BTreeSet::new();
        // Cards with nothing to show, which still lead to their
        // conversation's row when it has one: focusing one draws that row
        // as current.
        let mut quiet = Vec::new();
        // Conversations with news that no loaded project could own. A quiet
        // card of one must not give it a row the news could not.
        let mut unowned = BTreeSet::new();
        for (position, card) in durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .enumerate()
        {
            let resource = match &card.kind {
                crate::model::CardKind::Chat(session) => (0, session.id.as_uuid()),
                crate::model::CardKind::Terminal(session) => (0, session.id.as_uuid()),
                _ => (1, card.id.as_uuid()),
            };
            let summary = self.card_attention(card.id);
            // Inventory-only draft aliases carry no attention evidence. They
            // cannot veto the canonical card selected by journal attribution.
            // A pane row kept after its read has none either, and stays.
            if summary == AttentionSummary::default()
                && !self
                    .retained_attention
                    .contains_key(&SelectionRow::Card(card.id))
            {
                quiet.push((position, card, resource));
                continue;
            }
            let owner = if let Some(channel) = channels.get(&card.id) {
                channel.map(|owner| owner.0)
            } else if let Some(scope) = durable.card_scopes.get(&card.id) {
                roots
                    .get(scope.as_path())
                    .copied()
                    .flatten()
                    .map(|owner| owner.0)
            } else {
                self.attention_owner(card.id)
                    .map(|owner| owner.0)
                    .or_else(|| kept.get(&card.id).map(|(project, _)| *project))
            };
            let Some(owner) = owner.filter(|owner| loaded.contains(owner)) else {
                unowned.insert(resource);
                continue;
            };
            // A channel's pane is opened, read and cleared from its channel
            // row; no worktree row shows it, even though it keeps the scope of
            // the checkout its agent runs in. Counting it on that worktree
            // left a bell there that nothing under it could clear.
            let worktree = if channels.contains_key(&card.id) {
                None
            } else if let Some(scope) = durable.card_scopes.get(&card.id) {
                roots.get(scope.as_path()).copied().flatten()
            } else {
                self.attention_owner(card.id).or_else(|| {
                    kept.get(&card.id)
                        .and_then(|(project, worktree)| Some((*project, (*worktree)?)))
                })
            }
            .filter(|(project, _)| *project == owner)
            .filter(|owner| loaded_worktrees.contains(owner))
            .map(|(_, worktree)| worktree);
            card_resources.insert(card.id, resource);
            if channels.contains_key(&card.id) {
                channeled.insert(resource);
            } else if let crate::model::CardKind::Chat(_) | crate::model::CardKind::Terminal(_) =
                &card.kind
                && seen.insert(resource)
            {
                panes.push((
                    position,
                    resource,
                    card.id,
                    matches!(card.kind, crate::model::CardKind::Terminal(_)),
                ));
            }
            conversations
                .entry(resource)
                .and_modify(|old| {
                    if let Some((project, current_worktree, current)) = old {
                        if *project != owner {
                            *old = None;
                        } else {
                            if *current_worktree != worktree {
                                *current_worktree = None;
                            }
                            current.union(summary);
                        }
                    }
                })
                .or_insert(Some((owner, worktree, summary)));
        }
        // A conversation none of whose cards has anything to show still has
        // a pane, which the list names while its checkout is hidden. Only
        // the checkout its cards are open in can place it: the recorded
        // scope, or else the one the workspace files the launch directory
        // under, as for a plain terminal opened in a checkout.
        let mut idle: BTreeMap<_, Option<(ProjectId, Option<WorktreeId>)>> = BTreeMap::new();
        for &(position, card, resource) in &quiet {
            if conversations.contains_key(&resource) || unowned.contains(&resource) {
                continue;
            }
            if channels.contains_key(&card.id) {
                channeled.insert(resource);
                continue;
            }
            if !matches!(
                card.kind,
                crate::model::CardKind::Chat(_) | crate::model::CardKind::Terminal(_)
            ) {
                continue;
            }
            let placed = match durable.card_scopes.get(&card.id) {
                Some(scope) => roots.get(scope.as_path()).copied().flatten(),
                None => self.pane_checkouts.get(&card.id).copied(),
            };
            let Some((owner, worktree)) = placed.filter(|owner| loaded_worktrees.contains(owner))
            else {
                continue;
            };
            idle.entry(resource)
                .and_modify(|old| {
                    if let Some((project, current)) = old {
                        if *project != owner {
                            *old = None;
                        } else if *current != Some(worktree) {
                            *current = None;
                        }
                    }
                })
                .or_insert(Some((owner, Some(worktree))));
            if seen.insert(resource) {
                panes.push((
                    position,
                    resource,
                    card.id,
                    matches!(card.kind, crate::model::CardKind::Terminal(_)),
                ));
            }
        }
        panes.sort_by_key(|(position, ..)| *position);
        let mut cards = Vec::new();
        let mut rows = BTreeMap::new();
        for (_, resource, card, terminal) in panes {
            if channeled.contains(&resource) {
                continue;
            }
            let Some((project, worktree, summary)) =
                conversations.get(&resource).copied().flatten().or_else(|| {
                    let (project, worktree) = idle.get(&resource).copied().flatten()?;
                    Some((project, worktree, AttentionSummary::default()))
                })
            else {
                continue;
            };
            rows.insert(resource, card);
            cards.push(ProjectedCard {
                card,
                project,
                worktree,
                terminal,
                summary,
            });
        }
        let card_rows = card_resources
            .iter()
            .map(|(card, resource)| (*card, *resource))
            .chain(quiet.iter().map(|(_, card, resource)| (card.id, *resource)))
            .filter_map(|(card, resource)| Some((card, *rows.get(&resource)?)))
            .collect();
        let owners = card_resources
            .into_iter()
            .map(|(card, resource)| {
                let owner = conversations.get(&resource).copied().flatten().and_then(
                    |(project, worktree, _)| worktree.map(|worktree| (project, worktree)),
                );
                (card, owner)
            })
            .collect();
        let mut summaries: BTreeMap<ProjectId, AttentionSummary> = BTreeMap::new();
        let mut worktrees: BTreeMap<(ProjectId, WorktreeId), AttentionSummary> = BTreeMap::new();
        for (owner, worktree, summary) in conversations.into_values().flatten() {
            summaries.entry(owner).or_default().add(summary);
            if let Some(worktree) = worktree {
                worktrees.entry((owner, worktree)).or_default().add(summary);
            }
        }
        self.attention_projection = Some(AttentionProjection {
            dirty: false,
            owners,
            cards,
            card_rows,
            source: Arc::downgrade(durable),
            inventory,
            folders: projects
                .iter()
                .filter_map(|project| (project.kind == ProjectKind::Folder).then_some(project.id))
                .collect(),
            projects: summaries,
            worktrees,
        });
        self.follow_projected_card();
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

    pub fn in_progress_collapsed(&self) -> bool {
        self.in_progress_collapsed
    }

    /// Toggles In progress for a press the application handles, and says
    /// whether that press asks to fold it.
    ///
    /// Folding takes the rows out of the tree, so the fold waits until a
    /// probe of the tree drawn now says whether one of them has keyboard
    /// focus; [`Self::fold_in_progress`] then lands it. Unfolding is
    /// immediate. Each press toggles what the section will be, a fold still
    /// waiting included, so two quick presses leave it as it was however
    /// their probes interleave.
    pub fn toggle_in_progress(&mut self) -> bool {
        if self.in_progress_collapsed {
            self.in_progress_collapsed = false;
        } else {
            self.in_progress_fold_pending = !self.in_progress_fold_pending;
        }
        self.in_progress_fold_pending
    }

    /// Lands the fold [`Self::toggle_in_progress`] asked for, and says
    /// whether there was one: a later press may have taken it back.
    pub fn fold_in_progress(&mut self) -> bool {
        let pending = std::mem::take(&mut self.in_progress_fold_pending);
        self.in_progress_collapsed |= pending;
        pending
    }

    /// Unfolds In progress, so a row in it can be shown, and drops any fold
    /// still waiting on its probe.
    pub fn unfold_in_progress(&mut self) {
        self.in_progress_collapsed = false;
        self.in_progress_fold_pending = false;
    }

    /// The worktree the source-control panel is bound to.
    pub fn active_worktree(&self) -> Option<&ActiveWorktreeKey> {
        self.active_worktree.as_ref()
    }

    /// Binds the source-control and file panels to the worktree at `path`
    /// rather than the one focus names, or follows focus again with `None`.
    /// The list's selection keeps following focus either way. Takes effect at
    /// the next [`Self::sync_active_worktree`].
    pub fn pin_panel_worktree(&mut self, path: Option<PathBuf>) {
        self.panel_pin = path;
    }

    /// The row the list draws as current: the worktree focus names,
    /// otherwise the user's selection.
    fn drawn_selection(&self) -> Option<SidebarSelection> {
        self.followed_worktree
            .as_ref()
            .map(|key| SidebarSelection::Worktree {
                project_id: key.project_id,
                worktree_id: key.worktree_id,
            })
            .or(self.selection)
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
    ///
    /// A pinned worktree binds the panel instead while it still exists; the
    /// list's selection and disclosure follow focus regardless, so choosing a
    /// worktree for the panel never moves the list.
    pub fn sync_active_worktree(&mut self, model: &SidebarModel, focus: &SidebarFocus) -> bool {
        let followed = match focus.path().and_then(|path| model.worktree_at(path)) {
            Some(key) => Some(key),
            None => self
                .followed_worktree
                .clone()
                .filter(|key| model.contains_worktree(key)),
        };
        self.selection = followed.as_ref().map(|key| {
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
        if !self.channel_selected
            && self.followed_worktree != followed
            && let Some(key) = &followed
            && model.project(key.project_id).is_some_and(|project| {
                project.worktrees.iter().any(|worktree| {
                    worktree.id == key.worktree_id
                        && !is_primary_worktree(&project.repository, &worktree.path)
                })
            })
        {
            self.expanded_worktree_groups.insert(key.project_id);
            self.collapsed_projects.remove(&key.project_id);
        }
        let pinned = self
            .panel_pin
            .as_deref()
            .and_then(|path| model.worktree_at(path));
        let next = pinned.or_else(|| followed.clone());
        self.followed_worktree = followed;
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
            SidebarAction::SetFilter(filter) => self.set_filter(*filter),
            SidebarAction::HideReadAttention => self.retained_attention.clear(),
            SidebarAction::ToggleInProgress => {
                self.in_progress_collapsed = !self.in_progress_collapsed;
                self.in_progress_fold_pending = false;
            }
            SidebarAction::ToggleControls => {
                self.controls_expanded = !self.controls_expanded;
                if !self.controls_expanded {
                    self.clear_search_filters();
                }
            }
            SidebarAction::ToggleWorktrees(project_id) => {
                toggle_membership(&mut self.expanded_worktree_groups, *project_id);
            }
            SidebarAction::SetWorktreeDisplay { project_id, mode } => {
                if let Some(mode) = mode {
                    self.worktree_display_overrides.insert(*project_id, *mode);
                } else {
                    self.worktree_display_overrides.remove(project_id);
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
            SidebarAction::ToggleCheckouts
            | SidebarAction::OpenRepositoryPicker
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
            | SidebarAction::OpenCard { .. }
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
            | SidebarAction::OpenWork
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
    /// Drops the rows the Attention list keeps only because they were dealt
    /// with there.
    HideReadAttention,
    /// Folds or unfolds the Attention list's "In progress" section.
    ToggleInProgress,
    ToggleControls,
    ToggleProject(ProjectId),
    ToggleWorktrees(ProjectId),
    SetWorktreeDisplay {
        project_id: ProjectId,
        mode: Option<crate::settings::SidebarWorktrees>,
    },
    /// Shows or hides every project's checkouts: switches the default
    /// checkout display between Hidden and the visible mode it last had. The
    /// application owns that setting, so the state applies nothing.
    ToggleCheckouts,
    OpenRepositoryPicker,
    NewChannel {
        project_id: ProjectId,
    },
    OpenChannel {
        channel_id: ChannelId,
    },
    /// Opens the checkout or channel holding a pane, focuses the pane and
    /// reads its update.
    OpenCard {
        card: CardId,
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
    /// Opens the window's Work board.
    OpenWork,

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

/// Hiding read Attention rows removes the control that did it, so focus
/// returns to the filter those rows were kept for, in both densities.
pub fn attention_filter_focus_id() -> Id {
    filter_id(SidebarFilter::Attention)
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
        false,
    )
}

#[cfg(test)]
thread_local! {
    /// How many times the navigation cache compared every project.
    static PROJECT_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
pub struct NavigationCache(std::cell::RefCell<(Option<NavigationSnapshot>, u64)>);

/// Exactly the inputs to `project_list`. File trees, history and commit
/// composers belong to the source-control panel, which is rendered separately.
/// Keeping them out of this snapshot bounds comparisons and copies by the
/// number of navigation entities, even in a very large repository.
struct NavigationSnapshot {
    projects: Vec<SidebarProject>,
    /// The revision `projects` was last seen equal at. Comparing every
    /// project is most of what a frame that changes nothing costs in a large
    /// workspace, so an unchanged revision stands in for it.
    projects_revision: u64,
    editors: Vec<SidebarEditor>,
    rows: selection::Selection,
    query: String,
    filter: SidebarFilter,
    controls_expanded: bool,
    collapsed_projects: BTreeSet<ProjectId>,
    sidebar_worktrees: crate::settings::SidebarWorktrees,
    worktree_display_overrides: BTreeMap<ProjectId, crate::settings::SidebarWorktrees>,
    expanded_worktree_groups: BTreeSet<ProjectId>,
    in_progress_collapsed: bool,
    followed_card: Option<CardId>,
    selection: Option<SidebarSelection>,
    followed_worktree: Option<ActiveWorktreeKey>,
    channel_selected: bool,
    worktree_sort: crate::settings::WorktreeSort,
    hide_worktree_changes_icons: bool,
    agent_updates: BTreeMap<CardId, Option<(ProjectId, WorktreeId)>>,
    retained_attention: BTreeMap<SelectionRow, Listing>,
    failed_updates: BTreeSet<CardId>,
    project_attention: BTreeMap<ProjectId, AttentionSummary>,
    worktree_attention: BTreeMap<(ProjectId, WorktreeId), AttentionSummary>,
    agent_activity: BTreeMap<CardId, PaneActivity>,
    worktrees_with_panes: BTreeSet<(ProjectId, WorktreeId)>,
}

impl NavigationSnapshot {
    fn matches(&self, model: &SidebarModel, state: &SidebarState) -> bool {
        (self.projects_revision == Revisioned::revision(&model.projects) || {
            #[cfg(test)]
            PROJECT_COMPARISONS.with(|count| count.set(count.get() + 1));
            self.projects == *model.projects
        }) && self.editors == model.external_editors
            && self.rows == state.rows
            && self.query == state.query
            && self.filter == state.filter
            && self.controls_expanded == state.controls_expanded
            && self.collapsed_projects == state.collapsed_projects
            && self.sidebar_worktrees == state.sidebar_worktrees
            && self.worktree_display_overrides == state.worktree_display_overrides
            && self.expanded_worktree_groups == state.expanded_worktree_groups
            && self.in_progress_collapsed == state.in_progress_collapsed
            && self.followed_card == state.drawn_followed_card()
            && self.selection == state.selection
            && self.followed_worktree == state.followed_worktree
            && self.channel_selected == state.channel_selected
            && self.worktree_sort == state.worktree_sort
            && self.hide_worktree_changes_icons == state.hide_worktree_changes_icons
            && self.agent_updates == state.agent_updates
            && self.retained_attention == state.retained_attention
            && self.failed_updates == state.failed_updates
            && self.project_attention == *state.project_attention_map()
            && self.worktree_attention == *state.worktree_attention_map()
            && self.agent_activity == state.agent_activity
            && self.worktrees_with_panes == state.worktrees_with_panes
    }

    fn capture(model: &SidebarModel, state: &SidebarState) -> Self {
        Self {
            projects: (*model.projects).clone(),
            projects_revision: Revisioned::revision(&model.projects),
            editors: model.external_editors.clone(),
            rows: state.rows.clone(),
            query: state.query.clone(),
            filter: state.filter,
            controls_expanded: state.controls_expanded,
            collapsed_projects: state.collapsed_projects.clone(),
            sidebar_worktrees: state.sidebar_worktrees,
            worktree_display_overrides: state.worktree_display_overrides.clone(),
            expanded_worktree_groups: state.expanded_worktree_groups.clone(),
            in_progress_collapsed: state.in_progress_collapsed,
            followed_card: state.drawn_followed_card(),
            selection: state.selection,
            followed_worktree: state.followed_worktree.clone(),
            channel_selected: state.channel_selected,
            worktree_sort: state.worktree_sort,
            hide_worktree_changes_icons: state.hide_worktree_changes_icons,
            agent_updates: state.agent_updates.clone(),
            retained_attention: state.retained_attention.clone(),
            failed_updates: state.failed_updates.clone(),
            project_attention: state.project_attention_map().clone(),
            worktree_attention: state.worktree_attention_map().clone(),
            agent_activity: state.agent_activity.clone(),
            worktrees_with_panes: state.worktrees_with_panes.clone(),
        }
    }
}

impl NavigationCache {
    fn generation(&self, model: &SidebarModel, state: &SidebarState) -> u64 {
        let mut snapshot = self.0.borrow_mut();
        match &mut snapshot.0 {
            // A borrow for writing that changed nothing still took a new
            // revision; once the contents prove equal, adopt it, so later
            // frames are spared the comparison again.
            Some(previous) if previous.matches(model, state) => {
                previous.projects_revision = Revisioned::revision(&model.projects);
            }
            _ => {
                snapshot.0 = Some(NavigationSnapshot::capture(model, state));
                snapshot.1 = snapshot.1.wrapping_add(1);
            }
        }
        snapshot.1
    }
}

/// The pinned generic Lazy widget does not forward accessibility nodes. Keep
/// this list's actual element and child Tree together, including named native
/// state, and use that same child for every widget operation.
///
/// An unchanged list costs a frame almost nothing, though it holds every
/// project, channel and worktree row:
///
/// - Its child Tree lives in this widget's own state rather than among its
///   tree children. The runtime's per-frame sweep of named widget states walks
///   every tree child, so it never enters the list; the list reconciles its own
///   named states, among its own widgets, only when it is rebuilt.
/// - Its layout is kept while the dependency and the limits are unchanged and
///   nothing inside has asked for a new one. The window's layout holds only the
///   list's size; every call reaches the rows through the kept layout, placed
///   where the window put the list.
///
/// A named state therefore cannot move between the list and the rest of the
/// window. None needs to: only the list's scrollable is named on both sides,
/// when the last project goes, and an empty list has nothing to scroll.
mod cached_projects {
    use super::SidebarAction;
    use cosmic::{Element, Renderer, Theme, iced};
    use iced::advanced::widget::{Operation, Tree, tree};
    use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
    use iced::{Event, Length, Rectangle, Size, Vector};

    type Dependency = (u64, bool, bool);

    #[cfg(test)]
    thread_local! {
        /// How many times a list was actually laid out, rather than reused.
        pub(super) static LAYOUTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn view<'a>(
        dependency: Dependency,
        build: impl Fn() -> Element<'static, SidebarAction> + 'a,
    ) -> Element<'a, SidebarAction> {
        Element::new(CachedProjects {
            dependency,
            build: Box::new(build),
        })
    }

    /// The rows' own layout, which the window's layout leaves out.
    #[cfg(test)]
    pub(super) fn content_layout(tree: &Tree) -> &layout::Node {
        &tree
            .state
            .downcast_ref::<State>()
            .layout
            .as_ref()
            .unwrap()
            .node
    }

    struct CachedProjects<'a> {
        dependency: Dependency,
        build: Box<dyn Fn() -> Element<'static, SidebarAction> + 'a>,
    }

    struct State {
        dependency: Dependency,
        element: Element<'static, SidebarAction>,
        /// The element's Tree. Kept out of the tree children; see the module.
        tree: Tree,
        /// The rows' layout, until they are rebuilt; the window lays the new
        /// rows out before anything else reaches them.
        layout: Option<Kept>,
        /// The rows asked for a new layout. Until the window lays out again,
        /// they keep using the kept one, as the window keeps using its own.
        stale: bool,
    }

    struct Kept {
        limits: layout::Limits,
        node: layout::Node,
    }

    impl Kept {
        /// The rows' layout, placed where the window laid out the list.
        fn place<'a>(&'a self, list: Layout<'_>) -> Layout<'a> {
            Layout::with_offset(list.position() - self.node.bounds().position(), &self.node)
                .with_virtual_offset(list.virtual_offset())
        }
    }

    impl State {
        /// Reconciles a rebuilt element with the retained Tree, moving named
        /// states by name as the runtime does for the whole window, but only
        /// among this list's own widgets. States of rows that are gone are
        /// dropped with the scope; the window's own extracted states, which
        /// the runtime is still reconciling around this widget, are put back
        /// untouched. The old rows' layout goes with them, before the new rows
        /// are laid out, so the new layout can reuse its memory.
        fn rebuild(
            &mut self,
            dependency: Dependency,
            mut element: Element<'static, SidebarAction>,
        ) {
            use iced::advanced::widget::tree::NAMED;
            let named = self.tree.take_all_named();
            let window = NAMED.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), named));
            self.tree.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut self.tree);
            NAMED.with(|cell| *cell.borrow_mut() = window);
            self.element = element;
            self.dependency = dependency;
            self.layout = None;
        }
    }

    /// Runs `update` and says whether it asked for a new layout, apart from any
    /// request made before it, which is left as it was.
    fn requests_layout(
        shell: &mut Shell<'_, SidebarAction>,
        update: impl FnOnce(&mut Shell<'_, SidebarAction>),
    ) -> bool {
        let mut earlier = false;
        shell.revalidate_layout(|| earlier = true);
        update(shell);
        let requested = shell.is_layout_invalid();
        if earlier {
            shell.invalidate_layout();
        }
        requested
    }

    impl Widget<SidebarAction, Theme, Renderer> for CachedProjects<'_> {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<State>()
        }

        fn state(&self) -> tree::State {
            let element = (self.build)();
            tree::State::new(State {
                dependency: self.dependency,
                tree: Tree::new(&element),
                element,
                layout: None,
                stale: false,
            })
        }

        fn children(&self) -> Vec<Tree> {
            Vec::new()
        }

        fn diff(&mut self, tree: &mut Tree) {
            let state = tree.state.downcast_mut::<State>();
            if state.dependency != self.dependency {
                state.rebuild(self.dependency, (self.build)());
            }
            crate::reorder::diff_retained(&mut state.tree);
        }

        // project_scroll is Fill/Fill in empty, filtered and populated states.
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }

        fn size_hint(&self) -> Size<Length> {
            self.size()
        }

        fn layout(
            &mut self,
            tree: &mut Tree,
            renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            let state = tree.state.downcast_mut::<State>();
            let kept = match &state.layout {
                Some(kept) if !state.stale && kept.limits == *limits => kept,
                _ => {
                    #[cfg(test)]
                    LAYOUTS.with(|layouts| layouts.set(layouts.get() + 1));
                    // Free the stale layout first, as the window frees its own
                    // before laying out again, so the new one reuses it.
                    state.layout = None;
                    let node =
                        state
                            .element
                            .as_widget_mut()
                            .layout(&mut state.tree, renderer, limits);
                    state.stale = false;
                    state.layout.insert(Kept {
                        limits: *limits,
                        node,
                    })
                }
            };
            layout::Node::new(kept.node.size())
        }

        fn operate(
            &mut self,
            tree: &mut Tree,
            layout: Layout<'_>,
            renderer: &Renderer,
            operation: &mut dyn Operation,
        ) {
            let state = tree.state.downcast_mut::<State>();
            let Some(kept) = &state.layout else {
                return;
            };
            state.element.as_widget_mut().operate(
                &mut state.tree,
                kept.place(layout),
                renderer,
                operation,
            );
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
            let State {
                element,
                tree: child,
                layout: Some(kept),
                stale,
                ..
            } = tree.state.downcast_mut::<State>()
            else {
                return;
            };
            // A row may ask for a new layout, such as a hovered row making
            // room for its actions.
            *stale |= requests_layout(shell, |shell| {
                element.as_widget_mut().update(
                    child,
                    event,
                    kept.place(layout),
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                );
            });
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
            let Some(kept) = &state.layout else {
                return mouse::Interaction::None;
            };
            state.element.as_widget().mouse_interaction(
                &state.tree,
                kept.place(layout),
                cursor,
                viewport,
                renderer,
            )
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
            let Some(kept) = &state.layout else {
                return;
            };
            state.element.as_widget().draw(
                &state.tree,
                renderer,
                theme,
                style,
                kept.place(layout),
                cursor,
                viewport,
            );
        }

        fn a11y_nodes(
            &self,
            layout: Layout<'_>,
            tree: &Tree,
            cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            let state = tree.state.downcast_ref::<State>();
            let Some(kept) = &state.layout else {
                return iced_accessibility::A11yTree::default();
            };
            state
                .element
                .as_widget()
                .a11y_nodes(kept.place(layout), &state.tree, cursor)
        }

        fn drag_destinations(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            renderer: &Renderer,
            destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
        ) {
            let state = tree.state.downcast_ref::<State>();
            let Some(kept) = &state.layout else {
                return;
            };
            state.element.as_widget().drag_destinations(
                &state.tree,
                kept.place(layout),
                renderer,
                destinations,
            );
        }

        fn overlay<'a>(
            &'a mut self,
            tree: &'a mut Tree,
            layout: Layout<'a>,
            renderer: &Renderer,
            viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'a, SidebarAction, Theme, Renderer>> {
            let State {
                element,
                tree: child,
                layout: Some(kept),
                stale,
                ..
            } = tree.state.downcast_mut::<State>()
            else {
                return None;
            };
            element
                .as_widget_mut()
                .overlay(child, kept.place(layout), renderer, viewport, translation)
                .map(|content| Watched::element(content, stale))
        }
    }

    /// A row's overlay, such as its menu or tooltip, handles events apart from
    /// the list's `update`. When it asks for a new layout, the rows may have
    /// changed with it, so the list's kept layout is stale too.
    struct Watched<'a> {
        content: overlay::Element<'a, SidebarAction, Theme, Renderer>,
        stale: &'a mut bool,
    }

    impl<'a> Watched<'a> {
        fn element(
            content: overlay::Element<'a, SidebarAction, Theme, Renderer>,
            stale: &'a mut bool,
        ) -> overlay::Element<'a, SidebarAction, Theme, Renderer> {
            overlay::Element::new(Box::new(Self { content, stale }))
        }
    }

    impl overlay::Overlay<SidebarAction, Theme, Renderer> for Watched<'_> {
        fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
            self.content.as_overlay_mut().layout(renderer, bounds)
        }

        fn draw(
            &self,
            renderer: &mut Renderer,
            theme: &Theme,
            style: &renderer::Style,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
        ) {
            self.content
                .as_overlay()
                .draw(renderer, theme, style, layout, cursor);
        }

        fn operate(
            &mut self,
            layout: Layout<'_>,
            renderer: &Renderer,
            operation: &mut dyn Operation,
        ) {
            self.content
                .as_overlay_mut()
                .operate(layout, renderer, operation);
        }

        fn update(
            &mut self,
            event: &Event,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            renderer: &Renderer,
            clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, SidebarAction>,
        ) {
            let content = &mut self.content;
            *self.stale |= requests_layout(shell, |shell| {
                content
                    .as_overlay_mut()
                    .update(event, layout, cursor, renderer, clipboard, shell);
            });
        }

        fn mouse_interaction(
            &self,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            renderer: &Renderer,
        ) -> mouse::Interaction {
            self.content
                .as_overlay()
                .mouse_interaction(layout, cursor, renderer)
        }

        fn overlay<'b>(
            &'b mut self,
            layout: Layout<'b>,
            renderer: &Renderer,
        ) -> Option<overlay::Element<'b, SidebarAction, Theme, Renderer>> {
            let stale = &mut *self.stale;
            self.content
                .as_overlay_mut()
                .overlay(layout, renderer)
                .map(|content| Watched::element(content, stale))
        }

        fn index(&self) -> f32 {
            self.content.as_overlay().index()
        }
    }
}

/// `window_rounded` says the window draws rounded corners, as a floating
/// window does and a maximized or tiled one does not: the pane's top-left
/// corner is the window's, and follows it.
#[allow(clippy::too_many_arguments)]
pub fn view_cached<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    mode: SidebarMode,
    persisted_width: f32,
    modal_open: bool,
    cache: &'a NavigationCache,
    shortcuts: &crate::settings::KeyboardShortcuts,
    window_rounded: bool,
) -> Element<'a, SidebarAction> {
    view_inner(
        model,
        state,
        mode,
        persisted_width,
        modal_open,
        Some(cache),
        shortcuts,
        window_rounded,
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
    window_rounded: bool,
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
                cached_projects::view(
                    (cache.generation(model, state), compact, modal_open),
                    move || project_list(model, state, compact, modal_open),
                )
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
    .class(design::docked_primary(window_rounded))
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
    let mut actions: Vec<Element<'a, SidebarAction>> = Vec::with_capacity(5);
    if !model.projects.is_empty() {
        let attention = state.filter == SidebarFilter::Attention;
        actions.push(crate::interaction::tooltip(
            crate::interaction::pressed(
                centered_icon_button(Icon::Attention, ICON_CONTROL_SIZE, ICON_CONTROL_SIZE)
                    .class(sidebar_button_style(attention))
                    .id(filter_id(SidebarFilter::Attention))
                    .name("Show projects needing attention")
                    .selected(attention)
                    .on_press_maybe(gated(
                        SidebarAction::SetFilter(if attention {
                            SidebarFilter::All
                        } else {
                            SidebarFilter::Attention
                        }),
                        modal_open,
                    )),
                attention,
            ),
            if attention {
                "Show all projects"
            } else {
                "Show projects needing attention"
            },
        ));
        actions.push(checkouts_toggle(state, modal_open));
        let selected = search_toggle_lit(state);
        let search_button = crate::interaction::tooltip(
            crate::interaction::pressed(
                centered_icon_button(Icon::Search, ICON_CONTROL_SIZE, ICON_CONTROL_SIZE)
                    .class(sidebar_button_style(selected))
                    .id(search_toggle_focus_id())
                    .name("Search projects")
                    .selected(selected)
                    .on_press_maybe(gated(SidebarAction::ToggleControls, modal_open)),
                selected,
            ),
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

pub fn checkouts_toggle_focus_id() -> Id {
    stable_id("sidebar.checkouts")
}

/// The checkouts toggle's accessible name, whatever its state.
const CHECKOUTS_TOGGLE_NAME: &str = "Hide checkouts";

/// Shows or hides every project's checkouts. Like the filters beside it, it
/// is lit while it narrows the list, so a list without checkouts never
/// passes for the whole one. Its name stays fixed and its pressed state says
/// whether checkouts are hidden, as the bell's does for its filter; the
/// tooltip says what a press does. Hiding them by default overrides each
/// project's own mode, which the hidden tooltip says.
///
/// Only the All view lists checkouts: Attention names panes instead and Pull
/// requests always shows them. The setting persists, so the toggle stays
/// live elsewhere and its tooltip names the view it changes.
fn checkouts_toggle(state: &SidebarState, modal_open: bool) -> Element<'static, SidebarAction> {
    let hidden = !state.sidebar_worktrees.shows_checkouts();
    crate::interaction::tooltip(
        crate::interaction::pressed(
            centered_icon_handle_button(worktree_icon(), ICON_CONTROL_SIZE, ICON_CONTROL_SIZE)
                .class(sidebar_button_style(hidden))
                .id(checkouts_toggle_focus_id())
                .name(CHECKOUTS_TOGGLE_NAME)
                .selected(hidden)
                .on_press_maybe(gated(SidebarAction::ToggleCheckouts, modal_open)),
            hidden,
        ),
        checkouts_toggle_tooltip(state),
    )
}

/// What pressing the checkouts toggle does, in the view it changes.
fn checkouts_toggle_tooltip(state: &SidebarState) -> &'static str {
    let hidden = !state.sidebar_worktrees.shows_checkouts();
    match (hidden, state.filter == SidebarFilter::All) {
        (false, true) => "Hide checkouts",
        (true, true) => "Show checkouts (hidden in every project)",
        (false, false) => "Hide checkouts in the All view",
        (true, false) => "Show checkouts in the All view (hidden in every project)",
    }
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
        .followed_worktree
        .as_ref()
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
    let [settings, automations, work] = utility_controls(modal_open, Some(shortcuts));
    if compact {
        let children: Vec<Element<'a, SidebarAction>> = vec![toggle, settings, automations, work];
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
            work,
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
) -> [Element<'a, SidebarAction>; 3] {
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
    let work = crate::ui_controls::icon_button(Icon::Board, "Work board")
        .on_press_maybe(gated(SidebarAction::OpenWork, modal_open))
        .id(stable_id("work"))
        .into();
    [settings, automations, work]
}

// The standalone editor still mounts its utility controls below its file pane.
fn footer<'a>(compact: bool, modal_open: bool) -> Element<'a, SidebarAction> {
    let [settings, automations, work] = utility_controls(modal_open, None);
    if compact {
        widget::column::with_children(vec![work, automations, settings])
            .align_x(Alignment::Center)
            .spacing(space::XS)
            .padding(space::XS)
            .width(Length::Fill)
            .into()
    } else {
        widget::row::with_children(vec![work, automations, settings])
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
    let in_progress = in_progress_rows(model, state, compact);
    let in_progress = (!in_progress.is_empty())
        .then(|| in_progress_section(&in_progress, state, compact, modal_open));
    let sticky_viewport = row_interaction::StickyViewport::default();
    if projects.is_empty() {
        let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
        return project_scroll(
            match in_progress {
                Some(in_progress) => {
                    let in_progress = widget::container(in_progress)
                        .padding(RowGrid::list_padding(compact))
                        .width(Length::Fill)
                        .into();
                    if searching {
                        // The search matched, just nothing that needs the user:
                        // "No matches" above the matches would contradict them.
                        in_progress
                    } else {
                        // Nothing needs the user, though agents are still at work.
                        widget::column::with_children(vec![
                            empty(state, compact, modal_open),
                            in_progress,
                        ])
                        .into()
                    }
                }
                None => empty(state, compact, modal_open),
            },
            state.filter,
            sticky_viewport,
        );
    }

    // Search results are revealed while their input is available, including
    // the compact popover. Otherwise preserve explicit project collapses.
    let searching = (!compact || state.controls_expanded) && !state.query.trim().is_empty();
    let selection = state.drawn_selection();
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

    let mut column = RowInteraction::projects(
        column.into(),
        projects.iter().map(|project| project.project.id).collect(),
        !modal_open && !compact,
        sticky_viewport.clone(),
    );
    if lists_kept_attention_rows(&projects) {
        column =
            widget::column::with_children(vec![column, hide_read_attention(compact, modal_open)])
                .spacing(space::XS)
                .into();
    }
    if let Some(in_progress) = in_progress {
        column = widget::column::with_children(vec![column, in_progress])
            .spacing(if compact { space::XS } else { space::S })
            .into();
    }
    project_scroll(
        widget::container(column)
            .padding(RowGrid::list_padding(compact))
            .width(Length::Fill)
            .into(),
        state.filter,
        sticky_viewport,
    )
}

/// Whether the Attention list holds a row only because it was dealt with
/// there and has not needed attention since.
fn lists_kept_attention_rows(projects: &[FilteredProject<'_>]) -> bool {
    projects.iter().any(|project| {
        project
            .channels
            .iter()
            .any(|channel| project.state.channel_listing(channel) != Listing::Live)
            || project
                .cards
                .iter()
                .any(|card| project.state.card_listing(card) != Listing::Live)
    })
}

/// The Attention list keeps rows the user dealt with where they were, so
/// working down it never moves it. This is how the user asks for the list
/// without them. It closes the list rather than heading it: appearing above
/// the rows on the first read would push every row down, the very move the
/// kept rows avoid. It names no count: a search or a collapsed project can
/// hide kept rows that hiding still drops. Its name extends the visible
/// "Hide read" to say it drops handled rows too, keeping the label within it.
fn hide_read_attention(compact: bool, modal_open: bool) -> Element<'static, SidebarAction> {
    let action = gated(SidebarAction::HideReadAttention, modal_open);
    let id = stable_id("attention.hide_read");
    if compact {
        return icon_action(Icon::Hide, "Hide read and handled rows", action, id);
    }
    widget::container(
        crate::ui_controls::button::quiet("Hide read")
            .name("Hide read and handled rows")
            .on_press_maybe(action)
            .id(id),
    )
    .padding([space::XS, space::M])
    .width(Length::Fill)
    .into()
}

fn project_scroll(
    content: Element<'_, SidebarAction>,
    filter: SidebarFilter,
    viewport: row_interaction::StickyViewport,
) -> Element<'_, SidebarAction> {
    // Rows passing either edge fade into the pane, but while a project
    // header is pinned the top edge is the header's, which fades what passes
    // under it. The fade wraps the filtered scroll, whose direct child must
    // stay the scrollable: it swaps that child's state between filters.
    crate::interaction::overflow_fade_y_under_headers(
        RowInteraction::filtered_scroll(
            widget::scrollable(content)
                .id(stable_id("projects.scroll"))
                .direction(sidebar_scroll_direction())
                .class(cosmic::style::iced::Scrollable::Minimal)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            filter,
            viewport,
        ),
        pane_fill,
    )
}

/// The fill of a pinned project header, which hides the rows passing under
/// it: opaque even when the pane itself is translucent.
fn pinned_header_fill(theme: &cosmic::Theme) -> cosmic::iced::Color {
    cosmic::iced::Color::from(theme.cosmic().primary(false).base)
}

/// The pane's own fill, as [`crate::design::docked_primary`] draws it, which
/// rows passing an edge of the list fade into.
fn pane_fill(theme: &cosmic::Theme) -> cosmic::iced::Color {
    cosmic::iced::Color::from(theme.cosmic().primary(theme.transparent).base)
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
    /// The rail renders one centred glyph per row, with its status pinned to
    /// the glyph's corners; see [`rail`].
    compact: bool,
    /// The row's hover reveals a delete control over its end; see
    /// [`ROW_DELETE_STATUS_WIDTH`].
    deletable: bool,
}

/// One row's content while its slots are being filled.
type RowContent<'a> = widget::Row<'a, SidebarAction, cosmic::Theme, cosmic::Renderer>;

impl RowGrid {
    const fn project(compact: bool) -> Self {
        Self {
            kind: RowKind::Project,
            compact,
            deletable: false,
        }
    }

    const fn child(compact: bool) -> Self {
        Self {
            kind: RowKind::Child,
            compact,
            deletable: false,
        }
    }

    /// The same grid for a row whose end a hover-revealed delete covers.
    const fn deletable(self) -> Self {
        Self {
            deletable: true,
            ..self
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
    /// compact rail mirrors the scrollbar's rail on the left, which leaves
    /// exactly [`rail::CELL_WIDTH`] centred in the pane.
    fn list_padding(compact: bool) -> Padding {
        design::with_vertical_rail(if compact {
            Padding {
                top: ROW_GUTTER,
                right: 0.0,
                bottom: ROW_GUTTER,
                left: design::SCROLLBAR_GUTTER,
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
            let status = widget::container(slot(status))
                .center_x(Length::Fixed(ROW_STATUS_WIDTH))
                .center_y(Length::Fill);
            content = content.push(if self.deletable {
                widget::container(status)
                    .align_right(Length::Fixed(ROW_DELETE_STATUS_WIDTH))
                    .center_y(Length::Fill)
            } else {
                status
            });
        }
        content
    }

    /// A rail row's content: its glyph and corner badges, and nothing else.
    fn rail<'a>(
        self,
        glyph: impl Into<Element<'a, SidebarAction>>,
        badges: rail::Badges,
    ) -> RowContent<'a> {
        debug_assert!(self.compact, "only the rail draws rail cells");
        widget::row::with_capacity(1)
            .height(Length::Fill)
            .push(rail::cell(glyph, badges))
    }

    /// The button a row is: one height, one inset, one width rule.
    fn control<'a>(
        self,
        content: RowContent<'a>,
        selected: bool,
    ) -> button::Button<'a, SidebarAction> {
        if self.compact {
            return rail::control(content.into(), selected);
        }
        button::custom(content)
            .class(match self.kind {
                RowKind::Project => row_button_style::<false>(selected),
                RowKind::Child => row_button_style::<true>(selected),
            })
            .selected(selected)
            .padding([0.0, self.gutter()])
            .width(Length::Fill)
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
fn project_identity_row(
    project: &SidebarProject,
    selected: bool,
    modal_open: bool,
    compact: bool,
    review_mode: bool,
    query: &str,
    activity: Option<crate::agent::ActivityState>,
) -> Element<'static, SidebarAction> {
    let chrome = &project.chrome;
    let grid = RowGrid::project(compact);
    let label = (!compact).then(|| project_name_label(project, query));
    let mut name = if review_mode && project.github_ready {
        format!("Browse pull requests in {}", project.name)
    } else if compact {
        chrome.compact_open_name.to_string()
    } else {
        chrome.open_name.to_string()
    };
    match activity {
        Some(crate::agent::ActivityState::Waiting) => name.push_str(", agent needs input"),
        Some(crate::agent::ActivityState::Working) => name.push_str(", agent working"),
        _ => {}
    }
    let control = grid
        .control(
            grid.content(project_icon(project, ROW_ICON_SIZE), label, None),
            selected,
        )
        .id(chrome.open_id.clone())
        .name(name)
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
        theme::Text::default(),
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
fn operation_rows(
    project: &SidebarProject,
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
            operation_icon(operation.phase, CHILD_ROW_ICON_SIZE),
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
fn compact_operation_rows(
    project: &SidebarProject,
    modal_open: bool,
) -> impl Iterator<Item = Element<'static, SidebarAction>> {
    let grid = RowGrid::child(true);
    project.operations.iter().map(move |operation| {
        let control = grid
            .control(
                grid.rail(
                    operation_icon(operation.phase, rail::CHILD_GLYPH),
                    match operation.phase {
                        WorktreeOperationPhase::Working => rail::Badges::busy(),
                        WorktreeOperationPhase::Failed => rail::Badges::default(),
                    },
                ),
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
/// creation runs; a warning once it has failed. The spinner is the status
/// slot's in a row, and a corner badge's in the rail.
fn operation_icon(phase: WorktreeOperationPhase, size: u16) -> Element<'static, SidebarAction> {
    match phase {
        WorktreeOperationPhase::Working => worktree_glyph_muted(size).into(),
        WorktreeOperationPhase::Failed => toned_icon_sized(Icon::Warning, BadgeTone::Warning, size),
    }
}

fn operation_status(phase: WorktreeOperationPhase) -> Option<Element<'static, SidebarAction>> {
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

fn operation_control(
    operation: &WorktreeOperationRow,
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
    // The rail keeps the project's own chip, because which project could not
    // be read is the first thing to know, and marks it with the warning.
    let content = if compact {
        let (initials, color) = project_identity(&project.name);
        grid.rail(
            rail::project_chip(project.avatar.as_ref(), initials, color, true),
            rail::Badges::warning(),
        )
    } else {
        grid.content(
            toned_icon(Icon::Warning, BadgeTone::Warning),
            Some(project_name_label(project, &state.query)),
            None,
        )
    };
    let control = grid
        .control(content, false)
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
        return rail::group(column.into(), !project.operations.is_empty());
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

fn worktree_group_focus_id(project: ProjectId) -> widget::Id {
    entity_id("worktrees.toggle", project)
}

fn worktree_group_row(
    project: &SidebarProject,
    state: &SidebarState,
    compact: bool,
    modal_open: bool,
) -> Element<'static, SidebarAction> {
    let expanded = state.worktree_group_expanded(project);
    let count = project
        .worktrees
        .iter()
        .filter(|worktree| !is_primary_worktree(&project.repository, &worktree.path))
        .count();
    let attention = if expanded {
        AttentionSummary::default()
    } else {
        state.grouped_attention(project)
    };
    let name = format!(
        "{} Worktrees ({count}){}",
        if expanded { "Collapse" } else { "Expand" },
        if attention.state().is_some() {
            format!(", {}", attention.description())
        } else {
            String::new()
        }
    );
    let grid = RowGrid::child(compact);
    let glyph: Element<'static, SidebarAction> = if expanded {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    }
    .handle()
    .icon()
    .size(CHILD_ROW_ICON_SIZE)
    .into();
    let content = if compact {
        grid.rail(glyph, rail::Badges::attention(attention))
    } else {
        let label = widget::row::with_children(vec![
            design::body("Worktrees").width(Length::Fill).into(),
            design::caption(count.to_string()).into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .width(Length::Fill);
        let status = attention_indicator(attention).map(|indicator| {
            widget::container(indicator)
                .id(entity_id("worktrees.attention", project.id))
                .into()
        });
        grid.content(glyph, Some(label.into()), status)
    };
    let control = grid
        .control(content, false)
        .id(worktree_group_focus_id(project.id))
        .name(name.clone())
        .on_press_maybe(gated(
            SidebarAction::ToggleWorktrees(project.id),
            modal_open,
        ));
    RowInteraction::worktree_disclosure(
        grid.mount(crate::interaction::tooltip(control, name)),
        project.id,
        !modal_open,
    )
}

/// Whether the project's checkouts sit behind a "Worktrees" disclosure. The
/// Attention list lists no checkout rows, so it has none to fold away.
fn has_worktree_group(project: &SidebarProject, state: &SidebarState) -> bool {
    state.filter != SidebarFilter::Attention
        && state.worktree_display(project.id) == crate::settings::SidebarWorktrees::Grouped
        && project
            .worktrees
            .iter()
            .any(|worktree| !is_primary_worktree(&project.repository, &worktree.path))
}

fn primary_worktree_id(project: &SidebarProject) -> Option<WorktreeId> {
    project
        .worktrees
        .iter()
        .find(|worktree| is_primary_worktree(&project.repository, &worktree.path))
        .map(|worktree| worktree.id)
}

fn member_is_grouped(primary: Option<WorktreeId>, row: SelectionRow) -> bool {
    matches!(row, SelectionRow::Worktree(_, id) if Some(id) != primary)
}

fn ordered_members<T>(
    members: &mut [T],
    project: &SidebarProject,
    state: &SidebarState,
    row: impl Fn(&T) -> SelectionRow,
) {
    if state.query.trim().is_empty() {
        apply_partial_navigation_order(members, &project.navigation_order, |entry| {
            navigation_item(project, row(entry))
        });
    } else {
        let ranks = navigation_search_ranks(project, &fold(state.query.trim()));
        members.sort_by_cached_key(|entry| ranks.get(&row(entry)).copied().unwrap_or(usize::MAX));
    }
    if state.worktree_display(project.id) != crate::settings::SidebarWorktrees::Expanded {
        // Stable partition preserves the relative channel and worktree orders.
        let primary = primary_worktree_id(project);
        members.sort_by_key(|entry| member_is_grouped(primary, row(entry)));
    }
}

fn member_rows(
    members: Vec<(SelectionRow, Element<'static, SidebarAction>)>,
    project: &SidebarProject,
    state: &SidebarState,
    modal_open: bool,
) -> Element<'static, SidebarAction> {
    let ids = members.iter().map(|(row, _)| *row).collect();
    let disabled = project
        .channels
        .iter()
        .filter(|channel| channel.deletion == Some(RowDeletion::Deleting))
        .map(|channel| SelectionRow::Channel(channel.id))
        .chain(
            project
                .worktrees
                .iter()
                .filter(|worktree| worktree.deletion == Some(RowDeletion::Deleting))
                .map(|worktree| SelectionRow::Worktree(project.id, worktree.id)),
        )
        // A pane row is a way to a pane, not a destination with a place in
        // the project's order.
        .chain(project.panes.iter().map(|card| SelectionRow::Card(card.id)))
        .collect();
    let content = widget::column::with_children(members.into_iter().map(|(_, element)| element))
        .width(Length::Fill);
    RowInteraction::members(
        content.into(),
        project.id,
        ids,
        disabled,
        !modal_open && state.query.trim().is_empty(),
    )
}

/// The selection a project header draws. A current channel names the place
/// more exactly than the header, so the header does not light up for it,
/// whether or not the filter draws that channel's row; nor for a current
/// pane whose row the list draws. A checkout whose row is hidden names no
/// place the list draws, so the project stands in for it.
fn header_selection(
    project: &SidebarProject,
    state: &SidebarState,
    selection: Option<SidebarSelection>,
) -> Option<SidebarSelection> {
    let lists_panes = state.lists_panes(project);
    let named_by_row = project.channels.iter().any(|channel| channel.selected)
        || project
            .panes
            .iter()
            .any(|card| state.follows_card(card) && state.draws_pane(project, card));
    match selection.filter(|_| !named_by_row)? {
        SidebarSelection::Worktree { project_id, .. } if lists_panes => {
            Some(SidebarSelection::Project(project_id))
        }
        selection => Some(selection),
    }
}

/// A project's news that none of the channel and pane rows drawn under it
/// shows: news of a pane a search leaves out, or of no pane at all. Folder
/// roots and hidden checkouts need this on their header, or nobody shows it.
fn undrawn_attention<'a>(
    state: &SidebarState,
    project: ProjectId,
    channels: impl Iterator<Item = &'a SidebarChannel>,
    cards: impl Iterator<Item = &'a SidebarCard>,
) -> AttentionSummary {
    let mut drawn = AttentionSummary::default();
    for channel in channels {
        drawn.add(channel.attention);
    }
    for card in cards {
        drawn.add(card.attention);
    }
    state.project_attention(project).without(drawn)
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
    let selection = header_selection(entity, project.state, selection);
    if compact {
        return compact_project(project, expanded, selection, modal_open, sticky_viewport);
    }

    let worktree_count = project.worktrees.len();
    let has_children =
        worktree_count > 0 || !project.channels.is_empty() || !project.cards.is_empty();
    let attention_list = project.state.filter == SidebarFilter::Attention;
    // Collapsed headers keep the aggregate over all children. The Attention
    // list gives every pane and channel it counts a row of its own, so an
    // expanded header there repeats none of it. Otherwise an expanded header
    // says only what none of the rows drawn under it does: a Folder root,
    // which is its own destination with no row, whatever the checkout
    // display, and for hidden checkouts news of a pane a search leaves out
    // or of no pane at all.
    let header_attention = if !expanded {
        project
            .state
            .shown_attention(project.state.project_attention(project_id))
    } else if attention_list {
        AttentionSummary::default()
    } else if project.state.lists_panes(entity) || entity.kind == ProjectKind::Folder {
        undrawn_attention(
            project.state,
            project_id,
            project.channels.iter().copied(),
            project.cards.iter().copied(),
        )
    } else if !project.state.shows_checkouts(project_id) {
        project.state.project_attention(project_id)
    } else {
        AttentionSummary::default()
    };
    let project_identity = project_identity_row(
        entity,
        selection == Some(SidebarSelection::Project(project_id)),
        modal_open,
        false,
        project.state.filter == SidebarFilter::PullRequests,
        &project.state.query,
        if project.state.worktree_display(project_id) == crate::settings::SidebarWorktrees::Hidden
            || project.state.filter == SidebarFilter::Attention
            || (expanded && entity.kind == ProjectKind::Folder)
        {
            header_attention.activity()
        } else {
            None
        },
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
    if let Some(indicator) = attention_indicator(header_attention) {
        identity = identity.push(indicator);
    }
    if !expanded {
        let rows = if project.state.lists_panes(entity) {
            entity.panes.len()
        } else {
            entity.worktrees.len()
        };
        identity = identity.push(design::caption((entity.channels.len() + rows).to_string()));
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
    let header = context_controls_with_display(
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
        Some((
            project.state.sidebar_worktrees,
            project
                .state
                .worktree_display_overrides
                .get(&entity.id)
                .copied(),
        )),
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
                SelectionRow::Channel(channel.id),
                RowInteraction::selectable(
                    channel_view(
                        channel,
                        project.state.channel_listing(channel),
                        project.state.shown_attention(channel.attention),
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
        for &worktree in project
            .worktrees
            .iter()
            .filter(|worktree| project.state.shows_worktree(entity, worktree))
        {
            members.push((
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
                        project.state.worktree_row_state(project_id, worktree.id),
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
        for card in &project.cards {
            members.push((
                SelectionRow::Card(card.id),
                card_view(entity, card, project.state, modal_open, false),
            ));
        }
        ordered_members(&mut members, entity, project.state, |(row, _)| *row);
        if has_worktree_group(entity, project.state) {
            let primary = primary_worktree_id(entity);
            let (grouped, top): (Vec<_>, Vec<_>) = members
                .into_iter()
                .partition(|(row, _)| member_is_grouped(primary, *row));
            column = column.push(member_rows(top, entity, project.state, modal_open));
            column = column.push(worktree_group_row(entity, project.state, false, modal_open));
            column = column.push(member_rows(grouped, entity, project.state, modal_open));
        } else {
            column = column.push(member_rows(members, entity, project.state, modal_open));
        }
    }

    RowInteraction::group(header, column.into(), sticky_viewport)
}

pub(crate) fn is_primary_worktree(repository: &Path, worktree: &Path) -> bool {
    repository == worktree
        || (repository.file_name().is_some_and(|name| name == ".git")
            && repository.parent() == Some(worktree))
}

/// Stable identity independent of process hashes, theme, load order, and IDs.
fn project_identity(name: &str) -> (String, cosmic::iced::Color) {
    let letters: Vec<_> = name.chars().filter(|ch| ch.is_alphanumeric()).collect();
    let initials = match (letters.first(), letters.last()) {
        (Some(first), Some(last)) if letters.len() > 1 => format!("{first}{last}"),
        (Some(first), _) => first.to_string(),
        _ => "?".to_owned(),
    }
    .to_uppercase();
    let hash = name.bytes().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    });
    // Saturated, moderately dark colours support white text in either theme.
    let hue = (hash % 360) as f32 / 60.0;
    let x = 0.40 * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u8 {
        0 => (0.40, x, 0.0),
        1 => (x, 0.40, 0.0),
        2 => (0.0, 0.40, x),
        3 => (0.0, x, 0.40),
        4 => (x, 0.0, 0.40),
        _ => (0.40, 0.0, x),
    };
    (
        initials,
        cosmic::iced::Color::from_rgb(r + 0.08, g + 0.08, b + 0.08),
    )
}

/// Derived once for this compact project, rather than rescanning every card
/// for each worktree while building its hover description.
struct CompactProjectAttention {
    summary: AttentionSummary,
    activity: Option<crate::agent::ActivityState>,
    waiting_worktrees: BTreeSet<WorktreeId>,
}

fn compact_project_attention(
    project: &SidebarProject,
    state: &SidebarState,
) -> CompactProjectAttention {
    let worktrees: BTreeSet<_> = project.worktrees.iter().map(|row| row.id).collect();
    let waiting_worktrees = worktrees
        .into_iter()
        .filter(|worktree| state.worktree_attention(project.id, *worktree).waiting != 0)
        .collect();
    let summary = state.project_attention(project.id);
    CompactProjectAttention {
        summary,
        activity: summary.activity(),
        waiting_worktrees,
    }
}

fn compact_project_identity(
    project: &SidebarProject,
    selected: bool,
    modal_open: bool,
    state: &SidebarState,
    badge: Option<AttentionSummary>,
) -> Element<'static, SidebarAction> {
    let (initials, color) = project_identity(&project.name);
    let chip = rail::project_chip(project.avatar.as_ref(), initials, color, false);
    let attention = compact_project_attention(project, state);
    let activity = attention.activity;
    let mut description = project.name.clone();
    for worktree in &project.worktrees {
        use std::fmt::Write as _;
        let selected = state.selection
            == Some(SidebarSelection::Worktree {
                project_id: project.id,
                worktree_id: worktree.id,
            });
        let _ = write!(
            description,
            "\n{}{}: {}",
            if selected { "Selected " } else { "" },
            worktree.label(),
            status_summary(
                worktree.detached,
                worktree.locked,
                worktree.dirty.as_ref(),
                worktree.untracked_omitted,
                worktree.ahead,
                worktree.behind
            )
        );
        let mut worktree_attention = state.worktree_attention(project.id, worktree.id);
        if attention.waiting_worktrees.contains(&worktree.id) {
            description.push_str(", agent needs input");
            worktree_attention.waiting = 0;
        }
        let news = worktree_attention.description();
        if !news.is_empty() {
            description.push_str(", ");
            description.push_str(&news);
        }
    }
    let news = attention.summary.description();
    if !news.is_empty() {
        description.push('\n');
        description.push_str(&news);
    }
    if activity == Some(crate::agent::ActivityState::Waiting) {
        description.push_str("\nAn agent is waiting for input");
    }
    // The tooltip names the project and says what is going on in it; the
    // per-worktree breakdown is for a screen reader, which cannot glance at
    // the rows below.
    let mut tip = project.name.clone();
    if !news.is_empty() {
        tip.push('\n');
        tip.push_str(&news);
    }
    let grid = RowGrid::project(true);
    let control = grid
        .control(
            grid.rail(
                chip,
                badge.map_or_else(rail::Badges::default, |summary| {
                    rail::Badges::attention(state.shown_attention(summary))
                }),
            ),
            selected,
        )
        .id(project.chrome.open_id.clone())
        .name(description)
        .on_press_maybe(
            project_primary_action(project, state.filter)
                .and_then(|action| gated(action, modal_open)),
        );
    grid.mount(crate::interaction::tooltip(control, tip))
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
    let selection = if entity.channels.iter().any(|channel| channel.selected) {
        None
    } else {
        selection
    };
    let mut column = widget::column::with_capacity(project.worktrees.len() + 1).width(Length::Fill);
    for row in compact_operation_rows(entity, modal_open) {
        column = column.push(row);
    }
    let mut members = Vec::new();
    let drawn_channels: Vec<&SidebarChannel> = project
        .channels
        .iter()
        .copied()
        .filter(|channel| expanded && compact_rail_shows_channel(project.state, channel))
        .collect();
    for channel in &drawn_channels {
        members.push((
            SelectionRow::Channel(channel.id),
            RowInteraction::selectable(
                channel_view(
                    channel,
                    project.state.channel_listing(channel),
                    project.state.shown_attention(channel.attention),
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
            ),
        ));
    }
    for &worktree in project.worktrees.iter().filter(|worktree| {
        expanded
            && project.state.shows_worktree(entity, worktree)
            && compact_rail_shows_worktree(project.state, entity.id, worktree)
    }) {
        members.push((
            SelectionRow::Worktree(entity.id, worktree.id),
            RowInteraction::selectable(
                worktree_view(
                    entity.id,
                    worktree,
                    is_primary_worktree(&entity.repository, &worktree.path),
                    project.state.filter == SidebarFilter::PullRequests,
                    selection,
                    modal_open,
                    entity.github_ready,
                    project.editors,
                    project.state.worktree_row_state(entity.id, worktree.id),
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
            ),
        ));
    }
    for card in project.cards.iter().filter(|_| expanded) {
        members.push((
            SelectionRow::Card(card.id),
            card_view(entity, card, project.state, modal_open, true),
        ));
    }
    if !project.state.query.trim().is_empty() {
        let ranks = navigation_search_ranks(entity, &fold(project.state.query.trim()));
        members.sort_by_cached_key(|(row, _)| ranks.get(row).copied().unwrap_or(usize::MAX));
    }
    let has_children = !members.is_empty() || !entity.operations.is_empty();
    // Drawn children carry their own badges, so only an empty group needs
    // the full project aggregate. In All, a Folder root has no row of its
    // own and hidden checkouts leave their panes' rows, so the chip keeps
    // what no drawn channel or pane carries. The Attention list draws every
    // row it counts, in its main section or In progress, so its chip
    // repeats none of them, as the expanded header does not.
    let badge = if members.is_empty() {
        Some(project.state.project_attention(entity.id))
    } else if project.state.lists_panes(entity)
        || (entity.kind == ProjectKind::Folder && project.state.filter == SidebarFilter::All)
    {
        Some(undrawn_attention(
            project.state,
            entity.id,
            drawn_channels.iter().copied(),
            project.cards.iter().copied(),
        ))
    } else {
        None
    };
    let header = context_controls_with_display(
        compact_project_identity(entity, selected, modal_open, project.state, badge),
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
        Some((
            project.state.sidebar_worktrees,
            project
                .state
                .worktree_display_overrides
                .get(&entity.id)
                .copied(),
        )),
    );
    let header = RowInteraction::selectable(
        header,
        SelectionRow::Project(entity.id),
        project.state,
        !modal_open,
    );
    let primary = primary_worktree_id(entity);
    if project.state.worktree_display(entity.id) != crate::settings::SidebarWorktrees::Expanded {
        members.sort_by_key(|(row, _)| member_is_grouped(primary, *row));
    }
    let mut group_pending = expanded && has_worktree_group(entity, project.state);
    for (row, member) in members {
        if group_pending && member_is_grouped(primary, row) {
            column = column.push(worktree_group_row(entity, project.state, true, modal_open));
            group_pending = false;
        }
        column = column.push(member);
    }
    if group_pending {
        column = column.push(worktree_group_row(entity, project.state, true, modal_open));
    }
    rail::group(
        RowInteraction::group(header, column.into(), sticky_viewport),
        has_children,
    )
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
pub(crate) enum RowInk {
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

    fn listed(self, listing: Listing) -> Self {
        match listing {
            Listing::Live => self,
            Listing::Read | Listing::Handled => Self::Muted,
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

/// How the Attention filter lists a row. A row kept because it was dealt
/// with under Attention holds its place but recedes like an idle row, since
/// it is no longer news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Listing {
    /// Listed, if at all, for what the row is now.
    #[default]
    Live,
    /// Kept because the user read its update.
    Read,
    /// Kept because its waiting agent carried on. Who answered it, if anyone
    /// did, is not known here.
    Handled,
}

impl Listing {
    /// Spoken after the row's name, as the muted ink shows it.
    fn accessible_suffix(self) -> &'static str {
        match self {
            Self::Live => "",
            Self::Read => ", read",
            Self::Handled => ", handled",
        }
    }
}

/// Everything a worktree row draws about the agents and panes in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct WorktreeRowState {
    attention: AttentionSummary,
    unread: bool,
    activity: Option<crate::agent::ActivityState>,
    has_panes: bool,
}

impl WorktreeRowState {
    fn ink(self) -> RowInk {
        RowInk::of(self.has_panes, self.unread)
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

fn channel_ink(channel: &SidebarChannel, listing: Listing) -> RowInk {
    deletion_ink(
        RowInk::of(channel.has_panes, channel.unread).listed(listing),
        channel.deletion.as_ref(),
    )
}

/// The kind of workspace a row stands for, which decides the glyph it leads
/// with wherever it is drawn.
#[derive(Debug, Clone, Copy)]
enum WorkspaceGlyph<'a> {
    Channel,
    Worktree(&'a SidebarWorktree),
}

impl WorkspaceGlyph<'_> {
    fn icon(self) -> widget::icon::Handle {
        match self {
            Self::Channel => Icon::Channel.handle(),
            Self::Worktree(worktree) => worktree
                .chrome
                .pull_request
                .as_ref()
                .map_or_else(worktree_icon, pull_request_icon),
        }
    }

    /// A channel's identity stays neutral, its activity shown beside it; a
    /// worktree's pull request keeps its state's tone.
    fn tone(self, ink: RowInk) -> GlyphTone {
        match self {
            Self::Channel => GlyphTone::Ink(ink),
            Self::Worktree(worktree) => worktree_glyph_tone(
                worktree.chrome.pull_request.as_ref().map(pull_request_tone),
                ink,
            ),
        }
    }
}

/// The glyph a channel or worktree row leads with, in the row's `ink`. Every
/// row and mark for a workspace draws its glyph with it, so a workspace looks
/// the same wherever it is drawn.
fn row_glyph<'a, Message: 'a>(
    kind: WorkspaceGlyph<'_>,
    ink: RowInk,
    size: u16,
) -> widget::Container<'a, Message, cosmic::Theme> {
    toned_glyph(kind.icon(), kind.tone(ink), size)
}

/// A workspace the sidebar lists, for drawing its row's glyph elsewhere.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WorkspaceMark<'a> {
    /// A folder project, opened as a whole.
    Project(&'a SidebarProject),
    /// A folder outside any project.
    Folder,
    /// A worktree, or `None` for one no longer in the model, which keeps
    /// the plain worktree glyph.
    Worktree(Option<&'a SidebarWorktree>),
    /// A channel, or `None` for one no longer in the model, which keeps the
    /// plain channel glyph.
    Channel(Option<&'a SidebarChannel>),
}

/// The glyph the expanded sidebar leads a workspace's row with, in its
/// column: a folder project's avatar, a worktree's pull request or branch,
/// and a channel's members or else its chat glyph. The workspace switcher
/// draws its rows with it, so a workspace is recognised by the same mark in
/// both places.
pub(crate) fn workspace_mark<Message: 'static>(
    mark: WorkspaceMark<'_>,
    ink: RowInk,
) -> Element<'static, Message> {
    let glyph = match mark {
        WorkspaceMark::Project(project) => project_icon(project, ROW_ICON_SIZE),
        WorkspaceMark::Folder => folder_icon(ROW_ICON_SIZE),
        WorkspaceMark::Worktree(Some(worktree)) => worktree_status_glyph(worktree, false, ink),
        WorkspaceMark::Worktree(None) => {
            ink_glyph(worktree_icon(), ink, CHILD_ROW_ICON_SIZE).into()
        }
        WorkspaceMark::Channel(channel) => channel
            .and_then(|channel| members::view(&channel.members, members::Ground::Row, ink))
            .unwrap_or_else(|| row_glyph(WorkspaceGlyph::Channel, ink, CHILD_ROW_ICON_SIZE).into()),
    };
    widget::container(glyph)
        .center_x(Length::Fixed(ROW_ICON_WIDTH))
        .into()
}

/// Any other glyph that stands in a row's glyph column, in the row's `ink`.
fn ink_glyph<'a, Message: 'a>(
    icon: widget::icon::Handle,
    ink: RowInk,
    size: u16,
) -> widget::Container<'a, Message, cosmic::Theme> {
    toned_glyph(icon, GlyphTone::Ink(ink), size)
}

fn toned_glyph<'a, Message: 'a>(
    icon: widget::icon::Handle,
    tone: GlyphTone,
    size: u16,
) -> widget::Container<'a, Message, cosmic::Theme> {
    widget::container(widget::icon(icon).size(size)).class(theme::Container::custom(move |theme| {
        cosmic::iced::widget::container::Style {
            // Resolved here, against the theme this frame is drawing with: this
            // builder runs once per visible row, and reaching for
            // `cosmic::theme::active()` locked the process-global theme and
            // cloned it that many times per frame.
            icon_color: Some(tone.color(theme)),
            ..Default::default()
        }
    }))
}

/// A neutral pull request carries no news, so its glyph follows the row's ink
/// like the plain worktree glyph it stands in for; a draft follows it one step
/// quieter.
fn worktree_glyph_tone(pull_request: Option<PullRequestTone>, ink: RowInk) -> GlyphTone {
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
fn channel_indicator(
    attention: AttentionSummary,
    chats: usize,
) -> Option<Element<'static, SidebarAction>> {
    attention_indicator(attention)
        .or_else(|| (chats > 1).then(|| design::caption(chats.to_string()).into()))
}

/// Waiting is a live state, independent of whether its notification was read.
/// Channels and worktrees share the same vocabulary and priority.
fn agent_indicator(
    activity: Option<crate::agent::ActivityState>,
    unread: bool,
) -> Option<Element<'static, SidebarAction>> {
    attention_indicator(AttentionSummary::card(activity, unread, false))
}

pub fn attention_indicator<Message: Clone + 'static>(
    summary: AttentionSummary,
) -> Option<Element<'static, Message>> {
    let state = summary.state()?;
    let indicator: Element<'static, Message> = match state {
        AttentionState::Waiting => status_icon(
            Icon::Warning,
            "An agent is waiting for input",
            BadgeTone::Warning,
        ),
        AttentionState::Failed => {
            status_icon(Icon::Error, "Unread failed agent update", BadgeTone::Danger)
        }
        AttentionState::Limited => status_icon(
            Icon::Alarm,
            "An agent is stopped by a usage limit until it resets",
            BadgeTone::Warning,
        ),
        AttentionState::Working => crate::interaction::tooltip(
            crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::warning_color),
            "An agent is actively working",
        ),
        AttentionState::Unread => {
            status_icon(Icon::Attention, "Unread agent update", BadgeTone::Neutral)
        }
    };
    if summary.unread == 0 {
        return Some(indicator);
    }
    let count: Element<'static, Message> = widget::text(rail::count_label(summary.unread))
        .size(9)
        .into();
    let counted: Element<'static, Message> = widget::column::with_children(vec![indicator, count])
        .align_x(Alignment::Center)
        .into();
    Some(crate::interaction::tooltip(counted, summary.description()))
}

/// A deletion under way takes the reserved slot: it is the one live thing
/// about the row, and outranks whatever the row was doing before.
fn deleting_indicator() -> Element<'static, SidebarAction> {
    crate::interaction::tooltip(
        crate::ui_controls::activity_spinner(f32::from(ROW_ICON_SIZE), design::muted_icon_color),
        "Deleting…",
    )
}

/// A failed deletion is a fact about the row until it is retried, so it is a
/// badge after the reserved slot and leaves live activity where it was. The
/// rail shows it as a warning badge instead; see [`rail_deletion_badges`].
fn deletion_failed_badge(reason: &str) -> Element<'static, SidebarAction> {
    status_icon(Icon::Warning, reason.to_owned(), BadgeTone::Warning)
}

/// A rail row's badges: a deletion under way or failed takes them over, as it
/// takes over the expanded row's status slot; otherwise its live attention.
fn rail_deletion_badges(
    deletion: Option<&RowDeletion>,
    attention: AttentionSummary,
) -> rail::Badges {
    match deletion {
        Some(RowDeletion::Deleting) => rail::Badges::busy(),
        Some(RowDeletion::Failed(_)) => rail::Badges::warning(),
        None => rail::Badges::attention(attention),
    }
}

/// A row's tooltip. The rail has no room for the expanded row's "Deleting…"
/// caption or its failure badge, so its tooltip says what they would.
fn rail_tooltip(tooltip: &str, deletion: Option<&RowDeletion>, compact: bool) -> String {
    match deletion.filter(|_| compact) {
        Some(RowDeletion::Deleting) => format!("{tooltip}\nDeleting…"),
        Some(RowDeletion::Failed(reason)) => format!("{tooltip}\nCould not delete: {reason}"),
        None => tooltip.to_owned(),
    }
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
fn with_deletion_retry(
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
fn with_deletion_name(
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

/// `attention` is the news the row draws; see [`SidebarState::shown_attention`].
fn channel_view(
    channel: &SidebarChannel,
    listing: Listing,
    attention: AttentionSummary,
    modal_open: bool,
    compact: bool,
    selection_count: Option<usize>,
    query: &str,
) -> Element<'static, SidebarAction> {
    let grid = RowGrid::child(compact).deletable();
    let deletion = channel.deletion.as_ref();
    let deleting = deletion == Some(&RowDeletion::Deleting);
    let ink = channel_ink(channel, listing);
    // Who is at work stands in for the glyph. A channel on its way out shows
    // only the muted glyph its label recedes with.
    let mark = (!deleting)
        .then(|| {
            members::view(
                &channel.members,
                if compact {
                    members::Ground::Rail
                } else {
                    members::Ground::Row
                },
                ink,
            )
        })
        .flatten();
    let has_members = mark.is_some();
    let glyph: Element<'static, SidebarAction> = mark.unwrap_or_else(|| {
        row_glyph(
            WorkspaceGlyph::Channel,
            ink,
            if compact {
                rail::CHILD_GLYPH
            } else {
                CHILD_ROW_ICON_SIZE
            },
        )
        .into()
    });
    let mut content = if compact {
        grid.rail(glyph, rail_deletion_badges(deletion, attention))
    } else {
        let label = search_label(
            &channel.title,
            query,
            CHILD_ROW_TEXT_SIZE,
            ink.text_class(),
            false,
        );
        let indicator = match deletion {
            Some(RowDeletion::Deleting) => Some(deleting_indicator()),
            // The mark and its badge show who is there, and the tooltip
            // lists them, so the row's end keeps to news.
            _ if has_members => attention_indicator(attention),
            _ => channel_indicator(attention, channel.chats),
        };
        grid.content(glyph, Some(label), indicator)
    };
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
            Cow::Owned(format!(
                "{}{}",
                channel.accessible_name,
                listing.accessible_suffix()
            )),
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
        crate::interaction::tooltip(action, rail_tooltip(&channel.tooltip, deletion, compact)),
        &channel.delete_name,
        gated(
            SidebarAction::DeleteChannel {
                channel_id: channel.id,
            },
            inert,
        ),
        channel.delete_id.clone(),
        compact,
        channel.selected,
    );
    crate::reorder::channel_target(
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
        )),
        channel.id,
        !inert,
    )
}

/// A child row's leading glyph, in the row's tone.
fn child_glyph(icon: Icon, tone: GlyphTone, compact: bool) -> Element<'static, SidebarAction> {
    widget::container(widget::icon(icon.handle()).size(if compact {
        rail::CHILD_GLYPH
    } else {
        CHILD_ROW_ICON_SIZE
    }))
    .class(theme::Container::custom(move |theme| {
        cosmic::iced::widget::container::Style {
            icon_color: Some(tone.color(theme)),
            ..Default::default()
        }
    }))
    .into()
}

/// A row's title followed by where it is, dimmed: "Codex chat · fix-sidebar".
/// The title keeps the width it needs and the place gives way first, since
/// the title is what tells two rows apart. Only once the place is gone does
/// the title itself truncate.
fn titled_label(
    title: &str,
    place: Option<&str>,
    query: &str,
    ink: RowInk,
) -> Element<'static, SidebarAction> {
    let Some(place) = place else {
        return search_label(title, query, CHILD_ROW_TEXT_SIZE, ink.text_class(), false);
    };
    let ellipsized = |text: widget::Text<'static, cosmic::Theme>| {
        text.size(CHILD_ROW_TEXT_SIZE)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
    };
    let place = format!(" · {place}");
    let place = move || -> Element<'static, SidebarAction> {
        ellipsized(widget::text::body(place.clone()))
            .class(theme::Text::Custom(design::secondary_text))
            .width(Length::Fill)
            .into()
    };
    if search_highlight_ranges(title, query).is_empty() {
        // A shrinking text takes its natural width before the filling place
        // is laid out, and ellipsizes only if the row cannot hold even that.
        return widget::row::with_children(vec![
            ellipsized(widget::text::body(title.to_owned()))
                .class(ink.text_class())
                .into(),
            place(),
        ])
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into();
    }
    // A highlighted title is a fitting label that fills whatever slot it is
    // given, so it is given its natural width, measured once, or the whole
    // row when that is narrower.
    let title = title.to_owned();
    let query = query.to_owned();
    let natural = std::cell::OnceCell::new();
    crate::ui_controls::responsive(move |slot| {
        let natural = *natural.get_or_init(|| {
            let (font, emphasis) = search_label_fonts(false);
            search_spans_width(&title, &query, CHILD_ROW_TEXT_SIZE, font, emphasis).ceil()
        });
        widget::row::with_children(vec![
            widget::container(search_label(
                &title,
                &query,
                CHILD_ROW_TEXT_SIZE,
                ink.text_class(),
                false,
            ))
            .width(Length::Fixed(natural.min(slot.width)))
            .into(),
            place(),
        ])
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
    })
    // As tall as the title's text, as a title on its own is, so the
    // two share a pixel row; a height of their own would centre them
    // between two and blur them.
    .height(Length::Shrink)
    .into()
}

/// One pane the Attention list names on its own row: its kind, its title and
/// the checkout it runs in, and its news in the indicator slot. Opening it
/// goes to that checkout, focuses the pane and reads it.
fn card_view(
    project: &SidebarProject,
    card: &SidebarCard,
    state: &SidebarState,
    modal_open: bool,
    compact: bool,
) -> Element<'static, SidebarAction> {
    let grid = RowGrid::child(compact);
    let listing = state.card_listing(card);
    let ink = RowInk::Full.listed(listing);
    let checkout = card.checkout(project);
    let glyph = child_glyph(card.icon(), GlyphTone::Ink(ink), compact);
    let shown = state.shown_attention(card.attention);
    let content = if compact {
        grid.rail(glyph, rail::Badges::attention(shown))
    } else {
        grid.content(
            glyph,
            Some(titled_label(&card.title, checkout, &state.query, ink)),
            attention_indicator(shown),
        )
    };
    let place = checkout.map_or_else(String::new, |checkout| format!(" in {checkout}"));
    let news = card.attention.description();
    let news = if news.is_empty() {
        news
    } else {
        format!(", {news}")
    };
    let name = format!(
        "Open pane {}{place}{news}{}",
        card.title,
        listing.accessible_suffix()
    );
    let mut tip = checkout.map_or_else(
        || card.title.clone(),
        |checkout| format!("{} · {checkout}", card.title),
    );
    let news = card.attention.description();
    if !news.is_empty() {
        tip.push('\n');
        tip.push_str(&news);
    }
    let control = grid
        .control(content, state.follows_card(card))
        .id(card.open_id.clone())
        .name(name)
        .on_press_maybe(gated(SidebarAction::OpenCard { card: card.id }, modal_open));
    RowInteraction::selectable(
        grid.mount(crate::interaction::tooltip(control, tip)),
        SelectionRow::Card(card.id),
        state,
        !modal_open,
    )
}

pub fn in_progress_focus_id() -> Id {
    stable_id("attention.in_progress")
}

/// Whether keyboard focus is on one of the In progress rows drawn now.
///
/// Folding the section takes those rows out of the widget tree and their
/// focus with them, which would leave the next Tab to start over from the
/// window's first control. Only the tree drawn before the fold can say where
/// focus is, so a fold asks this first and then hands focus to the header.
pub fn in_progress_holds_focus(
    model: &SidebarModel,
    state: &SidebarState,
    compact: bool,
) -> cosmic::iced::Task<bool> {
    use cosmic::iced::advanced::widget::{Operation, operation};
    struct Probe {
        rows: Vec<Id>,
        inside: bool,
    }
    impl Operation<bool> for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<bool>)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            _: cosmic::iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.inside |= state.is_focused() && id.is_some_and(|id| self.rows.contains(id));
        }
        // Unlike `find_focused`, this always answers, so the fold it gates
        // happens whether or not anything has focus.
        fn finish(&self) -> operation::Outcome<bool> {
            operation::Outcome::Some(self.inside)
        }
    }
    let rows = in_progress_rows(model, state, compact)
        .into_iter()
        .map(|row| selection_row_focus_id(row.row()))
        .collect();
    cosmic::iced::advanced::widget::operate(Probe {
        rows,
        inside: false,
    })
}

/// The Attention list's last section: channels and panes whose agents are
/// working and need nothing from the user yet. They are worth knowing about
/// but not acting on, so they sit below everything that is, under one header
/// the user can fold; folded, the header's spinner still says work goes on.
fn in_progress_section(
    rows: &[InProgressRow<'_>],
    state: &SidebarState,
    compact: bool,
    modal_open: bool,
) -> Element<'static, SidebarAction> {
    let collapsed = state.in_progress_collapsed;
    let count = rows.len();
    // A channel can have more than one agent at work, so the header counts
    // agents, as its accessible name does, rather than the rows below it.
    let agents: usize = rows.iter().map(|row| row.attention().working).sum();
    let name = format!(
        "{} In progress, {agents} agent{} working",
        if collapsed { "Expand" } else { "Collapse" },
        if agents == 1 { "" } else { "s" }
    );
    let grid = RowGrid::child(compact);
    let glyph: Element<'static, SidebarAction> = if collapsed {
        Icon::ChevronRight
    } else {
        Icon::ChevronDown
    }
    .handle()
    .icon()
    .size(CHILD_ROW_ICON_SIZE)
    .into();
    let content = if compact {
        grid.rail(glyph, rail::Badges::in_progress(agents, collapsed))
    } else {
        // The count sits in the indicator slot, where every row's badge is.
        // Folded, the spinner above it says the hidden rows are still at work,
        // as a counted badge does on any other row.
        let count = design::caption(agents.to_string());
        let indicator: Element<'static, SidebarAction> = if collapsed {
            widget::column::with_children(vec![
                crate::ui_controls::activity_spinner(
                    f32::from(ROW_ICON_SIZE),
                    design::warning_color,
                ),
                count.size(9).into(),
            ])
            .align_x(Alignment::Center)
            .into()
        } else {
            count.into()
        };
        grid.content(
            glyph,
            Some(design::body("In progress").width(Length::Fill).into()),
            Some(indicator),
        )
    };
    let header = grid
        .control(content, false)
        .id(in_progress_focus_id())
        .name(name.clone())
        .on_press_maybe(gated(SidebarAction::ToggleInProgress, modal_open));
    let mut column = widget::column::with_capacity(1 + if collapsed { 0 } else { count })
        .width(Length::Fill)
        .push(grid.mount(crate::interaction::tooltip(header, name)));
    if !collapsed {
        for row in rows {
            column = column.push(in_progress_row(*row, state, compact, modal_open));
        }
    }
    if compact {
        rail::group(column.into(), true)
    } else {
        column.into()
    }
}

/// An "In progress" row, drawn like the row it stands for: its kind, its
/// title and where it is, and its work in the indicator slot. The section
/// gathers rows from every project, so the place names the project, and for
/// a pane also its checkout: "Claude Code · cagents › fix-sidebar".
fn in_progress_row(
    row: InProgressRow<'_>,
    state: &SidebarState,
    compact: bool,
    modal_open: bool,
) -> Element<'static, SidebarAction> {
    let grid = RowGrid::child(compact);
    let place = row.place();
    let (kind, title, icon, attention, selected, id, action) = match row.item {
        InProgressItem::Channel(channel) => (
            "channel",
            &channel.title,
            Icon::Channel,
            channel.attention,
            channel.selected,
            channel.open_id.clone(),
            SidebarAction::OpenChannel {
                channel_id: channel.id,
            },
        ),
        InProgressItem::Card(card) => (
            "pane",
            &card.title,
            card.icon(),
            card.attention,
            state.follows_card(card),
            card.open_id.clone(),
            SidebarAction::OpenCard { card: card.id },
        ),
    };
    let glyph = child_glyph(icon, GlyphTone::Ink(RowInk::Full), compact);
    let content = if compact {
        grid.rail(glyph, rail::Badges::attention(attention))
    } else {
        grid.content(
            glyph,
            Some(titled_label(
                title,
                Some(&place),
                &state.query,
                RowInk::Full,
            )),
            attention_indicator(attention),
        )
    };
    let news = attention.description();
    let control = grid
        .control(content, selected)
        .id(id)
        .name(format!("Open {kind} {title} in {place}, {news}"))
        .on_press_maybe(gated(action, modal_open));
    let control = grid.mount(crate::interaction::tooltip(
        control,
        format!("{title} · {place}\n{news}"),
    ));
    let control = match row.item {
        InProgressItem::Channel(channel) => channel_context_controls(
            control,
            channel.id,
            modal_open,
            state.rows.group_size(SelectionRow::Channel(channel.id)),
        ),
        InProgressItem::Card(_) => control,
    };
    RowInteraction::selectable(control, row.row(), state, !modal_open)
}

/// Keep the row's primary button and its destructive quick action separate.
/// An expanded row shows the button on hover or focus, over the end of the
/// row on the row's own hovered fill, `selected` saying whether that is the
/// selected row's. The content keeps its whole width either way, so nothing
/// in the row moves when the pointer enters it. The rail has no room for the
/// action at all.
fn row_quick_delete(
    content: Element<'static, SidebarAction>,
    label: &str,
    action: Option<SidebarAction>,
    id: Id,
    compact: bool,
    selected: bool,
) -> Element<'static, SidebarAction> {
    // A rail cell is one glyph wide: a hover-revealed delete would land on
    // the badges and next to the glyph a press means to open. The row's
    // context menu and the Delete key still reach it.
    if compact {
        return content;
    }
    let width = ICON_CONTROL_SIZE;
    let control = centered_icon_button(Icon::Delete, width, WORKTREE_ROW_HEIGHT)
        .class(sidebar_button_style(false))
        .id(id)
        .name(label.to_owned())
        .on_press_maybe(action);
    let ground = widget::container(crate::interaction::tooltip(control, label.to_owned()))
        .width(width)
        .height(WORKTREE_ROW_HEIGHT)
        .class(hovered_row_end(selected));
    let controls = widget::container(ground).align_right(Length::Fill).into();
    RowInteraction::actions(content, controls, 0.0)
}

/// The opaque fill of a hovered row's trailing end: the row's hovered fill
/// laid over the sidebar's ground, rounded on the right as the row is.
fn hovered_row_end(selected: bool) -> theme::Container<'static> {
    theme::Container::custom(move |theme| {
        let ground: cosmic::iced::Color = theme.cosmic().primary(false).base.into();
        let ink: cosmic::iced::Color = theme.current_container().on.into();
        let alpha = HOVERED_ROW_ALPHA + if selected { SELECTED_ROW_ALPHA } else { 0.0 };
        let radius = theme.cosmic().corner_radii.radius_s;
        cosmic::iced::widget::container::Style {
            background: Some(cosmic::iced::Background::Color(design::mix(
                ground, ink, alpha,
            ))),
            border: cosmic::iced::Border {
                radius: [0.0, radius[1], radius[2], 0.0].into(),
                ..Default::default()
            },
            ..Default::default()
        }
    })
}

/// The commands a channel row offers on right-click.
///
/// A channel is the only navigation row whose contents exist nowhere else, so
/// deleting one is destructive and the entry says so with an ellipsis: the
/// application answers it with a confirmation rather than a removal.
fn channel_context_controls(
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
            Some(Icon::Delete.handle()),
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
                    Some(Icon::Rename.handle()),
                    ChannelContextCommand(channel_id, false, ChannelMenuCommand::Rename),
                ),
            );
            entries.insert(
                1,
                Item::Button(
                    "Mark as read".into(),
                    Some(Icon::Check.handle()),
                    ChannelContextCommand(channel_id, false, ChannelMenuCommand::MarkRead),
                ),
            );
        }
        if selection_count == Some(1) {
            entries.insert(2, Item::Divider);
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
    WorktreeRowState {
        attention,
        unread,
        activity,
        has_panes,
    }: WorktreeRowState,
) -> Cow<'static, str> {
    let mut name: Cow<'static, str> = Cow::Owned(if has_panes {
        worktree.chrome.accessible_name.to_string()
    } else {
        worktree.chrome.paneless_accessible_name.to_string()
    });
    if unread {
        name.to_mut().push_str(", unread agent update");
    }
    let news = AttentionSummary {
        working: 0,
        waiting: 0,
        ..attention
    }
    .description();
    if !news.is_empty() {
        name.to_mut().push_str(", ");
        name.to_mut().push_str(&news);
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
/// The navigation rail's row and the context inspector both resolve their
/// badges through this, so the two cannot report the same worktree
/// differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeIndicators<'a> {
    /// The dirty badge's label, such as `3 changes`.
    pub dirty: Option<&'a str>,
    /// The count that label states.
    pub dirty_count: Option<u64>,
    pub pull_request: Option<&'a SidebarPullRequest>,
    pub pending_checks: bool,
}

pub fn worktree_indicators(worktree: &SidebarWorktree) -> WorktreeIndicators<'_> {
    let pull_request = worktree.chrome.pull_request.as_ref();
    WorktreeIndicators {
        dirty: worktree.chrome.dirty_badge.as_deref(),
        dirty_count: worktree
            .dirty
            .as_ref()
            .filter(|dirty| dirty.is_dirty())
            .map(dirty_count),
        pull_request,
        pending_checks: pull_request.is_some_and(SidebarPullRequest::has_pending_checks),
    }
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
    row_state: WorktreeRowState,
    compact: bool,
    selection_count: Option<usize>,
    query: &str,
    show_changes_icons: bool,
) -> Element<'static, SidebarAction> {
    let WorktreeRowState {
        unread: agent_update,
        activity,
        ..
    } = row_state;
    let chrome = &worktree.chrome;
    // A merged pull request's worktree has served its purpose, so hovering
    // it offers to delete it.
    let quick_delete = !primary
        && worktree
            .pull_request()
            .is_some_and(|pr| pr.state == Some(WorktreePullRequestState::Merged));
    // Unlike a channel row's, the grid keeps its usual status slot: a
    // worktree's badges follow the slot, so the delete covers them rather
    // than the slot, and widening it would cost the name room for nothing.
    let grid = RowGrid::child(compact);
    let deletion = worktree.deletion.as_ref();
    let deleting = deletion == Some(&RowDeletion::Deleting);
    let ink = row_state.ink();
    let mut content = if compact {
        // The rail is for live news only. Dirty changes and pending checks are
        // durable facts about the worktree, kept in its tooltip and accessible
        // name and in the expanded row; drawn here they made every worktree
        // look like it wanted attention.
        let mut attention = row_state.attention;
        attention.union(AttentionSummary::card(activity, agent_update, false));
        let glyph = if deleting {
            worktree_glyph_muted(rail::CHILD_GLYPH)
        } else {
            row_glyph(WorkspaceGlyph::Worktree(worktree), ink, rail::CHILD_GLYPH)
        };
        grid.rail(glyph, rail_deletion_badges(deletion, attention))
    } else {
        let label = search_label(
            &chrome.label,
            query,
            CHILD_ROW_TEXT_SIZE,
            deletion_ink(ink, deletion).text_class(),
            false,
        );
        let indicator = match deletion {
            Some(RowDeletion::Deleting) => Some(deleting_indicator()),
            _ => attention_indicator(row_state.attention)
                .or_else(|| agent_indicator(activity, agent_update)),
        };
        let glyph = if deleting {
            worktree_glyph_muted(CHILD_ROW_ICON_SIZE).into()
        } else {
            worktree_status_icon(worktree, false, ink)
        };
        grid.content(glyph, Some(label), indicator)
    };
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
            worktree_accessible_name(worktree, row_state),
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
        crate::interaction::hover_with_theme_radius(action)
    };
    let action =
        crate::interaction::tooltip(action, rail_tooltip(&chrome.tooltip, deletion, compact));
    let action = if quick_delete {
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
            selected,
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

fn project_icon<Message: 'static>(
    project: &SidebarProject,
    size: u16,
) -> Element<'static, Message> {
    match &project.avatar {
        Some(handle) => cosmic::iced::widget::image(handle.clone())
            .width(size)
            .height(size)
            .into(),
        None => folder_icon(size),
    }
}

/// A folder with no avatar of its own.
fn folder_icon<Message: 'static>(size: u16) -> Element<'static, Message> {
    widget::container(Icon::Folder.handle().icon().size(size))
        .class(theme::Container::custom(move |theme| {
            cosmic::iced::widget::container::Style {
                icon_color: Some(design::muted_icon_color(theme)),
                ..Default::default()
            }
        }))
        .into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectCommand {
    CopyPath,
    CopyBranch,
    PullRequest,
    NewWorktreeFromBranch,
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
enum ContextCommand<'a> {
    Project(ProjectId, Option<WorktreeId>, &'a ProjectCommand, bool),
    Display(ProjectId, Option<crate::settings::SidebarWorktrees>),
    /// Shows the project's hidden checkouts: its own Hidden mode gives way
    /// to the default, and a Hidden default is shown in every project, since
    /// it overrides each project's own mode.
    ShowCheckouts(ProjectId, crate::settings::SidebarWorktrees),
}

impl widget::menu::Action for ContextCommand<'_> {
    type Message = SidebarAction;

    fn message(&self) -> SidebarAction {
        match self {
            Self::Project(_, _, _, true) => SidebarAction::DeleteSelectedRows,
            Self::Project(project, worktree, command, false) => SidebarAction::ProjectCommand {
                project_id: *project,
                worktree_id: *worktree,
                command: (*command).clone(),
            },
            Self::Display(project, mode) => SidebarAction::SetWorktreeDisplay {
                project_id: *project,
                mode: *mode,
            },
            Self::ShowCheckouts(_, global) if !global.shows_checkouts() => {
                SidebarAction::ToggleCheckouts
            }
            Self::ShowCheckouts(project, _) => SidebarAction::SetWorktreeDisplay {
                project_id: *project,
                mode: None,
            },
        }
    }
}

fn project_channel_command_label(worktree: Option<WorktreeId>) -> &'static str {
    if worktree.is_some() {
        "New thread"
    } else {
        "New channel"
    }
}

fn project_menu_group(command: &ProjectCommand) -> u8 {
    match command {
        ProjectCommand::NewTerminal
        | ProjectCommand::NewChannel
        | ProjectCommand::NewWorktree
        | ProjectCommand::NewWorktreeFromBranch => 0,
        ProjectCommand::CopyPath | ProjectCommand::CopyBranch => 1,
        ProjectCommand::Reveal
        | ProjectCommand::OpenEditor(_)
        | ProjectCommand::RepositoryUrl
        | ProjectCommand::PullRequest => 2,
        ProjectCommand::MoveUp | ProjectCommand::MoveDown | ProjectCommand::ResetSidebarOrder => 3,
        ProjectCommand::Delete | ProjectCommand::Remove => 4,
    }
}

fn project_menu_icon(command: &ProjectCommand) -> Icon {
    match command {
        ProjectCommand::NewTerminal => Icon::Terminal,
        ProjectCommand::NewChannel => Icon::ChannelAdd,
        ProjectCommand::NewWorktree
        | ProjectCommand::NewWorktreeFromBranch
        | ProjectCommand::CopyBranch => Icon::Branch,
        ProjectCommand::CopyPath => Icon::CopyPath,
        ProjectCommand::Reveal | ProjectCommand::OpenEditor(_) => Icon::OpenFolder,
        ProjectCommand::RepositoryUrl => Icon::Browser,
        ProjectCommand::PullRequest => Icon::Changes,
        ProjectCommand::MoveUp => Icon::ChevronUp,
        ProjectCommand::MoveDown => Icon::ChevronDown,
        ProjectCommand::ResetSidebarOrder => Icon::History,
        ProjectCommand::Delete | ProjectCommand::Remove => Icon::Delete,
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
    context_controls_with_display(
        content,
        project_id,
        worktree_id,
        modal_open,
        git,
        has_branch,
        github_ready,
        editors,
        selection_count,
        None,
    )
}

fn context_controls_with_display<'a>(
    content: impl Into<Element<'a, SidebarAction>>,
    project_id: ProjectId,
    worktree_id: Option<WorktreeId>,
    modal_open: bool,
    git: bool,
    has_branch: bool,
    github_ready: bool,
    editors: &[SidebarEditor],
    selection_count: Option<usize>,
    display: Option<(
        crate::settings::SidebarWorktrees,
        Option<crate::settings::SidebarWorktrees>,
    )>,
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
            (
                project_channel_command_label(worktree_id).to_owned(),
                None,
                ProjectCommand::NewChannel,
            ),
        ];
        if git {
            commands.push(("New worktree".to_owned(), None, ProjectCommand::NewWorktree));
        }
        commands.push(("Copy path".into(), None, ProjectCommand::CopyPath));
        if worktree_id.is_some() {
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
        commands.sort_by_key(|(_, _, command)| project_menu_group(command));
        let mut previous_group = None;
        let mut entries = Vec::new();
        for (label, icon, command) in &commands {
            let group = project_menu_group(command);
            if previous_group.is_some_and(|previous| previous != group) {
                entries.push(Item::Divider);
            }
            previous_group = Some(group);
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
            entries.push(item(
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
                Some(
                    icon.clone()
                        .unwrap_or_else(|| project_menu_icon(command).handle()),
                ),
                ContextCommand::Project(
                    project_id,
                    worktree_id,
                    command,
                    selection_count.is_some_and(|count| count > 1)
                        && matches!(command, ProjectCommand::Delete | ProjectCommand::Remove),
                ),
            ));
            if let Some((global, override_mode)) = display.filter(|_| {
                selection_count == Some(1) && git && *command == ProjectCommand::NewWorktree
            }) {
                if !global.shows_checkouts() {
                    entries.push(Item::Button(
                        "Show checkouts in every project".into(),
                        Some(worktree_icon()),
                        ContextCommand::ShowCheckouts(project_id, global),
                    ));
                } else if override_mode.is_some_and(|mode| !mode.shows_checkouts()) {
                    entries.push(Item::Button(
                        "Show checkouts".into(),
                        Some(worktree_icon()),
                        ContextCommand::ShowCheckouts(project_id, global),
                    ));
                }
            }
        }
        if let Some((global, override_mode)) = display.filter(|_| selection_count == Some(1) && git)
        {
            // A hidden default hides every project's checkouts, whatever
            // mode the project has; that mode applies once they show.
            let mut choices = vec![Item::CheckBox(
                if global.shows_checkouts() {
                    format!("Use default ({})", global.label())
                } else {
                    "Use default (Hidden in every project)".to_owned()
                },
                None,
                override_mode.is_none(),
                ContextCommand::Display(project_id, None),
            )];
            choices.extend(
                crate::settings::SidebarWorktrees::ALL
                    .into_iter()
                    .map(|mode| {
                        Item::CheckBox(
                            mode.label().into(),
                            None,
                            override_mode == Some(mode),
                            ContextCommand::Display(project_id, Some(mode)),
                        )
                    }),
            );
            entries.push(Item::Divider);
            entries.push(Item::Folder("Checkouts in sidebar".into(), choices));
        }
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
    let mut previous_group = None;
    for (label, icon, command) in file_commands(row.directory, file_management) {
        let group = match command {
            FileCommand::Rename | FileCommand::CopyFile => 0,
            FileCommand::CopyPath | FileCommand::CopyRelativePath => 1,
            FileCommand::RevealInFileManager => 2,
        };
        if previous_group.is_some_and(|previous| previous != group) {
            entries.push(widget::menu::Item::Divider);
        }
        previous_group = Some(group);
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

/// The plain worktree glyph, muted: a worktree being created or deleted.
fn worktree_glyph_muted(size: u16) -> widget::Container<'static, SidebarAction, cosmic::Theme> {
    widget::container(widget::icon(worktree_icon()).size(size)).class(theme::Container::custom(
        |theme| cosmic::iced::widget::container::Style {
            icon_color: Some(design::muted_icon_color(theme)),
            ..Default::default()
        },
    ))
}

fn worktree_status_icon(
    worktree: &SidebarWorktree,
    update: bool,
    ink: RowInk,
) -> Element<'static, SidebarAction> {
    crate::interaction::tooltip(
        worktree_status_glyph(worktree, update, ink),
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

/// A worktree row's glyph, with the dot beside it that marks an update.
fn worktree_status_glyph<Message: 'static>(
    worktree: &SidebarWorktree,
    update: bool,
    ink: RowInk,
) -> Element<'static, Message> {
    let glyph = row_glyph(WorkspaceGlyph::Worktree(worktree), ink, CHILD_ROW_ICON_SIZE);
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
    widget::row::with_children(vec![glyph.into(), dot.into()])
        .align_y(Alignment::Center)
        .into()
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
            crate::design::horizontal_rule(),
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
    if let Some(branch) = worktree
        .and_then(|row| row.branch.as_deref())
        .filter(|_| !worktree.is_some_and(|row| row.detached))
    {
        header = header.push(icon_action(
            Icon::CopyPath,
            format!("Copy branch {branch}"),
            gated(
                SidebarAction::ProjectCommand {
                    project_id: key.project_id,
                    worktree_id: Some(key.worktree_id),
                    command: ProjectCommand::CopyBranch,
                },
                modal_open,
            ),
            entity_id("source.copy-branch", key.worktree_id),
        ));
    }
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
        // The rows pass under the tab row and the commit box's rule; they
        // fade into the pane there rather than being cut at either edge.
        Some(crate::interaction::keyboard_list(
            crate::interaction::overflow_fade_y(
                widget::scrollable(
                    widget::container(body)
                        .padding(design::with_vertical_rail(Padding::from([0.0, space::S])))
                        .width(Length::Fill),
                )
                .direction(sidebar_scroll_direction())
                .class(cosmic::style::iced::Scrollable::Minimal)
                .width(Length::Fill)
                .height(Length::Fill),
                design::base,
            ),
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
    let header = crate::interaction::tooltip(
        crate::interaction::hover_with_theme_radius(row),
        parts.action_name,
    );
    let header: Element<'a, SidebarAction> = if let Some(actions) = actions {
        RowInteraction::actions(header, actions, 0.0)
    } else {
        header
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
        crate::interaction::overflow_fade_y(
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
            .height(Length::Fill),
            design::base,
        ),
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
        placeholder: design::field_placeholder_color(theme),
        value: design::field_text_color(theme),
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
        // Ticking Amend relabels Commit; Push beside it stays put.
        crate::ui_controls::button::primary("Commit")
            .instead("Amend commit", state.commit_draft.amend)
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
    crate::interaction::keyboard_list_row(crate::interaction::hover_with_theme_radius(
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
/// come from the surface that raised the intent; the application mounting it
/// decides only where to put it and which `surface_id` identifies it.
pub fn discard_confirmation_dialog(
    pending: &DiscardConfirmation,
    surface_id: Id,
) -> Element<'_, SidebarAction> {
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
    crate::dialog_ui::confirmation_surface(
        surface_id,
        widget::column::with_children(vec![
            design::title("Discard changes?").into(),
            design::body(detail.join(". "))
                .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                .into(),
            design::caption("Discarded work is not recoverable from Git.").into(),
        ])
        .spacing(space::M),
        crate::dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(discard_cancel_id())
                .on_press(SidebarAction::CancelDiscard),
            crate::ui_controls::button::destructive("Discard")
                .id(discard_confirm_id())
                .on_press(pending.confirm_action()),
        ),
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
    row_button_style::<true>(selected)
}

/// A navigation row outside the sidebar, such as the workspace switcher's,
/// selected as the sidebar's rows are. It takes no hover fill of its own.
pub fn navigation_row_style(selected: bool) -> theme::Button {
    row_button_style::<false>(selected)
}

/// How strongly a selected navigation row is tinted with the text color.
pub const SELECTED_ROW_ALPHA: f32 = 0.10;
/// How much more strongly a hovered sidebar row is.
const HOVERED_ROW_ALPHA: f32 = 0.06;

/// Inset navigation fills use the theme radius; project labels remain plain on hover.
fn row_button_style<const HOVER: bool>(selected: bool) -> theme::Button {
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
        style.icon_color = Some(if selected || focused {
            text
        } else {
            design::muted_icon_color(theme)
        });
        style.border_radius = theme.cosmic().corner_radii.radius_s.into();
        if selected || focused || alpha > 0.0 {
            let mut surface = foreground;
            surface.a = if disabled {
                0.04
            } else {
                (if selected { SELECTED_ROW_ALPHA } else { 0.0 })
                    + alpha
                    + if focused { 0.06 } else { 0.0 }
            };
            style.background = Some(cosmic::iced::Background::Color(surface));
        }
        style
    }
    // Noncapturing closures keep the boxed callbacks allocation-free for the
    // hundreds of file rows rebuilt on terminal wakes.
    fn class<const SELECTED: bool, const HOVER: bool>() -> theme::Button {
        theme::Button::Custom {
            active: Box::new(|focused, theme| appearance(theme, focused, SELECTED, 0.0, false)),
            disabled: Box::new(|theme| appearance(theme, false, SELECTED, 0.0, true)),
            hovered: Box::new(|focused, theme| {
                appearance(
                    theme,
                    focused,
                    SELECTED,
                    if HOVER { HOVERED_ROW_ALPHA } else { 0.0 },
                    false,
                )
            }),
            pressed: Box::new(|focused, theme| {
                appearance(
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
        class::<true, HOVER>()
    } else {
        class::<false, HOVER>()
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
        centered_icon_button(icon_name, ICON_CONTROL_SIZE, ICON_CONTROL_SIZE)
            .class(sidebar_button_style(false))
            .id(id)
            .name(label.clone())
            .on_press_maybe(action),
        label,
    )
}

/// A fixed-size icon target whose glyph sits in the middle of its whole
/// hit area.
///
/// libcosmic's `button::icon` lays its glyph out in a row, and a row has no
/// main-axis alignment: given a fixed width wider than the glyph, the glyph
/// stays at the leading edge and the rest of the target is blank. Centering a
/// glyph container inside a custom button keeps every sidebar icon command
/// aligned however wide its slot is.
fn centered_icon_button<'a>(
    icon_name: Icon,
    width: f32,
    height: f32,
) -> button::Button<'a, SidebarAction> {
    centered_icon_handle_button(icon_name.handle(), width, height)
}

fn centered_icon_handle_button<'a>(
    icon: widget::icon::Handle,
    width: f32,
    height: f32,
) -> button::Button<'a, SidebarAction> {
    button::custom(
        widget::container(widget::icon(icon).size(ROW_ICON_SIZE))
            .class(design::transparent())
            .center(Length::Fill),
    )
    .padding(0)
    .width(width)
    .height(height)
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
        Some(dirty) if dirty.is_dirty() => parts.push(dirty_label(dirty, untracked_omitted)),
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

/// `1 change`, `3 tracked changes`: how much a worktree has uncommitted,
/// saying when untracked files were not counted.
fn dirty_label(dirty: &DirtyState, untracked_omitted: bool) -> String {
    let count = dirty_count(dirty);
    format!(
        "{count} {}{}",
        if untracked_omitted { "tracked " } else { "" },
        if count == 1 { "change" } else { "changes" }
    )
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
    toned_icon_sized(icon_name, tone, ROW_ICON_SIZE)
}

fn toned_icon_sized<'a, Message: 'a>(
    icon_name: Icon,
    tone: BadgeTone,
    size: u16,
) -> Element<'a, Message> {
    widget::container(widget::icon(icon_name.handle()).size(size))
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
    /// Pane rows: the Attention list's, or every open pane's while the
    /// project's checkouts are hidden.
    cards: Vec<&'a SidebarCard>,
    editors: &'a [SidebarEditor],
    state: &'a SidebarState,
}

/// Cache each visible row's rank once. Newly created channels precede the
/// saved mixed order; new worktrees follow it. Neither changes the relative
/// order the user chose for existing destinations.
pub(crate) fn apply_navigation_order<T>(
    rows: &mut [T],
    order: &[crate::workspace_model::NavigationItem],
    item: impl Fn(&T) -> crate::workspace_model::NavigationItem,
) {
    apply_partial_navigation_order(rows, order, |row| Some(item(row)));
}

/// [`apply_navigation_order`] for rows some of which name no destination,
/// such as a pane whose checkout is unknown. Those keep their relative order
/// after every row that names one.
fn apply_partial_navigation_order<T>(
    rows: &mut [T],
    order: &[crate::workspace_model::NavigationItem],
    item: impl Fn(&T) -> Option<crate::workspace_model::NavigationItem>,
) {
    if order.is_empty() || rows.len() < 2 {
        return;
    }
    let ranks: std::collections::HashMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(rank, item)| (item, rank))
        .collect();
    rows.sort_by_cached_key(|row| {
        let Some(item) = item(row) else {
            return (3, 0);
        };
        match ranks.get(&item) {
            Some(rank) => (1, *rank),
            None if matches!(item, crate::workspace_model::NavigationItem::Channel(_)) => (0, 0),
            None => (2, 0),
        }
    });
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
        // A pane row stands where its checkout does.
        SelectionRow::Card(id) => {
            let worktree = project.panes.iter().find(|card| card.id == id)?.worktree?;
            project
                .worktrees
                .iter()
                .find(|row| row.id == worktree)
                .map(|row| row.path.clone())
                .or_else(|| project.folder_key().map(|key| key.canonical_path))
                .map(NavigationItem::Worktree)
        }
        SelectionRow::Project(_) => None,
    }
}

fn navigation_search_ranks(
    project: &SidebarProject,
    query: &str,
) -> std::collections::HashMap<SelectionRow, usize> {
    project
        .channels
        .iter()
        .map(|channel| {
            (
                SelectionRow::Channel(channel.id),
                search_score(&channel.folded_title, query).unwrap_or(usize::MAX),
            )
        })
        .chain(project.worktrees.iter().map(|worktree| {
            (
                SelectionRow::Worktree(project.id, worktree.id),
                worktree.search_rank(query).unwrap_or(usize::MAX),
            )
        }))
        .chain(project.panes.iter().map(|card| {
            (
                SelectionRow::Card(card.id),
                card.search_rank(project, query).unwrap_or(usize::MAX),
            )
        }))
        .collect()
}

/// The rail has room only for rows worth a glance: a search's matches, where
/// the user is, what the Attention list holds (news, and an agent a usage
/// limit still stops after its news is read), live agents, deletions, and
/// rows the Attention list keeps in place. Drawing and keyboard order both
/// ask this, so the two cannot disagree about which rows exist.
fn compact_rail_shows_channel(state: &SidebarState, channel: &SidebarChannel) -> bool {
    // The Attention list holds only channels worth a glance.
    state.filter == SidebarFilter::Attention
        || compact_rail_searching(state)
        || channel.selected
        || channel.needs_attention()
        || channel.activity.is_some()
        || channel.deletion.is_some()
        || state.retained_in_attention(SelectionRow::Channel(channel.id))
}

/// [`compact_rail_shows_channel`], for a worktree. A worktree with open panes
/// counts as where the user is: the rail is how they switch back to it.
fn compact_rail_shows_worktree(
    state: &SidebarState,
    project: ProjectId,
    worktree: &SidebarWorktree,
) -> bool {
    compact_rail_searching(state)
        || state.drawn_selection()
            == Some(SidebarSelection::Worktree {
                project_id: project,
                worktree_id: worktree.id,
            })
        || state.has_agent_update(project, worktree.id)
        || state.worktree_has_panes(project, worktree.id)
        || state.agent_activity(project, worktree.id).is_some()
        || worktree.deletion.is_some()
}

/// The rail searches only while its popover holds the query.
fn compact_rail_searching(state: &SidebarState) -> bool {
    state.controls_expanded && !state.query.trim().is_empty()
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
                    .filter(|channel| !compact || compact_rail_shows_channel(state, channel))
                    .map(|channel| SelectionRow::Channel(channel.id)),
            );
            rows.extend(
                project
                    .worktrees
                    .iter()
                    .filter(|worktree| worktree.deletion != Some(RowDeletion::Deleting))
                    .filter(|worktree| state.shows_worktree(project.project, worktree))
                    .filter(|worktree| {
                        !compact || compact_rail_shows_worktree(state, project.project.id, worktree)
                    })
                    .map(|worktree| SelectionRow::Worktree(project.project.id, worktree.id)),
            );
            rows.extend(project.cards.iter().map(|card| SelectionRow::Card(card.id)));
            if compact {
                if searching {
                    let ranks = navigation_search_ranks(project.project, &fold(state.query.trim()));
                    rows[first..]
                        .sort_by_cached_key(|row| ranks.get(row).copied().unwrap_or(usize::MAX));
                }
                if state.worktree_display(project.project.id)
                    != crate::settings::SidebarWorktrees::Expanded
                {
                    let primary = primary_worktree_id(project.project);
                    rows[first..].sort_by_key(|row| member_is_grouped(primary, *row));
                }
            } else {
                ordered_members(&mut rows[first..], project.project, state, |row| *row);
            }
        }
    }
    if !state.in_progress_collapsed {
        rows.extend(
            in_progress_rows(model, state, compact)
                .into_iter()
                .map(InProgressRow::row),
        );
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
                return (project_matches || !project.operations.is_empty()).then_some(
                    FilteredProject {
                        project,
                        worktrees: Vec::new(),
                        channels: Vec::new(),
                        cards: Vec::new(),
                        editors: &model.external_editors,
                        state,
                    },
                );
            }
            let mut channels =
                channels_in_display_order(project.channels.iter().filter(|channel| {
                    (project_matches
                        || search_score(&channel.folded_title, query.as_ref()).is_some())
                        && match state.filter {
                            SidebarFilter::All => true,
                            SidebarFilter::Attention => state.lists_channel(channel),
                            SidebarFilter::PullRequests => false,
                        }
                }));
            // The Attention list names the panes needing the user instead of
            // their checkouts; hidden checkouts give way to every open pane.
            let lists_panes = state.lists_panes(project);
            let mut cards = if state.filter == SidebarFilter::Attention || lists_panes {
                cards_in_display_order(
                    project,
                    state.worktree_sort,
                    project.panes.iter().filter(|card| {
                        (lists_panes || state.lists_card(card))
                            && (project_matches
                                || card.search_rank(project, query.as_ref()).is_some())
                    }),
                )
            } else {
                Vec::new()
            };
            let shows_checkouts = state.shows_checkouts(project.id);
            let mut worktrees = worktrees_in_display_order(
                &project.repository,
                state.worktree_sort,
                project
                    .worktrees
                    .iter()
                    .filter(|_| shows_checkouts)
                    .filter(|worktree| match state.filter {
                        SidebarFilter::All => true,
                        SidebarFilter::PullRequests => {
                            state.selected_worktree(project) == Some(worktree.id)
                                || worktree.pull_request().is_some()
                        }
                        SidebarFilter::Attention => false,
                    })
                    .filter(|worktree| {
                        state.selected_worktree(project) == Some(worktree.id)
                            || project_matches
                            || worktree.matches(query.as_ref())
                    }),
            );
            if !query.is_empty() {
                channels.sort_by_key(|channel| {
                    search_score(&channel.folded_title, &query).unwrap_or(usize::MAX)
                });
                worktrees
                    .sort_by_key(|worktree| worktree.search_rank(&query).unwrap_or(usize::MAX));
                cards.sort_by_key(|card| card.search_rank(project, &query).unwrap_or(usize::MAX));
            }
            let visible = match state.filter {
                SidebarFilter::All => {
                    project_matches
                        || !worktrees.is_empty()
                        || !channels.is_empty()
                        || !cards.is_empty()
                }
                SidebarFilter::PullRequests => {
                    (project_matches && project.github_ready) || !worktrees.is_empty()
                }
                SidebarFilter::Attention => !channels.is_empty() || !cards.is_empty(),
            };
            (visible || !project.operations.is_empty()).then_some(FilteredProject {
                project,
                worktrees,
                channels,
                cards,
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
                .chain(
                    project
                        .cards
                        .iter()
                        .filter_map(|card| card.search_rank(project.project, &query)),
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
/// sorts where its real rank will put it. Equal assigned ranks keep their
/// source order. At the pending sentinel, the later insertion is newer until
/// an authoritative read supplies distinct ranks.
pub(crate) fn channels_in_display_order<'a>(
    rows: impl IntoIterator<Item = &'a SidebarChannel>,
) -> Vec<&'a SidebarChannel> {
    // A folded remote create can still carry the pending sentinel. Its stable
    // insertion position is the only creation order known until the read.
    let mut rows = rows.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|channel| std::cmp::Reverse(channel.ordinal));
    let pending = rows
        .iter()
        .take_while(|channel| channel.ordinal == crate::workspace_model::PENDING_ORDINAL)
        .count();
    rows[..pending].reverse();
    rows
}

/// Pane rows in the order their checkouts are drawn: the primary worktree
/// first, then the worktree order the user chose. Panes in one checkout keep
/// their workspace order, and a pane whose checkout is unknown comes last.
fn cards_in_display_order<'a>(
    project: &'a SidebarProject,
    sort: crate::settings::WorktreeSort,
    cards: impl IntoIterator<Item = &'a SidebarCard>,
) -> Vec<&'a SidebarCard> {
    let mut cards = cards.into_iter().collect::<Vec<_>>();
    if cards.len() > 1 {
        let positions: std::collections::HashMap<_, _> =
            worktrees_in_display_order(&project.repository, sort, &project.worktrees)
                .into_iter()
                .enumerate()
                .map(|(position, worktree)| (worktree.id, position))
                .collect();
        // A pane with no checkout row to follow - a Folder project's root, or a
        // conversation split across checkouts - comes after those that have.
        cards.sort_by_key(|card| {
            card.worktree
                .and_then(|id| positions.get(&id).copied())
                .unwrap_or(usize::MAX)
        });
    }
    cards
}

/// One row of the Attention list's "In progress" section: a channel or a
/// pane whose agents are working and that needs nothing from the user.
#[derive(Debug, Clone, Copy)]
struct InProgressRow<'a> {
    project: &'a SidebarProject,
    item: InProgressItem<'a>,
}

#[derive(Debug, Clone, Copy)]
enum InProgressItem<'a> {
    Channel(&'a SidebarChannel),
    Card(&'a SidebarCard),
}

impl InProgressRow<'_> {
    /// Where the row is, as its suffix and accessible name say: the project,
    /// and for a pane the checkout it runs in.
    fn place(self) -> String {
        let project = &self.project.name;
        match self.item {
            InProgressItem::Card(card) => match card.checkout(self.project) {
                Some(checkout) => format!("{project} › {checkout}"),
                None => project.clone(),
            },
            InProgressItem::Channel(_) => project.clone(),
        }
    }

    fn attention(self) -> AttentionSummary {
        match self.item {
            InProgressItem::Channel(channel) => channel.attention,
            InProgressItem::Card(card) => card.attention,
        }
    }

    fn row(self) -> SelectionRow {
        match self.item {
            InProgressItem::Channel(channel) => SelectionRow::Channel(channel.id),
            InProgressItem::Card(card) => SelectionRow::Card(card.id),
        }
    }
}

/// Where the list draws a row, folded or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneRowPlace {
    /// Under its project: in the Attention list's main section, or in a
    /// project whose checkouts are hidden.
    Project(ProjectId),
    /// In the Attention list's In progress section.
    InProgress,
}

/// Which section of the list holds `row`, before any project or the In
/// progress section is folded; `None` when the list does not draw it, under
/// the current filter and search.
pub fn pane_row_place(
    model: &SidebarModel,
    state: &SidebarState,
    compact: bool,
    row: SelectionRow,
) -> Option<PaneRowPlace> {
    filtered_projects(model, state, compact)
        .iter()
        .find(|project| match row {
            SelectionRow::Card(card) => project.cards.iter().any(|row| row.id == card),
            SelectionRow::Channel(channel) => project.channels.iter().any(|row| row.id == channel),
            SelectionRow::Project(_) | SelectionRow::Worktree(..) => false,
        })
        .map(|project| PaneRowPlace::Project(project.project.id))
        .or_else(|| {
            in_progress_rows(model, state, compact)
                .into_iter()
                .any(|candidate| candidate.row() == row)
                .then_some(PaneRowPlace::InProgress)
        })
}

/// The Attention list's "In progress" rows, in project order and then each
/// project's own row order. A row here is never also in the main list: that
/// one holds whatever needs the user or was dealt with there.
fn in_progress_rows<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    compact: bool,
) -> Vec<InProgressRow<'a>> {
    if state.filter != SidebarFilter::Attention {
        return Vec::new();
    }
    let query = if compact && !state.controls_expanded {
        fold("")
    } else {
        fold(state.query.trim())
    };
    let mut rows = Vec::new();
    for project in model
        .projects
        .iter()
        .filter(|project| project.unreachable.is_none())
    {
        let project_matches = query.is_empty() || project.matches(query.as_ref());
        let matches =
            |title: &str| project_matches || search_score(title, query.as_ref()).is_some();
        rows.extend(
            channels_in_display_order(project.channels.iter().filter(|channel| {
                channel.deletion != Some(RowDeletion::Deleting)
                    && channel.attention.only_working()
                    && !state.lists_channel(channel)
                    && matches(&channel.folded_title)
            }))
            .into_iter()
            .map(|channel| InProgressRow {
                project,
                item: InProgressItem::Channel(channel),
            }),
        );
        rows.extend(
            cards_in_display_order(
                project,
                state.worktree_sort,
                project.panes.iter().filter(|card| {
                    card.attention.only_working()
                        && !state.lists_card(card)
                        && (project_matches || card.search_rank(project, query.as_ref()).is_some())
                }),
            )
            .into_iter()
            .map(|card| InProgressRow {
                project,
                item: InProgressItem::Card(card),
            }),
        );
    }
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

/// A search query in the form row matchers compare it in.
pub(crate) fn fold_query(query: &str) -> Box<str> {
    fold(query.trim())
}

/// Case-folds a filter key. Entities store the result so the free-text filter
/// never folds a candidate while rendering.
fn fold(value: &str) -> Box<str> {
    value.case_fold().collect::<String>().into_boxed_str()
}

/// Match names first; ordered abbreviations allow narrow searches without
/// allocating another folded candidate on each frame.
fn search_score(candidate: &str, query: &str) -> Option<usize> {
    crate::search_match::score(candidate, query)
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
    }
    // Subsequence matching is useful for discovery, but scattered emphasis is
    // noise: only a contiguous match earns visual highlighting.
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
    let (font, emphasis) = search_label_fonts(heading);
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
    let label = label.to_owned();
    let query = query.to_owned();
    let fitted = std::cell::RefCell::new(None::<(u32, String)>);
    crate::ui_controls::responsive(move |slot| {
        // Measure the exact span/font partition before truncation: the pinned
        // renderer cannot ellipsize spans. Reuse the fit until the slot changes.
        let label = {
            let mut fitted = fitted.borrow_mut();
            if fitted
                .as_ref()
                .is_none_or(|(width, _)| *width != slot.width.to_bits())
            {
                *fitted = Some((
                    slot.width.to_bits(),
                    ellipsize_search_label(&label, slot.width, |candidate| {
                        search_spans_width(candidate, &query, size, font, emphasis)
                    }),
                ));
            }
            fitted.as_ref().expect("fitted label").1.clone()
        };
        let spans = search_label_spans(&label, &query, emphasis);
        widget::container(
            cosmic::iced::widget::rich_text(spans)
                .size(size)
                .font(font)
                .class(class)
                .wrapping(cosmic::iced::widget::text::Wrapping::None)
                .width(Length::Fill),
        )
        .width(Length::Fill)
        .clip(true)
        .into()
    })
    .width(Length::Fill)
    // As tall as its text, so a highlighted title sits on the same pixel
    // row as a plain one rather than blurred between two.
    .height(Length::Shrink)
    .into()
}

/// A search label's font, and the one its matches are drawn in.
fn search_label_fonts(heading: bool) -> (cosmic::iced::Font, cosmic::iced::Font) {
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
    (font, emphasis)
}

fn search_label_spans(
    label: &str,
    query: &str,
    emphasis: cosmic::iced::Font,
) -> Vec<cosmic::iced::widget::text::Span<'static, ()>> {
    use cosmic::iced::widget::text::Span;
    let mut spans = Vec::new();
    let mut cursor = 0;
    for range in search_highlight_ranges(label, query) {
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
    spans
}

fn search_spans_width(
    label: &str,
    query: &str,
    size: f32,
    font: cosmic::iced::Font,
    emphasis: cosmic::iced::Font,
) -> f32 {
    use cosmic::iced::{
        Pixels, Size,
        advanced::text::{self, Paragraph as _},
    };
    let spans = search_label_spans(label, query, emphasis);
    let paragraph = <cosmic::Renderer as text::Renderer>::Paragraph::with_spans(text::Text {
        content: &spans,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(size),
        line_height: text::LineHeight::Relative(1.5),
        font,
        align_x: text::Alignment::Left,
        align_y: cosmic::iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    paragraph.min_bounds().width
}

fn ellipsize_search_label(label: &str, width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(label) <= width {
        return label.to_owned();
    }
    if measure("…") > width {
        return String::new();
    }
    let boundaries: Vec<_> = label.char_indices().map(|(index, _)| index).collect();
    let mut low = 0;
    let mut high = boundaries.len();
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate = format!("{}…", &label[..boundaries[middle - 1]]);
        if measure(&candidate) <= width {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    format!("{}…", &label[..boundaries[low.saturating_sub(1)]])
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
    use crate::interaction::PinnedHeaders;
    use cosmic::iced::advanced::Renderer as _;
    use cosmic::iced::{
        Event, Length, Point, Rectangle, Size, Vector,
        advanced::{
            Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
            widget::{Id, Operation, Tree, operation, tree},
        },
        event::{PlatformSpecific, wayland},
        keyboard, mouse, window,
    };
    use cosmic::{Element, Renderer, Theme};
    use std::{cell::Cell, rc::Rc};

    /// Whether `event` is one every widget in the window receives on each
    /// frame or message: a redraw, the runtime's resize request, or pointer
    /// motion. None of these moves focus.
    ///
    /// A row scrolled out of the list's view can let these pass it by. It
    /// can't be under the pointer, and either isn't drawn or, as a member
    /// passing under its group's pinned header, is drawn hidden by it.
    /// Whatever its widgets would note from these, such as their hover
    /// status, they note from the redraw that precedes their next visible
    /// draw. Keys, modifier changes, presses and releases still reach every
    /// row, since a focused or pressed row may be scrolled away.
    fn routine(event: &Event) -> bool {
        matches!(
            event,
            Event::Window(window::Event::RedrawRequested(_))
                | Event::Mouse(mouse::Event::CursorMoved { .. })
                | Event::PlatformSpecific(PlatformSpecific::Wayland(
                    wayland::Event::RequestResize | wayland::Event::Frame(..)
                ))
        )
    }

    /// One coordinate shared by the list and its groups, refreshed on each widget
    /// callback, including operations/accessibility before the next draw.
    #[derive(Clone, Default)]
    pub(super) struct StickyViewport(Rc<Cell<f32>>, Rc<Cell<Option<f32>>>);

    impl StickyViewport {
        fn content_top(&self, layout: Layout<'_>) -> f32 {
            self.1.get().unwrap_or(layout.bounds().y) + layout.virtual_offset().y
        }
    }

    #[cfg(test)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum HeaderPass {
        Draw,
        Drag,
    }

    #[cfg(test)]
    std::thread_local! {
        static HEADER_GEOMETRY: std::cell::RefCell<Option<(HeaderPass, Vec<Rectangle>)>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub(super) fn capture_headers(pass: HeaderPass, collect: impl FnOnce()) -> Vec<Rectangle> {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                HEADER_GEOMETRY.with(|headers| {
                    headers.replace(None);
                });
            }
        }
        HEADER_GEOMETRY.with(|headers| {
            assert!(headers.replace(Some((pass, Vec::new()))).is_none());
        });
        let _reset = Reset;
        collect();
        HEADER_GEOMETRY.with(|headers| headers.take().unwrap().1)
    }

    #[cfg(test)]
    fn record_header(pass: HeaderPass, layout: Layout<'_>) {
        HEADER_GEOMETRY.with(|headers| {
            if let Some((current, headers)) = headers.borrow_mut().as_mut()
                && *current == pass
            {
                headers.push(layout.bounds() - layout.virtual_offset());
            }
        });
    }

    /// Widget-operation context is scoped to members of this actual group.
    /// Header targets themselves do not inherit their own sticky obstruction.
    pub(super) enum RevealInset {
        Enter(f32),
        Exit,
    }

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
        WorktreeDisclosure {
            project: ProjectId,
            enabled: bool,
        },
        Selectable {
            row: super::SelectionRow,
            /// This row is in the explicit multi-selection.
            selected: bool,
            /// Some row is, so Escape has a selection to clear.
            any_selected: bool,
            enabled: bool,
        },
        FilteredScroll {
            filter: SidebarFilter,
            viewport: StickyViewport,
        },
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
        /// Where the row's own target sits, relative to the row, so that it
        /// follows the row when a pinned header or the list moves it.
        selection_area: Option<Rectangle>,
        origin: Option<(super::SelectionRow, Point)>,
        dragging: bool,
        drop: Option<(Option<super::SelectionRow>, f32)>,
        hovered: bool,
        focused: bool,
        /// Whether any control in the row has focus, such as its delete
        /// button, not only the row's own target.
        focus_within: bool,
        /// Whether `focused`, `focus_within` and `selection_area` are
        /// current: they are found again after each event that may move
        /// focus and each operation, and lapse when the row is rebuilt or
        /// laid out again.
        focus_known: bool,
        /// Whether the last layout narrowed the base row for its actions.
        revealed: bool,
        /// Whether something in the row, such as a menu, was open the last
        /// time the window collected overlays.
        overlaid: Cell<bool>,
        header_node: Option<layout::Node>,
        scroll: Option<Box<FilterScrollState>>,
    }

    impl State {
        /// Where the row's own target is drawn in `layout`.
        fn selection_bounds(&self, layout: Layout<'_>) -> Option<Rectangle> {
            self.selection_area
                .map(|area| area + (layout.position() - Point::ORIGIN))
        }
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
    struct FocusWithin(bool, Option<Id>, Option<Rectangle>, bool);

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
            self.3 |= state.is_focused();
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
        pub(super) fn worktree_disclosure(
            content: Element<'a, SidebarAction>,
            project: ProjectId,
            enabled: bool,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::WorktreeDisclosure { project, enabled },
            })
        }

        pub(super) fn filtered_scroll(
            content: Element<'a, SidebarAction>,
            filter: SidebarFilter,
            viewport: StickyViewport,
        ) -> Element<'a, SidebarAction> {
            Element::new(Self {
                children: vec![content],
                kind: Kind::FilteredScroll { filter, viewport },
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

        fn capture_viewport_origin(&self, layout: Layout<'_>) {
            if let Kind::FilteredScroll { viewport, .. } = &self.kind {
                // The scrollable is this wrapper's direct child. Its viewport
                // origin is distinct from the padded project content origin.
                viewport.1.set(Some(layout.child(0).bounds().y));
            }
        }

        fn focus(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
            let mut focus = FocusWithin::default();
            match self.kind {
                Kind::Selectable { row, .. } => focus.1 = Some(super::selection_row_focus_id(row)),
                Kind::WorktreeDisclosure { project, .. } => {
                    focus.1 = Some(super::worktree_group_focus_id(project))
                }
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
            state.selection_area = focus
                .2
                .map(|bounds| bounds - (layout.position() - Point::ORIGIN));
            state.focus_within = focus.3;
            state.focus_known = true;
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
                    Kind::FilteredScroll { filter: active, .. } => {
                        Some(Box::new(FilterScrollState {
                            active,
                            saved: [None, None, None],
                        }))
                    }
                    _ => None,
                },
                ..State::default()
            })
        }

        fn children(&self) -> Vec<Tree> {
            self.children.iter().map(Tree::new).collect()
        }

        fn diff(&mut self, tree: &mut Tree) {
            if let Kind::FilteredScroll { filter, .. } = self.kind {
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
            tree.state.downcast_mut::<State>().focus_known = false;
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
            // Where the row's target sits may change with the layout.
            tree.state.downcast_mut::<State>().focus_known = false;
            // Overlay controls are on screen only while the row is hovered or
            // focused, so only then does the base row lose the width they
            // cover. Every other frame the title is measured against the whole
            // row, which is what keeps a reservation out of the resting state.
            let reserve = match self.kind {
                Kind::Actions { reserve } if reserve > 0.0 => {
                    let state = tree.state.downcast_mut::<State>();
                    state.revealed = state.hovered || state.focused;
                    if state.revealed { reserve } else { 0.0 }
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
            self.capture_viewport_origin(layout);
            if let Kind::Reorder {
                viewport, project, ..
            } = &self.kind
            {
                viewport.0.set(viewport.content_top(layout));
                // The list's top fade keeps clear of its groups' headers.
                if project.is_none() {
                    operation.custom(None, layout.bounds(), &mut PinnedHeaders::List);
                }
            }
            if matches!(self.kind, Kind::Group { .. }) {
                operation.custom(None, layout.bounds(), &mut PinnedHeaders::Group);
            }
            if matches!(self.kind, Kind::Group { .. })
                && let Some(header) =
                    child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout).next()
            {
                crate::reorder::block_channel_drop(
                    operation,
                    header.bounds() - header.virtual_offset(),
                );
            }
            operation.container(None, layout.bounds());
            operation.traverse(&mut |operation| {
                let header_height = matches!(self.kind, Kind::Group { .. }).then(|| {
                    tree.state
                        .downcast_ref::<State>()
                        .header_node
                        .as_ref()
                        .expect("group has been laid out")
                        .size()
                        .height
                });
                for (index, ((child, tree), layout)) in self
                    .children
                    .iter_mut()
                    .zip(&mut tree.children)
                    .zip(child_layouts(
                        &self.kind,
                        tree.state.downcast_ref::<State>(),
                        layout,
                    ))
                    .enumerate()
                {
                    let inset = header_height.filter(|_| index == 1);
                    if let Some(height) = inset {
                        operation.custom(None, layout.bounds(), &mut RevealInset::Enter(height));
                    }
                    child
                        .as_widget_mut()
                        .operate(tree, layout, renderer, operation);
                    if inset.is_some() {
                        operation.custom(None, layout.bounds(), &mut RevealInset::Exit);
                    }
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
            // Without this, every frame and every message walks each of a
            // long list's rows, however few are in view. A
            // row that last saw the pointer, or that a drag started in, still
            // hears the event that tells it the pointer has gone. A row with
            // focus anywhere in it keeps every frame: its focused control may
            // need redraws, such as an input placing its input method, and a
            // focus moved here by an operation reveals the row's actions on
            // the next one. Containers such as a group don't track focus, which
            // would walk their members on each event; a focused member of a
            // group out of view hears the redraw that brings it back into view
            // before it is drawn.
            // So does a row whose tooltip or menu is still on screen: a
            // tooltip fades out over the frames after the pointer leaves, and
            // would stay frozen there without them.
            let routine = routine(event);
            let state = tree.state.downcast_ref::<State>();
            if !layout.bounds().intersects(viewport)
                && !state.hovered
                && !state.focus_within
                && !state.overlaid.get()
                && state.origin.is_none()
                && routine
            {
                return;
            }
            // Finding focus walks the row's controls, so a frame that moves
            // no focus relies on what the last event or operation found.
            if !routine || !state.focus_known {
                self.focus(tree, layout, renderer);
            }
            if let Kind::Reorder {
                viewport: sticky, ..
            } = &self.kind
            {
                sticky.0.set(viewport.y);
            }
            let reserves = matches!(self.kind, Kind::Actions { reserve } if reserve > 0.0);
            let state = tree.state.downcast_mut::<State>();
            // A focus operation moves focus without a shell to ask for the
            // relayout that reveals the actions, so lay out what the last
            // layout did not show on the next event.
            if reserves && (state.hovered || state.focused) != state.revealed {
                shell.invalidate_layout();
            }
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
            if let Kind::WorktreeDisclosure {
                project,
                enabled: true,
            } = self.kind
                && state.focused
                && let Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(space),
                    modifiers,
                    ..
                }) = event
                && space.as_str() == " "
                && modifiers.is_empty()
            {
                shell.publish(SidebarAction::ToggleWorktrees(project));
                shell.capture_event();
                return;
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
                                .selection_bounds(layout)
                                .is_some_and(|bounds| cursor.is_over(bounds)) =>
                    {
                        // A plain click is navigation: the row's own button
                        // opens it, and the selection only learns the anchor.
                        // A pane row never joins a selection, so a modified
                        // click on one simply opens it.
                        let intent = if !row.selectable() {
                            super::SelectionIntent::Navigate
                        } else if state.modifiers.shift() {
                            super::SelectionIntent::Range { extend: additive }
                        } else if additive {
                            super::SelectionIntent::Toggle
                        } else {
                            super::SelectionIntent::Navigate
                        };
                        shell.publish(SidebarAction::SelectRows { row, intent });
                        if intent != super::SelectionIntent::Navigate {
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
                        // A pane row names a pane rather than owning it;
                        // closing panes belongs to the workspace, not here.
                        keyboard::key::Named::Delete | keyboard::key::Named::Backspace
                            if row.selectable() =>
                        {
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
                        if let Some((project, _)) = state.origin.take()
                            && state.dragging
                        {
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
            if !routine {
                let focused = tree.state.downcast_ref::<State>().focused;
                self.focus(tree, layout, renderer);
                if reserves && tree.state.downcast_ref::<State>().focused != focused {
                    shell.invalidate_layout();
                }
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
            // The pointer can't be over a row out of view.
            if !layout.bounds().intersects(viewport) {
                return mouse::Interaction::None;
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
                // How far the header has travelled from its place in the list:
                // rows are passing under it while it is pinned.
                let pinned = header.bounds().y - layout.bounds().y;
                if pinned > 0.0 {
                    // What passes under the header recedes into it rather
                    // than being cut at its edge. The fade grows over the
                    // first stretch of travel, so it never pops in.
                    let area = Rectangle {
                        y: header.bounds().y + header.bounds().height,
                        height: pinned.min(crate::interaction::OVERFLOW_FADE),
                        ..header.bounds()
                    };
                    if let Some(clip) = area
                        .intersection(&layout.bounds())
                        .and_then(|area| area.intersection(viewport))
                    {
                        renderer.with_layer(clip, |renderer| {
                            crate::interaction::fade_below_header(
                                renderer,
                                area,
                                super::pinned_header_fill(theme),
                            );
                        });
                    }
                }
                if let Some(clip) = header.bounds().intersection(viewport) {
                    renderer.with_layer(clip, |renderer| {
                        if pinned > 0.0 {
                            renderer.fill_quad(
                                renderer::Quad {
                                    bounds: header.bounds(),
                                    ..Default::default()
                                },
                                super::pinned_header_fill(theme),
                            );
                        }
                        // Witness the actual header draw under the scrollable's translation.
                        #[cfg(test)]
                        record_header(HeaderPass::Draw, header);
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
                let bounds = state.selection_bounds(layout).unwrap_or(layout.bounds());
                if let Some(clip) = bounds.intersection(viewport) {
                    renderer.with_layer(clip, |renderer| {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds,
                                border: cosmic::iced::Border {
                                    radius: theme.cosmic().corner_radii.radius_s.into(),
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
            if let Some((_, y)) = state.drop
                && let Some(clip) = layout.bounds().intersection(viewport)
            {
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
            // The window collects overlays twice a frame, so a long list
            // must not ask each row it has scrolled away. Such a row has
            // nothing to show unless something it had open, such as a menu,
            // is still open as it leaves.
            if !(layout.bounds() + translation).intersects(viewport)
                && !state.overlaid.get()
                && !state.hovered
                && !state.focus_within
            {
                return None;
            }
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
            tree.state
                .downcast_ref::<State>()
                .overlaid
                .set(!overlays.is_empty());
            (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
        }

        fn a11y_nodes(
            &self,
            layout: Layout<'_>,
            tree: &Tree,
            cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            self.capture_viewport_origin(layout);
            if let Kind::Reorder { viewport, .. } = &self.kind {
                viewport.0.set(viewport.content_top(layout));
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
            // Descendants received zero ancestor offset above, so their bounds
            // are still in list coordinates even when a button normally applies
            // its virtual offset. Translate once at this outer list boundary.
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
            self.capture_viewport_origin(layout);
            if let Kind::Reorder { viewport, .. } = &self.kind {
                viewport.0.set(viewport.content_top(layout));
            }
            #[cfg(test)]
            if matches!(self.kind, Kind::Group { .. })
                && let Some(header) =
                    child_layouts(&self.kind, tree.state.downcast_ref::<State>(), layout).next()
            {
                record_header(HeaderPass::Drag, header);
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
pub(crate) mod tests;

/// Focus a navigation row and reveal it within the Projects scroll viewport.
pub fn focus_and_reveal<T: Send + 'static>(id: Id) -> cosmic::iced::Task<T> {
    reveal_navigation_row(id, true)
}

/// Reveal a newly selected destination without taking focus from its composer.
pub fn reveal<T: Send + 'static>(id: Id) -> cosmic::iced::Task<T> {
    reveal_navigation_row(id, false)
}

fn reveal_navigation_row<T: Send + 'static>(id: Id, focus: bool) -> cosmic::iced::Task<T> {
    use cosmic::iced::advanced::widget::{Operation, operation};
    struct FocusRow {
        id: Id,
        bounds: Option<cosmic::iced::Rectangle>,
        focus: bool,
        insets: Vec<f32>,
        top_inset: f32,
    }
    impl<T: 'static> Operation<T> for FocusRow {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&Id>,
            _: cosmic::iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            match state.downcast_ref::<row_interaction::RevealInset>() {
                Some(row_interaction::RevealInset::Enter(height)) => self.insets.push(*height),
                Some(row_interaction::RevealInset::Exit) => {
                    self.insets.pop();
                }
                None => {}
            }
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: cosmic::iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.id) {
                if self.focus {
                    state.focus();
                }
                self.bounds = Some(bounds);
                self.top_inset = self.insets.last().copied().unwrap_or_default();
            } else if self.focus {
                state.unfocus();
            }
        }
        fn finish(&self) -> operation::Outcome<T> {
            self.bounds.map_or(operation::Outcome::None, |bounds| {
                operation::Outcome::Chain(Box::new(RevealRow {
                    bounds,
                    top_inset: self.top_inset,
                }))
            })
        }
    }
    struct RevealRow {
        bounds: cosmic::iced::Rectangle,
        top_inset: f32,
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
            if id != Some(&stable_id("projects.scroll")) {
                return;
            }
            let row = self.bounds - translation;
            let delta = reveal_scroll(
                row.y - bounds.y..row.y - bounds.y + row.height,
                self.top_inset,
                bounds.height,
                content.height,
                translation.y,
            ) - translation.y;
            if delta.abs() > 0.01 {
                state.scroll_by(
                    operation::scrollable::AbsoluteOffset { x: 0.0, y: delta },
                    bounds,
                    content,
                );
            }
        }
    }
    cosmic::iced::advanced::widget::operate(FocusRow {
        id,
        bounds: None,
        focus,
        insets: Vec::new(),
        top_inset: 0.0,
    })
}

/// The scroll offset that brings `row`, measured from the top of the
/// project list's viewport, into view, with `inset` at the viewport's top
/// held by a pinned header. Rows already in view stay where they are.
///
/// Both edges fade while rows lie beyond them: the top under a pinned header
/// or at the viewport's own edge, the bottom into the pane. The row is
/// revealed clear of either fade, sharing what room a short list has, and the
/// clamp to the list's ends lets the first and last rows sit flush where
/// nothing lies beyond and nothing fades.
fn reveal_scroll(
    row: std::ops::Range<f32>,
    inset: f32,
    viewport: f32,
    content: f32,
    offset: f32,
) -> f32 {
    let height = row.end - row.start;
    let top = inset.min((viewport - height).max(0.0));
    let margin = crate::interaction::OVERFLOW_FADE.min(((viewport - top - height) / 2.0).max(0.0));
    let delta = if row.start - margin < top {
        row.start - margin - top
    } else if row.end + margin > viewport {
        row.end + margin - viewport
    } else {
        0.0
    };
    (offset + delta).clamp(0.0, (content - viewport).max(0.0))
}
