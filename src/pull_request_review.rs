//! One pull request under review: the files it changes, which of them the
//! viewer has already looked at, and the fetched Git range each diff renders
//! from.
//!
//! Both shells mount this. The cagents pull request card puts the file list
//! beside a standalone repository runtime; ceditor puts it in the sidebar next
//! to the tab's runtime. Neither knows how a viewed mark reaches GitHub or how
//! a pull request's objects reach the local repository: they hand the
//! [`State`] a target, forward its actions, and drain the [`Effect`]s it asks
//! them to carry out.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use cosmic::{
    Element,
    iced::{self, Alignment, Length, Padding, advanced::text::EllipsizeHeightLimit},
    widget::{self, button},
};
use url::Url;

use crate::{
    design::{self, ROW_ICON_SIZE, space},
    github::{
        FileViewedState, GitHubRepoKey, GitHubReviewApi, PullRequest, PullRequestFiles,
        PullRequestKey, PullRequestPage, ReviewContext, ReviewMutationOutcome,
        remote_for_repository,
    },
    repository::{
        CancellationToken, ChangeKind, DiffBasis, DiffEntry, Oid, PullRequestRange, RepoPath,
        RepositoryError, RepositoryService, ValidatedBranchName, build_file_tree,
        line_change_counts,
    },
    sidebar::{CheckStatus, FileListMode, WorktreePullRequestState, sidebar_button_style},
    ui_controls::{Icon, icon_button},
};

type ActionTask = iced::Task<Action>;

pub mod cache;
pub mod checkout;
pub mod discussion;
pub mod discussion_ui;
pub mod file_pages;
pub mod request_ui;

/// Indent per tree level, matching the sidebar's file tree.
const TREE_INDENT: u16 = 12;

/// The slot an icon sits in, matching the sidebar's rows.
const ROW_ICON_WIDTH: f32 = 20.0;

/// How many lines a pull request title may wrap to before it is elided.
const SUMMARY_TITLE_LINES: usize = 3;

/// The pull request a shell asked to review, and where its objects go.
///
/// The `PullRequest` inside is the row the user clicked. Its head may already
/// be stale by the time the review loads; the load re-reads the head from
/// GitHub and fetches that, so the range always matches what GitHub's viewed
/// marks describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub repository: GitHubRepoKey,
    pub worktree: PathBuf,
    pub pull_request: Arc<PullRequest>,
}

/// A checkout always starts from the exact commit shown by the review. The
/// private fetch ref can move when another window refreshes the same PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutTarget {
    pub repository: PathBuf,
    pub number: u64,
    pub head: crate::repository::Oid,
}

impl CheckoutTarget {
    pub fn suggested_branch(&self) -> String {
        format!("pr/{}-{}", self.number, &self.head.as_str()[..8])
    }
}

/// Whether a file's local viewed mark matches GitHub's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewedSync {
    /// This file's remote viewed state has not been fetched.
    Unknown,
    /// GitHub agrees with the mark shown.
    Settled,
    /// A mutation is in flight; the mark shown is the optimistic one.
    Pending,
    /// The mutation's outcome could not be read. The mark shown is what was
    /// asked for; the next refresh reconciles it.
    Unverified,
}

/// One changed file with its review bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub entry: DiffEntry,
    pub viewed: bool,
    pub sync: ViewedSync,
}

/// Everything one load produces, so a stale load can be dropped whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub pull_request: PullRequest,
    pub context: ReviewContext,
    pub files: PullRequestFiles,
    pub next_file_cursor: Option<String>,
    pub range: PullRequestRange,
    pub entries: Vec<DiffEntry>,
    /// A cheap unchanged refresh can leave the already loaded conversation intact.
    pub discussion_stamp: Option<String>,
}

/// The state of the load that owns the file list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// Nothing is being reviewed.
    Idle,
    /// The first load for the target; no files are known yet.
    Loading,
    Ready,
    Failed(String),
}

/// What a viewed mutation reported, reduced to what the pane needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewedOutcome {
    Applied,
    /// GitHub refused; the local mark reverts.
    NotApplied(String),
    /// The answer was lost; the local mark stands until a refresh.
    Unknown(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    BrowserViewport {
        offset: f32,
        height: f32,
    },
    FilePage(file_pages::PageAction),
    BrowserFenced {
        generation: u64,
        action: Box<Action>,
    },
    Fenced {
        generation: u64,
        authority_revision: u64,
        action: Box<Action>,
    },
    Discussion(discussion_ui::Action),
    SetSource(Option<ReviewSource>),
    Cached {
        generation: u64,
        result: Result<Option<Arc<cache::Snapshot>>, String>,
    },
    CacheSaved {
        generation: u64,
        order: u64,
        draft_version: Option<String>,
        result: Result<(), String>,
    },
    RetryCacheSave,
    /// A shell without its own list asks it to (re)load.
    LoadList,
    LoadMore,
    BrowserQuery(String),
    BrowserFilterIndex(usize),
    ListLoaded {
        generation: u64,
        result: Result<PullRequestPage, String>,
    },
    /// A list row was chosen by number; the shell resolves it to a target,
    /// because only the shell knows the repository and worktree.
    Select(u64),
    OpenNumber(u64),
    TargetLoaded {
        generation: u64,
        result: Result<ReviewTarget, String>,
    },
    Open(ReviewTarget),
    Loaded {
        generation: u64,
        load: u64,
        result: Result<Arc<Loaded>, String>,
    },
    Refresh,
    RefreshIfDue(Instant),
    /// Back to the list, forgetting the target.
    Close,
    SelectFile(RepoPath),
    ToggleDirectory(RepoPath),
    NextFile,
    PreviousFile,
    NextUnviewed,
    NextChange,
    PreviousChange,
    SetFileListMode(FileListMode),
    ToggleHideViewed,
    ToggleViewed(RepoPath),
    SetDirectoryViewed {
        directory: RepoPath,
        viewed: bool,
    },
    /// Marks the selected file viewed and moves to the next one.
    ToggleViewedAndAdvance,
    ViewedApplied {
        generation: u64,
        authority_revision: u64,
        results: Vec<(RepoPath, bool, ViewedOutcome)>,
    },
    ViewedBatchSummary {
        generation: u64,
        authority_revision: u64,
        failed: usize,
        total: usize,
    },
    OpenUrl(Url),
    Checkout,
    DismissNotice,
}

impl Action {
    /// Completions are fenced by generation and change nothing a modal
    /// protects, so they may land while one is open.
    pub fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::FilePage(file_pages::PageAction::Loaded { .. })
                | Self::Loaded { .. }
                | Self::ListLoaded { .. }
                | Self::TargetLoaded { .. }
                | Self::Cached { .. }
                | Self::CacheSaved { .. }
                | Self::ViewedApplied { .. }
                | Self::ViewedBatchSummary { .. }
                | Self::Discussion(
                    discussion_ui::Action::Loaded { .. }
                        | discussion_ui::Action::Finished { .. }
                        | discussion_ui::Action::Reanchored { .. }
                )
        )
    }
}

/// Work the shell carries out on the state's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    OpenFile {
        path: RepoPath,
        old_path: Option<RepoPath>,
        basis: DiffBasis,
    },
    /// The selected file is gone from the range, so the viewer should show
    /// nothing rather than a stale diff.
    CloseFile,
    NextChange,
    PreviousChange,
    OpenUrl(Url),
    Checkout(CheckoutTarget),
    /// Which pull request is under review changed; `None` after `Close`.
    SelectionChanged(Option<u64>),
    /// A discussion of the whole file opened above the first diff row, so
    /// the diff scrolls back to it.
    RevealFileDiscussion,
}

/// One row of the rendered tree: a directory or a file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: RepoPath,
    depth: usize,
    label: Box<str>,
    /// Index into [`State::files`] for a file row; `None` for a directory.
    file: Option<usize>,
}

/// The list a shell without a project sidebar browses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListState {
    Idle,
    Loading,
    Ready(Vec<Arc<PullRequest>>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BrowserFilter {
    #[default]
    All,
    Drafts,
    Ready,
    Passing,
    Failing,
    NeedsReview,
    ReRequested,
}

impl BrowserFilter {
    const ALL: [Self; 7] = [
        Self::All,
        Self::Drafts,
        Self::Ready,
        Self::Passing,
        Self::Failing,
        Self::NeedsReview,
        Self::ReRequested,
    ];
    fn matches(
        self,
        pull_request: &PullRequest,
        request: Option<&crate::github::review_requests::scan::Requested>,
    ) -> bool {
        match self {
            Self::All => true,
            Self::Drafts => pull_request.is_draft,
            Self::Ready => !pull_request.is_draft,
            Self::Passing => {
                CheckStatus::from_checks(&pull_request.check_summaries()) == CheckStatus::Passing
            }
            Self::Failing => {
                CheckStatus::from_checks(&pull_request.check_summaries()) == CheckStatus::Failing
            }
            Self::NeedsReview => request.is_some(),
            Self::ReRequested => request.is_some_and(|request| {
                request.history == crate::github::review_requests::scan::History::ReRequested
            }),
        }
    }
}

impl std::fmt::Display for BrowserFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl AsRef<str> for BrowserFilter {
    fn as_ref(&self) -> &str {
        match self {
            Self::All => "All open",
            Self::Drafts => "Drafts",
            Self::Ready => "Ready for review",
            Self::Passing => "Checks passing",
            Self::Failing => "Checks failing",
            Self::NeedsReview => "Needs your review",
            Self::ReRequested => "Review re-requested",
        }
    }
}

/// Where a shell's list and reviews come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSource {
    pub repository: GitHubRepoKey,
    pub worktree: PathBuf,
}

impl ReviewSource {
    /// Local remote identity is sufficient for cached reads. Mutation authority
    /// still comes from the freshly verified review context.
    pub fn from_remotes(
        worktree: &Path,
        remotes: &[crate::repository::RemoteRecord],
    ) -> Option<Self> {
        let keys = remotes
            .iter()
            .filter_map(|remote| crate::github::remote_repository_key(remote.fetch_url.as_deref()?))
            .collect::<Vec<_>>();
        let repository = keys
            .iter()
            .find(|key| {
                crate::github::remote_for_repository(remotes, key).as_deref() == Some("origin")
            })
            .or_else(|| keys.first())?
            .clone();
        Some(Self {
            repository,
            worktree: worktree.to_path_buf(),
        })
    }
}

pub struct State {
    unavailable_reason: Option<String>,
    discussion: discussion_ui::DiscussionState,
    parked_discussions: Vec<(ReviewTarget, discussion_ui::DiscussionState)>,
    api: Arc<dyn GitHubReviewApi>,
    cache: Option<Arc<dyn cache::Store>>,
    cache_dirty: bool,
    cache_order: u64,
    cache_owner: String,
    cache_save_error: bool,
    cache_loading: bool,
    cached_viewer: Option<String>,
    restore_selection: Option<RepoPath>,
    git: RepositoryService,
    /// Set by shells that list through this state rather than their own.
    source: Option<ReviewSource>,
    list: ListState,
    list_generation: u64,
    list_cancellation: CancellationToken,
    list_cursor: Option<String>,
    list_loading: bool,
    list_appending: bool,
    list_error: Option<String>,
    browser_query: String,
    browser_keys: BTreeMap<u64, (String, String)>,
    browser_viewport: (f32, f32),
    browser_filter: BrowserFilter,
    requests: request_ui::State,
    target: Option<ReviewTarget>,
    /// Bumped on every `Open` and `Close`: results from another target are
    /// dropped whatever they carried.
    generation: u64,
    /// Bumped per load within a target, so a refresh supersedes the load
    /// before it without invalidating viewed mutations in flight.
    load_generation: u64,
    last_refresh: Option<Instant>,
    cancellation: CancellationToken,
    viewed_cancellation: CancellationToken,
    // A mutation can settle before an older refresh returns. Keep those
    // acknowledged marks until a read started after their outcome can verify them.
    viewed_after_load: BTreeSet<RepoPath>,
    load: LoadState,
    /// True while a refresh runs behind an already listed set of files.
    refreshing: bool,
    context: Option<ReviewContext>,
    loaded_snapshot: Option<Arc<Loaded>>,
    force_conversation: bool,
    authority_revision: u64,
    range: Option<PullRequestRange>,
    files: Vec<ReviewFile>,
    files_truncated: bool,
    pages: file_pages::Pages,
    file_scroll_id: widget::Id,
    reveal_row: Option<(usize, usize)>,
    rows: Vec<Row>,
    file_list_mode: FileListMode,
    hide_viewed: bool,
    collapsed_directories: BTreeSet<RepoPath>,
    selected: Option<RepoPath>,
    notice: Option<String>,
    effects: Vec<Effect>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("State")
            .field("target", &self.target)
            .field("generation", &self.generation)
            .field("load", &self.load)
            .field("files", &self.files.len())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.viewed_cancellation.cancel();
        self.list_cancellation.cancel();
    }
}

impl State {
    pub fn with_cache(mut self, cache: Option<Arc<dyn cache::Store>>) -> Self {
        self.cache = cache;
        self
    }

    fn adopt_cache(&mut self, snapshot: cache::Snapshot) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if snapshot.validate().is_err()
            || !snapshot.key.repository.same_repository(&target.repository)
            || snapshot.key.number != target.pull_request.number
        {
            return;
        }
        self.restore_drafts(snapshot.drafts.clone(), snapshot.drafts_version.clone());
        self.restore_selection = snapshot.selected.clone();
        // A browser row may already know a newer head. Keep only the path
        // preference in that case; cached marks belong to the old head.
        if target.pull_request.head_ref_oid != snapshot.range.head.as_str() {
            return;
        }
        self.cached_viewer = Some(snapshot.viewer.clone());
        self.notice = Some(format!(
            "Cached review for {} · read only until GitHub refreshes",
            snapshot.viewer
        ));
        self.range = Some(snapshot.range);
        self.files = snapshot
            .files
            .into_iter()
            .map(|file| ReviewFile {
                entry: file.entry.clone(),
                viewed: file.viewed,
                sync: if snapshot.unknown_paths.contains(&file.entry.path) {
                    ViewedSync::Unknown
                } else if file.unverified {
                    ViewedSync::Unverified
                } else {
                    ViewedSync::Settled
                },
            })
            .collect();
        self.rebuild_rows();
        self.load = LoadState::Ready;
        if let Some(path) = self.restore_selection.take() {
            self.select_file(path);
        }
    }

    fn persist_cache(&mut self) -> ActionTask {
        if !std::mem::take(&mut self.cache_dirty) {
            return ActionTask::none();
        }
        let (Some(cache), Some(target), Some(range)) =
            (self.cache.clone(), &self.target, &self.range)
        else {
            return ActionTask::none();
        };
        let Ok(key) = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        else {
            return ActionTask::none();
        };
        let mut pull_request = (*target.pull_request).clone();
        pull_request.head_ref_oid = range.head.as_str().to_owned();
        pull_request.base_ref_oid = Some(range.base_tip.as_str().to_owned());
        let viewer = if let Some(context) = &self.context {
            pull_request.id = Some(context.pull_request_id.clone());
            context.viewer.clone()
        } else if let Some(viewer) = &self.cached_viewer {
            viewer.clone()
        } else {
            return ActionTask::none();
        };
        let write_order = cache::WriteOrder::capture();
        let draft_version = self
            .drafts_need_save()
            .then(|| uuid::Uuid::new_v4().to_string());
        let snapshot = cache::Snapshot {
            key,
            viewer,
            pull_request,
            range: range.clone(),
            files: self
                .files
                .iter()
                .map(|file| cache::File {
                    entry: file.entry.clone(),
                    viewed: file.viewed,
                    unverified: file.sync != ViewedSync::Settled,
                })
                .collect(),
            unknown_paths: self
                .files
                .iter()
                .filter(|file| file.sync == ViewedSync::Unknown)
                .map(|file| file.entry.path.clone())
                .collect(),
            selected: self.selected.clone(),
            drafts: self.local_drafts(),
            drafts_changed: self.drafts_need_save(),
            drafts_owner: Some(self.cache_owner.clone()),
            drafts_version: draft_version.clone(),
            drafts_basis: self.draft_cache_basis(),
            saved_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        };
        static ORDER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let order = ORDER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.cache_order = order;
        let generation = self.generation;
        ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    write_order.and_then(|write_order| cache.save(&snapshot, &write_order))
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Action::CacheSaved {
                    generation,
                    order,
                    draft_version,
                    result,
                }
            },
            |action| action,
        )
    }

    pub fn new(api: Arc<dyn GitHubReviewApi>, git: RepositoryService) -> Self {
        Self {
            unavailable_reason: None,
            discussion: discussion_ui::DiscussionState::default(),
            parked_discussions: Vec::new(),
            api,
            cache: None,
            cache_dirty: false,
            cache_order: 0,
            cache_owner: uuid::Uuid::new_v4().to_string(),
            cache_save_error: false,
            cache_loading: false,
            cached_viewer: None,
            restore_selection: None,
            git,
            source: None,
            list: ListState::Idle,
            list_generation: 0,
            list_cancellation: CancellationToken::new(),
            list_cursor: None,
            list_loading: false,
            list_appending: false,
            list_error: None,
            browser_query: String::new(),
            browser_keys: BTreeMap::new(),
            browser_viewport: (0.0, 480.0),
            browser_filter: BrowserFilter::default(),
            requests: request_ui::State::default(),
            target: None,
            generation: 0,
            load_generation: 0,
            last_refresh: None,
            cancellation: CancellationToken::new(),
            viewed_cancellation: CancellationToken::new(),
            viewed_after_load: BTreeSet::new(),
            load: LoadState::Idle,
            refreshing: false,
            context: None,
            loaded_snapshot: None,
            force_conversation: true,
            authority_revision: 0,
            range: None,
            files: Vec::new(),
            files_truncated: false,
            pages: file_pages::Pages::default(),
            file_scroll_id: widget::Id::unique(),
            reveal_row: None,
            rows: Vec::new(),
            file_list_mode: FileListMode::Tree,
            hide_viewed: false,
            collapsed_directories: BTreeSet::new(),
            selected: None,
            notice: None,
            effects: Vec::new(),
        }
    }

    /// Points the state's own list at a repository. A change of source drops
    /// the list and any review of the old one.
    pub fn set_source(&mut self, source: Option<ReviewSource>) -> ActionTask {
        if self.source == source {
            return ActionTask::none();
        }
        if self.block_discussion_navigation() {
            return ActionTask::none();
        }
        self.source = source;
        self.requests.reset();
        self.list_cancellation.cancel();
        self.list_generation = self.list_generation.wrapping_add(1);
        self.list = ListState::Idle;
        self.list_cursor = None;
        self.list_loading = false;
        self.list_error = None;
        self.browser_query.clear();
        self.browser_keys.clear();
        self.browser_viewport.0 = 0.0;
        self.browser_filter = BrowserFilter::default();
        let closed = self.close();
        match self.source {
            Some(_) => ActionTask::batch([closed, self.load_list()]),
            None => closed,
        }
    }

    pub fn source(&self) -> Option<&ReviewSource> {
        self.source.as_ref()
    }

    pub fn list(&self) -> &ListState {
        &self.list
    }

    pub fn target(&self) -> Option<&ReviewTarget> {
        self.target.as_ref()
    }

    pub fn load_state(&self) -> &LoadState {
        &self.load
    }

    pub fn files(&self) -> &[ReviewFile] {
        &self.files
    }

    pub fn range(&self) -> Option<&PullRequestRange> {
        self.range.as_ref()
    }

    pub fn selected(&self) -> Option<&RepoPath> {
        self.selected.as_ref()
    }

    pub fn is_open(&self) -> bool {
        self.target.is_some()
    }

    /// Resolves a listed number to a target when this state owns the list.
    pub fn listed_target(&self, number: u64) -> Option<ReviewTarget> {
        let source = self.source.as_ref()?;
        let ListState::Ready(pull_requests) = &self.list else {
            return None;
        };
        let pull_request = pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)?;
        Some(ReviewTarget {
            repository: source.repository.clone(),
            worktree: source.worktree.clone(),
            pull_request: Arc::clone(pull_request),
        })
    }

    /// Takes the effects accumulated since the last drain, oldest first.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn fenced(&self, action: Action) -> Action {
        Action::Fenced {
            generation: self.generation,
            authority_revision: self.authority_revision,
            action: Box::new(action),
        }
    }

    pub fn update(&mut self, action: Action) -> ActionTask {
        let task = self.update_inner(action);
        let generation = self.generation;
        let authority_revision = self.authority_revision;
        let reveal = self
            .reveal_row
            .take()
            .map_or_else(ActionTask::none, |(position, depth)| {
                file_pages::reveal_row(self.file_scroll_id.clone(), position, depth).map(
                    move |action| Action::Fenced {
                        generation,
                        authority_revision,
                        action: Box::new(Action::FilePage(action)),
                    },
                )
            });
        ActionTask::batch([
            task,
            reveal,
            self.load_file_page(false),
            self.persist_cache(),
        ])
    }

    fn update_inner(&mut self, action: Action) -> ActionTask {
        match action {
            Action::FilePage(action) => self.page_action(action),
            Action::Cached { generation, result } => {
                if generation != self.generation || !self.cache_loading || self.context.is_some() {
                    return ActionTask::none();
                }
                self.cache_loading = false;
                match result {
                    Ok(Some(snapshot)) => self.adopt_cache(Arc::unwrap_or_clone(snapshot)),
                    Ok(None) => {}
                    Err(error) => {
                        self.notice = Some(format!("Could not read review cache: {error}"))
                    }
                }
                self.refreshing = self.load == LoadState::Ready;
                self.start_load()
            }
            Action::CacheSaved {
                generation,
                order,
                draft_version,
                result,
            } => {
                if generation == self.generation && order == self.cache_order {
                    match result {
                        Ok(()) => {
                            self.cache_save_error = false;
                            self.drafts_cache_saved(draft_version);
                        }
                        Err(error) => {
                            self.cache_save_error = true;
                            self.notice = Some(format!("Could not save review draft: {error}"))
                        }
                    }
                }
                ActionTask::none()
            }
            Action::RetryCacheSave => {
                self.cache_dirty = true;
                self.cache_save_error = false;
                ActionTask::none()
            }
            Action::BrowserFenced { generation, action } => {
                if generation != self.list_generation {
                    return ActionTask::none();
                }
                self.update(*action)
            }
            Action::Fenced {
                generation,
                authority_revision,
                action,
            } => {
                if generation != self.generation || authority_revision != self.authority_revision {
                    return ActionTask::none();
                }
                self.update(*action)
            }
            Action::Discussion(action) => self.update_discussion(action),
            Action::SetSource(source) => self.set_source(source),
            Action::LoadList => self.load_list(),
            Action::LoadMore => self.load_list_page(true),
            Action::BrowserQuery(query) => {
                self.browser_query = query;
                self.browser_viewport.0 = 0.0;
                iced::widget::operation::scroll_to(
                    widget::Id::new("pr.browser"),
                    iced::widget::operation::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
            Action::BrowserViewport { offset, height } => {
                self.browser_viewport = (offset, height);
                ActionTask::none()
            }
            Action::BrowserFilterIndex(index) => {
                if let Some(filter) = BrowserFilter::ALL.get(index) {
                    self.browser_filter = *filter;
                    self.browser_viewport.0 = 0.0;
                }
                iced::widget::operation::scroll_to(
                    widget::Id::new("pr.browser"),
                    iced::widget::operation::AbsoluteOffset { x: 0.0, y: 0.0 },
                )
            }
            Action::ListLoaded { generation, result } => {
                if generation != self.list_generation {
                    return ActionTask::none();
                }
                self.list_loading = false;
                match result {
                    Ok(page) => {
                        self.list_cursor = page.next_cursor;
                        let mut rows = if self.list_appending {
                            match std::mem::replace(&mut self.list, ListState::Idle) {
                                ListState::Ready(rows) => rows,
                                _ => Vec::new(),
                            }
                        } else {
                            Vec::new()
                        };
                        let mut known: BTreeSet<_> = rows.iter().map(|row| row.number).collect();
                        rows.extend(
                            page.pull_requests
                                .into_iter()
                                .filter(|row| known.insert(row.number))
                                .map(Arc::new),
                        );
                        self.browser_keys = rows
                            .iter()
                            .map(|row| {
                                (
                                    row.number,
                                    (
                                        row.title.to_lowercase(),
                                        row.author.as_ref().map_or_else(String::new, |author| {
                                            author.login.to_lowercase()
                                        }),
                                    ),
                                )
                            })
                            .collect();
                        self.list = ListState::Ready(rows);
                        self.list_error = None;
                    }
                    Err(message) if matches!(self.list, ListState::Ready(_)) => {
                        self.list_error = Some(message)
                    }
                    Err(message) => self.list = ListState::Failed(message),
                }
                // A partial page cannot establish which PR refs are obsolete.
                if self.list_cursor.is_none() && self.list_error.is_none() {
                    self.prune_fetched_refs()
                } else {
                    ActionTask::none()
                }
            }
            Action::Select(number) => match self.listed_target(number) {
                Some(target) => self.open(target),
                None => ActionTask::none(),
            },
            Action::OpenNumber(number) => self.open_number(number),
            Action::TargetLoaded { generation, result } => {
                if generation != self.generation {
                    return ActionTask::none();
                }
                match result {
                    Ok(target) => self.open(target),
                    Err(message) => {
                        self.list_error = Some(message);
                        ActionTask::none()
                    }
                }
            }
            Action::Open(target) => self.open(target),
            Action::Loaded {
                generation,
                load,
                result,
            } => {
                if generation != self.generation || load != self.load_generation {
                    return ActionTask::none();
                }
                self.refreshing = false;
                match result {
                    Ok(loaded) => {
                        let unchanged_conversation = !self.force_conversation
                            && loaded.context.pending.is_none()
                            && self.discussion.has_conversation()
                            && self.loaded_snapshot.as_ref().is_some_and(|previous| {
                                loaded.discussion_stamp.is_some()
                                    && loaded.discussion_stamp == previous.discussion_stamp
                                    && same_diff_authority(&loaded.context, &previous.context)
                            });
                        self.force_conversation = false;
                        self.loaded_snapshot = Some(loaded.clone());
                        self.adopt(&loaded);
                        if unchanged_conversation {
                            if let Some(target) = &self.target {
                                crate::github::polling::succeeded(&target.repository.host);
                            }
                            return ActionTask::none();
                        }
                        return self.load_discussion();
                    }
                    Err(message) => {
                        if self.load == LoadState::Loading {
                            self.load = LoadState::Failed(message);
                        } else {
                            self.notice = Some(message);
                        }
                    }
                }
                ActionTask::none()
            }
            Action::Refresh => self.refresh(),
            Action::RefreshIfDue(now) => {
                if !self.refreshing
                    && !self.discussion_is_busy()
                    && self.target.as_ref().is_some_and(|target| {
                        crate::github::polling::ready(&target.repository.host, false)
                    })
                    && self.last_refresh.is_some_and(|last| {
                        now.saturating_duration_since(last) >= Duration::from_secs(30)
                    })
                {
                    self.refresh_mode(false)
                } else {
                    ActionTask::none()
                }
            }
            Action::Close => self.close(),
            Action::SelectFile(path) => {
                self.select_file(path);
                ActionTask::none()
            }
            Action::ToggleDirectory(path) => {
                if !self.collapsed_directories.remove(&path) {
                    self.collapsed_directories.insert(path);
                }
                ActionTask::none()
            }
            Action::NextFile => {
                self.step_file(1);
                ActionTask::none()
            }
            Action::PreviousFile => {
                self.step_file(-1);
                ActionTask::none()
            }
            Action::NextUnviewed => {
                self.next_unviewed();
                ActionTask::none()
            }
            Action::NextChange | Action::PreviousChange => {
                if self.selected.is_some() {
                    self.effects.push(if matches!(action, Action::NextChange) {
                        Effect::NextChange
                    } else {
                        Effect::PreviousChange
                    });
                }
                ActionTask::none()
            }
            Action::SetFileListMode(mode) => {
                self.file_list_mode = mode;
                self.rebuild_rows();
                ActionTask::none()
            }
            Action::ToggleHideViewed => {
                self.hide_viewed = !self.hide_viewed;
                ActionTask::none()
            }
            Action::ToggleViewed(path) => {
                let Some(index) = self.file_index(&path) else {
                    return ActionTask::none();
                };
                let viewed = !self.files[index].viewed;
                self.set_viewed(vec![index], viewed)
            }
            Action::SetDirectoryViewed { directory, viewed } => {
                let indices = self.files_under(&directory);
                self.set_viewed(indices, viewed)
            }
            Action::ToggleViewedAndAdvance => {
                if self.context.is_none() {
                    return ActionTask::none();
                }
                let Some(index) = self
                    .selected
                    .as_ref()
                    .and_then(|path| self.file_index(path))
                else {
                    return ActionTask::none();
                };
                if self.files[index].sync == ViewedSync::Unknown {
                    self.notice = Some("Viewed state is still loading. Wait for the remaining files, then try again.".into());
                    self.pages.hydrate_all = true;
                    return ActionTask::none();
                }
                let viewed = !self.files[index].viewed;
                let task = self.set_viewed(vec![index], viewed);
                if viewed {
                    self.step_file(1);
                }
                task
            }
            Action::ViewedApplied {
                generation,
                authority_revision,
                results,
            } => {
                if generation != self.generation || authority_revision != self.authority_revision {
                    return ActionTask::none();
                }
                let failures = results
                    .iter()
                    .filter(|(_, _, outcome)| !matches!(outcome, ViewedOutcome::Applied))
                    .count();
                let total = results.len();
                for (path, viewed, outcome) in results {
                    let Some(index) = self.file_index(&path) else {
                        continue;
                    };
                    if self.refreshing || self.load == LoadState::Loading {
                        self.viewed_after_load.insert(path.clone());
                    }
                    let file = &mut self.files[index];
                    match outcome {
                        ViewedOutcome::Applied => {
                            file.sync = ViewedSync::Settled;
                        }
                        ViewedOutcome::NotApplied(message) => {
                            file.viewed = !viewed;
                            file.sync = ViewedSync::Settled;
                            self.notice = Some(format!(
                                "GitHub did not mark {} as {}. {message}",
                                path.display(),
                                if viewed { "viewed" } else { "not viewed" }
                            ));
                        }
                        ViewedOutcome::Unknown(message) => {
                            file.sync = ViewedSync::Unverified;
                            self.notice = Some(format!(
                                "Could not confirm the viewed mark on {}; refresh to check. {message}",
                                path.display()
                            ));
                        }
                    }
                }
                if failures > 0 && total > 1 {
                    let detail = self.notice.take().unwrap_or_default();
                    self.notice = Some(format!(
                        "{failures} of {total} files in this batch could not be marked. {detail}"
                    ));
                }
                self.cache_dirty = true;
                ActionTask::none()
            }
            Action::ViewedBatchSummary {
                generation,
                authority_revision,
                failed,
                total,
            } => {
                if generation == self.generation
                    && authority_revision == self.authority_revision
                    && failed > 0
                {
                    let detail = self.notice.take().unwrap_or_default();
                    self.notice = Some(format!(
                        "{failed} of {total} files could not be marked. {detail}"
                    ));
                }
                ActionTask::none()
            }
            Action::OpenUrl(url) => {
                self.effects.push(Effect::OpenUrl(url));
                ActionTask::none()
            }
            Action::Checkout => {
                if !self.refreshing
                    && let (Some(target), Some(context), Some(range)) =
                        (&self.target, &self.context, &self.range)
                    && context.head_oid == range.head.as_str()
                {
                    self.effects.push(Effect::Checkout(CheckoutTarget {
                        repository: target.worktree.clone(),
                        number: target.pull_request.number,
                        head: range.head.clone(),
                    }));
                }
                ActionTask::none()
            }
            Action::DismissNotice => {
                self.notice = None;
                ActionTask::none()
            }
        }
    }

    fn load_list(&mut self) -> ActionTask {
        self.load_list_page(false)
    }

    fn open_number(&mut self, number: u64) -> ActionTask {
        let Some(source) = self.source.clone() else {
            return ActionTask::none();
        };
        if self.block_discussion_navigation() {
            return ActionTask::none();
        }
        let closed = self.close();
        let generation = self.generation;
        let cancellation = self.cancellation.clone();
        let api = Arc::clone(&self.api);
        let cache = self.cache.clone();
        let load = ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let key = PullRequestKey::new(source.repository.clone(), number)
                        .map_err(|error| error.to_string())?;
                    if let Some(cache) = cache
                        && let Ok(Some(snapshot)) = cache.load(&key)
                    {
                        return Ok(ReviewTarget {
                            repository: source.repository,
                            worktree: source.worktree,
                            pull_request: Arc::new(snapshot.pull_request),
                        });
                    }
                    api.pull_request(&key, &cancellation)
                        .map(|pull_request| ReviewTarget {
                            repository: source.repository,
                            worktree: source.worktree,
                            pull_request: Arc::new(pull_request),
                        })
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(format!("Pull request lookup failed: {error}")));
                Action::TargetLoaded { generation, result }
            },
            |action| action,
        );
        ActionTask::batch([closed, load])
    }

    fn load_list_page(&mut self, append: bool) -> ActionTask {
        if append && (self.list_loading || self.list_cursor.is_none()) {
            return ActionTask::none();
        }
        let Some(source) = self.source.clone() else {
            return ActionTask::none();
        };
        self.list_cancellation.cancel();
        self.list_cancellation = CancellationToken::new();
        let cancellation = self.list_cancellation.clone();
        self.list_generation = self.list_generation.wrapping_add(1);
        let generation = self.list_generation;
        let cursor = if append {
            self.list_cursor.clone()
        } else {
            None
        };
        self.list_loading = true;
        self.list_appending = append;
        self.list_error = None;
        if !matches!(self.list, ListState::Ready(_)) {
            self.list = ListState::Loading;
        }
        let api = Arc::clone(&self.api);
        ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    api.pull_request_page(&source.repository, cursor.as_deref(), &cancellation)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(format!("pull request list task failed: {error}")));
                Action::ListLoaded { generation, result }
            },
            |action| action,
        )
    }

    /// Drops fetched ranges for pull requests the list no longer shows as
    /// open, so closed ones stop pinning their objects. Best effort: a failure
    /// costs disk, not correctness, and the next list retries it.
    fn prune_fetched_refs(&self) -> ActionTask {
        let (Some(source), ListState::Ready(pull_requests)) = (&self.source, &self.list) else {
            return ActionTask::none();
        };
        let mut retain: Vec<u64> = pull_requests.iter().map(|pr| pr.number).collect();
        retain.extend(
            self.target
                .as_ref()
                .map(|target| target.pull_request.number),
        );
        prune_fetched_refs(
            self.git.clone(),
            source.worktree.clone(),
            retain,
            self.list_cancellation.clone(),
        )
        .discard()
    }

    fn open(&mut self, target: ReviewTarget) -> ActionTask {
        if self.target.as_ref() == Some(&target) {
            return ActionTask::none();
        }
        if self.block_discussion_navigation() {
            return ActionTask::none();
        }
        let number = target.pull_request.number;
        self.reset_target();
        if let Some(index) = self.parked_discussions.iter().position(|(saved, _)| {
            saved.repository == target.repository
                && saved.worktree == target.worktree
                && saved.pull_request.number == number
        }) {
            self.discussion = self.parked_discussions.swap_remove(index).1;
        }
        self.target = Some(target);
        self.load = LoadState::Loading;
        self.effects.push(Effect::SelectionChanged(Some(number)));
        if let Some(cache) = self.cache.clone() {
            self.cache_loading = true;
            let generation = self.generation;
            let target = self.target.as_ref().expect("installed target");
            let key = PullRequestKey::new(target.repository.clone(), number);
            return ActionTask::perform(
                async move {
                    let result = tokio::task::spawn_blocking(move || {
                        cache
                            .load(&key.map_err(|error| error.to_string())?)
                            .map(|snapshot| snapshot.map(Arc::new))
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    Action::Cached { generation, result }
                },
                |action| action,
            );
        }
        self.start_load()
    }

    fn close(&mut self) -> ActionTask {
        if self.block_discussion_navigation() {
            return ActionTask::none();
        }
        let was_open = self.target.is_some();
        self.reset_target();
        if was_open {
            self.effects.push(Effect::SelectionChanged(None));
        }
        ActionTask::none()
    }

    fn reset_target(&mut self) {
        self.cache_loading = false;
        self.cache_dirty = false;
        self.cache_save_error = false;
        self.cached_viewer = None;
        self.restore_selection = None;
        let discussion = std::mem::take(&mut self.discussion);
        if discussion.has_local_work()
            && let Some(target) = self.target.clone()
        {
            self.parked_discussions
                .push((target, discussion.into_draft_state()));
        }
        self.cancellation.cancel();
        self.cancellation = CancellationToken::new();
        self.viewed_cancellation.cancel();
        self.viewed_cancellation = CancellationToken::new();
        self.generation = self.generation.wrapping_add(1);
        self.load_generation = 0;
        self.viewed_after_load.clear();
        self.last_refresh = None;
        self.target = None;
        self.load = LoadState::Idle;
        self.refreshing = false;
        self.context = None;
        self.loaded_snapshot = None;
        self.force_conversation = true;
        self.range = None;
        self.files.clear();
        self.pages = file_pages::Pages::default();
        self.file_scroll_id = widget::Id::unique();
        self.reveal_row = None;
        self.files_truncated = false;
        self.rows.clear();
        self.collapsed_directories.clear();
        // The shell's viewer is showing a file of the review that is going
        // away; left alone it would keep rendering a range nothing owns.
        if self.selected.take().is_some() {
            self.effects.push(Effect::CloseFile);
        }
        self.notice = None;
    }

    fn refresh(&mut self) -> ActionTask {
        self.refresh_mode(true)
    }

    fn refresh_mode(&mut self, manual: bool) -> ActionTask {
        self.force_conversation |= manual;
        if self.target.is_none() || self.discussion_is_busy() {
            return ActionTask::none();
        }
        if !self
            .target
            .as_ref()
            .is_some_and(|target| crate::github::polling::ready(&target.repository.host, manual))
        {
            self.notice = self
                .target
                .as_ref()
                .and_then(|target| crate::github::polling::rate_limit(&target.repository.host))
                .map(|limit| limit.to_string());
            return ActionTask::none();
        }
        match self.load {
            LoadState::Loading => return ActionTask::none(),
            LoadState::Failed(_) => self.load = LoadState::Loading,
            LoadState::Ready => self.refreshing = true,
            LoadState::Idle => return ActionTask::none(),
        }
        self.notice = None;
        self.start_load()
    }

    fn start_load(&mut self) -> ActionTask {
        let Some(target) = self.target.clone() else {
            return ActionTask::none();
        };
        self.last_refresh = Some(Instant::now());
        self.load_generation = self.load_generation.wrapping_add(1);
        self.viewed_after_load.clear();
        self.pages.loading = false;
        let generation = self.generation;
        let load = self.load_generation;
        let cancellation = self.cancellation.clone();
        let api = Arc::clone(&self.api);
        let git = self.git.clone();
        let previous = self.loaded_snapshot.clone();
        ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let result = load_review(
                        api.as_ref(),
                        &git,
                        &target,
                        previous.as_deref(),
                        &cancellation,
                    );
                    match &result {
                        Ok(_) => {}
                        Err(message)
                            if crate::github::polling::ready(&target.repository.host, false) =>
                        {
                            crate::github::polling::failed(
                                &target.repository.host,
                                &crate::github::GitHubError::Inspection(message.clone()),
                            )
                        }
                        Err(_) => {}
                    }
                    result.map(Arc::new)
                })
                .await
                .unwrap_or_else(|error| Err(format!("review load task failed: {error}")));
                Action::Loaded {
                    generation,
                    load,
                    result,
                }
            },
            |action| action,
        )
    }

    /// Installs a load without undoing mutations which were pending or settled
    /// after this refresh started. A subsequent read can reconcile those marks.
    fn adopt(&mut self, loaded: &Loaded) {
        if let Some(target) = &mut self.target {
            self.cache_dirty |= *target.pull_request != loaded.pull_request;
            target.pull_request = Arc::new(loaded.pull_request.clone());
        }
        self.cache_loading = false;
        self.cached_viewer = None;
        self.notice = None;
        let entries_changed = self
            .files
            .iter()
            .map(|f| &f.entry)
            .ne(loaded.entries.iter());
        let previous: HashMap<RepoPath, (bool, ViewedSync)> = self
            .files
            .drain(..)
            .map(|file| (file.entry.path, (file.viewed, file.sync)))
            .collect();
        // The open file compares against the whole basis, which the base
        // moving under the pull request changes as surely as a new head.
        let basis_changed = self
            .range
            .as_ref()
            .is_some_and(|range| range.basis() != loaded.range.basis());
        let authority_changed = self.context.as_ref().is_none_or(|previous| {
            previous.pull_request_id != loaded.context.pull_request_id
                || previous.head_oid != loaded.context.head_oid
                || previous.base_oid != loaded.context.base_oid
                || previous.viewer != loaded.context.viewer
        });
        if authority_changed {
            self.viewed_cancellation.cancel();
            self.viewed_cancellation = CancellationToken::new();
            self.pages = file_pages::Pages {
                cursor: loaded.next_file_cursor.clone(),
                offset: self.pages.offset,
                height: self.pages.height,
                // A new head invalidates all later marks. Hydrate them in the
                // background rather than requiring the reader to scroll first.
                hydrate_all: self.context.is_some(),
                ..Default::default()
            };
        } else {
            // The cursor and completed pages still describe this exact revision.
            // A refresh must not restart pagination merely to recover old marks.
            self.pages.loading = false;
        }
        self.files = join_files(&loaded.entries, &loaded.files);
        if loaded.next_file_cursor.is_some() {
            let known: BTreeSet<&str> = loaded
                .files
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect();
            for file in &mut self.files {
                if !known.contains(file.entry.path.display().as_ref()) {
                    file.sync = ViewedSync::Unknown;
                }
            }
        }
        for file in &mut self.files {
            if !authority_changed
                && let Some((viewed, sync)) = previous.get(&file.entry.path)
                && (*sync == ViewedSync::Pending
                    || self.viewed_after_load.contains(&file.entry.path)
                    || file.sync == ViewedSync::Unknown)
            {
                file.viewed = *viewed;
                file.sync = *sync;
            }
        }
        self.cache_dirty |= authority_changed
            || entries_changed
            || self
                .files
                .iter()
                .any(|file| previous.get(&file.entry.path) != Some(&(file.viewed, file.sync)));
        self.files_truncated = loaded.files.truncated;
        self.rebuild_rows();
        if authority_changed {
            self.authority_revision = self.authority_revision.wrapping_add(1);
        }
        self.context = Some(loaded.context.clone());
        self.range = Some(loaded.range.clone());
        self.load = LoadState::Ready;
        if self.selected.is_none()
            && let Some(path) = self.restore_selection.take()
        {
            self.select_file(path);
        }
        if self.files_truncated {
            self.notice = Some(
                "GitHub listed only part of this pull request; viewed marks past that point are not shown."
                    .into(),
            );
        }
        if let Some(selected) = self.selected.clone() {
            match self.file_index(&selected) {
                Some(index) if basis_changed => self.emit_open(index),
                Some(_) => {}
                None => {
                    self.selected = None;
                    self.effects.push(Effect::CloseFile);
                }
            }
        }
    }

    fn file_index(&self, path: &RepoPath) -> Option<usize> {
        self.files.iter().position(|file| &file.entry.path == path)
    }

    fn rebuild_rows(&mut self) {
        self.rows = tree_rows(&self.files);
        if self.file_list_mode == FileListMode::Flat {
            self.rows.retain(|row| row.file.is_some());
            for row in &mut self.rows {
                row.depth = 0;
                row.label = row.path.display().into_owned().into_boxed_str();
            }
            self.rows.sort_by(|a, b| a.path.cmp(&b.path));
        }
    }

    /// Wrap once through the displayed ordering, including collapsed folders.
    fn next_unviewed(&mut self) {
        let files: Vec<usize> = self.rows.iter().filter_map(|row| row.file).collect();
        let start = self
            .selected
            .as_ref()
            .and_then(|path| {
                files
                    .iter()
                    .position(|index| self.files[*index].entry.path == *path)
            })
            .map_or(0, |index| index + 1);
        let next = (0..files.len())
            .map(|offset| files[(start + offset) % files.len()])
            .find(|index| !self.files[*index].viewed);
        if let Some(index) = next {
            if self.files[index].sync == ViewedSync::Unknown {
                self.pages.wanted = Some(self.files[index].entry.path.clone());
                self.pages.seek_unviewed = true;
            } else {
                self.select_file(self.files[index].entry.path.clone());
            }
        } else {
            self.pages.seek_unviewed = false;
            self.pages.wanted = None;
        }
    }

    /// Files under a directory, in tree order.
    fn files_under(&self, directory: &RepoPath) -> Vec<usize> {
        let Some(start) = self
            .rows
            .iter()
            .position(|row| row.file.is_none() && &row.path == directory)
        else {
            return Vec::new();
        };
        let depth = self.rows[start].depth;
        self.rows[start + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .filter_map(|row| row.file)
            .collect()
    }

    fn select_file(&mut self, path: RepoPath) {
        let Some(index) = self.file_index(&path) else {
            return;
        };
        self.collapsed_directories
            .retain(|directory| !path.as_path().starts_with(directory.as_path()));
        self.pages.seek_unviewed = false;
        self.pages.wanted = None;
        self.selected = Some(path);
        self.reveal_selected_file();
        self.cache_dirty = true;
        self.emit_open(index);
    }

    fn emit_open(&mut self, index: usize) {
        let Some(range) = &self.range else {
            return;
        };
        let entry = &self.files[index].entry;
        self.effects.push(Effect::OpenFile {
            path: entry.path.clone(),
            old_path: entry.old_path.clone(),
            basis: range.basis(),
        });
    }

    fn reveal_selected_file(&mut self) {
        let Some(selected) = &self.selected else {
            return;
        };
        let displayed = self.displayed_file_rows(&directory_tallies(&self.rows, &self.files));
        let Some(position) = displayed
            .iter()
            .position(|index| self.rows[*index].path == *selected)
        else {
            return;
        };
        self.reveal_row = Some((position, self.rows[displayed[position]].depth));
    }

    /// Moves the selection through the tree's file rows. With nothing
    /// selected, forward selects the first file and backward the last.
    fn step_file(&mut self, direction: isize) {
        let files: Vec<usize> = self.rows.iter().filter_map(|row| row.file).collect();
        if files.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|path| self.file_index(path))
            .and_then(|index| files.iter().position(|file| *file == index));
        let visible = |index: &&usize| !self.hide_viewed || !self.files[**index].viewed;
        let next = if direction > 0 {
            files
                .iter()
                .skip(current.map_or(0, |position| position + 1))
                .find(visible)
        } else {
            files[..current.unwrap_or(files.len())]
                .iter()
                .rev()
                .find(visible)
        };
        let Some(&index) = next else {
            return;
        };
        if self.selected.as_ref() == Some(&self.files[index].entry.path) {
            return;
        }
        self.select_file(self.files[index].entry.path.clone());
    }

    /// Optimistic marks settle one bounded aliased mutation chunk at a time.
    /// Authority is checked once; each chunk yields progress back to the UI.
    fn set_viewed(&mut self, indices: Vec<usize>, viewed: bool) -> ActionTask {
        if indices
            .iter()
            .any(|index| self.files[*index].sync == ViewedSync::Unknown)
        {
            self.notice = Some(
                "Viewed state is still loading. Wait for the remaining files, then try again."
                    .into(),
            );
            self.pages.error = None;
            self.pages.hydrate_all = true;
            return ActionTask::none();
        }
        let (Some(target), Some(context)) = (&self.target, &self.context) else {
            return ActionTask::none();
        };
        let mut paths = Vec::new();
        for index in indices {
            let file = &mut self.files[index];
            if matches!(file.sync, ViewedSync::Pending | ViewedSync::Unknown)
                || file.viewed == viewed
            {
                continue;
            }
            file.viewed = viewed;
            file.sync = ViewedSync::Pending;
            paths.push(file.entry.path.clone());
        }
        if paths.is_empty() {
            return ActionTask::none();
        }
        self.cache_dirty = true;
        let repository = target.repository.clone();
        let pull_request_id = context.pull_request_id.clone();
        let authority = discussion::Authority {
            target: target.clone(),
            context: context.clone(),
            merge_base: self.range.as_ref().map(|range| range.merge_base.clone()),
        };
        let git = self.git.clone();
        let api = Arc::clone(&self.api);
        let cancellation = self.viewed_cancellation.clone();
        let generation = self.generation;
        let authority_revision = self.authority_revision;
        let total = paths.len();
        let chunks: Vec<Vec<RepoPath>> = paths
            .chunks(crate::github::VIEWED_BATCH_SIZE)
            .map(<[RepoPath]>::to_vec)
            .collect();
        let stream = iced::futures::stream::unfold(
            (chunks.into_iter(), false, None::<String>, 0usize, false),
            move |(mut chunks, verified, refused, failed, finished)| {
                let api = api.clone();
                let git = git.clone();
                let authority = authority.clone();
                let cancellation = cancellation.clone();
                let repository = repository.clone();
                let pull_request_id = pull_request_id.clone();
                async move {
                    let paths = match chunks.next() {
                        Some(paths) => paths,
                        None if failed > 0 && !finished => {
                            return Some((
                                Action::ViewedBatchSummary {
                                    generation,
                                    authority_revision,
                                    failed,
                                    total,
                                },
                                (chunks, verified, refused, failed, true),
                            ));
                        }
                        None => return None,
                    };
                    let lost = paths.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let mut refused = refused;
                        if cancellation.is_cancelled() { refused = Some("The review revision changed before this chunk was sent.".into()); }
                        if !verified && refused.is_none() {
                            if let Err(failure) = discussion::verify_authority(api.as_ref(), &git, &authority, &cancellation) {
                                refused = Some(match failure {
                                    discussion::Outcome::Refused(message) | discussion::Outcome::Unknown(message) => message,
                                    discussion::Outcome::Applied => unreachable!(),
                                });
                            }
                        }
                        if refused.is_none() && !crate::github::polling::ready(&repository.host, false) {
                            refused = Some("GitHub polling is paused after a failure or rate limit. Retry after the reset.".into());
                        }
                        let results: Vec<(RepoPath, bool, ViewedOutcome)> = if let Some(message) = &refused {
                            paths.into_iter().map(|path| (path, viewed, ViewedOutcome::NotApplied(message.clone()))).collect()
                        } else {
                            let names: Vec<String> = paths.iter().map(|path| path.display().into_owned()).collect();
                            let mut outcomes: HashMap<_, _> = api.set_files_viewed(&repository, &pull_request_id, &names, viewed, &cancellation).into_iter().collect();
                            paths.into_iter().map(|path| {
                                let outcome = outcomes.remove(path.display().as_ref()).map(viewed_outcome)
                                    .unwrap_or_else(|| ViewedOutcome::Unknown("GitHub omitted this file from the batch response".into()));
                                (path, viewed, outcome)
                            }).collect()
                        };
                        (results, refused)
                    }).await;
                    let (results, refused) = result.unwrap_or_else(|error| {
                        let message = error.to_string();
                        (
                            lost.into_iter()
                                .map(|path| (path, viewed, ViewedOutcome::Unknown(message.clone())))
                                .collect(),
                            Some(message),
                        )
                    });
                    let failed = failed
                        + results
                            .iter()
                            .filter(|(_, _, outcome)| !matches!(outcome, ViewedOutcome::Applied))
                            .count();
                    Some((
                        Action::ViewedApplied {
                            generation,
                            authority_revision,
                            results,
                        },
                        (chunks, true, refused, failed, false),
                    ))
                }
            },
        );
        ActionTask::run(stream, |action| action)
    }

    /// The header row: which pull request is under review and the controls
    /// that act on it as a whole. It keeps the height of the toolbar beside
    /// it, so the title, which needs more room than a toolbar row has, reads
    /// in [`Self::summary`] beneath it instead of being elided to a few
    /// letters between the controls.
    pub fn header<'a>(&'a self, modal_open: bool) -> Element<'a, Action> {
        let Some(target) = &self.target else {
            return widget::Space::new().into();
        };
        let pull_request = &target.pull_request;
        let state = WorktreePullRequestState::from_github(pull_request);
        let (icon, state_label) = match state {
            Some(WorktreePullRequestState::Merged) => (Icon::MergedPullRequest, "Merged"),
            Some(WorktreePullRequestState::Closed) => (Icon::ClosedPullRequest, "Closed"),
            Some(WorktreePullRequestState::Draft) => (Icon::PullRequest, "Draft"),
            Some(WorktreePullRequestState::Open) | None => (Icon::PullRequest, "Open"),
        };
        let tone = match state {
            Some(WorktreePullRequestState::Merged) => design::Tone::Themed(design::accent_color),
            Some(WorktreePullRequestState::Closed) => {
                design::Tone::Themed(design::destructive_color)
            }
            Some(WorktreePullRequestState::Draft) | None => {
                design::Tone::Themed(design::muted_icon_color)
            }
            Some(WorktreePullRequestState::Open) => design::Tone::Themed(design::success_color),
        };
        let identity = crate::interaction::tooltip(
            widget::row::with_children(vec![
                widget::icon::icon(icon.handle())
                    .size(ROW_ICON_SIZE)
                    .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
                        cosmic::iced::widget::svg::Style {
                            color: Some(tone.resolve(theme)),
                        }
                    })))
                    .into(),
                design::heading(format!("#{}", pull_request.number))
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center),
            format!(
                "{state_label} pull request #{} in {}/{}",
                pull_request.number, target.repository.owner, target.repository.name
            ),
        );
        let busy = self.refreshing || self.load == LoadState::Loading;
        // The spinner has a permanent slot and the refresh control stays
        // mounted while busy: swapping one for the other would move the named
        // controls after it by an index, and iced reconciles named children
        // by the index their name held last frame.
        let activity: Element<'_, Action> = widget::container(if busy {
            crate::ui_controls::activity_spinner(14.0, design::muted_icon_color)
        } else {
            widget::Space::new().into()
        })
        .center(Length::Fixed(ROW_ICON_WIDTH))
        .into();
        let mut controls: Vec<Element<'_, Action>> = vec![
            icon_button(Icon::Branch, "Create worktree from reviewed commit")
                .on_press_maybe(
                    (!modal_open && !busy && self.context.is_some() && self.range.is_some())
                        .then(|| self.fenced(Action::Checkout)),
                )
                .into(),
            icon_button(
                Icon::Refresh,
                if busy {
                    "Refreshing pull request"
                } else {
                    "Refresh pull request"
                },
            )
            .on_press_maybe((!modal_open && !busy).then_some(Action::Refresh))
            .into(),
            icon_button(Icon::PopOut, "Open on GitHub")
                .on_press_maybe((!modal_open).then(|| Action::OpenUrl(pull_request.url.clone())))
                .into(),
            icon_button(Icon::Close, "Close review")
                .on_press_maybe((!modal_open).then_some(Action::Close))
                .into(),
        ];
        if self.cache_save_error && self.drafts_need_save() {
            controls.insert(
                0,
                crate::ui_controls::button::secondary("Retry saving draft")
                    .on_press_maybe((!modal_open).then(|| self.fenced(Action::RetryCacheSave)))
                    .into(),
            );
        }
        let controls = crate::ui_controls::toolbar_actions(controls);
        widget::row::with_children(vec![
            identity,
            widget::Space::new().width(Length::Fill).into(),
            activity,
            controls,
        ])
        .spacing(space::S)
        .align_y(Alignment::Center)
        .padding(Padding::from([0.0, space::M]))
        .height(design::HEADER_CONTENT_HEIGHT)
        .width(Length::Fill)
        .into()
    }

    /// What is under review, in full: the title given the room to wrap, who
    /// proposes merging which branch into which, and the review entry point.
    pub fn summary<'a>(&'a self, modal_open: bool) -> Element<'a, Action> {
        let Some(target) = &self.target else {
            return widget::Space::new().into();
        };
        let pull_request = &target.pull_request;
        let title = crate::interaction::tooltip(
            design::heading(pull_request.title.as_str())
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(SUMMARY_TITLE_LINES),
                ))
                .width(Length::Fill),
            pull_request.title.as_str(),
        );
        let branches = format!(
            "{} ← {}",
            pull_request.base_ref_name, pull_request.head_ref_name
        );
        let detail = match &pull_request.author {
            Some(author) => format!("{} · {branches}", author.login),
            None => branches,
        };
        let mut rows = vec![
            title,
            crate::interaction::tooltip(
                design::caption(detail.clone())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::Middle(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill),
                detail,
            ),
        ];
        if let Some(viewer) = &self.cached_viewer {
            rows.push(
                design::caption(format!("Cached for {viewer} · read only"))
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
        rows.push(
            widget::row::with_children(vec![
                design::caption(self.progress_label())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                self.review_control(modal_open),
            ])
            .spacing(space::M)
            .align_y(Alignment::Center)
            .padding(Padding {
                top: space::S,
                ..Padding::ZERO
            })
            .into(),
        );
        widget::column::with_children(rows)
            .spacing(space::XS)
            .padding(Padding::from([space::M, space::M]))
            .width(Length::Fill)
            .into()
    }

    /// Viewed files over all files.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.files.iter().filter(|file| file.viewed).count(),
            self.files.len(),
        )
    }

    /// How far the review has come, or why there is nothing to count yet.
    fn progress_label(&self) -> String {
        match self.progress() {
            (_, 0) if self.load == LoadState::Loading => "Loading files".to_owned(),
            (_, 0) => "No files".to_owned(),
            (viewed, total) => format!("{viewed} of {total} viewed"),
        }
    }

    /// The notice slot: a load failure, a refused mark, or nothing.
    ///
    /// Always an element, empty when there is nothing to say. The file rows
    /// beneath it are named, and a notice that appeared as a new child would
    /// shift them by an index under a click already in flight.
    pub fn notice(&self, modal_open: bool) -> Element<'_, Action> {
        let notice = self.pane_notice(modal_open);
        match self.discussion_notice(modal_open) {
            Some(discussion) => widget::column::with_children(vec![discussion, notice])
                .width(Length::Fill)
                .into(),
            None => notice,
        }
    }

    fn pane_notice(&self, modal_open: bool) -> Element<'_, Action> {
        let message = match (&self.load, &self.notice) {
            (LoadState::Failed(message), _) => {
                Cow::Owned(format!("Couldn’t load this pull request. {message}"))
            }
            (_, Some(notice)) => Cow::Borrowed(notice.as_str()),
            _ => {
                return widget::container(widget::Space::new())
                    .width(Length::Fill)
                    .into();
            }
        };
        let dismiss = if matches!(self.load, LoadState::Failed(_)) {
            icon_button(Icon::Refresh, "Try again")
                .on_press_maybe((!modal_open).then_some(Action::Refresh))
        } else {
            icon_button(Icon::Close, "Dismiss")
                .on_press_maybe((!modal_open).then_some(Action::DismissNotice))
        };
        widget::container(
            widget::row::with_children(vec![
                design::caption(message)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
                dismiss.into(),
            ])
            .align_y(Alignment::Center)
            .spacing(space::S)
            .padding(Padding::from([space::XS, space::M])),
        )
        .width(Length::Fill)
        .into()
    }

    /// The file tree with a viewed mark per file and a tri-state mark per
    /// directory. Folders start open and may be folded independently.
    pub fn file_list(&self, modal_open: bool) -> Element<'_, Action> {
        let list: Element<'_, Action> = crate::ui_controls::responsive(move |size| {
            self.file_scrollable(modal_open, size.height)
        })
        .into();
        let (mode_icon, mode_label, next_mode) = match self.file_list_mode {
            FileListMode::Tree => (Icon::List, "View as list", FileListMode::Flat),
            FileListMode::Flat => (Icon::Tree, "View as tree", FileListMode::Tree),
        };
        let controls = crate::ui_controls::toolbar_actions(vec![
            icon_button(mode_icon, mode_label)
                .on_press_maybe((!modal_open).then_some(Action::SetFileListMode(next_mode)))
                .into(),
            icon_button(
                Icon::Filter,
                if self.hide_viewed {
                    "Show viewed files"
                } else {
                    "Hide viewed files"
                },
            )
            .selected(self.hide_viewed)
            .on_press_maybe((!modal_open).then_some(Action::ToggleHideViewed))
            .into(),
            icon_button(Icon::ChevronRight, "Next unviewed file")
                .on_press_maybe(
                    (!modal_open && self.files.iter().any(|file| !file.viewed))
                        .then_some(Action::NextUnviewed),
                )
                .into(),
        ]);
        let viewed = self.files.iter().filter(|file| file.viewed).count();
        let toolbar = widget::row::with_children(vec![
            design::caption(format!("{viewed}/{} viewed", self.files.len()))
                .width(Length::Fill)
                .into(),
            controls,
        ])
        .align_y(Alignment::Center)
        .padding([0.0, space::M]);
        let unknown = self
            .files
            .iter()
            .filter(|file| file.sync == ViewedSync::Unknown)
            .count();
        // Keep the scrollable in a stable child slot when pagination finishes
        // or fails. Moving it within the column discards its scroll state.
        let mut status = Vec::new();
        if unknown > 0 {
            status.push(design::caption(format!("{unknown} viewed states not loaded")).into());
        }
        if let Some(error) = &self.pages.error {
            status.push(design::caption(error).into());
        }
        if self.pages.cursor.is_some() {
            status.push(
                crate::ui_controls::button::quiet(if self.pages.loading {
                    "Loading viewed states…"
                } else if self.pages.error.is_some() {
                    "Retry viewed states"
                } else {
                    "Load more viewed states"
                })
                .on_press_maybe(
                    (!self.pages.loading && !modal_open)
                        .then_some(Action::FilePage(file_pages::PageAction::More)),
                )
                .into(),
            );
        }
        widget::column::with_children(vec![
            toolbar.into(),
            widget::container(widget::column::with_children(status)).into(),
            list,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn displayed_file_rows(&self, tallies: &[(usize, usize, usize)]) -> Vec<usize> {
        visible_review_rows(&self.rows, &self.collapsed_directories)
            .filter(|(index, row)| {
                !self.hide_viewed
                    || match row.file {
                        Some(file) => !self.files[file].viewed,
                        None => tallies[*index].0 != tallies[*index].1,
                    }
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn file_scrollable(&self, modal_open: bool, height: f32) -> Element<'_, Action> {
        let tallies = directory_tallies(&self.rows, &self.files);
        let mut rows: Vec<crate::sticky_tree::Row<'_, Action>> = Vec::new();
        let displayed = self.displayed_file_rows(&tallies);
        let window = file_pages::window_rows(&self.rows, &displayed, self.pages.offset, height);
        let mut previous = 0;
        for position in window {
            let index = displayed[position];
            let row = &self.rows[index];
            if position > previous {
                rows.push(crate::sticky_tree::Row {
                    depth: row.depth,
                    directory: false,
                    element: widget::Space::new()
                        .height(Length::Fixed(
                            (position - previous) as f32 * design::DENSE_ROW_HEIGHT,
                        ))
                        .into(),
                });
            }
            previous = position + 1;
            let element = match row.file {
                Some(file) => self.file_row(row, &self.files[file], modal_open),
                None => {
                    let (viewed, total, unknown) = tallies[index];
                    self.directory_row(row, viewed, total, unknown, modal_open)
                }
            };
            rows.push(crate::sticky_tree::Row {
                depth: row.depth,
                directory: row.file.is_none(),
                element,
            });
        }
        if previous < displayed.len() {
            rows.push(crate::sticky_tree::Row {
                depth: 0,
                directory: false,
                element: widget::Space::new()
                    .height(Length::Fixed(
                        (displayed.len() - previous) as f32 * design::DENSE_ROW_HEIGHT,
                    ))
                    .into(),
            });
        }
        if rows.is_empty() {
            let message = match &self.load {
                LoadState::Loading => "",
                LoadState::Ready if self.hide_viewed && !self.files.is_empty() => {
                    "All files viewed."
                }
                LoadState::Ready => "This pull request changes no files.",
                LoadState::Idle | LoadState::Failed(_) => "",
            };
            if !message.is_empty() {
                rows.push(crate::sticky_tree::Row {
                    depth: 0,
                    directory: false,
                    element: widget::container(design::caption(message))
                        .padding(Padding::from([space::S, space::M]))
                        .into(),
                });
            }
        }
        widget::scrollable(
            widget::container(crate::sticky_tree::rows(rows, design::DENSE_ROW_HEIGHT))
                .padding(design::with_vertical_rail(Padding::from([
                    space::XS,
                    space::S,
                ])))
                .width(Length::Fill),
        )
        .id(self.file_scroll_id.clone())
        .on_scroll(|viewport| {
            Action::FilePage(file_pages::PageAction::Viewport {
                offset: viewport.absolute_offset().y,
                height: viewport.bounds().height,
            })
        })
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn file_row<'a>(
        &'a self,
        row: &'a Row,
        file: &'a ReviewFile,
        modal_open: bool,
    ) -> Element<'a, Action> {
        let selected = self.selected.as_ref() == Some(&row.path);
        let counts = line_change_counts(file.entry.added_lines, file.entry.removed_lines);
        let mut trailing: Vec<Element<'a, Action>> = Vec::with_capacity(3);
        if let Some(letter) = change_letter(&file.entry.kind) {
            trailing.push(
                design::caption(letter)
                    .class(cosmic::theme::Text::Custom(change_text_style(
                        &file.entry.kind,
                    )))
                    .into(),
            );
        }
        if !counts.is_empty() {
            trailing.push(crate::ui_controls::change_counts(&counts));
        }
        let (comments, unresolved) = self.discussion_counts(&row.path.display());
        if comments > 0 {
            trailing.push(
                widget::tooltip(
                    design::caption(if unresolved > 0 {
                        format!("{comments} · {unresolved} open")
                    } else {
                        format!("{comments}")
                    }),
                    design::caption(format!(
                        "{comments} comments, {unresolved} unresolved threads"
                    )),
                    widget::tooltip::Position::Top,
                )
                .into(),
            );
        }
        trailing.push(viewed_control(
            Mark::from_file(file),
            if file.sync == ViewedSync::Unknown {
                "Viewed state not loaded"
            } else {
                "Mark viewed"
            },
            "Mark not viewed",
            self.context.is_some() && file.sync != ViewedSync::Pending && !modal_open,
            self.fenced(Action::ToggleViewed(row.path.clone())),
        ));
        let name = design::body(&*row.label)
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::Start(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill);
        let name: Element<'a, Action> = if file.viewed {
            name.class(cosmic::theme::Text::Custom(design::muted_text))
                .into()
        } else {
            name.into()
        };
        let content = widget::row::with_children(vec![
            widget::Space::new()
                .width(Length::Fixed(f32::from(
                    u16::try_from(row.depth)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(TREE_INDENT),
                )))
                .into(),
            widget::container(crate::file_icon::icon(row.path.as_path()))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            name,
            widget::row::with_children(trailing)
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .height(Length::Fill);
        crate::interaction::hover(
            button::custom(content)
                .class(sidebar_button_style(selected))
                .name(format!("Review {}", row.path.display()))
                .selected(selected)
                .padding([0.0, space::XS])
                .height(Length::Fixed(design::DENSE_ROW_HEIGHT))
                .width(Length::Fill)
                .on_press_maybe((!modal_open).then(|| Action::SelectFile(row.path.clone()))),
        )
    }

    fn directory_row<'a>(
        &'a self,
        row: &'a Row,
        viewed: usize,
        total: usize,
        unknown: usize,
        modal_open: bool,
    ) -> Element<'a, Action> {
        let mark = if unknown > 0 {
            Mark::Mixed
        } else {
            Mark::from_tally(viewed, total)
        };
        let collapsed = self.collapsed_directories.contains(&row.path);
        let content = widget::row::with_children(vec![
            widget::Space::new()
                .width(Length::Fixed(f32::from(
                    u16::try_from(row.depth)
                        .unwrap_or(u16::MAX)
                        .saturating_mul(TREE_INDENT),
                )))
                .into(),
            widget::container(crate::file_icon::folder(!collapsed))
                .center_x(Length::Fixed(ROW_ICON_WIDTH))
                .into(),
            design::body(&*row.label)
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center)
        .height(Length::Fill);
        let folder = button::custom(content)
            .class(sidebar_button_style(false))
            .name(format!(
                "{} folder {}",
                if collapsed { "Expand" } else { "Collapse" },
                row.path.display()
            ))
            .on_press_maybe((!modal_open).then(|| Action::ToggleDirectory(row.path.clone())))
            .padding([0.0, space::XS])
            .height(Length::Fixed(design::DENSE_ROW_HEIGHT))
            .width(Length::Fill);
        widget::row::with_children(vec![
            folder.into(),
            viewed_control(
                mark,
                "Mark folder viewed",
                "Mark folder not viewed",
                self.context.is_some() && !modal_open && total > 0 && unknown == 0,
                self.fenced(Action::SetDirectoryViewed {
                    directory: row.path.clone(),
                    viewed: mark != Mark::Checked,
                }),
            ),
        ])
        .align_y(Alignment::Center)
        .height(Length::Fixed(design::DENSE_ROW_HEIGHT))
        .into()
    }
}

/// The list a shell without a project sidebar shows, one row per open pull
/// request. Choosing a row emits [`Action::Select`].
impl State {
    pub fn set_unavailable_reason(&mut self, reason: Option<String>) {
        self.unavailable_reason = reason;
    }

    pub fn browser<'a>(&'a self, modal_open: bool) -> Element<'a, Action> {
        if self.source.is_none() {
            return widget::container(
                design::caption(
                    self.unavailable_reason
                        .as_deref()
                        .unwrap_or("Open a project with a GitHub remote to browse pull requests."),
                )
                .wrapping(iced::widget::text::Wrapping::Word),
            )
            .padding(space::M)
            .into();
        }
        let query = self.browser_query.trim().to_lowercase();
        let mut browser_count = 0;
        let mut rows: Vec<Element<'a, Action>> = match &self.list {
            ListState::Idle | ListState::Loading => {
                vec![design::caption("Loading pull requests…").into()]
            }
            ListState::Failed(message) => vec![
                widget::container(
                    widget::column::with_children(vec![
                        design::caption(format!("Couldn’t list pull requests. {message}"))
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .into(),
                        crate::ui_controls::button::secondary("Try again")
                            .on_press_maybe((!modal_open).then_some(Action::LoadList))
                            .into(),
                    ])
                    .spacing(space::S),
                )
                .padding(Padding::from([space::S, space::M]))
                .into(),
            ],
            ListState::Ready(pull_requests) if pull_requests.is_empty() => vec![
                widget::container(design::caption("No open pull requests."))
                    .padding(Padding::from([space::S, space::M]))
                    .into(),
            ],
            ListState::Ready(pull_requests) => {
                let matches: Vec<_> = pull_requests
                    .iter()
                    .filter(|pr| {
                        self.browser_filter
                            .matches(pr, self.requests.requested().get(&pr.number))
                            && (query.is_empty()
                                || self.browser_keys.get(&pr.number).map_or_else(
                                    || pr.title.to_lowercase().contains(&query),
                                    |(title, _)| title.contains(&query),
                                )
                                || pr.number.to_string() == query.trim_start_matches('#')
                                || pr.author.as_ref().is_some_and(|author| {
                                    self.browser_keys.get(&pr.number).map_or_else(
                                        || {
                                            author
                                                .login
                                                .to_lowercase()
                                                .contains(query.trim_start_matches('@'))
                                        },
                                        |(_, author)| {
                                            author.contains(query.trim_start_matches('@'))
                                        },
                                    )
                                }))
                    })
                    .collect();
                browser_count = matches.len();
                let window = browser_row_window(
                    matches.len(),
                    self.browser_viewport.0,
                    self.browser_viewport.1,
                );
                let mut rows: Vec<Element<'a, Action>> = vec![
                    widget::Space::new()
                        .height(window.start as f32 * design::PULL_REQUEST_BROWSER_ROW_HEIGHT)
                        .into(),
                ];
                rows.extend(matches[window.clone()].iter().map(|pull_request| {
                    let row: Element<'a, Action> = widget::container(browser_row(
                        pull_request,
                        self.requests.label(pull_request.number),
                        self.target.as_ref().is_some_and(|target| {
                            target.pull_request.number == pull_request.number
                        }),
                        modal_open,
                    ))
                    .height(design::PULL_REQUEST_BROWSER_ROW_HEIGHT)
                    .into();
                    row.map(|action| Action::BrowserFenced {
                        generation: self.list_generation,
                        action: Box::new(action),
                    })
                }));
                rows.push(
                    widget::Space::new()
                        .height(
                            (matches.len() - window.end) as f32
                                * design::PULL_REQUEST_BROWSER_ROW_HEIGHT,
                        )
                        .into(),
                );
                rows
            }
        };
        // Request discovery is independent of the visible list's first page.
        // Offer exact numbers discovered later without inventing missing PR DTOs.
        if matches!(
            self.browser_filter,
            BrowserFilter::NeedsReview | BrowserFilter::ReRequested
        ) {
            for request in self.requests.requested().values() {
                let entry = &request.entry;
                let listed = matches!(&self.list, ListState::Ready(prs) if prs.iter().any(|pr| pr.number == entry.number));
                if listed
                    || (self.browser_filter == BrowserFilter::ReRequested
                        && request.history
                            != crate::github::review_requests::scan::History::ReRequested)
                    || !(query.is_empty()
                        || entry.title.to_lowercase().contains(&query)
                        || entry.number.to_string() == query.trim_start_matches('#'))
                {
                    continue;
                }
                rows.push(
                    button::custom(widget::column::with_children(vec![
                        design::body(entry.title.as_str()).into(),
                        design::caption(format!(
                            "#{} · {}",
                            entry.number,
                            self.requests
                                .label(entry.number)
                                .unwrap_or("Needs your review")
                        ))
                        .into(),
                    ]))
                    .class(sidebar_button_style(false))
                    .width(Length::Fill)
                    .padding([space::XS, space::S])
                    .on_press_maybe((!modal_open).then_some(Action::BrowserFenced {
                        generation: self.list_generation,
                        action: Box::new(Action::OpenNumber(entry.number)),
                    }))
                    .into(),
                );
            }
        }
        let count = match &self.list {
            ListState::Ready(pull_requests) if !pull_requests.is_empty() => browser_count,
            _ => 0,
        };
        let list: Element<'a, Action> = widget::scrollable(
            widget::container(widget::column::with_children(rows))
                .padding(design::with_vertical_rail(Padding::from([
                    space::XS,
                    space::S,
                ])))
                .width(Length::Fill),
        )
        .id(widget::Id::new("pr.browser"))
        .on_scroll(|viewport| Action::BrowserViewport {
            offset: viewport.absolute_offset().y,
            height: viewport.bounds().height,
        })
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        let controls = widget::column::with_children(vec![
            crate::ui_controls::field::search(
                "Search loaded PRs by title, # or @author",
                &self.browser_query,
            )
            .on_input(Action::BrowserQuery)
            .into(),
            widget::row::with_children(vec![
                crate::ui_controls::field::select(
                    &BrowserFilter::ALL,
                    Some(self.browser_filter as usize),
                    Action::BrowserFilterIndex,
                )
                .into(),
                icon_button(Icon::Refresh, "Refresh pull requests")
                    .on_press_maybe((!modal_open && !self.list_loading).then_some(Action::LoadList))
                    .into(),
            ])
            .spacing(space::S)
            .into(),
            design::caption(format!(
                "{count} matching · {} loaded",
                match &self.list {
                    ListState::Ready(rows) => rows.len(),
                    _ => 0,
                }
            ))
            .into(),
            widget::container(widget::column::with_children(
                self.requests
                    .status()
                    .map(|status| design::caption(status).into())
                    .into_iter()
                    .collect::<Vec<_>>(),
            ))
            .into(),
        ])
        .spacing(space::S)
        .padding(space::M);
        let mut column = widget::column::with_children(vec![controls.into()]);
        if let Ok(number) = query.trim_start_matches('#').parse::<u64>()
            && number > 0
        {
            column = column.push(
                crate::ui_controls::button::secondary(format!("Open #{number}")).on_press_maybe(
                    (!modal_open).then_some(Action::BrowserFenced {
                        generation: self.list_generation,
                        action: Box::new(Action::OpenNumber(number)),
                    }),
                ),
            );
        }
        let mut column = column.push(list);
        if let Some(error) = &self.list_error {
            column =
                column.push(design::caption(error).wrapping(iced::widget::text::Wrapping::Word));
        }
        if self.list_cursor.is_some() {
            column = column.push(
                crate::ui_controls::button::secondary("Load more pull requests").on_press_maybe(
                    (!modal_open && !self.list_loading).then_some(Action::LoadMore),
                ),
            );
        }
        column.height(Length::Fill).width(Length::Fill).into()
    }
}

/// One pull request in the browser: state glyph, title, and number, author
/// and check status beneath.
pub fn browser_row<'a>(
    pull_request: &'a PullRequest,
    review_request: Option<&str>,
    selected: bool,
    modal_open: bool,
) -> Element<'a, Action> {
    let state = WorktreePullRequestState::from_github(pull_request);
    let checks = CheckStatus::from_checks(&pull_request.check_summaries());
    let icon = Icon::handle(match state {
        Some(WorktreePullRequestState::Merged) => Icon::MergedPullRequest,
        Some(WorktreePullRequestState::Closed) => Icon::ClosedPullRequest,
        _ => Icon::PullRequest,
    });
    let tone: fn(&cosmic::Theme) -> iced::Color = match (state, checks) {
        (Some(WorktreePullRequestState::Merged), _) => design::accent_color,
        (Some(WorktreePullRequestState::Closed), _) | (_, CheckStatus::Failing) => {
            design::destructive_color
        }
        (_, CheckStatus::Pending) => design::warning_color,
        (Some(WorktreePullRequestState::Draft), _) => design::muted_icon_color,
        (_, CheckStatus::Passing) => design::success_color,
        (_, CheckStatus::Unknown) => design::muted_icon_color,
    };
    let mut detail = vec![format!("#{}", pull_request.number)];
    if let Some(author) = &pull_request.author {
        detail.push(author.login.clone());
    }
    if pull_request.is_draft {
        detail.push("Draft".into());
    }
    if let Some(label) = review_request {
        detail.push(label.into());
    }
    match checks {
        CheckStatus::Passing => detail.push("Checks passing".into()),
        CheckStatus::Pending => detail.push("Checks pending".into()),
        CheckStatus::Failing => detail.push("Checks failing".into()),
        CheckStatus::Unknown => {}
    }
    let one_line = |text: widget::text::Text<'a, cosmic::Theme, cosmic::Renderer>| {
        text.wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
    };
    let content = widget::row::with_children(vec![
        widget::container(widget::icon::icon(icon).size(ROW_ICON_SIZE).class(
            cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
                cosmic::iced::widget::svg::Style {
                    color: Some(tone(theme)),
                }
            })),
        ))
        .center_x(Length::Fixed(ROW_ICON_WIDTH))
        .into(),
        widget::column::with_children(vec![
            one_line(design::body(pull_request.title.as_str())).into(),
            one_line(design::caption(detail.join(" · "))).into(),
        ])
        .width(Length::Fill)
        .into(),
    ])
    .spacing(space::S)
    .align_y(Alignment::Center);
    crate::interaction::hover(
        button::custom(content)
            .class(sidebar_button_style(selected))
            .name(format!(
                "Review pull request #{} {}",
                pull_request.number, pull_request.title
            ))
            .selected(selected)
            .padding([space::XS, space::S])
            .width(Length::Fill)
            .on_press_maybe((!modal_open).then_some(Action::Select(pull_request.number))),
    )
}

/// The three states a viewed mark draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Unchecked,
    Mixed,
    Checked,
}

impl Mark {
    fn from_file(file: &ReviewFile) -> Self {
        if file.sync == ViewedSync::Unknown {
            return Self::Mixed;
        }
        if file.viewed {
            Self::Checked
        } else {
            Self::Unchecked
        }
    }

    fn from_tally(viewed: usize, total: usize) -> Self {
        if total == 0 || viewed == 0 {
            Self::Unchecked
        } else if viewed == total {
            Self::Checked
        } else {
            Self::Mixed
        }
    }
}

fn viewed_control<'a>(
    mark: Mark,
    label_when_unchecked: &'static str,
    label_when_checked: &'static str,
    enabled: bool,
    action: Action,
) -> Element<'a, Action> {
    let (icon, label) = match mark {
        Mark::Unchecked => (Icon::Checkbox, label_when_unchecked),
        Mark::Mixed => (Icon::CheckboxMixed, label_when_unchecked),
        Mark::Checked => (Icon::CheckboxChecked, label_when_checked),
    };
    let control = icon_button(icon, label).on_press_maybe(enabled.then_some(action));
    match mark {
        Mark::Unchecked => control.into(),
        Mark::Mixed | Mark::Checked => control
            .tone(design::Tone::Themed(design::accent_color))
            .into(),
    }
}

/// A one-letter change status, as review tools conventionally show it.
fn change_letter(kind: &ChangeKind) -> Option<&'static str> {
    Some(match kind {
        ChangeKind::Added | ChangeKind::Untracked => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
        ChangeKind::Renamed => "R",
        ChangeKind::Copied => "C",
        ChangeKind::TypeChanged => "T",
        ChangeKind::Unmerged => "U",
        ChangeKind::Ignored | ChangeKind::Unknown(_) => return None,
    })
}

fn change_text_style(kind: &ChangeKind) -> fn(&cosmic::Theme) -> iced::widget::text::Style {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked | ChangeKind::Copied => design::added_text,
        ChangeKind::Deleted => design::removed_text,
        _ => design::muted_text,
    }
}

/// Per row, `(viewed, total)` files beneath a directory; `(0, 0)` for files.
///
/// One pass with a stack of open directories, so a tree of hundreds of files
/// costs hundreds of steps per frame rather than directories times files.
fn directory_tallies(rows: &[Row], files: &[ReviewFile]) -> Vec<(usize, usize, usize)> {
    let mut tallies = vec![(0usize, 0usize, 0usize); rows.len()];
    let mut open: Vec<(usize, usize)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        while open.last().is_some_and(|(_, depth)| *depth >= row.depth) {
            open.pop();
        }
        match row.file {
            Some(file) => {
                let viewed = usize::from(files[file].viewed);
                for (directory, _) in &open {
                    let tally = &mut tallies[*directory];
                    tally.0 += viewed;
                    tally.1 += 1;
                    tally.2 += usize::from(files[file].sync == ViewedSync::Unknown);
                }
            }
            None => open.push((index, row.depth)),
        }
    }
    tallies
}

/// Keep the original indices so directory marks still use tallies over all
/// files, including those hidden under a folded folder.
fn visible_review_rows<'a>(
    rows: &'a [Row],
    collapsed: &BTreeSet<RepoPath>,
) -> impl Iterator<Item = (usize, &'a Row)> {
    let mut skip_below = None;
    rows.iter().enumerate().filter(move |(_, row)| {
        if let Some(depth) = skip_below {
            if row.depth > depth {
                return false;
            }
            skip_below = None;
        }
        if row.file.is_none() && collapsed.contains(&row.path) {
            skip_below = Some(row.depth);
        }
        true
    })
}

/// The tree in display order, with each file row pointing at its file.
fn tree_rows(files: &[ReviewFile]) -> Vec<Row> {
    let paths: Vec<RepoPath> = files.iter().map(|file| file.entry.path.clone()).collect();
    let index: HashMap<&RepoPath, usize> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| (path, index))
        .collect();
    build_file_tree(&paths)
        .into_iter()
        .map(|entry| Row {
            label: entry.path.file_name().into_owned().into_boxed_str(),
            file: if entry.directory {
                None
            } else {
                index.get(&entry.path).copied()
            },
            depth: entry.depth,
            path: entry.path,
        })
        .collect()
}

/// Git's entries carry the rename pairs and measured counts; GitHub's carry
/// the viewed marks. Git leads, GitHub-only paths follow so nothing GitHub
/// lists goes unmarkable.
fn join_files(entries: &[DiffEntry], files: &PullRequestFiles) -> Vec<ReviewFile> {
    let mut remote: HashMap<&str, &crate::github::PullRequestFile> = files
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut joined: Vec<ReviewFile> = entries
        .iter()
        .map(|entry| {
            let viewed = remote
                .remove(entry.path.display().as_ref())
                .is_some_and(|file| file.viewer_viewed_state == FileViewedState::Viewed);
            ReviewFile {
                entry: entry.clone(),
                viewed,
                sync: ViewedSync::Settled,
            }
        })
        .collect();
    let mut extra: Vec<&crate::github::PullRequestFile> = remote.into_values().collect();
    extra.sort_by(|a, b| a.path.cmp(&b.path));
    for file in extra {
        let Ok(path) = RepoPath::parse(&file.path) else {
            continue;
        };
        joined.push(ReviewFile {
            entry: DiffEntry {
                path,
                old_path: None,
                kind: github_change_kind(file.change_type.as_ref().map(|value| value.0.as_str())),
                added_lines: Some(file.additions),
                removed_lines: Some(file.deletions),
            },
            viewed: file.viewer_viewed_state == FileViewedState::Viewed,
            sync: ViewedSync::Settled,
        });
    }
    joined
}

fn github_change_kind(change_type: Option<&str>) -> ChangeKind {
    match change_type.map(str::to_ascii_uppercase).as_deref() {
        Some("ADDED") => ChangeKind::Added,
        Some("MODIFIED") => ChangeKind::Modified,
        Some("DELETED") => ChangeKind::Deleted,
        Some("RENAMED") => ChangeKind::Renamed,
        Some("COPIED") => ChangeKind::Copied,
        Some("CHANGED") => ChangeKind::TypeChanged,
        _ => ChangeKind::Unknown('?'),
    }
}

fn viewed_outcome(
    outcome: Result<ReviewMutationOutcome<()>, crate::github::GitHubError>,
) -> ViewedOutcome {
    match outcome {
        Ok(ReviewMutationOutcome::Applied(())) => ViewedOutcome::Applied,
        Ok(ReviewMutationOutcome::NotApplied { failure }) => {
            ViewedOutcome::NotApplied(failure.message)
        }
        Ok(ReviewMutationOutcome::OutcomeUnknown { failure }) => {
            ViewedOutcome::Unknown(failure.message)
        }
        // Validation and process failures never reached GitHub; anything else
        // may have.
        Err(
            error @ (crate::github::GitHubError::Validation(_)
            | crate::github::GitHubError::Process(_)),
        ) => ViewedOutcome::NotApplied(error.to_string()),
        Err(error) => ViewedOutcome::Unknown(error.to_string()),
    }
}

/// Drops the fetched ranges of pull requests not in `retain`, so closed ones
/// stop pinning their objects. Best effort: a failure costs disk, not
/// correctness, and the next listing retries it. A shell that lists pull
/// requests itself calls this when its listing lands; a state with a source
/// does so on its own.
pub fn prune_fetched_refs(
    git: RepositoryService,
    worktree: PathBuf,
    retain: Vec<u64>,
    cancellation: CancellationToken,
) -> iced::Task<()> {
    iced::Task::future(async move {
        drop(
            tokio::task::spawn_blocking(move || {
                git.prune_pull_request_refs(
                    &worktree,
                    |number| retain.contains(&number),
                    &cancellation,
                )
            })
            .await,
        );
    })
}

/// The whole load, on the blocking pool: GitHub's view of the pull request,
/// then the fetch that brings its objects here, then the file list from the
/// fetched range. A head that moves between the two is re-read once.
fn metadata_matches_context(pull_request: &PullRequest, context: &ReviewContext) -> bool {
    pull_request.head_ref_oid == context.head_oid
        && pull_request
            .id
            .as_ref()
            .is_none_or(|id| id == &context.pull_request_id)
        && pull_request
            .base_ref_oid
            .as_ref()
            .is_none_or(|oid| oid == &context.base_oid)
}

fn same_diff_authority(a: &ReviewContext, b: &ReviewContext) -> bool {
    a.pull_request_id == b.pull_request_id
        && a.head_oid == b.head_oid
        && a.base_oid == b.base_oid
        && a.viewer == b.viewer
}

fn load_review<E: crate::repository::ProcessExecutor>(
    api: &dyn GitHubReviewApi,
    git: &RepositoryService<E>,
    target: &ReviewTarget,
    previous: Option<&Loaded>,
    cancellation: &CancellationToken,
) -> Result<Loaded, String> {
    let key = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        .map_err(|error| error.to_string())?;
    // The context query fences the expensive Git lane. Unchanged revisions
    // need only fresh viewed/conversation data, never another fetch or diff.
    let initial = api
        .review_refresh(&key, cancellation)
        .map_err(|error| error.to_string())?;
    let initial_context = &initial.context;
    if let Some(previous) = previous
        && same_diff_authority(&previous.context, initial_context)
    {
        let page = initial.files;
        if !file_pages::matches_context(&page, initial_context) {
            return Err("The viewer or pull request changed while refreshing viewed marks. Refresh to retry.".into());
        }
        let mut pull_request = previous.pull_request.clone();
        let discussion_stamp = initial
            .metadata
            .as_ref()
            .map(|metadata| metadata.updated_at.clone());
        if let Some(metadata) = initial.metadata {
            pull_request.title = metadata.title;
            pull_request.body = metadata.body;
            pull_request.state = metadata.state;
            pull_request.is_draft = metadata.is_draft;
            pull_request.updated_at = metadata.updated_at;
            pull_request.review_decision = metadata.review_decision;
        }
        return Ok(Loaded {
            context: initial.context,
            pull_request,
            discussion_stamp,
            files: PullRequestFiles {
                files: page.files,
                truncated: false,
            },
            next_file_cursor: page.next_cursor,
            ..previous.clone()
        });
    }
    let mut initial = Some(initial);
    let remotes = git
        .remotes(&target.worktree, cancellation)
        .map_err(|error| error.to_string())?;
    let remote = remote_for_repository(&remotes, &target.repository).ok_or_else(|| {
        format!(
            "No Git remote in this worktree points at {}/{}.",
            target.repository.owner, target.repository.name
        )
    })?;
    let mut attempts = 0;
    let (pull_request, context, page, range, discussion_stamp) = loop {
        let snapshot = match initial.take() {
            Some(snapshot) => snapshot,
            None => api
                .review_refresh(&key, cancellation)
                .map_err(|error| error.to_string())?,
        };
        let context = snapshot.context;
        let files = snapshot.files;
        let discussion_stamp = snapshot.metadata.map(|metadata| metadata.updated_at);
        let pull_request = api
            .pull_request(&key, cancellation)
            .map_err(|error| error.to_string())?;
        if !metadata_matches_context(&pull_request, &context) {
            if attempts == 0 {
                attempts += 1;
                continue;
            }
            return Err("The pull request changed while loading its header. Refresh to load a consistent revision.".into());
        }
        let base_branch = ValidatedBranchName::parse(pull_request.base_ref_name.as_str())
            .map_err(|error| error.to_string())?;
        if !file_pages::matches_context(&files, &context) {
            if attempts == 0 {
                attempts += 1;
                continue;
            }
            return Err(
                "The viewer or pull request changed while loading viewed marks. Refresh to retry."
                    .into(),
            );
        }
        let head = Oid::parse(context.head_oid.as_str()).map_err(|error| error.to_string())?;
        match git.fetch_pull_request_range(
            &target.worktree, &remote, target.pull_request.number, &base_branch, &head, cancellation,
        ) {
            Ok(range) if range.base_tip.as_str() == context.base_oid => break (pull_request, context, files, range, discussion_stamp),
            Ok(_) | Err(RepositoryError::PullRequestHeadMoved { .. }) if attempts == 0 => { attempts += 1; }
            Ok(_) => return Err("The pull request base moved while loading. Refresh to fetch a consistent review range.".into()),
            Err(error) => return Err(error.to_string()),
        }
    };
    let entries = git
        .range_diff_entries(
            &target.worktree,
            &range.merge_base,
            &range.head,
            cancellation,
        )
        .map_err(|error| error.to_string())?;
    Ok(Loaded {
        pull_request,
        context,
        files: PullRequestFiles {
            files: page.files,
            truncated: false,
        },
        next_file_cursor: page.next_cursor,
        range,
        entries,
        discussion_stamp,
    })
}

/// Shared by shells that hand out one API to many panes.
pub type SharedApi = Arc<dyn GitHubReviewApi>;

/// A review API that records mutations and answers from a script, for tests
/// of the state machine above and of the shells that drive it.
#[cfg(test)]
pub(crate) struct ScriptedApi {
    pub viewed: std::sync::Mutex<Vec<(String, bool)>>,
    pub viewed_outcome: std::sync::Mutex<ReviewMutationOutcome<()>>,
    pub conversation: std::sync::Mutex<crate::github::PullRequestConversation>,
    pub submitted: std::sync::Mutex<Vec<crate::github::SubmitReviewRequest>>,
    pub context: std::sync::Mutex<Option<ReviewContext>>,
    pub context_calls: std::sync::atomic::AtomicUsize,
    pub page: std::sync::Mutex<Option<crate::github::PullRequestFilePage>>,
    pub batch_calls: std::sync::atomic::AtomicUsize,
    pub replies: std::sync::Mutex<Vec<crate::github::ReplyToThreadRequest>>,
    pub discarded: std::sync::Mutex<Vec<String>>,
    pub merge_base: std::sync::Mutex<Option<String>>,
}

#[cfg(test)]
impl ScriptedApi {
    pub(crate) fn new() -> Self {
        Self {
            viewed: std::sync::Mutex::new(Vec::new()),
            viewed_outcome: std::sync::Mutex::new(ReviewMutationOutcome::Applied(())),
            conversation: std::sync::Mutex::new(Default::default()),
            submitted: std::sync::Mutex::new(Vec::new()),
            context: Default::default(),
            context_calls: Default::default(),
            page: Default::default(),
            batch_calls: Default::default(),
            replies: Default::default(),
            discarded: Default::default(),
            merge_base: Default::default(),
        }
    }
}

#[cfg(test)]
impl GitHubReviewApi for ScriptedApi {
    fn pull_request(
        &self,
        _key: &PullRequestKey,
        _cancellation: &CancellationToken,
    ) -> Result<PullRequest, crate::github::GitHubError> {
        unreachable!("tests install targets directly")
    }
    fn pull_request_page(
        &self,
        _repository: &GitHubRepoKey,
        _cursor: Option<&str>,
        _cancellation: &CancellationToken,
    ) -> Result<PullRequestPage, crate::github::GitHubError> {
        unreachable!("tests install pages directly")
    }
    fn review_context(
        &self,
        _key: &PullRequestKey,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewContext, crate::github::GitHubError> {
        self.context_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self
            .context
            .lock()
            .unwrap()
            .clone()
            .expect("scripted context"))
    }

    fn review_merge_base(
        &self,
        _: &PullRequestKey,
        _: &str,
        _: &str,
        _: &CancellationToken,
    ) -> Result<String, crate::github::GitHubError> {
        Ok(self
            .merge_base
            .lock()
            .unwrap()
            .clone()
            .expect("scripted merge base"))
    }

    fn pull_request_file_page(
        &self,
        _key: &PullRequestKey,
        _cursor: Option<&str>,
        _cancellation: &CancellationToken,
    ) -> Result<crate::github::PullRequestFilePage, crate::github::GitHubError> {
        Ok(self
            .page
            .lock()
            .unwrap()
            .clone()
            .expect("scripted file page"))
    }

    fn pull_request_files(
        &self,
        _key: &PullRequestKey,
        _cancellation: &CancellationToken,
    ) -> Result<PullRequestFiles, crate::github::GitHubError> {
        unreachable!("tests install loads directly")
    }

    fn conversation(
        &self,
        _key: &PullRequestKey,
        _cancellation: &CancellationToken,
    ) -> Result<crate::github::PullRequestConversation, crate::github::GitHubError> {
        Ok(self.conversation.lock().unwrap().clone())
    }

    fn start_review(
        &self,
        _repository: &GitHubRepoKey,
        _pull_request_id: &str,
        _commit_oid: &str,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<crate::github::PendingReview>, crate::github::GitHubError>
    {
        unreachable!()
    }

    fn add_review_thread(
        &self,
        _repository: &GitHubRepoKey,
        _request: &crate::github::AddReviewThreadRequest,
        _cancellation: &CancellationToken,
    ) -> Result<
        ReviewMutationOutcome<crate::github::PullRequestReviewThread>,
        crate::github::GitHubError,
    > {
        unreachable!()
    }

    fn reply_to_thread(
        &self,
        _repository: &GitHubRepoKey,
        request: &crate::github::ReplyToThreadRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<crate::github::PullRequestComment>, crate::github::GitHubError>
    {
        self.replies.lock().unwrap().push(request.clone());
        Ok(ReviewMutationOutcome::Applied(serde_json::from_value(serde_json::json!({
            "id":"reply", "body":request.body, "createdAt":"2026-01-01T00:00:00Z", "url":"https://github.com/o/r/pull/1#reply"
        })).unwrap()))
    }

    fn update_comment(
        &self,
        _repository: &GitHubRepoKey,
        _comment_id: &str,
        _body: &str,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<crate::github::PullRequestComment>, crate::github::GitHubError>
    {
        unreachable!()
    }

    fn delete_comment(
        &self,
        _repository: &GitHubRepoKey,
        _comment_id: &str,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, crate::github::GitHubError> {
        unreachable!()
    }

    fn submit_review(
        &self,
        _repository: &GitHubRepoKey,
        request: &crate::github::SubmitReviewRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<crate::github::SubmittedReview>, crate::github::GitHubError>
    {
        self.submitted.lock().unwrap().push(request.clone());
        Ok(ReviewMutationOutcome::Applied(
            crate::github::SubmittedReview {
                id: "submitted".into(),
                state: crate::github::RemoteValue("APPROVED".into()),
                url: Url::parse("https://github.com/o/r/pull/1#review-1").unwrap(),
            },
        ))
    }

    fn discard_review(
        &self,
        _repository: &GitHubRepoKey,
        review_id: &str,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, crate::github::GitHubError> {
        self.discarded.lock().unwrap().push(review_id.into());
        Ok(ReviewMutationOutcome::Applied(()))
    }

    fn set_thread_resolved(
        &self,
        _repository: &GitHubRepoKey,
        _thread_id: &str,
        _resolved: bool,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, crate::github::GitHubError> {
        unreachable!()
    }

    fn set_file_viewed(
        &self,
        _repository: &GitHubRepoKey,
        _pull_request_id: &str,
        path: &str,
        viewed: bool,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, crate::github::GitHubError> {
        self.viewed.lock().unwrap().push((path.to_owned(), viewed));
        Ok(match &*self.viewed_outcome.lock().unwrap() {
            ReviewMutationOutcome::Applied(()) => ReviewMutationOutcome::Applied(()),
            ReviewMutationOutcome::NotApplied { failure } => ReviewMutationOutcome::NotApplied {
                failure: failure.clone(),
            },
            ReviewMutationOutcome::OutcomeUnknown { failure } => {
                ReviewMutationOutcome::OutcomeUnknown {
                    failure: failure.clone(),
                }
            }
        })
    }

    fn set_files_viewed(
        &self,
        repository: &GitHubRepoKey,
        pr: &str,
        paths: &[String],
        viewed: bool,
        cancellation: &CancellationToken,
    ) -> Vec<(
        String,
        Result<ReviewMutationOutcome<()>, crate::github::GitHubError>,
    )> {
        self.batch_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    self.set_file_viewed(repository, pr, path, viewed, cancellation),
                )
            })
            .collect()
    }

    fn list_pull_requests(
        &self,
        _request: &crate::github::ListPullRequestsRequest,
        _cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, crate::github::GitHubError> {
        unreachable!("tests install lists directly")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::github::{PullRequestFile, RemoteValue};
    use crate::repository::ValidatedRefName;

    fn path(value: &str) -> RepoPath {
        RepoPath::parse(value).unwrap()
    }

    pub(crate) fn entry(value: &str, kind: ChangeKind) -> DiffEntry {
        DiffEntry {
            path: path(value),
            old_path: None,
            kind,
            added_lines: Some(1),
            removed_lines: None,
        }
    }

    pub(crate) fn remote_file(value: &str, viewed: bool) -> PullRequestFile {
        PullRequestFile {
            path: value.to_owned(),
            additions: 1,
            deletions: 0,
            change_type: Some(RemoteValue("MODIFIED".into())),
            viewer_viewed_state: if viewed {
                FileViewedState::Viewed
            } else {
                FileViewedState::Unviewed
            },
        }
    }

    pub(crate) fn pull_request(number: u64) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "id": "PR_1",
            "number": number,
            "title": format!("Change {number}"),
            "body": "",
            "state": "OPEN",
            "isDraft": false,
            "headRefName": "feature",
            "headRefOid": "a".repeat(40),
            "baseRefName": "main",
            "updatedAt": "2026-01-01T00:00:00Z",
            "url": format!("https://github.com/o/r/pull/{number}"),
        }))
        .unwrap()
    }

    pub(crate) fn target(number: u64) -> ReviewTarget {
        ReviewTarget {
            repository: GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
            worktree: PathBuf::from("/repo"),
            pull_request: Arc::new(pull_request(number)),
        }
    }

    pub(crate) fn loaded(entries: Vec<DiffEntry>, files: Vec<PullRequestFile>) -> Loaded {
        let head = Oid::parse("a".repeat(40)).unwrap();
        Loaded {
            discussion_stamp: None,
            pull_request: pull_request(1),
            context: ReviewContext {
                pull_request_id: "PR_1".into(),
                head_oid: head.as_str().to_owned(),
                base_oid: "b".repeat(40),
                viewer: "me".into(),
                viewer_is_author: false,
                pending: None,
            },
            next_file_cursor: None,
            files: PullRequestFiles {
                files,
                truncated: false,
            },
            range: PullRequestRange {
                number: 1,
                base_branch: ValidatedBranchName::parse("main").unwrap(),
                head_mirror: ValidatedRefName::parse("refs/cagents/pull/1/head").unwrap(),
                base_mirror: ValidatedRefName::parse("refs/cagents/pull/1/base").unwrap(),
                head,
                base_tip: Oid::parse("b".repeat(40)).unwrap(),
                merge_base: Oid::parse("c".repeat(40)).unwrap(),
            },
            entries,
        }
    }

    pub(crate) fn state() -> (State, Arc<ScriptedApi>) {
        let api = Arc::new(ScriptedApi::new());
        let state = State::new(api.clone(), RepositoryService::system("git"));
        (state, api)
    }

    fn open_with(state: &mut State, loaded: Loaded) {
        drop(state.update(Action::Open(target(1))));
        finish_load(state, loaded);
    }

    #[test]
    fn unchanged_refresh_never_calls_git_or_rewrites_unchanged_cache() {
        struct NoGit;
        impl crate::repository::ProcessExecutor for NoGit {
            fn execute(
                &self,
                _: &crate::repository::CommandSpec,
                _: &CancellationToken,
            ) -> Result<crate::repository::ProcessOutput, crate::repository::ProcessFailure>
            {
                panic!("unchanged review must not run Git, including fetch or diff");
            }
        }
        let api = ScriptedApi::new();
        let previous = loaded(
            vec![entry("file.rs", ChangeKind::Modified)],
            vec![remote_file("file.rs", true)],
        );
        *api.context.lock().unwrap() = Some(previous.context.clone());
        *api.page.lock().unwrap() = Some(crate::github::PullRequestFilePage {
            files: previous.files.files.clone(),
            next_cursor: None,
            viewer: previous.context.viewer.clone(),
            pull_request_id: previous.context.pull_request_id.clone(),
            head_oid: previous.context.head_oid.clone(),
            base_oid: previous.context.base_oid.clone(),
        });
        let refreshed = load_review(
            &api,
            &RepositoryService::new("git", Arc::new(NoGit)),
            &target(1),
            Some(&previous),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            api.context_calls.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let (mut state, _) = state();
        state.target = Some(target(1));
        state.adopt(&previous);
        state.cache_dirty = false;
        state.adopt(&refreshed);
        assert!(!state.cache_dirty);
    }

    #[tokio::test]
    async fn two_hundred_folder_marks_use_one_authority_and_four_chunks() {
        use cosmic::iced::futures::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["remote", "add", "origin", "https://github.com/o/r"])
                .status()
                .unwrap()
                .success()
        );
        for viewed in [true, false] {
            let (mut state, api) = state();
            let mut target = target(1);
            target.worktree = dir.path().into();
            state.target = Some(target);
            let files: Vec<_> = (0..200).map(|i| format!("folder/file-{i}.rs")).collect();
            let previous = loaded(
                files
                    .iter()
                    .map(|name| entry(name, ChangeKind::Modified))
                    .collect(),
                files
                    .iter()
                    .map(|name| remote_file(name, !viewed))
                    .collect(),
            );
            *api.context.lock().unwrap() = Some(previous.context.clone());
            state.adopt(&previous);
            let task = state.set_viewed((0..200).collect(), viewed);
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            let mut completions = 0;
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    drop(state.update_inner(action));
                    completions += 1;
                }
            }
            assert_eq!(completions, 4);
            assert_eq!(
                api.context_calls.load(std::sync::atomic::Ordering::Relaxed),
                1
            );
            assert_eq!(
                api.batch_calls.load(std::sync::atomic::Ordering::Relaxed),
                4
            );
            assert!(
                state
                    .files
                    .iter()
                    .all(|file| file.viewed == viewed && file.sync == ViewedSync::Settled)
            );
        }
    }

    #[test]
    fn refreshed_metadata_replaces_stale_header_and_checks() {
        let (mut state, _) = state();
        let mut fresh = loaded(vec![], vec![]);
        fresh.pull_request.title = "Renamed on GitHub".into();
        fresh.pull_request.base_ref_name = "release".into();
        fresh.pull_request.review_decision = Some(RemoteValue("APPROVED".into()));
        open_with(&mut state, fresh.clone());
        assert_eq!(
            *state.target.as_ref().unwrap().pull_request,
            fresh.pull_request
        );
        assert!(metadata_matches_context(
            &fresh.pull_request,
            &fresh.context
        ));
        fresh.pull_request.head_ref_oid = "d".repeat(40);
        assert!(!metadata_matches_context(
            &fresh.pull_request,
            &fresh.context
        ));
    }

    #[test]
    fn cached_review_is_read_only_and_survives_offline_refresh() {
        let (mut state, _) = state();
        drop(state.update(Action::Open(target(1))));
        let generation = state.generation;
        state.cache_loading = true;
        drop(state.update(Action::Cached {
            generation,
            result: Ok(Some(Arc::new(cache::fixture()))),
        }));
        assert_eq!(state.selected, Some(path("src/main.rs")));
        assert_eq!(state.load, LoadState::Ready);
        assert!(state.context.is_none());
        assert_eq!(state.files[0].sync, ViewedSync::Unverified);
        drop(state.update(Action::ToggleViewed(path("src/main.rs"))));
        assert!(state.files[0].viewed);
        drop(state.update(Action::Loaded {
            generation,
            load: state.load_generation,
            result: Err("Offline".into()),
        }));
        assert_eq!(state.load, LoadState::Ready);
        assert!(state.context.is_none());
        let mut fresh = loaded(
            vec![entry("src/main.rs", ChangeKind::Modified)],
            vec![remote_file("src/main.rs", false)],
        );
        fresh.context.viewer = "other-account".into();
        state.adopt(&fresh);
        assert!(!state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Settled);
        assert!(state.cached_viewer.is_none());
        assert_eq!(state.selected, Some(path("src/main.rs")));
        drop(state.update(Action::Cached {
            generation,
            result: Ok(Some(Arc::new(cache::fixture()))),
        }));
        assert!(
            !state.files[0].viewed,
            "late cache cannot replace fresh authority"
        );
    }

    #[test]
    fn cached_old_head_restores_only_selection_and_stale_load_cannot_open_another_target() {
        let (mut state, _) = state();
        let mut newer = target(1);
        Arc::make_mut(&mut newer.pull_request).head_ref_oid = "d".repeat(40);
        drop(state.open(newer));
        state.adopt_cache(cache::fixture());
        assert!(state.files.is_empty());
        assert!(state.range.is_none());
        state.adopt(&loaded(
            vec![entry("src/main.rs", ChangeKind::Modified)],
            vec![remote_file("src/main.rs", false)],
        ));
        assert_eq!(state.selected, Some(path("src/main.rs")));
        let generation = state.generation;
        drop(state.open(target(2)));
        drop(state.update(Action::Cached {
            generation,
            result: Ok(Some(Arc::new(cache::fixture()))),
        }));
        assert!(state.files.is_empty());
        assert!(state.selected.is_none());
    }

    #[test]
    fn cached_viewed_shortcut_does_not_advance_without_marking() {
        let (mut state, _) = state();
        drop(state.open(target(1)));
        let mut snapshot = cache::fixture();
        snapshot.files[0].viewed = false;
        snapshot.files.push(cache::File {
            entry: entry("z.rs", ChangeKind::Modified),
            viewed: false,
            unverified: false,
        });
        state.adopt_cache(snapshot);
        drop(state.update(Action::ToggleViewedAndAdvance));
        assert_eq!(state.selected, Some(path("src/main.rs")));
        assert!(!state.files[0].viewed);
    }

    /// Answers the load the state is waiting on, as the worker would.
    pub(crate) fn finish_load(state: &mut State, loaded: Loaded) {
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(loaded)),
        }));
    }

    #[test]
    fn browser_pages_are_lazy_deduplicated_and_retryable() {
        let (mut state, _) = state();
        drop(state.set_source(Some(ReviewSource {
            repository: target(1).repository,
            worktree: "/repo".into(),
        })));
        drop(state.update(Action::ListLoaded {
            generation: state.list_generation,
            result: Ok(PullRequestPage {
                pull_requests: vec![pull_request(1)],
                next_cursor: Some("page-2".into()),
            }),
        }));
        assert!(!state.list_loading);
        assert!(state.listed_target(1).is_some());
        drop(state.update(Action::LoadMore));
        let page_generation = state.list_generation;
        drop(state.update(Action::LoadMore));
        assert_eq!(state.list_generation, page_generation, "one page at a time");
        drop(state.update(Action::ListLoaded {
            generation: page_generation,
            result: Err("offline".into()),
        }));
        assert!(
            state.listed_target(1).is_some(),
            "failed next page retains loaded rows"
        );
        assert_eq!(state.list_cursor.as_deref(), Some("page-2"));
        drop(state.update(Action::LoadMore));
        drop(state.update(Action::ListLoaded {
            generation: state.list_generation,
            result: Ok(PullRequestPage {
                pull_requests: vec![pull_request(1), pull_request(2)],
                next_cursor: None,
            }),
        }));
        let ListState::Ready(rows) = &state.list else {
            panic!("missing rows");
        };
        assert_eq!(
            rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(state.list_error.is_none());
        let completed = state.list_generation;
        drop(state.update(Action::LoadMore));
        assert_eq!(state.list_generation, completed);
    }

    #[test]
    fn a_new_source_or_closed_lookup_rejects_old_browser_results() {
        let (mut state, _) = state();
        drop(state.set_source(Some(ReviewSource {
            repository: target(1).repository,
            worktree: "/repo".into(),
        })));
        let previous_list = state.list_generation;
        drop(state.set_source(None));
        drop(state.update(Action::ListLoaded {
            generation: previous_list,
            result: Ok(PullRequestPage {
                pull_requests: vec![pull_request(1)],
                next_cursor: None,
            }),
        }));
        assert_eq!(state.list, ListState::Idle);
        drop(state.set_source(Some(ReviewSource {
            repository: target(1).repository,
            worktree: "/repo".into(),
        })));
        drop(state.update(Action::OpenNumber(99)));
        let first = state.generation;
        drop(state.update(Action::OpenNumber(100)));
        assert_ne!(state.generation, first);
        drop(state.update(Action::TargetLoaded {
            generation: first,
            result: Ok(target(99)),
        }));
        assert!(state.target.is_none());
        let second = state.generation;
        drop(state.update(Action::Close));
        drop(state.update(Action::TargetLoaded {
            generation: second,
            result: Ok(target(100)),
        }));
        assert!(state.target.is_none());
    }

    #[test]
    fn git_entries_lead_and_github_marks_join_by_path() {
        let files = join_files(
            &[
                entry("src/lib.rs", ChangeKind::Modified),
                entry("docs/new.md", ChangeKind::Added),
            ],
            &PullRequestFiles {
                files: vec![
                    remote_file("src/lib.rs", true),
                    remote_file("only-on-github.txt", false),
                ],
                truncated: false,
            },
        );
        assert_eq!(
            files
                .iter()
                .map(|file| (file.entry.path.display().into_owned(), file.viewed))
                .collect::<Vec<_>>(),
            vec![
                ("src/lib.rs".to_owned(), true),
                ("docs/new.md".to_owned(), false),
                ("only-on-github.txt".to_owned(), false),
            ]
        );
        assert_eq!(files[2].entry.kind, ChangeKind::Modified);
    }

    #[test]
    fn folded_review_folder_hides_only_its_descendants_and_keeps_marks() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("src/nested/a.rs", ChangeKind::Modified),
                    entry("src/other.rs", ChangeKind::Modified),
                    entry("top.rs", ChangeKind::Modified),
                ],
                vec![remote_file("src/nested/a.rs", true)],
            ),
        );
        let paths = |state: &State| {
            visible_review_rows(&state.rows, &state.collapsed_directories)
                .map(|(_, row)| row.path.display().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths(&state),
            [
                "src",
                "src/nested",
                "src/nested/a.rs",
                "src/other.rs",
                "top.rs"
            ]
        );
        drop(state.update(Action::ToggleDirectory(
            RepoPath::parse("src/nested").unwrap(),
        )));
        assert_eq!(
            paths(&state),
            ["src", "src/nested", "src/other.rs", "top.rs"]
        );
        drop(state.update(Action::ToggleDirectory(RepoPath::parse("src").unwrap())));
        assert_eq!(paths(&state), ["src", "top.rs"]);
        drop(state.update(Action::ToggleDirectory(RepoPath::parse("src").unwrap())));
        assert_eq!(
            paths(&state),
            ["src", "src/nested", "src/other.rs", "top.rs"]
        );
        let tallies = directory_tallies(&state.rows, &state.files);
        let src = state
            .rows
            .iter()
            .position(|row| row.path == RepoPath::parse("src").unwrap())
            .unwrap();
        assert_eq!(tallies[src], (1, 2, 0));
    }

    #[test]
    fn directory_marks_tally_their_files() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("src/a.rs", ChangeKind::Modified),
                    entry("src/b.rs", ChangeKind::Modified),
                    entry("top.rs", ChangeKind::Modified),
                ],
                vec![remote_file("src/a.rs", true)],
            ),
        );
        let tallies = directory_tallies(&state.rows, &state.files);
        let src = state
            .rows
            .iter()
            .position(|row| row.file.is_none() && row.path == path("src"))
            .unwrap();
        assert_eq!(tallies[src], (1, 2, 0));
        assert_eq!(Mark::from_tally(1, 2), Mark::Mixed);
        assert_eq!(state.files_under(&path("src")).len(), 2);
        assert_eq!(state.progress(), (1, 3));
        assert_eq!(state.progress_label(), "1 of 3 viewed");
    }

    #[test]
    fn a_file_toggle_is_optimistic_and_settles_on_success() {
        let (mut state, api) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        drop(state.update(Action::ToggleViewed(path("a.rs"))));
        assert!(state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Pending);
        // A second toggle while pending is ignored rather than raced.
        drop(state.update(Action::ToggleViewed(path("a.rs"))));
        assert!(state.files[0].viewed);
        drop(state.update(Action::ViewedApplied {
            authority_revision: state.authority_revision,
            generation: state.generation,
            results: vec![(path("a.rs"), true, ViewedOutcome::Applied)],
        }));
        assert_eq!(state.files[0].sync, ViewedSync::Settled);
        assert!(state.notice.is_none());
        assert!(api.viewed.lock().unwrap().is_empty(), "the task never ran");
    }

    #[test]
    fn viewed_marks_and_queued_toggles_do_not_cross_a_refreshed_head() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        let queued = state.fenced(Action::ToggleViewed(path("a.rs")));
        let previous_revision = state.authority_revision;
        drop(state.update(Action::ToggleViewed(path("a.rs"))));
        assert_eq!(state.files[0].sync, ViewedSync::Pending);
        let mut next = loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]);
        next.context.head_oid = "d".repeat(40);
        next.range.head = Oid::parse("d".repeat(40)).unwrap();
        state.adopt(&next);
        assert!(!state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Settled);
        drop(state.update(queued));
        drop(state.update(Action::ViewedApplied {
            generation: state.generation,
            authority_revision: previous_revision,
            results: vec![(path("a.rs"), true, ViewedOutcome::Applied)],
        }));
        assert!(!state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Settled);
    }

    #[test]
    fn a_refused_mark_reverts_and_an_unknown_one_stands_unverified() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("a.rs", ChangeKind::Modified),
                    entry("b.rs", ChangeKind::Modified),
                ],
                vec![],
            ),
        );
        drop(state.update(Action::ToggleViewed(path("a.rs"))));
        drop(state.update(Action::ToggleViewed(path("b.rs"))));
        drop(state.update(Action::ViewedApplied {
            authority_revision: state.authority_revision,
            generation: state.generation,
            results: vec![
                (
                    path("a.rs"),
                    true,
                    ViewedOutcome::NotApplied("refused".into()),
                ),
                (path("b.rs"), true, ViewedOutcome::Unknown("lost".into())),
            ],
        }));
        assert!(!state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Settled);
        assert!(state.files[1].viewed);
        assert_eq!(state.files[1].sync, ViewedSync::Unverified);
        assert!(state.notice.as_deref().unwrap().contains("b.rs"));
        // A refresh that reports GitHub's truth reconciles the unverified mark.
        drop(state.update(Action::Refresh));
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(loaded(
                vec![
                    entry("a.rs", ChangeKind::Modified),
                    entry("b.rs", ChangeKind::Modified),
                ],
                vec![remote_file("b.rs", false)],
            ))),
        }));
        assert!(!state.files[1].viewed);
        assert_eq!(state.files[1].sync, ViewedSync::Settled);
    }

    #[test]
    fn refresh_started_before_a_viewed_ack_cannot_undo_that_outcome() {
        for (initial, requested, outcome, expected, sync) in [
            (
                false,
                true,
                ViewedOutcome::Applied,
                true,
                ViewedSync::Settled,
            ),
            (
                true,
                false,
                ViewedOutcome::Applied,
                false,
                ViewedSync::Settled,
            ),
            (
                false,
                true,
                ViewedOutcome::NotApplied("refused".into()),
                false,
                ViewedSync::Settled,
            ),
            (
                false,
                true,
                ViewedOutcome::Unknown("lost receipt".into()),
                true,
                ViewedSync::Unverified,
            ),
        ] {
            let (mut state, _) = state();
            let snapshot = loaded(
                vec![entry("a.rs", ChangeKind::Modified)],
                vec![remote_file("a.rs", initial)],
            );
            open_with(&mut state, snapshot.clone());
            drop(state.update(Action::Refresh));
            drop(state.update(Action::ToggleViewed(path("a.rs"))));
            drop(state.update(Action::ViewedApplied {
                generation: state.generation,
                authority_revision: state.authority_revision,
                results: vec![(path("a.rs"), requested, outcome)],
            }));
            finish_load(&mut state, snapshot.clone());
            assert_eq!(
                (state.files[0].viewed, state.files[0].sync),
                (expected, sync),
                "an older refresh cannot undo the newer mutation outcome"
            );
            // A genuinely subsequent read still accepts a change from another
            // GitHub window, instead of pinning this local mark indefinitely.
            drop(state.update(Action::Refresh));
            finish_load(&mut state, snapshot);
            assert_eq!(state.files[0].viewed, initial);
            assert_eq!(state.files[0].sync, ViewedSync::Settled);
        }
    }

    #[test]
    fn a_refresh_keeps_a_pending_mark_over_the_older_answer() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        drop(state.update(Action::ToggleViewed(path("a.rs"))));
        drop(state.update(Action::Refresh));
        assert!(state.refreshing);
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(loaded(
                vec![entry("a.rs", ChangeKind::Modified)],
                vec![remote_file("a.rs", false)],
            ))),
        }));
        assert!(!state.refreshing);
        assert!(state.files[0].viewed);
        assert_eq!(state.files[0].sync, ViewedSync::Pending);
    }

    #[test]
    fn file_navigation_skips_hidden_viewed_rows_and_reveals_collapsed_ancestors() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("a.rs", ChangeKind::Modified),
                    entry("b.rs", ChangeKind::Modified),
                    entry("src/c.rs", ChangeKind::Modified),
                ],
                vec![],
            ),
        );
        drop(state.update(Action::SetFileListMode(FileListMode::Flat)));
        state.files[1].viewed = true;
        state.hide_viewed = true;
        state.select_file(path("a.rs"));
        state.collapsed_directories.insert(path("src"));
        drop(state.update(Action::NextFile));
        assert_eq!(state.selected, Some(path("src/c.rs")));
        assert!(!state.collapsed_directories.contains(&path("src")));
        drop(state.update(Action::PreviousFile));
        assert_eq!(state.selected, Some(path("a.rs")));
        // An optimistic mark hides the current file before advancing; retain
        // its position so navigation does not jump back to the first file.
        state.select_file(path("b.rs"));
        drop(state.update(Action::NextFile));
        assert_eq!(state.selected, Some(path("src/c.rs")));
    }

    #[test]
    fn next_unviewed_wraps_past_viewed_and_collapsed_files_in_both_layouts() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("a.rs", ChangeKind::Modified),
                    entry("src/b.rs", ChangeKind::Modified),
                    entry("z.rs", ChangeKind::Modified),
                ],
                vec![],
            ),
        );
        state.files[0].viewed = true;
        state.collapsed_directories.insert(path("src"));
        state.select_file(path("z.rs"));
        drop(state.update(Action::NextUnviewed));
        assert_eq!(state.selected, Some(path("src/b.rs")));
        assert!(!state.collapsed_directories.contains(&path("src")));
        state.collapsed_directories.insert(path("src"));
        drop(state.update(Action::SetFileListMode(FileListMode::Flat)));
        assert!(
            state
                .rows
                .iter()
                .all(|row| row.file.is_some() && row.depth == 0)
        );
        assert_eq!(&*state.rows[1].label, "src/b.rs");
        drop(state.update(Action::NextUnviewed));
        assert_eq!(state.selected, Some(path("z.rs")));
        state.files.iter_mut().for_each(|file| file.viewed = true);
        drop(state.update(Action::NextUnviewed));
        assert_eq!(state.selected, Some(path("z.rs")));
        drop(state.update(Action::SetFileListMode(FileListMode::Tree)));
        assert!(state.collapsed_directories.contains(&path("src")));
        assert!(state.rows.iter().any(|row| row.file.is_none()));
        state.files.clear();
        state.rebuild_rows();
        drop(state.update(Action::NextUnviewed));
    }

    #[test]
    fn selection_opens_files_against_the_fetched_range_and_walks_the_tree() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(
                vec![
                    entry("z.rs", ChangeKind::Modified),
                    entry("src/a.rs", ChangeKind::Modified),
                ],
                vec![],
            ),
        );
        assert_eq!(
            state.take_effects(),
            vec![Effect::SelectionChanged(Some(1))]
        );
        drop(state.update(Action::NextFile));
        // Tree order puts the directory first.
        assert_eq!(state.selected, Some(path("src/a.rs")));
        let effects = state.take_effects();
        assert!(matches!(
            &effects[..],
            [Effect::OpenFile { path, basis: DiffBasis::PullRequest { number: 1, .. }, .. }]
                if *path == self::path("src/a.rs")
        ));
        drop(state.update(Action::NextFile));
        assert_eq!(state.selected, Some(path("z.rs")));
        drop(state.update(Action::NextFile));
        assert_eq!(state.selected, Some(path("z.rs")));
        assert_eq!(state.take_effects().len(), 1);
        drop(state.update(Action::PreviousFile));
        assert_eq!(state.selected, Some(path("src/a.rs")));
        drop(state.update(Action::ToggleViewedAndAdvance));
        assert!(state.files[1].viewed);
        assert_eq!(state.selected, Some(path("z.rs")));
    }

    #[test]
    fn a_stale_load_or_mark_for_another_target_is_dropped() {
        let (mut state, _) = state();
        drop(state.update(Action::Open(target(1))));
        let generation = state.generation;
        let load = state.load_generation;
        drop(state.update(Action::Close));
        assert_eq!(
            state.take_effects().last(),
            Some(&Effect::SelectionChanged(None))
        );
        drop(state.update(Action::Loaded {
            generation,
            load,
            result: Ok(Arc::new(loaded(
                vec![entry("a.rs", ChangeKind::Modified)],
                vec![],
            ))),
        }));
        assert!(state.files.is_empty());
        assert_eq!(state.load, LoadState::Idle);
        drop(state.update(Action::Open(target(2))));
        drop(state.update(Action::ViewedApplied {
            authority_revision: state.authority_revision,
            generation,
            results: vec![(path("a.rs"), true, ViewedOutcome::Applied)],
        }));
        assert_eq!(state.load, LoadState::Loading);
    }

    #[test]
    fn a_selected_file_that_leaves_the_range_closes_the_viewer() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        drop(state.update(Action::SelectFile(path("a.rs"))));
        drop(state.take_effects());
        drop(state.update(Action::Refresh));
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(loaded(
                vec![entry("b.rs", ChangeKind::Modified)],
                vec![],
            ))),
        }));
        assert_eq!(state.selected, None);
        assert_eq!(state.take_effects(), vec![Effect::CloseFile]);
    }

    #[test]
    fn closing_with_a_file_open_closes_the_viewer_too() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        drop(state.update(Action::SelectFile(path("a.rs"))));
        drop(state.take_effects());
        drop(state.update(Action::Close));
        assert_eq!(
            state.take_effects(),
            vec![Effect::CloseFile, Effect::SelectionChanged(None)]
        );
        // Already closed: nothing to tell the shell.
        drop(state.update(Action::Close));
        assert_eq!(state.take_effects(), vec![]);
    }

    #[test]
    fn checkout_uses_the_loaded_commit_and_refuses_stale_or_offline_authority() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        state.take_effects();
        let action = state.fenced(Action::Checkout);
        drop(state.update(action.clone()));
        let effects = state.take_effects();
        let [Effect::Checkout(target)] = effects.as_slice() else {
            panic!("checkout effect");
        };
        assert_eq!(target.repository, PathBuf::from("/repo"));
        assert_eq!(target.head.as_str(), "a".repeat(40));
        assert_eq!(target.suggested_branch(), "pr/1-aaaaaaaa");
        state.context = None;
        drop(state.update(Action::Checkout));
        assert!(state.take_effects().is_empty());
        state.authority_revision += 1;
        drop(state.update(action));
        assert!(state.take_effects().is_empty());
    }

    /// A refresh after the head moved re-fetches the range; a file still in
    /// it is reopened against the new head so the viewer is not left on
    /// the old diff.
    #[test]
    fn a_refresh_that_moves_the_head_reopens_the_selected_file() {
        let (mut state, _) = state();
        open_with(
            &mut state,
            loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]),
        );
        drop(state.update(Action::SelectFile(path("a.rs"))));
        drop(state.take_effects());

        drop(state.update(Action::Refresh));
        let unchanged = loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]);
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(unchanged)),
        }));
        assert_eq!(state.take_effects(), vec![], "same range, nothing to redo");

        drop(state.update(Action::Refresh));
        let mut moved = loaded(vec![entry("a.rs", ChangeKind::Modified)], vec![]);
        moved.range.head = Oid::parse("d".repeat(40)).unwrap();
        drop(state.update(Action::Loaded {
            generation: state.generation,
            load: state.load_generation,
            result: Ok(Arc::new(moved)),
        }));
        assert_eq!(state.selected, Some(path("a.rs")));
        assert!(matches!(
            &state.take_effects()[..],
            [Effect::OpenFile { path, .. }] if *path == self::path("a.rs")
        ));
    }

    #[test]
    fn the_own_list_resolves_numbers_to_targets() {
        let (mut state, _) = state();
        drop(state.set_source(Some(ReviewSource {
            repository: GitHubRepoKey::new("github.com", None, "o", "r").unwrap(),
            worktree: PathBuf::from("/repo"),
        })));
        assert_eq!(state.list, ListState::Loading);
        drop(state.update(Action::ListLoaded {
            generation: state.list_generation,
            result: Ok(PullRequestPage {
                pull_requests: vec![pull_request(7)],
                next_cursor: None,
            }),
        }));
        assert!(state.listed_target(8).is_none());
        drop(state.update(Action::Select(7)));
        assert_eq!(
            state
                .target
                .as_ref()
                .map(|target| target.pull_request.number),
            Some(7)
        );
        drop(state.set_source(None));
        assert!(state.target.is_none());
        assert_eq!(state.list, ListState::Idle);
    }
}

fn browser_row_window(total: usize, offset: f32, height: f32) -> std::ops::Range<usize> {
    const OVERSCAN: usize = 8;
    let height = height.max(0.0);
    let offset = offset
        .max(0.0)
        .min((total as f32 * design::PULL_REQUEST_BROWSER_ROW_HEIGHT - height).max(0.0));
    let first = (offset / design::PULL_REQUEST_BROWSER_ROW_HEIGHT).floor() as usize;
    let start = first.saturating_sub(OVERSCAN).min(total);
    let end =
        (first + (height / design::PULL_REQUEST_BROWSER_ROW_HEIGHT).ceil() as usize + OVERSCAN)
            .min(total);
    start..end
}

#[cfg(test)]
mod browser_window_tests {
    use super::*;
    #[test]
    fn loaded_pr_window_is_bounded_and_reaches_the_end() {
        let window = browser_row_window(
            10_000,
            5000.0 * design::PULL_REQUEST_BROWSER_ROW_HEIGHT,
            480.0,
        );
        assert!(window.len() <= 26);
        assert!(window.contains(&5000));
        assert_eq!(browser_row_window(10_000, f32::MAX, 480.0).end, 10_000);
        assert!(browser_row_window(0, 10.0, 480.0).is_empty());
    }
}
