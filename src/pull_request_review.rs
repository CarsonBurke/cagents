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
    collections::{BTreeSet, HashMap},
    path::PathBuf,
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
    design::{self, ROW_ICON_SIZE},
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
    ui_controls::{icon_control, symbolic_icon},
};

type ActionTask = iced::Task<Action>;

pub mod discussion;
pub mod discussion_ui;

/// Indent per tree level, matching the sidebar's file tree.
const TREE_INDENT: u16 = 12;

/// The slot an icon sits in, matching the sidebar's rows.
const ROW_ICON_WIDTH: f32 = 20.0;

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

/// Whether a file's local viewed mark matches GitHub's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewedSync {
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
    pub context: ReviewContext,
    pub files: PullRequestFiles,
    pub range: PullRequestRange,
    pub entries: Vec<DiffEntry>,
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
    OpenUrl(Url),
    DismissNotice,
}

impl Action {
    /// Completions are fenced by generation and change nothing a modal
    /// protects, so they may land while one is open.
    pub fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Loaded { .. }
                | Self::ListLoaded { .. }
                | Self::TargetLoaded { .. }
                | Self::ViewedApplied { .. }
                | Self::Discussion(
                    discussion_ui::Action::Loaded { .. } | discussion_ui::Action::Finished { .. }
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
    OpenUrl(Url),
    /// Which pull request is under review changed; `None` after `Close`.
    SelectionChanged(Option<u64>),
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
}

impl BrowserFilter {
    const ALL: [Self; 5] = [
        Self::All,
        Self::Drafts,
        Self::Ready,
        Self::Passing,
        Self::Failing,
    ];
    fn matches(self, pull_request: &PullRequest) -> bool {
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
        }
    }
}

/// Where a shell's list and reviews come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSource {
    pub repository: GitHubRepoKey,
    pub worktree: PathBuf,
}

pub struct State {
    discussion: discussion_ui::DiscussionState,
    api: Arc<dyn GitHubReviewApi>,
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
    browser_filter: BrowserFilter,
    target: Option<ReviewTarget>,
    /// Bumped on every `Open` and `Close`: results from another target are
    /// dropped whatever they carried.
    generation: u64,
    /// Bumped per load within a target, so a refresh supersedes the load
    /// before it without invalidating viewed mutations in flight.
    load_generation: u64,
    last_refresh: Option<Instant>,
    cancellation: CancellationToken,
    load: LoadState,
    /// True while a refresh runs behind an already listed set of files.
    refreshing: bool,
    context: Option<ReviewContext>,
    authority_revision: u64,
    range: Option<PullRequestRange>,
    files: Vec<ReviewFile>,
    files_truncated: bool,
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
        self.list_cancellation.cancel();
    }
}

impl State {
    pub fn new(api: Arc<dyn GitHubReviewApi>, git: RepositoryService) -> Self {
        Self {
            discussion: discussion_ui::DiscussionState::default(),
            api,
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
            browser_filter: BrowserFilter::default(),
            target: None,
            generation: 0,
            load_generation: 0,
            last_refresh: None,
            cancellation: CancellationToken::new(),
            load: LoadState::Idle,
            refreshing: false,
            context: None,
            authority_revision: 0,
            range: None,
            files: Vec::new(),
            files_truncated: false,
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
        self.source = source;
        self.list_cancellation.cancel();
        self.list_generation = self.list_generation.wrapping_add(1);
        self.list = ListState::Idle;
        self.list_cursor = None;
        self.list_loading = false;
        self.list_error = None;
        self.browser_query.clear();
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
        match action {
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
                ActionTask::none()
            }
            Action::BrowserFilterIndex(index) => {
                if let Some(filter) = BrowserFilter::ALL.get(index) {
                    self.browser_filter = *filter;
                }
                ActionTask::none()
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
                        self.adopt(&loaded);
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
                    && self.last_refresh.is_some_and(|last| {
                        now.saturating_duration_since(last) >= Duration::from_secs(30)
                    })
                {
                    self.refresh()
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
                let Some(index) = self
                    .selected
                    .as_ref()
                    .and_then(|path| self.file_index(path))
                else {
                    return ActionTask::none();
                };
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
                for (path, viewed, outcome) in results {
                    let Some(index) = self.file_index(&path) else {
                        continue;
                    };
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
                ActionTask::none()
            }
            Action::OpenUrl(url) => {
                self.effects.push(Effect::OpenUrl(url));
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
        let closed = self.close();
        let generation = self.generation;
        let cancellation = self.cancellation.clone();
        let api = Arc::clone(&self.api);
        let load = ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    let key = PullRequestKey::new(source.repository.clone(), number)
                        .map_err(|error| error.to_string())?;
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
        let number = target.pull_request.number;
        self.reset_target();
        self.target = Some(target);
        self.load = LoadState::Loading;
        self.effects.push(Effect::SelectionChanged(Some(number)));
        self.start_load()
    }

    fn close(&mut self) -> ActionTask {
        let was_open = self.target.is_some();
        self.reset_target();
        if was_open {
            self.effects.push(Effect::SelectionChanged(None));
        }
        ActionTask::none()
    }

    fn reset_target(&mut self) {
        self.discussion = discussion_ui::DiscussionState::default();
        self.cancellation.cancel();
        self.cancellation = CancellationToken::new();
        self.generation = self.generation.wrapping_add(1);
        self.load_generation = 0;
        self.last_refresh = None;
        self.target = None;
        self.load = LoadState::Idle;
        self.refreshing = false;
        self.context = None;
        self.range = None;
        self.files.clear();
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
        if self.target.is_none() {
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
        let generation = self.generation;
        let load = self.load_generation;
        let cancellation = self.cancellation.clone();
        let api = Arc::clone(&self.api);
        let git = self.git.clone();
        ActionTask::perform(
            async move {
                let result = tokio::task::spawn_blocking(move || {
                    load_review(api.as_ref(), &git, &target, &cancellation).map(Arc::new)
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

    /// Installs a load, carrying pending marks across so an in-flight mutation
    /// is not undone by the older answer a refresh may return.
    fn adopt(&mut self, loaded: &Loaded) {
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
        self.files = join_files(&loaded.entries, &loaded.files);
        for file in &mut self.files {
            if !authority_changed
                && let Some((viewed, ViewedSync::Pending)) = previous.get(&file.entry.path)
            {
                file.viewed = *viewed;
                file.sync = ViewedSync::Pending;
            }
        }
        self.files_truncated = loaded.files.truncated;
        self.rebuild_rows();
        if authority_changed {
            self.authority_revision = self.authority_revision.wrapping_add(1);
        }
        self.context = Some(loaded.context.clone());
        self.range = Some(loaded.range.clone());
        self.load = LoadState::Ready;
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
            self.select_file(self.files[index].entry.path.clone());
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
        self.selected = Some(path);
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

    /// Flips the marks optimistically and sends them to GitHub in one
    /// sequential task: a directory of fifty files is fifty `gh` runs, and
    /// running them at once would only contend for the same permits.
    fn set_viewed(&mut self, indices: Vec<usize>, viewed: bool) -> ActionTask {
        let (Some(target), Some(context)) = (&self.target, &self.context) else {
            return ActionTask::none();
        };
        let mut paths = Vec::new();
        for index in indices {
            let file = &mut self.files[index];
            if file.sync == ViewedSync::Pending || file.viewed == viewed {
                continue;
            }
            file.viewed = viewed;
            file.sync = ViewedSync::Pending;
            paths.push(file.entry.path.clone());
        }
        if paths.is_empty() {
            return ActionTask::none();
        }
        let repository = target.repository.clone();
        let pull_request_id = context.pull_request_id.clone();
        let authority = discussion::Authority {
            target: target.clone(),
            context: context.clone(),
        };
        let git = self.git.clone();
        let api = Arc::clone(&self.api);
        let cancellation = self.cancellation.clone();
        let generation = self.generation;
        // If the blocking task itself is lost, the marks' fate is unknown,
        // not applied; leaving them pending would freeze them until the
        // pull request is reopened.
        let lost = paths.clone();
        let authority_revision = self.authority_revision;
        ActionTask::perform(
            async move {
                let results = tokio::task::spawn_blocking(move || {
                    let mut refused: Option<String> = None;
                    paths
                        .into_iter()
                        .map(|path| {
                            if let Some(message) = &refused {
                                return (path, viewed, ViewedOutcome::NotApplied(message.clone()));
                            }
                            if let Err(failure) = discussion::verify_authority(
                                api.as_ref(),
                                &git,
                                &authority,
                                &cancellation,
                            ) {
                                let message = match failure {
                                    discussion::Outcome::Refused(message)
                                    | discussion::Outcome::Unknown(message) => message,
                                    discussion::Outcome::Applied => unreachable!(),
                                };
                                refused = Some(message.clone());
                                return (path, viewed, ViewedOutcome::NotApplied(message));
                            }
                            let outcome = api.set_file_viewed(
                                &repository,
                                &pull_request_id,
                                &path.display(),
                                viewed,
                                &cancellation,
                            );
                            (path, viewed, viewed_outcome(outcome))
                        })
                        .collect()
                })
                .await
                .unwrap_or_else(|error| {
                    let outcome = ViewedOutcome::Unknown(error.to_string());
                    lost.into_iter()
                        .map(|path| (path, viewed, outcome.clone()))
                        .collect()
                });
                Action::ViewedApplied {
                    generation,
                    authority_revision,
                    results,
                }
            },
            |action| action,
        )
    }

    /// The header row: what is under review, how far along it is, and the
    /// controls that act on the whole pull request.
    pub fn header<'a>(&'a self, modal_open: bool) -> Element<'a, Action> {
        let spacing = cosmic::theme::spacing();
        let Some(target) = &self.target else {
            return widget::Space::new().into();
        };
        let pull_request = &target.pull_request;
        let (viewed, total) = self.progress();
        let state = WorktreePullRequestState::from_github(pull_request);
        let icon = symbolic_icon(match state {
            Some(WorktreePullRequestState::Merged) => "merged-pull-request-symbolic",
            Some(WorktreePullRequestState::Closed) => "closed-pull-request-symbolic",
            _ => "pull-request-symbolic",
        });
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
        let one_line = |text: widget::text::Text<'a, cosmic::Theme, cosmic::Renderer>| {
            text.wrapping(iced::widget::text::Wrapping::None).ellipsize(
                iced::widget::text::Ellipsize::End(EllipsizeHeightLimit::Lines(1)),
            )
        };
        let mut detail = vec![format!("#{}", pull_request.number)];
        if let Some(author) = &pull_request.author {
            detail.push(author.login.clone());
        }
        detail.push(format!(
            "{} ← {}",
            pull_request.base_ref_name, pull_request.head_ref_name
        ));
        let progress = if total == 0 {
            match &self.load {
                LoadState::Loading => "Loading files".to_owned(),
                _ => "No files".to_owned(),
            }
        } else {
            format!("{viewed} of {total} viewed")
        };
        let title = widget::column::with_children(vec![
            one_line(design::heading(pull_request.title.as_str()))
                .width(Length::Fill)
                .into(),
            one_line(design::caption(format!(
                "{} · {progress}",
                detail.join(" · ")
            )))
            .width(Length::Fill)
            .into(),
        ])
        .spacing(0)
        .width(Length::Fill);
        let busy = self.refreshing || self.load == LoadState::Loading;
        // The spinner has a permanent slot and the refresh control stays
        // mounted while busy: swapping one for the other would move the named
        // controls after it by an index, and iced reconciles named children
        // by the index their name held last frame.
        let activity: Element<'_, Action> = widget::container(if busy {
            crate::ui_controls::activity_spinner_tone(14.0, design::muted_icon_color)
        } else {
            widget::Space::new().into()
        })
        .center(Length::Fixed(ROW_ICON_WIDTH))
        .into();
        let controls = crate::ui_controls::toolbar_actions(vec![
            icon_control(
                symbolic_icon("view-refresh-symbolic"),
                if busy {
                    "Refreshing pull request"
                } else {
                    "Refresh pull request"
                },
                false,
                !modal_open && !busy,
                Some(Action::Refresh),
            )
            .into(),
            icon_control(
                symbolic_icon("external-link-symbolic"),
                "Open on GitHub",
                false,
                !modal_open,
                Some(Action::OpenUrl(pull_request.url.clone())),
            )
            .into(),
            icon_control(
                symbolic_icon("window-close-symbolic"),
                "Close review",
                false,
                !modal_open,
                Some(Action::Close),
            )
            .into(),
        ]);
        widget::row::with_children(vec![
            widget::container(widget::icon::icon(icon).size(ROW_ICON_SIZE).class(
                cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
                    cosmic::iced::widget::svg::Style {
                        color: Some(tone.resolve(theme)),
                    }
                })),
            ))
            .center_y(Length::Fill)
            .into(),
            title.into(),
            activity,
            controls,
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center)
        .padding(Padding::from([0, spacing.space_xs]))
        .height(design::HEADER_CONTENT_HEIGHT)
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

    /// The notice slot: a load failure, a refused mark, or nothing.
    ///
    /// Always an element, empty when there is nothing to say. The file rows
    /// beneath it are named, and a notice that appeared as a new child would
    /// shift them by an index under a click already in flight.
    pub fn notice(&self, modal_open: bool) -> Element<'_, Action> {
        let spacing = cosmic::theme::spacing();
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
            icon_control(
                symbolic_icon("view-refresh-symbolic"),
                "Try again",
                false,
                !modal_open,
                Some(Action::Refresh),
            )
        } else {
            icon_control(
                symbolic_icon("window-close-symbolic"),
                "Dismiss",
                false,
                !modal_open,
                Some(Action::DismissNotice),
            )
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
            .spacing(spacing.space_xxs)
            .padding(Padding::from([spacing.space_xxxs, spacing.space_xs])),
        )
        .width(Length::Fill)
        .into()
    }

    /// The file tree with a viewed mark per file and a tri-state mark per
    /// directory. Folders start open and may be folded independently.
    pub fn file_list(&self, modal_open: bool) -> Element<'_, Action> {
        let spacing = cosmic::theme::spacing();
        let tallies = directory_tallies(&self.rows, &self.files);
        let mut rows: Vec<crate::sticky_tree::Row<'_, Action>> =
            Vec::with_capacity(self.rows.len());
        for (index, row) in visible_review_rows(&self.rows, &self.collapsed_directories) {
            if self.hide_viewed
                && match row.file {
                    Some(file) => self.files[file].viewed,
                    None => tallies[index].0 == tallies[index].1,
                }
            {
                continue;
            }
            let element = match row.file {
                Some(file) => self.file_row(row, &self.files[file], modal_open),
                None => {
                    let (viewed, total) = tallies[index];
                    self.directory_row(row, viewed, total, modal_open)
                }
            };
            rows.push(crate::sticky_tree::Row {
                depth: row.depth,
                directory: row.file.is_none(),
                element,
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
                        .padding(Padding::from([spacing.space_xxs, spacing.space_xs]))
                        .into(),
                });
            }
        }
        let list: Element<'_, Action> = widget::scrollable(
            widget::container(crate::sticky_tree::rows(rows, design::DENSE_ROW_HEIGHT))
                .padding(design::with_vertical_rail(Padding::from([
                    spacing.space_xxxs,
                    spacing.space_xxs,
                ])))
                .width(Length::Fill),
        )
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        let (mode_icon, mode_label, next_mode) = match self.file_list_mode {
            FileListMode::Tree => ("view-list-symbolic", "View as list", FileListMode::Flat),
            FileListMode::Flat => (
                "view-list-tree-symbolic",
                "View as tree",
                FileListMode::Tree,
            ),
        };
        let controls = crate::ui_controls::toolbar_actions(vec![
            icon_control(
                symbolic_icon(mode_icon),
                mode_label,
                false,
                !modal_open,
                Some(Action::SetFileListMode(next_mode)),
            )
            .into(),
            icon_control(
                symbolic_icon("view-filter-symbolic"),
                if self.hide_viewed {
                    "Show viewed files"
                } else {
                    "Hide viewed files"
                },
                self.hide_viewed,
                !modal_open,
                Some(Action::ToggleHideViewed),
            )
            .into(),
            icon_control(
                symbolic_icon("go-next-symbolic"),
                "Next unviewed file",
                false,
                !modal_open && self.files.iter().any(|file| !file.viewed),
                Some(Action::NextUnviewed),
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
        .padding([0, spacing.space_xs]);
        widget::column::with_children(vec![toolbar.into(), list])
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
        let spacing = cosmic::theme::spacing();
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
            trailing.push(design::change_counts(&counts));
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
            "Mark viewed",
            "Mark not viewed",
            file.sync != ViewedSync::Pending && !modal_open,
            self.fenced(Action::ToggleViewed(row.path.clone())),
        ));
        let name = design::body(&*row.label)
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
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
                .spacing(spacing.space_xxs)
                .align_y(Alignment::Center)
                .into(),
        ])
        .spacing(spacing.space_xxxs)
        .align_y(Alignment::Center)
        .height(Length::Fill);
        crate::interaction::hover(
            button::custom(content)
                .class(sidebar_button_style(selected))
                .name(format!("Review {}", row.path.display()))
                .selected(selected)
                .padding([0, spacing.space_xxxs])
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
        modal_open: bool,
    ) -> Element<'a, Action> {
        let spacing = cosmic::theme::spacing();
        let mark = Mark::from_tally(viewed, total);
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
        .spacing(spacing.space_xxxs)
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
            .padding([0, spacing.space_xxxs])
            .height(Length::Fixed(design::DENSE_ROW_HEIGHT))
            .width(Length::Fill);
        widget::row::with_children(vec![
            folder.into(),
            viewed_control(
                mark,
                "Mark folder viewed",
                "Mark folder not viewed",
                !modal_open && total > 0,
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
    pub fn browser<'a>(&'a self, modal_open: bool) -> Element<'a, Action> {
        let spacing = cosmic::theme::spacing();
        if self.source.is_none() {
            return widget::container(
                design::caption("Open an authenticated GitHub repository to browse pull requests.")
                    .wrapping(iced::widget::text::Wrapping::Word),
            )
            .padding(spacing.space_xs)
            .into();
        }
        let query = self.browser_query.trim().to_lowercase();
        let rows: Vec<Element<'a, Action>> = match &self.list {
            ListState::Idle | ListState::Loading => {
                vec![design::caption("Loading pull requests…").into()]
            }
            ListState::Failed(message) => vec![
                widget::container(
                    widget::column::with_children(vec![
                        design::caption(format!("Couldn’t list pull requests. {message}"))
                            .wrapping(iced::widget::text::Wrapping::Word)
                            .into(),
                        button::standard("Try again")
                            .on_press_maybe((!modal_open).then_some(Action::LoadList))
                            .into(),
                    ])
                    .spacing(spacing.space_xxs),
                )
                .padding(Padding::from([spacing.space_xxs, spacing.space_xs]))
                .into(),
            ],
            ListState::Ready(pull_requests) if pull_requests.is_empty() => vec![
                widget::container(design::caption("No open pull requests."))
                    .padding(Padding::from([spacing.space_xxs, spacing.space_xs]))
                    .into(),
            ],
            ListState::Ready(pull_requests) => pull_requests
                .iter()
                .filter(|pr| {
                    self.browser_filter.matches(pr)
                        && (query.is_empty()
                            || pr.title.to_lowercase().contains(&query)
                            || pr.number.to_string() == query.trim_start_matches('#')
                            || pr.author.as_ref().is_some_and(|author| {
                                author
                                    .login
                                    .to_lowercase()
                                    .contains(query.trim_start_matches('@'))
                            }))
                })
                .map(|pull_request| {
                    browser_row(pull_request, false, modal_open).map(|action| {
                        Action::BrowserFenced {
                            generation: self.list_generation,
                            action: Box::new(action),
                        }
                    })
                })
                .collect(),
        };
        let count = match &self.list {
            ListState::Ready(pull_requests) if !pull_requests.is_empty() => rows.len(),
            _ => 0,
        };
        let list = widget::scrollable(
            widget::container(widget::column::with_children(rows).spacing(0))
                .padding(design::with_vertical_rail(Padding::from([
                    spacing.space_xxxs,
                    spacing.space_xxs,
                ])))
                .width(Length::Fill),
        )
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .into();
        let controls = widget::column::with_children(vec![
            widget::text_input(
                "Search loaded PRs by title, # or @author",
                &self.browser_query,
            )
            .on_input(Action::BrowserQuery)
            .into(),
            widget::row::with_children(vec![
                widget::dropdown(
                    &BrowserFilter::ALL,
                    Some(self.browser_filter as usize),
                    Action::BrowserFilterIndex,
                )
                .into(),
                icon_control(
                    symbolic_icon("view-refresh-symbolic"),
                    "Refresh pull requests",
                    false,
                    !modal_open && !self.list_loading,
                    Some(Action::LoadList),
                )
                .into(),
            ])
            .spacing(spacing.space_xxs)
            .into(),
            design::caption(format!(
                "{count} matching · {} loaded",
                match &self.list {
                    ListState::Ready(rows) => rows.len(),
                    _ => 0,
                }
            ))
            .into(),
        ])
        .spacing(spacing.space_xxs)
        .padding(spacing.space_xs);
        let mut column = widget::column::with_children(vec![controls.into(), list]).spacing(0);
        if let Some(error) = &self.list_error {
            column =
                column.push(design::caption(error).wrapping(iced::widget::text::Wrapping::Word));
        }
        if self.list_cursor.is_some() {
            column = column.push(
                button::standard(if self.list_loading {
                    "Loading…"
                } else {
                    "Load more pull requests"
                })
                .on_press_maybe((!modal_open && !self.list_loading).then_some(Action::LoadMore)),
            );
        }
        column.height(Length::Fill).width(Length::Fill).into()
    }
}

/// One pull request in the browser: state glyph, title, and number, author
/// and check status beneath.
pub fn browser_row<'a>(
    pull_request: &'a PullRequest,
    selected: bool,
    modal_open: bool,
) -> Element<'a, Action> {
    let spacing = cosmic::theme::spacing();
    let state = WorktreePullRequestState::from_github(pull_request);
    let checks = CheckStatus::from_checks(&pull_request.check_summaries());
    let icon = symbolic_icon(match state {
        Some(WorktreePullRequestState::Merged) => "merged-pull-request-symbolic",
        Some(WorktreePullRequestState::Closed) => "closed-pull-request-symbolic",
        _ => "pull-request-symbolic",
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
        .spacing(0)
        .width(Length::Fill)
        .into(),
    ])
    .spacing(spacing.space_xxs)
    .align_y(Alignment::Center);
    crate::interaction::hover(
        button::custom(content)
            .class(sidebar_button_style(selected))
            .name(format!(
                "Review pull request #{} {}",
                pull_request.number, pull_request.title
            ))
            .selected(selected)
            .padding([spacing.space_xxxs, spacing.space_xxs])
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
        Mark::Unchecked => ("checkbox-symbolic", label_when_unchecked),
        Mark::Mixed => ("checkbox-mixed-symbolic", label_when_unchecked),
        Mark::Checked => ("checkbox-checked-symbolic", label_when_checked),
    };
    let control = icon_control(symbolic_icon(icon), label, false, enabled, Some(action));
    match mark {
        Mark::Unchecked => control.into(),
        Mark::Mixed | Mark::Checked => control
            .icon_tone(design::Tone::Themed(design::accent_color))
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
fn directory_tallies(rows: &[Row], files: &[ReviewFile]) -> Vec<(usize, usize)> {
    let mut tallies = vec![(0usize, 0usize); rows.len()];
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
fn load_review(
    api: &dyn GitHubReviewApi,
    git: &RepositoryService,
    target: &ReviewTarget,
    cancellation: &CancellationToken,
) -> Result<Loaded, String> {
    let key = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        .map_err(|error| error.to_string())?;
    let remotes = git
        .remotes(&target.worktree, cancellation)
        .map_err(|error| error.to_string())?;
    let remote = remote_for_repository(&remotes, &target.repository).ok_or_else(|| {
        format!(
            "No Git remote in this worktree points at {}/{}.",
            target.repository.owner, target.repository.name
        )
    })?;
    let base_branch = ValidatedBranchName::parse(target.pull_request.base_ref_name.as_str())
        .map_err(|error| error.to_string())?;
    let mut context = api
        .review_context(&key, cancellation)
        .map_err(|error| error.to_string())?;
    let mut files = api
        .pull_request_files(&key, cancellation)
        .map_err(|error| error.to_string())?;
    let mut attempts = 0;
    let range = loop {
        let head = Oid::parse(context.head_oid.as_str()).map_err(|error| error.to_string())?;
        match git.fetch_pull_request_range(
            &target.worktree,
            &remote,
            target.pull_request.number,
            &base_branch,
            &head,
            cancellation,
        ) {
            Ok(range) if range.base_tip.as_str() == context.base_oid => break range,
            Ok(_) if attempts == 0 => {
                attempts += 1;
                context = api.review_context(&key, cancellation).map_err(|error| error.to_string())?;
                files = api.pull_request_files(&key, cancellation).map_err(|error| error.to_string())?;
            }
            Ok(_) => return Err("The pull request base moved while loading. Refresh to fetch a consistent review range.".into()),
            Err(RepositoryError::PullRequestHeadMoved { .. }) if attempts == 0 => {
                attempts += 1;
                context = api
                    .review_context(&key, cancellation)
                    .map_err(|error| error.to_string())?;
                files = api
                    .pull_request_files(&key, cancellation)
                    .map_err(|error| error.to_string())?;
            }
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
        context,
        files,
        range,
        entries,
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
}

#[cfg(test)]
impl ScriptedApi {
    pub(crate) fn new() -> Self {
        Self {
            viewed: std::sync::Mutex::new(Vec::new()),
            viewed_outcome: std::sync::Mutex::new(ReviewMutationOutcome::Applied(())),
            conversation: std::sync::Mutex::new(Default::default()),
            submitted: std::sync::Mutex::new(Vec::new()),
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
        unreachable!("tests install loads directly")
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
        _request: &crate::github::ReplyToThreadRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<crate::github::PullRequestComment>, crate::github::GitHubError>
    {
        unreachable!()
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
        _review_id: &str,
        _cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, crate::github::GitHubError> {
        unreachable!()
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
            context: ReviewContext {
                pull_request_id: "PR_1".into(),
                head_oid: head.as_str().to_owned(),
                base_oid: "b".repeat(40),
                viewer: "me".into(),
                viewer_is_author: false,
                pending: None,
            },
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
        assert_eq!(tallies[src], (1, 2));
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
        assert_eq!(tallies[src], (1, 2));
        assert_eq!(Mark::from_tally(1, 2), Mark::Mixed);
        assert_eq!(state.files_under(&path("src")).len(), 2);
        assert_eq!(state.progress(), (1, 3));
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
