//! Typed GitHub CLI integration.
//!
//! Authentication remains owned by `gh`.  The service neither requests nor
//! persists tokens, and every repository selector and user value is passed as
//! a distinct process argument.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::Url;

use crate::repository::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessFailure, ProcessFailureKind,
    ProcessOutput, SystemProcessExecutor,
};

#[cfg(feature = "ui")]
pub mod avatar;

const PR_LIST_FIELDS: &str = "id,number,title,state,isDraft,headRefName,headRefOid,headRepository,headRepositoryOwner,baseRefName,baseRefOid,author,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels,statusCheckRollup";
const PR_VIEW_FIELDS: &str = "id,number,title,body,state,isDraft,headRefName,headRefOid,headRepository,headRepositoryOwner,baseRefName,baseRefOid,author,createdAt,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels,additions,deletions,changedFiles,statusCheckRollup";
/// The fields every review comment carries, whichever query returns it.
const REVIEW_COMMENT_FIELDS: &str = "id author{login} body createdAt url viewerDidAuthor viewerCanUpdate viewerCanDelete commentState:state";
/// One review thread with its first page of comments.
const REVIEW_THREAD_FIELDS: &str = "id path line originalLine startLine originalStartLine diffSide startDiffSide isOutdated isResolved isCollapsed viewerCanResolve viewerCanUnresolve comments(first:50){nodes{id author{login} body createdAt url viewerDidAuthor viewerCanUpdate viewerCanDelete commentState:state} pageInfo{hasNextPage endCursor}}";
/// How many pages of changed files one listing reads before reporting truncation.
const FILE_PAGE_LIMIT: usize = 30;
const PR_CHECK_FIELDS: &str =
    "bucket,completedAt,description,event,link,name,startedAt,state,workflow";

/// How many `gh` processes may be in flight across the whole process.
///
/// Refreshes are issued one per project and run concurrently, so the per-refresh
/// bound alone would still multiply by the project count. This ceiling is above
/// the one-`gh`-per-project the serial code reached, so it bounds the burst
/// without making a many-project refresh slower than it used to be. Every
/// invocation takes a permit, not only the checks fetch: a review pane's
/// file-by-file mutations and the sidebar's refresh share one budget.
const GH_PROCESS_CONCURRENCY: usize = 12;

/// How long a completed checks result may be reused.
///
/// Check runs belong to the head commit rather than to the pull request record,
/// so re-running a workflow or a late external status changes the checks without
/// changing `updatedAt`. Identity and version alone would therefore pin a green
/// result indefinitely; the expiry bounds how long a stale one can survive.
const CHECKS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Completed check results, keyed by pull request identity and version.
///
/// The cache is process-wide because a [`GitHubService`] is constructed per
/// refresh, so an instance-local cache would never see a second lookup.
static CHECKS_CACHE: LazyLock<Mutex<HashMap<ChecksCacheKey, CachedChecks>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CHECKS_FLIGHTS: LazyLock<Mutex<HashMap<ChecksCacheKey, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Permits shared by every `gh` invocation; see [`GH_PROCESS_CONCURRENCY`].
static GH_PERMITS: LazyLock<CheckPermits> =
    LazyLock::new(|| CheckPermits::new(GH_PROCESS_CONCURRENCY));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChecksCacheKey {
    pull_request: PullRequestKey,
    /// `gh` reports this for every pull request it lists, and it changes whenever
    /// the pull request record does, so it is the version half of the key.
    updated_at: String,
    head_oid: String,
}

#[derive(Debug, Clone)]
struct CachedChecks {
    ttl: Duration,
    fetched_at: Instant,
    checks: Vec<PullRequestCheck>,
}

/// A panic elsewhere must not disable the cache, and the guarded map holds no
/// invariant that a panic could leave broken.
fn lock_checks_cache() -> MutexGuard<'static, HashMap<ChecksCacheKey, CachedChecks>> {
    CHECKS_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A counting semaphore. `tokio`'s lives on the async side of the application,
/// and this bound is taken by the blocking threads that run `gh`.
struct CheckPermits {
    available: Mutex<usize>,
    released: Condvar,
}

impl CheckPermits {
    fn new(limit: usize) -> Self {
        Self {
            available: Mutex::new(limit),
            released: Condvar::new(),
        }
    }

    /// Blocks until a permit is free. A holder never waits for anything but the
    /// subprocess it launched, and every permit is returned on drop, so waiting
    /// here cannot deadlock.
    fn acquire(&self) -> CheckPermit<'_> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *available == 0 {
            available = self
                .released
                .wait(available)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *available -= 1;
        CheckPermit(self)
    }
}

struct CheckPermit<'pool>(&'pool CheckPermits);

impl Drop for CheckPermit<'_> {
    fn drop(&mut self) {
        *self
            .0
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        self.0.released.notify_one();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubRepoKey {
    pub host: String,
    pub port: Option<u16>,
    pub owner: String,
    pub name: String,
}

impl GitHubRepoKey {
    pub fn same_repository(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }

    pub fn new(
        host: impl Into<String>,
        port: Option<u16>,
        owner: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, GitHubValidationError> {
        let host = host.into().to_ascii_lowercase();
        let owner = owner.into();
        let name = name.into().trim_end_matches(".git").to_owned();
        let authority = match port {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let parsed = Url::parse(&format!("https://{authority}/"))
            .map_err(|_| GitHubValidationError::InvalidHost(host.clone()))?;
        if parsed.host_str() != Some(host.as_str())
            || parsed.username() != ""
            || parsed.password().is_some()
            || !valid_repo_component(&owner)
            || !valid_repo_component(&name)
        {
            return Err(GitHubValidationError::InvalidRepository { owner, name });
        }
        Ok(Self {
            host,
            port,
            owner,
            name,
        })
    }

    pub fn gh_selector(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}/{}/{}", self.host, self.owner, self.name),
            None => format!("{}/{}/{}", self.host, self.owner, self.name),
        }
    }

    pub fn authority(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        let validated = Self::new(
            self.host.clone(),
            self.port,
            self.owner.clone(),
            self.name.clone(),
        )?;
        if validated == *self {
            Ok(())
        } else {
            Err(GitHubValidationError::InvalidRepository {
                owner: self.owner.clone(),
                name: self.name.clone(),
            })
        }
    }
}

fn valid_repo_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == 0)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitHubValidationError {
    #[error("invalid GitHub host: {0}")]
    InvalidHost(String),
    #[error("invalid GitHub repository {owner}/{name}")]
    InvalidRepository { owner: String, name: String },
    #[error("invalid pull request number")]
    InvalidPullRequestNumber,
    #[error("invalid pull request URL: {0}")]
    InvalidPullRequestUrl(String),
    #[error("invalid pull request field {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PullRequestKey {
    pub repository: GitHubRepoKey,
    pub number: u64,
}

impl PullRequestKey {
    pub fn new(repository: GitHubRepoKey, number: u64) -> Result<Self, GitHubValidationError> {
        if number == 0 {
            Err(GitHubValidationError::InvalidPullRequestNumber)
        } else {
            Ok(Self { repository, number })
        }
    }

    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        self.repository.validate()?;
        if self.number == 0 {
            Err(GitHubValidationError::InvalidPullRequestNumber)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteValue(pub String);

/// A GraphQL variable, typed so `gh` sends the JSON scalar the schema expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphQlValue {
    String(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for GraphQlValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for GraphQlValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<u64> for GraphQlValue {
    fn from(value: u64) -> Self {
        Self::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<bool> for GraphQlValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubActor {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "is_bot", default)]
    pub is_bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubHeadRepository {
    pub name: String,
}

/// Source identity is never inferred from the PR's base repository or author.
impl PullRequest {
    pub fn head_repository_key(&self) -> Option<GitHubRepoKey> {
        GitHubRepoKey::new(
            self.url.host_str()?,
            self.url.port(),
            &self.head_repository_owner.as_ref()?.login,
            &self.head_repository.as_ref()?.name,
        )
        .ok()
    }

    pub fn belongs_to_head(&self, repository: &GitHubRepoKey) -> bool {
        self.head_repository_key()
            .is_some_and(|head| head.same_repository(repository))
    }
}

pub fn worktree_head_repository(
    worktree: &Path,
    branch: &str,
    cancellation: &CancellationToken,
) -> Option<GitHubRepoKey> {
    let branch = crate::repository::ValidatedBranchName::parse(branch).ok()?;
    let remote = crate::repository::RepositoryService::system("git")
        .branch_remote(worktree, &branch, cancellation)
        .ok()??;
    remote
        .push_url
        .as_deref()
        .or(remote.fetch_url.as_deref())
        .and_then(remote_repository_key)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    /// The GraphQL node id, which every review mutation addresses.
    #[serde(default)]
    pub id: Option<String>,
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub state: RemoteValue,
    #[serde(default)]
    pub is_draft: bool,
    pub head_ref_name: String,
    #[serde(default)]
    pub head_ref_oid: String,
    #[serde(default)]
    pub head_repository: Option<GitHubHeadRepository>,
    #[serde(default)]
    pub head_repository_owner: Option<GitHubActor>,
    pub base_ref_name: String,
    /// The base branch tip as the forge last computed the pull request against.
    #[serde(default)]
    pub base_ref_oid: Option<String>,
    #[serde(default)]
    pub author: Option<GitHubActor>,
    #[serde(default)]
    pub created_at: Option<String>,
    pub updated_at: String,
    pub url: Url,
    #[serde(default)]
    pub mergeable: Option<RemoteValue>,
    #[serde(default)]
    pub merge_state_status: Option<RemoteValue>,
    #[serde(default)]
    pub review_decision: Option<RemoteValue>,
    #[serde(default)]
    pub labels: Vec<GitHubLabel>,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub changed_files: Option<u64>,
    #[serde(default)]
    pub status_check_rollup: Option<Vec<PullRequestCheckRollup>>,
}

/// The two real GraphQL check variants returned in `gh pr list`'s rollup.
/// Keep conclusions distinct from execution status so a completed failure
/// cannot accidentally become a passing check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "__typename")]
pub enum PullRequestCheckRollup {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: RemoteValue,
        #[serde(default)]
        conclusion: Option<RemoteValue>,
        #[serde(default)]
        details_url: Option<String>,
        #[serde(default)]
        started_at: Option<String>,
        #[serde(default)]
        completed_at: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: RemoteValue,
        #[serde(default)]
        target_url: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
    },
}

impl PullRequest {
    /// Local projection only: the list response already contains every check.
    /// Detailed workflow/description data remains an inspector-only request.
    pub fn check_summaries(&self) -> Vec<PullRequestCheck> {
        self.status_check_rollup
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|check| {
                let (name, state, link, started_at, completed_at) = match check {
                    PullRequestCheckRollup::CheckRun {
                        name,
                        status,
                        conclusion,
                        details_url,
                        started_at,
                        completed_at,
                    } => (
                        name,
                        if status.0 == "COMPLETED" {
                            conclusion
                                .as_ref()
                                .filter(|value| !value.0.is_empty())
                                .unwrap_or(status)
                        } else {
                            status
                        },
                        details_url,
                        started_at.clone(),
                        completed_at.clone(),
                    ),
                    PullRequestCheckRollup::StatusContext {
                        context,
                        state,
                        target_url,
                        created_at,
                    } => (context, state, target_url, created_at.clone(), None),
                };
                let bucket = match state.0.as_str() {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => "pass",
                    "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE" | "STALE" => "fail",
                    "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" => {
                        "pending"
                    }
                    _ => "unknown",
                };
                PullRequestCheck {
                    bucket: RemoteValue(bucket.into()),
                    completed_at,
                    description: String::new(),
                    event: String::new(),
                    link: link
                        .as_deref()
                        .and_then(|link| Url::parse(link).ok())
                        .unwrap_or_else(|| self.url.clone()),
                    name: name.clone(),
                    started_at,
                    state: state.clone(),
                    workflow: String::new(),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCheck {
    pub bucket: RemoteValue,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub event: String,
    pub link: Url,
    pub name: String,
    #[serde(default)]
    pub started_at: Option<String>,
    pub state: RemoteValue,
    #[serde(default)]
    pub workflow: String,
}

/// Read-only inspector data. A failed section remains an error, never an empty
/// successful section; callers can still display the independently loaded data.
#[derive(Debug, Clone)]
pub struct PullRequestInspection {
    pub pull_request: PullRequest,
    pub checks: Result<Vec<PullRequestCheck>, String>,
    pub conversation: Result<PullRequestConversation, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestComment {
    /// Absent only for payloads that did not ask for it.
    #[serde(default)]
    pub id: Option<String>,
    pub author: Option<GitHubActor>,
    pub body: String,
    pub created_at: String,
    pub url: Url,
    #[serde(default)]
    pub viewer_did_author: bool,
    #[serde(default)]
    pub viewer_can_update: bool,
    #[serde(default)]
    pub viewer_can_delete: bool,
    /// `PENDING` while the comment belongs to an unsubmitted review; only the
    /// viewer's own pending comments are ever returned.
    #[serde(default)]
    pub comment_state: Option<RemoteValue>,
}

impl PullRequestComment {
    pub fn is_pending(&self) -> bool {
        self.comment_state
            .as_ref()
            .is_some_and(|state| state.0 == "PENDING")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestReview {
    #[serde(flatten)]
    pub comment: PullRequestComment,
    pub state: RemoteValue,
}

/// Which side of a diff a review comment anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDiffSide {
    Left,
    Right,
}

impl ReviewDiffSide {
    fn as_graphql(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestReviewThread {
    pub id: String,
    pub path: String,
    /// The last line of the anchored range in the current diff, or the
    /// original line when the thread is outdated.
    pub line: Option<u64>,
    /// The first line of a multi-line range; `None` for a single line.
    pub start_line: Option<u64>,
    pub side: ReviewDiffSide,
    pub start_side: Option<ReviewDiffSide>,
    pub outdated: bool,
    pub resolved: bool,
    pub collapsed: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub comments: Vec<PullRequestComment>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PullRequestConversation {
    pub threads: Vec<PullRequestReviewThread>,
    pub comments: Vec<PullRequestComment>,
    pub reviews: Vec<PullRequestReview>,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewThreadPage {
    id: String,
    path: String,
    line: Option<u64>,
    original_line: Option<u64>,
    #[serde(default)]
    start_line: Option<u64>,
    #[serde(default)]
    original_start_line: Option<u64>,
    #[serde(default = "right_side")]
    diff_side: ReviewDiffSide,
    #[serde(default)]
    start_diff_side: Option<ReviewDiffSide>,
    is_outdated: bool,
    is_resolved: bool,
    #[serde(default)]
    is_collapsed: bool,
    #[serde(default)]
    viewer_can_resolve: bool,
    #[serde(default)]
    viewer_can_unresolve: bool,
    comments: Connection<PullRequestComment>,
}

fn right_side() -> ReviewDiffSide {
    ReviewDiffSide::Right
}

impl ReviewThreadPage {
    fn into_thread(
        self,
        comments: Vec<PullRequestComment>,
        truncated: bool,
    ) -> PullRequestReviewThread {
        PullRequestReviewThread {
            id: self.id,
            path: self.path,
            line: self.line.or(self.original_line),
            start_line: self.start_line.or(self.original_start_line),
            side: self.diff_side,
            start_side: self.start_diff_side,
            outdated: self.is_outdated,
            resolved: self.is_resolved,
            collapsed: self.is_collapsed,
            viewer_can_resolve: self.viewer_can_resolve,
            viewer_can_unresolve: self.viewer_can_unresolve,
            comments,
            truncated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhAuthState {
    Authenticated,
    Unauthenticated,
    Invalid,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhAuthStatus {
    pub host: String,
    pub login: Option<String>,
    pub state: GhAuthState,
    pub scopes: Option<String>,
    pub git_protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestStateFilter {
    Open,
    Closed,
    Merged,
    All,
}

impl PullRequestStateFilter {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPullRequestsRequest {
    pub repository: GitHubRepoKey,
    pub state: PullRequestStateFilter,
    pub limit: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePullRequestRequest {
    pub repository: GitHubRepoKey,
    pub base: String,
    pub head: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub labels: Vec<String>,
    pub reviewers: Vec<String>,
}

impl CreatePullRequestRequest {
    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        self.repository.validate()?;
        for (field, value) in [
            ("base", self.base.as_str()),
            ("head", self.head.as_str()),
            ("title", self.title.as_str()),
        ] {
            if value.is_empty()
                || value
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
            {
                return Err(GitHubValidationError::InvalidField {
                    field,
                    reason: "must be non-empty and contain no control characters".into(),
                });
            }
        }
        if self.body.as_bytes().contains(&0) {
            return Err(GitHubValidationError::InvalidField {
                field: "body",
                reason: "must not contain NUL".into(),
            });
        }
        for value in self.labels.iter().chain(&self.reviewers) {
            if value.is_empty()
                || value
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
            {
                return Err(GitHubValidationError::InvalidField {
                    field: "label_or_reviewer",
                    reason: "must be non-empty and contain no control characters".into(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PullRequestCreationOutcome {
    Created {
        key: PullRequestKey,
        url: Url,
        pull_request: Option<Box<PullRequest>>,
        normalization_warning: Option<Box<ProcessFailure>>,
    },
    NotCreated {
        failure: ProcessFailure,
    },
    OutcomeUnknown {
        failure: ProcessFailure,
    },
}

/// The forge's per-viewer record of whether a changed file has been looked at.
/// `Dismissed` is GitHub's own state for a file that changed after it was
/// marked viewed; it renders unviewed but is distinct so a client can say why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileViewedState {
    Viewed,
    Unviewed,
    Dismissed,
    #[serde(other)]
    Unknown,
}

impl FileViewedState {
    pub fn is_viewed(self) -> bool {
        matches!(self, Self::Viewed)
    }
}

fn unviewed() -> FileViewedState {
    FileViewedState::Unviewed
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestFile {
    pub path: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub change_type: Option<RemoteValue>,
    #[serde(default = "unviewed")]
    pub viewer_viewed_state: FileViewedState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PullRequestFiles {
    pub files: Vec<PullRequestFile>,
    /// More pages existed than [`FILE_PAGE_LIMIT`] reads.
    pub truncated: bool,
}

/// The viewer's unsubmitted review, if one exists. GitHub keeps at most one
/// per viewer per pull request, and only its owner can see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReview {
    pub id: String,
    pub comment_count: u64,
}

/// What a review pane needs before its first mutation: the node id every
/// mutation addresses, the endpoints its diff must match, and whether a
/// pending review already exists to adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewContext {
    pub pull_request_id: String,
    pub head_oid: String,
    pub base_oid: String,
    pub viewer: String,
    pub viewer_is_author: bool,
    pub pending: Option<PendingReview>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    Approve,
    Comment,
    RequestChanges,
}

impl ReviewEvent {
    fn as_graphql(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::Comment => "Comment",
            Self::RequestChanges => "Request changes",
        }
    }
}

/// A new thread on one line, or on an inclusive `start_line..=line` range, of
/// the pull request's current diff, attached to a pending review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddReviewThreadRequest {
    pub review_id: String,
    pub path: String,
    pub line: u64,
    pub side: ReviewDiffSide,
    pub start_line: Option<u64>,
    pub start_side: Option<ReviewDiffSide>,
    pub body: String,
}

impl AddReviewThreadRequest {
    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        validate_node_id("review_id", &self.review_id)?;
        validate_review_path(&self.path)?;
        validate_comment_body(&self.body)?;
        if self.line == 0 {
            return Err(GitHubValidationError::InvalidField {
                field: "line",
                reason: "must be at least 1".into(),
            });
        }
        // A one-line range (`start_line == line`) is legal; only an inverted
        // range is not.
        if let Some(start) = self.start_line
            && (start == 0 || start > self.line)
        {
            return Err(GitHubValidationError::InvalidField {
                field: "start_line",
                reason: "must be at least 1 and not after the last line".into(),
            });
        }
        if self.start_side.is_some() && self.start_line.is_none() {
            return Err(GitHubValidationError::InvalidField {
                field: "start_side",
                reason: "needs a start line".into(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyToThreadRequest {
    pub thread_id: String,
    /// Attach the reply to a pending review instead of posting it at once.
    pub review_id: Option<String>,
    pub body: String,
}

impl ReplyToThreadRequest {
    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        validate_node_id("thread_id", &self.thread_id)?;
        if let Some(review) = &self.review_id {
            validate_node_id("review_id", review)?;
        }
        validate_comment_body(&self.body)
    }
}

/// Submits the pending review `review_id` with `event`, or, when there is no
/// pending review, creates and submits one in a single mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitReviewRequest {
    pub pull_request_id: String,
    pub review_id: Option<String>,
    pub event: ReviewEvent,
    pub body: String,
}

impl SubmitReviewRequest {
    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        validate_node_id("pull_request_id", &self.pull_request_id)?;
        if let Some(review) = &self.review_id {
            validate_node_id("review_id", review)?;
        }
        validate_text("body", &self.body)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedReview {
    pub id: String,
    pub state: RemoteValue,
    pub url: Url,
}

/// The three things a forge mutation can have done, in the same shape as
/// [`PullRequestCreationOutcome`]: a caller may retry `NotApplied` freely but
/// must reconcile `OutcomeUnknown` before retrying, or it risks a duplicate.
#[derive(Debug, Clone)]
pub enum ReviewMutationOutcome<T> {
    Applied(T),
    NotApplied { failure: ProcessFailure },
    OutcomeUnknown { failure: ProcessFailure },
}

impl<T> ReviewMutationOutcome<T> {
    pub fn map<U>(self, map: impl FnOnce(T) -> U) -> ReviewMutationOutcome<U> {
        match self {
            Self::Applied(value) => ReviewMutationOutcome::Applied(map(value)),
            Self::NotApplied { failure } => ReviewMutationOutcome::NotApplied { failure },
            Self::OutcomeUnknown { failure } => ReviewMutationOutcome::OutcomeUnknown { failure },
        }
    }
}

fn validate_node_id(field: &'static str, value: &str) -> Result<(), GitHubValidationError> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'=' | b'+' | b'/' | b':')
        });
    if valid {
        Ok(())
    } else {
        Err(GitHubValidationError::InvalidField {
            field,
            reason: "must be a GraphQL node id".into(),
        })
    }
}

fn validate_text(field: &'static str, value: &str) -> Result<(), GitHubValidationError> {
    if value.as_bytes().contains(&0) {
        Err(GitHubValidationError::InvalidField {
            field,
            reason: "must not contain NUL".into(),
        })
    } else {
        Ok(())
    }
}

/// GitHub's ceiling for a comment body, in bytes.
const MAX_COMMENT_BODY_BYTES: usize = 65_536;

fn validate_comment_body(body: &str) -> Result<(), GitHubValidationError> {
    validate_text("body", body)?;
    if body.trim().is_empty() {
        Err(GitHubValidationError::InvalidField {
            field: "body",
            reason: "must not be empty".into(),
        })
    } else if body.len() > MAX_COMMENT_BODY_BYTES {
        Err(GitHubValidationError::InvalidField {
            field: "body",
            reason: format!("must be at most {MAX_COMMENT_BODY_BYTES} bytes"),
        })
    } else {
        Ok(())
    }
}

/// A path as the forge reports it: repository-relative, slash-separated, and
/// never one Git would refuse to name.
fn validate_review_path(path: &str) -> Result<(), GitHubValidationError> {
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if invalid {
        Err(GitHubValidationError::InvalidField {
            field: "path",
            reason: "must be a repository-relative file path".into(),
        })
    } else {
        Ok(())
    }
}

/// Everything a review surface asks the forge, behind one object so a shell
/// can hand its state machine a fake and so a later HTTP transport is a
/// drop-in. Reads return the forge's answer or a typed error; mutations
/// return a [`ReviewMutationOutcome`] once their inputs validate.
pub trait GitHubReviewApi: Send + Sync {
    fn review_context(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<ReviewContext, GitHubError>;

    fn pull_request_files(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestFiles, GitHubError>;

    fn conversation(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestConversation, GitHubError>;

    /// Creates the viewer's pending review; fails at the forge if one exists.
    fn start_review(
        &self,
        repository: &GitHubRepoKey,
        pull_request_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PendingReview>, GitHubError>;

    fn add_review_thread(
        &self,
        repository: &GitHubRepoKey,
        request: &AddReviewThreadRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestReviewThread>, GitHubError>;

    fn reply_to_thread(
        &self,
        repository: &GitHubRepoKey,
        request: &ReplyToThreadRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestComment>, GitHubError>;

    fn update_comment(
        &self,
        repository: &GitHubRepoKey,
        comment_id: &str,
        body: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestComment>, GitHubError>;

    fn delete_comment(
        &self,
        repository: &GitHubRepoKey,
        comment_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError>;

    fn submit_review(
        &self,
        repository: &GitHubRepoKey,
        request: &SubmitReviewRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<SubmittedReview>, GitHubError>;

    /// Deletes a pending review and every comment in it.
    fn discard_review(
        &self,
        repository: &GitHubRepoKey,
        review_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError>;

    fn set_thread_resolved(
        &self,
        repository: &GitHubRepoKey,
        thread_id: &str,
        resolved: bool,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError>;

    fn set_file_viewed(
        &self,
        repository: &GitHubRepoKey,
        pull_request_id: &str,
        path: &str,
        viewed: bool,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError>;

    /// The pull requests a review surface offers to open.
    fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError>;
}

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error(transparent)]
    Validation(#[from] GitHubValidationError),
    #[error(transparent)]
    Process(#[from] ProcessFailure),
    #[error("GitHub authentication is required for {host}")]
    AuthenticationRequired {
        host: String,
        failure: Box<ProcessFailure>,
    },
    #[error("invalid gh JSON: {message}")]
    InvalidJson { message: String, excerpt: Vec<u8> },
    #[error("invalid gh response: {source}")]
    InvalidResponse {
        #[source]
        source: Box<GitHubError>,
        process: ProcessOutput,
    },
    #[error("gh returned no active account for {0}")]
    NoActiveAccount(String),
    #[error("pull request checks are pending")]
    ChecksPending {
        checks: Vec<PullRequestCheck>,
        process: ProcessOutput,
    },
    #[error("{0}")]
    Inspection(String),
}

/// Authority to mutate is the repository identity established by the last
/// completed successful refresh. It therefore survives an in-flight refresh,
/// which `Loading` carries forward, and is revoked only by a completed refresh
/// that reports the repository unauthenticated, unassociated or unreadable.
/// Cached presentation rows deliberately live outside this value and cannot
/// supply a repository to a mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubAvailability {
    MissingCli,
    Unauthenticated,
    NotAssociated,
    Loading {
        confirmed: Option<GitHubRepoKey>,
    },
    Ready {
        repository_key: GitHubRepoKey,
        pull_requests: Vec<(PullRequest, Vec<PullRequestCheck>)>,
        refreshed_at: i64,
    },
    Failed {
        detail: String,
    },
}

impl GitHubAvailability {
    pub fn repository_key(&self) -> Option<&GitHubRepoKey> {
        match self {
            Self::Ready { repository_key, .. } => Some(repository_key),
            Self::Loading { confirmed } => confirmed.as_ref(),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::MissingCli => "GitHub CLI is unavailable; local Git remains available.",
            Self::Unauthenticated => {
                "GitHub authentication is required; local Git remains available."
            }
            Self::NotAssociated => "No supported GitHub remote is associated.",
            Self::Loading { .. } => "Refreshing GitHub.",
            Self::Ready { pull_requests, .. } if pull_requests.is_empty() => {
                "GitHub is up to date; no open pull requests."
            }
            Self::Ready { .. } => "GitHub is up to date.",
            Self::Failed { .. } => "GitHub refresh failed; retry to authorize mutations.",
        }
    }

    pub fn from_error(error: GitHubError) -> Self {
        match error {
            GitHubError::Process(failure) if failure.kind == ProcessFailureKind::Spawn => {
                Self::MissingCli
            }
            GitHubError::Process(failure) if failure.exit_code == Some(4) => Self::Unauthenticated,
            GitHubError::AuthenticationRequired { .. } | GitHubError::NoActiveAccount(_) => {
                Self::Unauthenticated
            }
            GitHubError::Validation(_) => Self::NotAssociated,
            GitHubError::InvalidResponse { source, .. } => Self::from_error(*source),
            error => Self::Failed {
                detail: error.to_string(),
            },
        }
    }
}

/// A syntactically valid GitHub-shaped remote identity. Forge support for
/// non-public hosts must additionally be established by gh's configured hosts.
pub fn remote_repository_key(remote: &str) -> Option<GitHubRepoKey> {
    let parsed = if remote.contains("://") {
        Url::parse(remote).ok()?
    } else {
        let (authority, path) = remote.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        Url::parse(&format!("ssh://{authority}/{path}")).ok()?
    };
    if !matches!(parsed.scheme(), "https" | "ssh") {
        return None;
    }
    let mut path = parsed.path().trim_matches('/').split('/');
    let (owner, repository) = (path.next()?, path.next()?);
    if path.next().is_some() {
        return None;
    }
    GitHubRepoKey::new(parsed.host_str()?, parsed.port(), owner, repository).ok()
}

/// The remote whose fetch URL names `key`, preferring `origin` when several
/// do. Pull request refs are fetched from it, so the choice must be the
/// repository GitHub's own numbers belong to rather than a fork of it.
pub fn remote_for_repository(
    remotes: &[crate::repository::RemoteRecord],
    key: &GitHubRepoKey,
) -> Option<String> {
    let mut matching = remotes.iter().filter(|remote| {
        remote
            .fetch_url
            .as_deref()
            .and_then(remote_repository_key)
            .is_some_and(|candidate| candidate == *key)
    });
    let first = matching.next()?;
    Some(
        std::iter::once(first)
            .chain(matching)
            .find(|remote| remote.name == "origin")
            .unwrap_or(first)
            .name
            .clone(),
    )
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// Public GitHub or an enterprise host already known to gh. Arbitrary HTTPS
    /// remotes are not evidence that a repository belongs to GitHub.
    pub fn has_associated_remote(
        &self,
        remotes: &[crate::repository::RemoteRecord],
        cancellation: &CancellationToken,
    ) -> Result<bool, GitHubError> {
        let keys: Vec<_> = remotes
            .iter()
            .filter_map(|remote| remote.fetch_url.as_deref())
            .filter_map(remote_repository_key)
            .collect();
        if keys.iter().any(|key| key.host == "github.com") {
            return Ok(true);
        }
        if keys.is_empty() {
            return Ok(false);
        }
        let output = self.execute(
            &self.gh_command(["auth", "status", "--json", "hosts"]),
            cancellation,
        )?;
        // gh can return a failing auth exit status while still reporting known
        // accounts as JSON; those hosts remain associated but cannot authorize.
        let envelope: GhAuthEnvelope = parse_json(&output.stdout)?;
        Ok(keys
            .iter()
            .any(|key| envelope.hosts.contains_key(&key.authority())))
    }
}

#[derive(Debug, Deserialize)]
struct GhAuthEnvelope {
    hosts: BTreeMap<String, Vec<GhAuthAccount>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhAuthAccount {
    state: String,
    active: bool,
    host: String,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    scopes: Option<String>,
    #[serde(default)]
    git_protocol: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryEnvelope {
    name_with_owner: String,
    url: Url,
}

#[derive(Clone)]
pub struct GitHubService<E = SystemProcessExecutor> {
    gh: PathBuf,
    executor: Arc<E>,
}

impl GitHubService<SystemProcessExecutor> {
    pub fn system(gh: impl Into<PathBuf>) -> Self {
        Self::new(gh, Arc::new(SystemProcessExecutor))
    }
}

impl<E: ProcessExecutor> GitHubService<E> {
    pub fn new(gh: impl Into<PathBuf>, executor: Arc<E>) -> Self {
        Self {
            gh: gh.into(),
            executor,
        }
    }

    pub fn auth_status_command(&self, repository: &GitHubRepoKey) -> CommandSpec {
        let authority = repository.authority();
        self.gh_command([
            "auth",
            "status",
            "--active",
            "--hostname",
            authority.as_str(),
            "--json",
            "hosts",
        ])
    }
    pub fn resolve_repository_command(&self, repository: &Path) -> CommandSpec {
        self.gh_command(["repo", "view", "--json", "nameWithOwner,url"])
            .cwd(repository)
    }

    pub fn list_command(&self, request: &ListPullRequestsRequest) -> CommandSpec {
        self.gh_command([
            OsString::from("pr"),
            OsString::from("list"),
            OsString::from("-R"),
            request.repository.gh_selector().into(),
            OsString::from("--state"),
            request.state.as_str().into(),
            OsString::from("--limit"),
            request.limit.clamp(1, 200).to_string().into(),
            OsString::from("--json"),
            PR_LIST_FIELDS.into(),
        ])
    }

    pub fn view_command(&self, key: &PullRequestKey) -> CommandSpec {
        self.gh_command([
            OsString::from("pr"),
            OsString::from("view"),
            key.number.to_string().into(),
            OsString::from("-R"),
            key.repository.gh_selector().into(),
            OsString::from("--json"),
            PR_VIEW_FIELDS.into(),
        ])
    }

    pub fn checks_command(&self, key: &PullRequestKey) -> CommandSpec {
        self.gh_command([
            OsString::from("pr"),
            OsString::from("checks"),
            key.number.to_string().into(),
            OsString::from("-R"),
            key.repository.gh_selector().into(),
            OsString::from("--json"),
            PR_CHECK_FIELDS.into(),
        ])
    }

    pub fn create_command(
        &self,
        request: &CreatePullRequestRequest,
    ) -> Result<CommandSpec, GitHubValidationError> {
        request.validate()?;
        let mut command = self
            .gh_command([
                OsString::from("pr"),
                OsString::from("create"),
                OsString::from("-R"),
                request.repository.gh_selector().into(),
                OsString::from("--base"),
                request.base.clone().into(),
                OsString::from("--head"),
                request.head.clone().into(),
                OsString::from("--title"),
                request.title.clone().into(),
                OsString::from("--body-file"),
                OsString::from("-"),
            ])
            .stdin(request.body.as_bytes().to_vec());
        if request.draft {
            command.arguments.push("--draft".into());
        }
        for label in &request.labels {
            command.arguments.push("--label".into());
            command.arguments.push(label.into());
        }
        for reviewer in &request.reviewers {
            command.arguments.push("--reviewer".into());
            command.arguments.push(reviewer.into());
        }
        Ok(command)
    }

    pub fn resolve_repository(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<GitHubRepoKey, GitHubError> {
        let command = self.resolve_repository_command(repository);
        let output = self.execute(&command, cancellation)?;
        if !output.success {
            return Err(output.require_success(&self.gh).unwrap_err().into());
        }
        let envelope: RepositoryEnvelope = parse_json(&output.stdout)?;
        if envelope.url.scheme() != "https" || envelope.url.username() != "" {
            return Err(GitHubValidationError::InvalidHost(envelope.url.to_string()).into());
        }
        let (owner, name) = envelope.name_with_owner.split_once('/').ok_or_else(|| {
            GitHubValidationError::InvalidRepository {
                owner: envelope.name_with_owner.clone(),
                name: String::new(),
            }
        })?;
        GitHubRepoKey::new(
            envelope
                .url
                .host_str()
                .ok_or_else(|| GitHubValidationError::InvalidHost(envelope.url.to_string()))?,
            envelope.url.port(),
            owner,
            name,
        )
        .map_err(Into::into)
    }

    pub fn auth_status(
        &self,
        repository: &GitHubRepoKey,
        cancellation: &CancellationToken,
    ) -> Result<GhAuthStatus, GitHubError> {
        let command = self.auth_status_command(repository);
        let output = self.run_checked(&command, repository, cancellation)?;
        attach_process(
            parse_auth_status(&output.stdout, &repository.authority()),
            &output,
        )
    }

    pub fn list(
        &self,
        request: &ListPullRequestsRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let command = self.list_command(request);
        let output = self.run_checked(&command, &request.repository, cancellation)?;
        attach_process(
            parse_pull_request_list(&output.stdout, &request.repository),
            &output,
        )
    }

    pub fn view(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequest, GitHubError> {
        key.validate()?;
        let command = self.view_command(key);
        let output = self.run_checked(&command, &key.repository, cancellation)?;
        let pull_request =
            attach_process(parse_pull_request(&output.stdout, &key.repository), &output)?;
        if pull_request.number != key.number {
            return Err(GitHubError::InvalidResponse {
                source: Box::new(
                    GitHubValidationError::InvalidPullRequestUrl(pull_request.url.to_string())
                        .into(),
                ),
                process: output,
            });
        }
        Ok(pull_request)
    }

    pub fn invalidate_checks(repository: &GitHubRepoKey, number: u64) {
        lock_checks_cache().retain(|key, _| {
            key.pull_request.repository != *repository || key.pull_request.number != number
        });
    }

    /// Resolve branch identity before fetching auxiliary checks and discussion.
    /// Ambiguous fork branches must never borrow another pull request's facts.
    pub fn resolve_branch(
        &self,
        repository: &GitHubRepoKey,
        head_repository: &GitHubRepoKey,
        branch: &str,
        head_oid: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Option<PullRequest>, GitHubError> {
        let candidates = self.branch_candidates(repository, branch, cancellation)?;
        Ok(select_branch_pull_request(&candidates, head_repository, branch, head_oid)?.cloned())
    }

    /// Resolve an attached checkout's PR. Exact commits win; a unique PR from
    /// the proven source repository also matches after a local tip has moved.
    pub fn resolve_branch_head(
        &self,
        repository: &GitHubRepoKey,
        head_repository: &GitHubRepoKey,
        branch: &str,
        head_oid: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<PullRequest>, GitHubError> {
        let candidates = self.branch_candidates(repository, branch, cancellation)?;
        if candidates.len() >= 100 {
            return Err(GitHubError::Inspection(
                "This branch has too many pull requests to resolve safely".into(),
            ));
        }
        let matching = candidates
            .into_iter()
            .filter(|pr| pr.head_ref_name == branch && pr.belongs_to_head(head_repository))
            .collect::<Vec<_>>();
        let mut exact = matching.iter().filter(|pr| pr.head_ref_oid == head_oid);
        let first = exact.next();
        if exact.next().is_some() {
            return Err(GitHubError::Inspection(
                "Several pull requests match this checkout's branch and commit".into(),
            ));
        }
        if let Some(first) = first {
            return Ok(Some(first.clone()));
        }
        Ok((matching.len() == 1).then(|| matching[0].clone()))
    }

    fn branch_candidates(
        &self,
        repository: &GitHubRepoKey,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let mut command = self.list_command(&ListPullRequestsRequest {
            repository: repository.clone(),
            state: PullRequestStateFilter::All,
            limit: 100,
        });
        // pr list supports the detail fields: one request supplies the initial UI.
        // The repository selection already established authentication, and the
        // real request still reports revoked credentials through run_checked.
        *command.arguments.last_mut().expect("JSON fields argument") = PR_VIEW_FIELDS.into();
        command
            .arguments
            .extend([OsString::from("--head"), branch.into()]);
        let output = self.run_checked(&command, repository, cancellation)?;
        attach_process(parse_pull_request_list(&output.stdout, repository), &output)
    }

    pub fn inspect_pull_request(
        &self,
        repository: &GitHubRepoKey,
        pull_request: PullRequest,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestInspection, GitHubError> {
        let key = PullRequestKey::new(repository.clone(), pull_request.number)?;
        let (checks, conversation) = thread::scope(|scope| {
            let checks =
                scope.spawn(|| self.cached_checks(repository, &pull_request, cancellation));
            let conversation = self
                .conversation(&key, cancellation)
                .map_err(|error| error.to_string());
            (
                checks
                    .join()
                    .unwrap_or_else(|_| Err("Checks worker failed.".into())),
                conversation,
            )
        });
        // Do not attach checks fetched across a remote HEAD transition.
        let latest = self.view(&key, cancellation)?;
        if latest.head_ref_oid != pull_request.head_ref_oid
            || latest.head_ref_name != pull_request.head_ref_name
        {
            return Err(GitHubError::Inspection(
                "The pull request head changed while loading. Refresh to retry.".into(),
            ));
        }
        Ok(PullRequestInspection {
            pull_request: latest,
            checks,
            conversation,
        })
    }

    fn graphql_command(
        &self,
        repository: &GitHubRepoKey,
        query: &str,
        variables: Vec<(&str, GraphQlValue)>,
    ) -> CommandSpec {
        let mut command = self.gh_command([
            OsString::from("api"),
            "graphql".into(),
            "--hostname".into(),
            repository.authority().into(),
            "-f".into(),
            format!("query={query}").into(),
        ]);
        for (name, value) in variables {
            // `-f` always sends a string; `-F` converts a bare number or
            // boolean to the JSON scalar GraphQL requires for `Int!`/`Boolean!`.
            let (flag, value) = match value {
                GraphQlValue::String(value) => ("-f", value),
                GraphQlValue::Int(value) => ("-F", value.to_string()),
                GraphQlValue::Bool(value) => ("-F", value.to_string()),
            };
            command
                .arguments
                .extend([OsString::from(flag), format!("{name}={value}").into()]);
        }
        command
    }

    fn graphql(
        &self,
        repository: &GitHubRepoKey,
        query: &str,
        variables: Vec<(&str, GraphQlValue)>,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, GitHubError> {
        let command = self.graphql_command(repository, query, variables);
        let output = self.run_checked(&command, repository, cancellation)?;
        attach_process(parse_graphql(&output.stdout), &output)
    }

    fn pull_request_variables(key: &PullRequestKey) -> Vec<(&'static str, GraphQlValue)> {
        vec![
            ("owner", key.repository.owner.as_str().into()),
            ("name", key.repository.name.as_str().into()),
            ("number", GraphQlValue::Int(key.number as i64)),
        ]
    }

    pub fn conversation(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestConversation, GitHubError> {
        key.validate()?;
        // Separate connections prevent an issue-comment page from multiplying
        // every review-thread page. Explicit caps keep large discussions bounded.
        let mut result = PullRequestConversation::default();
        for section in ["reviewThreads", "comments", "reviews"] {
            let fields = match section {
                "reviewThreads" => REVIEW_THREAD_FIELDS,
                "reviews" => "id author{login} body createdAt url state viewerDidAuthor",
                _ => "id author{login} body createdAt url viewerDidAuthor",
            };
            let query = format!(
                "query($owner:String!,$name:String!,$number:Int!,$cursor:String){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{section}(first:50,after:$cursor){{nodes{{{fields}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
            );
            let mut cursor: Option<String> = None;
            for page in 0..4 {
                let mut variables = Self::pull_request_variables(key);
                if let Some(cursor) = &cursor {
                    variables.push(("cursor", cursor.as_str().into()));
                }
                let data = self.graphql(&key.repository, &query, variables, cancellation)?;
                let connection = data
                    .pointer(&format!("/repository/pullRequest/{section}"))
                    .ok_or_else(|| {
                        GitHubError::Inspection(
                            "GitHub returned no pull request conversation.".into(),
                        )
                    })?;
                let info = if section == "reviewThreads" {
                    let connection: Connection<ReviewThreadPage> = decode_value(connection)?;
                    for mut thread in connection.nodes {
                        let mut comments = std::mem::take(&mut thread.comments.nodes);
                        let mut truncated = thread.comments.page_info.has_next_page;
                        if truncated {
                            let cursor = next_cursor(&thread.comments.page_info, None)?;
                            let data = self.graphql(
                                &key.repository,
                                &format!(
                                    "query($id:ID!,$cursor:String!){{node(id:$id){{... on PullRequestReviewThread{{comments(first:50,after:$cursor){{nodes{{{REVIEW_COMMENT_FIELDS}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
                                ),
                                vec![
                                    ("id", thread.id.as_str().into()),
                                    ("cursor", cursor.into()),
                                ],
                                cancellation,
                            )?;
                            let more: Connection<PullRequestComment> =
                                decode_value(data.pointer("/node/comments").ok_or_else(|| {
                                    GitHubError::Inspection(
                                        "GitHub returned no review thread.".into(),
                                    )
                                })?)?;
                            truncated = more.page_info.has_next_page;
                            comments.extend(more.nodes);
                        }
                        result.threads.push(thread.into_thread(comments, truncated));
                    }
                    connection.page_info
                } else if section == "reviews" {
                    let connection: Connection<PullRequestReview> = decode_value(connection)?;
                    result.reviews.extend(connection.nodes);
                    connection.page_info
                } else {
                    let connection: Connection<PullRequestComment> = decode_value(connection)?;
                    result.comments.extend(connection.nodes);
                    connection.page_info
                };
                if !info.has_next_page {
                    break;
                }
                if page == 3 {
                    result.truncated = true;
                    break;
                }
                cursor = Some(next_cursor(&info, cursor.as_deref())?);
            }
        }
        Ok(result)
    }

    pub fn checks(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequestCheck>, GitHubError> {
        key.validate()?;
        let command = self.checks_command(key);
        let output = self.execute(&command, cancellation)?;
        match output.exit_code {
            Some(8) => {
                let checks = attach_process(parse_json(&output.stdout), &output)?;
                Err(GitHubError::ChecksPending {
                    checks,
                    process: output,
                })
            }
            _ if output.success => attach_process(parse_json(&output.stdout), &output),
            // gh uses exit 1 both for failed checks and operational errors.
            // Only a structured checks array is evidence of the former.
            Some(1) if !output.stdout.is_empty() => {
                attach_process(parse_json(&output.stdout), &output)
            }
            Some(1)
                if std::str::from_utf8(&output.stderr).is_ok_and(|stderr| {
                    let message = stderr.trim();
                    message.starts_with("no checks reported on the '")
                        && message.ends_with("' branch")
                        && !message.contains('\n')
                }) =>
            {
                Ok(Vec::new())
            }
            Some(4) => Err(GitHubError::AuthenticationRequired {
                host: key.repository.host.clone(),
                failure: Box::new(output.require_success(&self.gh).unwrap_err()),
            }),
            _ => Err(output.require_success(&self.gh).unwrap_err().into()),
        }
    }

    /// Inspector refreshes share a versioned cache and a per-key in-flight lock.
    /// Failed/cancelled results are never cached. Expiry covers rerun workflows
    /// that change checks without changing updatedAt or the head commit.
    fn cached_checks(
        &self,
        repository: &GitHubRepoKey,
        pr: &PullRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequestCheck>, String> {
        let key = ChecksCacheKey {
            pull_request: PullRequestKey {
                repository: repository.clone(),
                number: pr.number,
            },
            updated_at: pr.updated_at.clone(),
            head_oid: pr.head_ref_oid.clone(),
        };
        let flight = {
            let mut flights = CHECKS_FLIGHTS
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            flights.retain(|_, flight| Arc::strong_count(flight) > 1);
            flights
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _flight = flight.lock().unwrap_or_else(|error| error.into_inner());
        if cancellation.is_cancelled() {
            return Err("Checks request cancelled.".into());
        }
        {
            let now = Instant::now();
            let mut cache = lock_checks_cache();
            cache.retain(|cached_key, entry| {
                now.duration_since(entry.fetched_at) < entry.ttl
                    && (cached_key.pull_request != key.pull_request || cached_key == &key)
            });
            if let Some(entry) = cache.get(&key) {
                return Ok(entry.checks.clone());
            }
        }
        let (checks, ttl) = match self.checks(&key.pull_request, cancellation) {
            Ok(checks) => (checks, CHECKS_CACHE_TTL),
            Err(GitHubError::ChecksPending { checks, .. }) => (checks, Duration::from_secs(2)),
            Err(error) => return Err(error.to_string()),
        };
        if !cancellation.is_cancelled() {
            lock_checks_cache().insert(
                key,
                CachedChecks {
                    fetched_at: Instant::now(),
                    ttl,
                    checks: checks.clone(),
                },
            );
        }
        Ok(checks)
    }

    pub fn create(
        &self,
        request: &CreatePullRequestRequest,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestCreationOutcome, GitHubError> {
        let command = self.create_command(request)?;
        let output = match self.execute(&command, cancellation) {
            Ok(output) => output,
            Err(failure) if failure.started => {
                return Ok(PullRequestCreationOutcome::OutcomeUnknown { failure });
            }
            Err(failure) => {
                return Ok(PullRequestCreationOutcome::NotCreated { failure });
            }
        };
        if !output.success {
            let failure = output.require_success(&self.gh).unwrap_err();
            return Ok(if failure.exit_code == Some(4) {
                PullRequestCreationOutcome::NotCreated { failure }
            } else {
                PullRequestCreationOutcome::OutcomeUnknown { failure }
            });
        }

        let url = match parse_created_url(&output.stdout, &request.repository) {
            Ok(url) => url,
            Err(error) => {
                return Ok(PullRequestCreationOutcome::OutcomeUnknown {
                    failure: parse_warning(&self.gh, output, error.to_string()),
                });
            }
        };
        let number = pull_request_number(&url, &request.repository)?;
        let key = PullRequestKey::new(request.repository.clone(), number)?;
        // A successful URL is authoritative. Normalization is best-effort and uses
        // a fresh token so cancellation after the external mutation cannot erase a
        // known successful result.
        let normalization_token = CancellationToken::new();
        let view = self.view_command(&key);
        let (pull_request, normalization_warning) = match self.execute(&view, &normalization_token)
        {
            Ok(output) if output.success => {
                match parse_pull_request(&output.stdout, &request.repository) {
                    Ok(pull_request) if pull_request.number == key.number => {
                        (Some(pull_request), None)
                    }
                    Ok(pull_request) => (
                        None,
                        Some(parse_warning(
                            &self.gh,
                            output,
                            format!(
                                "gh normalized pull request {}, expected {}",
                                pull_request.number, key.number
                            ),
                        )),
                    ),
                    Err(error) => (
                        None,
                        Some(parse_warning(&self.gh, output, error.to_string())),
                    ),
                }
            }
            Ok(output) => (None, Some(output.require_success(&self.gh).unwrap_err())),
            Err(failure) => (None, Some(failure)),
        };
        Ok(PullRequestCreationOutcome::Created {
            key,
            url,
            pull_request: pull_request.map(Box::new),
            normalization_warning: normalization_warning.map(Box::new),
        })
    }

    /// Every `gh` process goes through here so the concurrency bound holds for
    /// all of them. A permit is held only for the subprocess, never across a
    /// second call, so the pool cannot deadlock on nested acquisition.
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let _permit = GH_PERMITS.acquire();
        self.executor.execute(command, cancellation)
    }

    fn run_checked(
        &self,
        command: &CommandSpec,
        repository: &GitHubRepoKey,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, GitHubError> {
        repository.validate()?;
        let output = self.execute(command, cancellation)?;
        if output.success {
            Ok(output)
        } else if output.exit_code == Some(4) {
            Err(GitHubError::AuthenticationRequired {
                host: repository.host.clone(),
                failure: Box::new(output.require_success(&self.gh).unwrap_err()),
            })
        } else {
            Err(output.require_success(&self.gh).unwrap_err().into())
        }
    }

    fn gh_command<I, S>(&self, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        CommandSpec::new(&self.gh)
            .args(arguments)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("NO_COLOR", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env_remove("GH_FORCE_TTY")
            .env_remove("GH_REPO")
            .env_remove("GH_HOST")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewContextPage {
    id: String,
    head_ref_oid: String,
    base_ref_oid: String,
    #[serde(default)]
    viewer_did_author: bool,
    reviews: Connection<PendingReviewPage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewPage {
    id: String,
    author: Option<GitHubActor>,
    comments: TotalCount,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TotalCount {
    total_count: u64,
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// Runs one mutation and classifies what the forge did with it.
    ///
    /// `payload` is the JSON pointer, under `data`, of the mutation's payload
    /// object. A payload that arrived is proof the mutation applied even if
    /// the response also carries errors for some field of it; a response
    /// with errors and no payload is proof it did not; anything else after
    /// the process started is unknown, because the request may have reached
    /// the forge before the failure.
    fn mutate(
        &self,
        repository: &GitHubRepoKey,
        mutation: &str,
        variables: Vec<(&str, GraphQlValue)>,
        payload: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<serde_json::Value>, GitHubError> {
        repository.validate()?;
        let command = self.graphql_command(repository, mutation, variables);
        let output = match self.execute(&command, cancellation) {
            Ok(output) => output,
            Err(failure) if failure.started => {
                return Ok(ReviewMutationOutcome::OutcomeUnknown { failure });
            }
            Err(failure) => return Ok(ReviewMutationOutcome::NotApplied { failure }),
        };
        if output.exit_code == Some(4) {
            return Err(GitHubError::AuthenticationRequired {
                host: repository.host.clone(),
                failure: Box::new(output.require_success(&self.gh).unwrap_err()),
            });
        }
        let body: Option<serde_json::Value> = serde_json::from_slice(&output.stdout).ok();
        let applied = body
            .as_ref()
            .and_then(|body| body.pointer(&format!("/data{payload}")))
            .filter(|value| !value.is_null())
            .cloned();
        if let Some(value) = applied {
            return Ok(ReviewMutationOutcome::Applied(value));
        }
        let errors: Vec<&serde_json::Value> = body
            .as_ref()
            .and_then(|body| body.get("errors"))
            .and_then(serde_json::Value::as_array)
            .map(|errors| errors.iter().collect())
            .unwrap_or_default();
        if errors.is_empty() {
            return Ok(if output.success {
                ReviewMutationOutcome::OutcomeUnknown {
                    failure: parse_warning(
                        &self.gh,
                        output,
                        "GitHub returned no mutation payload".into(),
                    ),
                }
            } else {
                ReviewMutationOutcome::OutcomeUnknown {
                    failure: output.require_success(&self.gh).unwrap_err(),
                }
            });
        }
        let message = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        // Only evidence proves the write did not happen: a response with no
        // `data` member never reached execution, and a typed refusal names a
        // check the forge runs before writing. An untyped error beside a null
        // payload is how GitHub reports a timeout or an internal failure, and
        // those can land after the write — retrying them would post twice.
        let refused = body.as_ref().is_some_and(|body| body.get("data").is_none())
            || errors.iter().all(|error| {
                error
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(mutation_refusal_type)
            });
        Ok(if refused {
            ReviewMutationOutcome::NotApplied {
                failure: parse_warning(&self.gh, output, format!("GitHub refused: {message}")),
            }
        } else {
            ReviewMutationOutcome::OutcomeUnknown {
                failure: parse_warning(
                    &self.gh,
                    output,
                    format!("GitHub did not confirm the change: {message}"),
                ),
            }
        })
    }

    /// Decodes an applied payload; a payload the forge sent but this build
    /// cannot read is an unknown outcome, never a failure to apply.
    fn decode_outcome<T: DeserializeOwned>(
        &self,
        outcome: ReviewMutationOutcome<serde_json::Value>,
    ) -> ReviewMutationOutcome<T> {
        match outcome {
            ReviewMutationOutcome::Applied(value) => match decode_value::<T>(&value) {
                Ok(decoded) => ReviewMutationOutcome::Applied(decoded),
                Err(error) => ReviewMutationOutcome::OutcomeUnknown {
                    failure: ProcessFailure {
                        kind: ProcessFailureKind::Exit,
                        program: self.gh.clone(),
                        exit_code: Some(0),
                        signal: None,
                        stdout: excerpt(value.to_string().as_bytes()),
                        stderr: Vec::new(),
                        message: format!("applied, but the payload was unreadable: {error}"),
                        started: true,
                    },
                },
            },
            ReviewMutationOutcome::NotApplied { failure } => {
                ReviewMutationOutcome::NotApplied { failure }
            }
            ReviewMutationOutcome::OutcomeUnknown { failure } => {
                ReviewMutationOutcome::OutcomeUnknown { failure }
            }
        }
    }
}

impl<E: ProcessExecutor + Send + Sync> GitHubReviewApi for GitHubService<E> {
    fn review_context(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<ReviewContext, GitHubError> {
        key.validate()?;
        let data = self.graphql(
            &key.repository,
            "query($owner:String!,$name:String!,$number:Int!){viewer{login} repository(owner:$owner,name:$name){pullRequest(number:$number){id headRefOid baseRefOid viewerDidAuthor reviews(first:10,states:[PENDING]){nodes{id author{login} comments{totalCount}} pageInfo{hasNextPage endCursor}}}}}",
            Self::pull_request_variables(key),
            cancellation,
        )?;
        let viewer: GitHubActor = decode_value(
            data.pointer("/viewer")
                .ok_or_else(|| GitHubError::Inspection("GitHub returned no viewer.".into()))?,
        )?;
        let page: ReviewContextPage =
            decode_value(data.pointer("/repository/pullRequest").ok_or_else(|| {
                GitHubError::Inspection("GitHub returned no pull request.".into())
            })?)?;
        // Only the viewer's own pending reviews are visible, but the author
        // check costs nothing and keeps a forge quirk from adopting someone
        // else's draft.
        let pending = page
            .reviews
            .nodes
            .into_iter()
            .find(|review| {
                review
                    .author
                    .as_ref()
                    .is_some_and(|author| author.login == viewer.login)
            })
            .map(|review| PendingReview {
                id: review.id,
                comment_count: review.comments.total_count,
            });
        Ok(ReviewContext {
            pull_request_id: page.id,
            head_oid: page.head_ref_oid,
            base_oid: page.base_ref_oid,
            viewer: viewer.login,
            viewer_is_author: page.viewer_did_author,
            pending,
        })
    }

    fn pull_request_files(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestFiles, GitHubError> {
        key.validate()?;
        let mut result = PullRequestFiles::default();
        let mut cursor: Option<String> = None;
        for page in 0..FILE_PAGE_LIMIT {
            let mut variables = Self::pull_request_variables(key);
            if let Some(cursor) = &cursor {
                variables.push(("cursor", cursor.as_str().into()));
            }
            let data = self.graphql(
                &key.repository,
                "query($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){files(first:100,after:$cursor){nodes{path additions deletions changeType viewerViewedState} pageInfo{hasNextPage endCursor}}}}}",
                variables,
                cancellation,
            )?;
            let connection: Connection<PullRequestFile> = decode_value(
                data.pointer("/repository/pullRequest/files")
                    .ok_or_else(|| {
                        GitHubError::Inspection("GitHub returned no changed files.".into())
                    })?,
            )?;
            result.files.extend(connection.nodes);
            if !connection.page_info.has_next_page {
                return Ok(result);
            }
            if page + 1 == FILE_PAGE_LIMIT {
                result.truncated = true;
                return Ok(result);
            }
            cursor = Some(next_cursor(&connection.page_info, cursor.as_deref())?);
        }
        Ok(result)
    }

    fn conversation(
        &self,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestConversation, GitHubError> {
        GitHubService::conversation(self, key, cancellation)
    }

    fn start_review(
        &self,
        repository: &GitHubRepoKey,
        pull_request_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PendingReview>, GitHubError> {
        validate_node_id("pull_request_id", pull_request_id)?;
        let outcome = self.mutate(
            repository,
            "mutation($pullRequest:ID!){addPullRequestReview(input:{pullRequestId:$pullRequest}){pullRequestReview{id comments{totalCount}}}}",
            vec![("pullRequest", pull_request_id.into())],
            "/addPullRequestReview/pullRequestReview",
            cancellation,
        )?;
        Ok(self
            .decode_outcome::<PendingReviewPage>(outcome)
            .map(|review| PendingReview {
                id: review.id,
                comment_count: review.comments.total_count,
            }))
    }

    fn add_review_thread(
        &self,
        repository: &GitHubRepoKey,
        request: &AddReviewThreadRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestReviewThread>, GitHubError> {
        request.validate()?;
        let mut variables: Vec<(&str, GraphQlValue)> = vec![
            ("review", request.review_id.as_str().into()),
            ("path", request.path.as_str().into()),
            ("line", request.line.into()),
            ("side", request.side.as_graphql().into()),
            ("body", request.body.as_str().into()),
        ];
        if let Some(start) = request.start_line {
            variables.push(("startLine", start.into()));
            variables.push((
                "startSide",
                request
                    .start_side
                    .unwrap_or(request.side)
                    .as_graphql()
                    .into(),
            ));
        }
        let outcome = self.mutate(
            repository,
            &format!(
                "mutation($review:ID!,$path:String!,$line:Int!,$side:DiffSide!,$startLine:Int,$startSide:DiffSide,$body:String!){{addPullRequestReviewThread(input:{{pullRequestReviewId:$review,path:$path,line:$line,side:$side,startLine:$startLine,startSide:$startSide,body:$body}}){{thread{{{REVIEW_THREAD_FIELDS}}}}}}}"
            ),
            variables,
            "/addPullRequestReviewThread/thread",
            cancellation,
        )?;
        Ok(self
            .decode_outcome::<ReviewThreadPage>(outcome)
            .map(|mut page| {
                let truncated = page.comments.page_info.has_next_page;
                let comments = std::mem::take(&mut page.comments.nodes);
                page.into_thread(comments, truncated)
            }))
    }

    fn reply_to_thread(
        &self,
        repository: &GitHubRepoKey,
        request: &ReplyToThreadRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestComment>, GitHubError> {
        request.validate()?;
        let mut variables: Vec<(&str, GraphQlValue)> = vec![
            ("thread", request.thread_id.as_str().into()),
            ("body", request.body.as_str().into()),
        ];
        if let Some(review) = &request.review_id {
            variables.push(("review", review.as_str().into()));
        }
        let outcome = self.mutate(
            repository,
            &format!(
                "mutation($thread:ID!,$body:String!,$review:ID){{addPullRequestReviewThreadReply(input:{{pullRequestReviewThreadId:$thread,body:$body,pullRequestReviewId:$review}}){{comment{{{REVIEW_COMMENT_FIELDS}}}}}}}"
            ),
            variables,
            "/addPullRequestReviewThreadReply/comment",
            cancellation,
        )?;
        Ok(self.decode_outcome(outcome))
    }

    fn update_comment(
        &self,
        repository: &GitHubRepoKey,
        comment_id: &str,
        body: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<PullRequestComment>, GitHubError> {
        validate_node_id("comment_id", comment_id)?;
        validate_comment_body(body)?;
        let outcome = self.mutate(
            repository,
            &format!(
                "mutation($comment:ID!,$body:String!){{updatePullRequestReviewComment(input:{{pullRequestReviewCommentId:$comment,body:$body}}){{pullRequestReviewComment{{{REVIEW_COMMENT_FIELDS}}}}}}}"
            ),
            vec![("comment", comment_id.into()), ("body", body.into())],
            "/updatePullRequestReviewComment/pullRequestReviewComment",
            cancellation,
        )?;
        Ok(self.decode_outcome(outcome))
    }

    fn delete_comment(
        &self,
        repository: &GitHubRepoKey,
        comment_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError> {
        validate_node_id("comment_id", comment_id)?;
        let outcome = self.mutate(
            repository,
            "mutation($comment:ID!){deletePullRequestReviewComment(input:{id:$comment}){clientMutationId}}",
            vec![("comment", comment_id.into())],
            "/deletePullRequestReviewComment",
            cancellation,
        )?;
        Ok(outcome.map(drop))
    }

    fn submit_review(
        &self,
        repository: &GitHubRepoKey,
        request: &SubmitReviewRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<SubmittedReview>, GitHubError> {
        request.validate()?;
        let mut variables: Vec<(&str, GraphQlValue)> =
            vec![("event", request.event.as_graphql().into())];
        if !request.body.trim().is_empty() {
            variables.push(("body", request.body.as_str().into()));
        }
        let outcome = match &request.review_id {
            Some(review) => {
                variables.push(("review", review.as_str().into()));
                self.mutate(
                    repository,
                    "mutation($review:ID!,$event:PullRequestReviewEvent!,$body:String){submitPullRequestReview(input:{pullRequestReviewId:$review,event:$event,body:$body}){pullRequestReview{id state url}}}",
                    variables,
                    "/submitPullRequestReview/pullRequestReview",
                    cancellation,
                )?
            }
            None => {
                variables.push(("pullRequest", request.pull_request_id.as_str().into()));
                self.mutate(
                    repository,
                    "mutation($pullRequest:ID!,$event:PullRequestReviewEvent!,$body:String){addPullRequestReview(input:{pullRequestId:$pullRequest,event:$event,body:$body}){pullRequestReview{id state url}}}",
                    variables,
                    "/addPullRequestReview/pullRequestReview",
                    cancellation,
                )?
            }
        };
        Ok(self.decode_outcome(outcome))
    }

    fn discard_review(
        &self,
        repository: &GitHubRepoKey,
        review_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError> {
        validate_node_id("review_id", review_id)?;
        let outcome = self.mutate(
            repository,
            "mutation($review:ID!){deletePullRequestReview(input:{pullRequestReviewId:$review}){pullRequestReview{id}}}",
            vec![("review", review_id.into())],
            "/deletePullRequestReview",
            cancellation,
        )?;
        Ok(outcome.map(drop))
    }

    fn set_thread_resolved(
        &self,
        repository: &GitHubRepoKey,
        thread_id: &str,
        resolved: bool,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError> {
        validate_node_id("thread_id", thread_id)?;
        let (mutation, payload) = if resolved {
            (
                "mutation($thread:ID!){resolveReviewThread(input:{threadId:$thread}){thread{id isResolved}}}",
                "/resolveReviewThread/thread",
            )
        } else {
            (
                "mutation($thread:ID!){unresolveReviewThread(input:{threadId:$thread}){thread{id isResolved}}}",
                "/unresolveReviewThread/thread",
            )
        };
        let outcome = self.mutate(
            repository,
            mutation,
            vec![("thread", thread_id.into())],
            payload,
            cancellation,
        )?;
        Ok(outcome.map(drop))
    }

    fn set_file_viewed(
        &self,
        repository: &GitHubRepoKey,
        pull_request_id: &str,
        path: &str,
        viewed: bool,
        cancellation: &CancellationToken,
    ) -> Result<ReviewMutationOutcome<()>, GitHubError> {
        validate_node_id("pull_request_id", pull_request_id)?;
        validate_review_path(path)?;
        let (mutation, payload) = if viewed {
            (
                "mutation($pullRequest:ID!,$path:String!){markFileAsViewed(input:{pullRequestId:$pullRequest,path:$path}){clientMutationId}}",
                "/markFileAsViewed",
            )
        } else {
            (
                "mutation($pullRequest:ID!,$path:String!){unmarkFileAsViewed(input:{pullRequestId:$pullRequest,path:$path}){clientMutationId}}",
                "/unmarkFileAsViewed",
            )
        };
        let outcome = self.mutate(
            repository,
            mutation,
            vec![
                ("pullRequest", pull_request_id.into()),
                ("path", path.into()),
            ],
            payload,
            cancellation,
        )?;
        Ok(outcome.map(drop))
    }

    fn list_pull_requests(
        &self,
        request: &ListPullRequestsRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        self.list(request, cancellation)
    }
}

pub fn parse_auth_status(output: &[u8], host: &str) -> Result<GhAuthStatus, GitHubError> {
    let envelope: GhAuthEnvelope = parse_json(output)?;
    let accounts = envelope
        .hosts
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(host))
        .map(|(_, accounts)| accounts)
        .ok_or_else(|| GitHubError::NoActiveAccount(host.into()))?;
    let account = accounts
        .iter()
        .find(|account| account.active)
        .ok_or_else(|| GitHubError::NoActiveAccount(host.into()))?;
    let state = match account.state.as_str() {
        "success" => GhAuthState::Authenticated,
        "none" | "unauthenticated" => GhAuthState::Unauthenticated,
        "error" | "failed" => GhAuthState::Invalid,
        value => GhAuthState::Unknown(value.into()),
    };
    Ok(GhAuthStatus {
        host: account.host.clone(),
        login: account.login.clone(),
        state,
        scopes: account.scopes.clone(),
        git_protocol: account.git_protocol.clone(),
    })
}

pub fn parse_pull_request_list(
    output: &[u8],
    repository: &GitHubRepoKey,
) -> Result<Vec<PullRequest>, GitHubError> {
    let pull_requests: Vec<PullRequest> = parse_json(output)?;
    for pull_request in &pull_requests {
        validate_pull_request(pull_request, repository)?;
    }
    Ok(pull_requests)
}

pub fn parse_pull_request(
    output: &[u8],
    repository: &GitHubRepoKey,
) -> Result<PullRequest, GitHubError> {
    let pull_request: PullRequest = parse_json(output)?;
    validate_pull_request(&pull_request, repository)?;
    Ok(pull_request)
}

fn validate_pull_request(
    pull_request: &PullRequest,
    repository: &GitHubRepoKey,
) -> Result<(), GitHubError> {
    if pull_request.number == 0
        || pull_request_number(&pull_request.url, repository)? != pull_request.number
    {
        return Err(
            GitHubValidationError::InvalidPullRequestUrl(pull_request.url.to_string()).into(),
        );
    }
    Ok(())
}

fn parse_created_url(output: &[u8], repository: &GitHubRepoKey) -> Result<Url, GitHubError> {
    let text = std::str::from_utf8(output).map_err(|error| GitHubError::InvalidJson {
        message: format!("gh create output was not UTF-8: {error}"),
        excerpt: excerpt(output),
    })?;
    let candidate = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| GitHubValidationError::InvalidPullRequestUrl("empty output".into()))?;
    let url = Url::parse(candidate.trim())
        .map_err(|_| GitHubValidationError::InvalidPullRequestUrl(candidate.into()))?;
    pull_request_number(&url, repository)?;
    Ok(url)
}

pub fn pull_request_number(
    url: &Url,
    repository: &GitHubRepoKey,
) -> Result<u64, GitHubValidationError> {
    let expected_port = repository.port.or_else(|| match url.scheme() {
        "https" => Some(443),
        _ => None,
    });
    let actual_port = url.port_or_known_default();
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(&repository.host))
        || actual_port != expected_port
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GitHubValidationError::InvalidPullRequestUrl(
            url.to_string(),
        ));
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or_else(|| GitHubValidationError::InvalidPullRequestUrl(url.to_string()))?
        .collect();
    if segments.len() != 4
        || segments[0] != repository.owner
        || segments[1] != repository.name
        || segments[2] != "pull"
    {
        return Err(GitHubValidationError::InvalidPullRequestUrl(
            url.to_string(),
        ));
    }
    segments[3]
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| GitHubValidationError::InvalidPullRequestUrl(url.to_string()))
}

fn select_branch_pull_request<'a>(
    candidates: &'a [PullRequest],
    head_repository: &GitHubRepoKey,
    branch: &str,
    head_oid: Option<&str>,
) -> Result<Option<&'a PullRequest>, GitHubError> {
    if candidates.len() >= 100 {
        return Err(GitHubError::Inspection(
            "This branch has too many pull requests to resolve safely. Open GitHub to inspect them.".into(),
        ));
    }
    let matching: Vec<_> = candidates
        .iter()
        .filter(|pr| pr.head_ref_name == branch && pr.belongs_to_head(head_repository))
        .collect();
    let open: Vec<_> = matching
        .iter()
        .copied()
        .filter(|pr| pr.state.0 == "OPEN")
        .collect();
    let relevant = if open.is_empty() { &matching } else { &open };
    if relevant.len() <= 1 {
        return Ok(relevant.first().copied());
    }
    let mut exact = relevant
        .iter()
        .copied()
        .filter(|pr| head_oid.is_some_and(|oid| !oid.is_empty() && pr.head_ref_oid == oid));
    if let Some(candidate) = exact.next()
        && exact.next().is_none()
    {
        return Ok(Some(candidate));
    }
    Err(GitHubError::Inspection(
        "Multiple pull requests match this branch. Fetch the matching head or inspect them on GitHub.".into(),
    ))
}

fn decode_value<T: DeserializeOwned>(value: &serde_json::Value) -> Result<T, GitHubError> {
    T::deserialize(value).map_err(|error| {
        GitHubError::Inspection(format!("Invalid GitHub conversation response: {error}"))
    })
}

fn parse_graphql(output: &[u8]) -> Result<serde_json::Value, GitHubError> {
    let mut envelope: serde_json::Value = parse_json(output)?;
    if let Some(errors) = envelope.get("errors").and_then(serde_json::Value::as_array)
        && !errors.is_empty()
    {
        let messages = errors
            .iter()
            .filter_map(|error| error.get("message").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(GitHubError::Inspection(format!(
            "GitHub GraphQL error: {messages}"
        )));
    }
    envelope
        .as_object_mut()
        .and_then(|envelope| envelope.remove("data"))
        .filter(|data| data.is_object())
        .ok_or_else(|| GitHubError::Inspection("GitHub returned no GraphQL data.".into()))
}

fn next_cursor(info: &PageInfo, previous: Option<&str>) -> Result<String, GitHubError> {
    info.end_cursor
        .as_ref()
        .filter(|cursor| !cursor.is_empty() && Some(cursor.as_str()) != previous)
        .cloned()
        .ok_or_else(|| GitHubError::Inspection("GitHub pagination did not advance.".into()))
}

fn parse_json<T: DeserializeOwned>(output: &[u8]) -> Result<T, GitHubError> {
    serde_json::from_slice(output).map_err(|error| GitHubError::InvalidJson {
        message: error.to_string(),
        excerpt: excerpt(output),
    })
}

fn attach_process<T>(
    result: Result<T, GitHubError>,
    process: &ProcessOutput,
) -> Result<T, GitHubError> {
    result.map_err(|source| GitHubError::InvalidResponse {
        source: Box::new(source),
        process: process.clone(),
    })
}

fn excerpt(output: &[u8]) -> Vec<u8> {
    output[..output.len().min(1024)].to_vec()
}

/// GraphQL error types GitHub raises before a mutation writes anything, so a
/// response carrying only these proves the change was not applied.
fn mutation_refusal_type(kind: &str) -> bool {
    matches!(
        kind,
        "NOT_FOUND" | "FORBIDDEN" | "UNPROCESSABLE" | "INSUFFICIENT_SCOPES" | "RATE_LIMITED"
    )
}

fn parse_warning(program: &Path, output: ProcessOutput, message: String) -> ProcessFailure {
    ProcessFailure {
        kind: ProcessFailureKind::Io,
        program: program.to_path_buf(),
        exit_code: output.exit_code,
        signal: output.signal,
        stdout: output.stdout,
        stderr: output.stderr,
        message,
        started: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, sync::Mutex};

    #[derive(Default)]
    struct FakeExecutor {
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<Vec<Result<ProcessOutput, ProcessFailure>>>,
    }

    impl FakeExecutor {
        fn with_outputs(outputs: Vec<Result<ProcessOutput, ProcessFailure>>) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().rev().collect()),
            }
        }
    }

    fn remote(name: &str, fetch_url: &str) -> crate::repository::RemoteRecord {
        crate::repository::RemoteRecord {
            name: name.into(),
            fetch_url: Some(fetch_url.into()),
            push_url: None,
        }
    }

    #[test]
    fn remote_for_repository_prefers_origin_among_matching_remotes() {
        let key = GitHubRepoKey::new("github.com", None, "acme", "widgets").unwrap();
        let remotes = [
            remote("upstream", "git@github.com:acme/widgets.git"),
            remote("fork", "https://github.com/someone/widgets.git"),
            remote("origin", "https://github.com/acme/widgets"),
        ];
        assert_eq!(
            remote_for_repository(&remotes, &key).as_deref(),
            Some("origin")
        );
        assert_eq!(
            remote_for_repository(&remotes[..2], &key).as_deref(),
            Some("upstream"),
            "without origin the first matching remote is used",
        );
        assert_eq!(
            remote_for_repository(&remotes[1..2], &key),
            None,
            "a remote of another repository never matches",
        );
        assert_eq!(
            remote_for_repository(
                &[crate::repository::RemoteRecord {
                    name: "origin".into(),
                    fetch_url: None,
                    push_url: None,
                }],
                &key,
            ),
            None,
            "a remote without a fetch URL cannot be fetched from",
        );
    }

    impl ProcessExecutor for FakeExecutor {
        fn execute(
            &self,
            command: &CommandSpec,
            _cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.commands.lock().unwrap().push(command.clone());
            self.outputs.lock().unwrap().pop().unwrap()
        }
    }

    fn success(stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            exit_code: Some(0),
            signal: None,
            success: true,
            stdout: stdout.into(),
            stderr: Vec::new(),
        })
    }

    fn repository() -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap()
    }

    #[test]
    fn remote_association_requires_public_github_or_a_known_enterprise_host() {
        let remote = |url: &str| crate::repository::RemoteRecord {
            name: "origin".into(),
            fetch_url: Some(url.into()),
            push_url: None,
        };
        let service = GitHubService::new("gh", Arc::new(FakeExecutor::default()));
        assert!(
            service
                .has_associated_remote(
                    &[remote("git@github.com:owner/repo.git")],
                    &CancellationToken::new()
                )
                .unwrap()
        );
        assert!(
            !service
                .has_associated_remote(&[remote("/local/repository")], &CancellationToken::new())
                .unwrap()
        );
        let service = GitHubService::new(
            "gh",
            Arc::new(FakeExecutor::with_outputs(vec![
                success(br#"{"hosts":{}}"#),
                success(br#"{"hosts":{"enterprise.example":[]}}"#),
            ])),
        );
        assert!(
            !service
                .has_associated_remote(
                    &[remote("https://gitlab.com/owner/repo")],
                    &CancellationToken::new()
                )
                .unwrap()
        );
        assert!(
            service
                .has_associated_remote(
                    &[remote("ssh://git@enterprise.example/owner/repo.git")],
                    &CancellationToken::new()
                )
                .unwrap()
        );
    }

    #[test]
    fn auth_parser_uses_active_state_without_exposing_token_source() {
        let output = br#"{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octo","tokenSource":"secret/path","scopes":"repo","gitProtocol":"https"}]}}"#;
        let status = parse_auth_status(output, "github.com").unwrap();
        assert_eq!(status.login.as_deref(), Some("octo"));
        assert_eq!(status.state, GhAuthState::Authenticated);
        assert_eq!(status.scopes.as_deref(), Some("repo"));
    }

    #[test]
    fn list_parser_accepts_unknown_remote_enum_values() {
        let output = br#"[{"number":7,"title":"Title","state":"FUTURE_STATE","isDraft":false,"headRefName":"feature","headRefOid":"abc","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","author":null,"updatedAt":"2026-01-01T00:00:00Z","url":"https://github.com/owner/repo/pull/7","mergeable":"FUTURE","mergeStateStatus":null,"reviewDecision":null,"labels":[]}]"#;
        let pull_requests = parse_pull_request_list(output, &repository()).unwrap();
        assert_eq!(pull_requests[0].state.0, "FUTURE_STATE");
        assert_eq!(pull_requests[0].mergeable.as_ref().unwrap().0, "FUTURE");
    }

    #[test]
    fn rollup_preserves_running_completed_failures_and_legacy_statuses() {
        let output = br#"[{"number":7,"title":"Title","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7","statusCheckRollup":[
            {"__typename":"CheckRun","name":"running","status":"IN_PROGRESS","conclusion":"SUCCESS","detailsUrl":""},
            {"__typename":"CheckRun","name":"failed","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":null},
            {"__typename":"StatusContext","context":"legacy","state":"SUCCESS","targetUrl":"https://ci.example/check/1"}
        ]}]"#;
        let prs = parse_pull_request_list(output, &repository()).unwrap();
        let checks = prs[0].check_summaries();
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.name.as_str(), check.bucket.0.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("running", "pending"),
                ("failed", "fail"),
                ("legacy", "pass")
            ]
        );
        assert_eq!(checks[0].link, prs[0].url);
        assert_eq!(checks[2].link.as_str(), "https://ci.example/check/1");
    }

    #[test]
    fn parsers_reject_truncated_json_and_wrong_repository_urls() {
        assert!(parse_pull_request_list(b"[{", &repository()).is_err());
        let wrong = br#"[{"number":7,"title":"Title","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/other/repo/pull/7"}]"#;
        assert!(parse_pull_request_list(wrong, &repository()).is_err());
    }

    #[test]
    fn create_command_uses_stdin_and_distinct_arguments() {
        let service = GitHubService::new("/usr/bin/gh", Arc::new(FakeExecutor::default()));
        let request = CreatePullRequestRequest {
            repository: repository(),
            base: "main".into(),
            head: "feature; echo pwned".into(),
            title: "Title $(touch /tmp/nope)".into(),
            body: "Body with 'quotes' and newlines\n".into(),
            draft: true,
            labels: vec!["priority 1".into()],
            reviewers: vec!["octocat".into()],
        };
        let command = service.create_command(&request).unwrap();
        assert_eq!(command.program, PathBuf::from("/usr/bin/gh"));
        assert_eq!(command.stdin.as_deref(), Some(request.body.as_bytes()));
        assert!(
            command
                .arguments
                .contains(&OsString::from("feature; echo pwned"))
        );
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
        assert_eq!(
            command.environment.get(OsStr::new("GH_PROMPT_DISABLED")),
            Some(&OsString::from("1"))
        );
    }

    #[test]
    fn rejects_credentials_queries_and_untrusted_pr_urls() {
        let service = GitHubService::new("gh", Arc::new(FakeExecutor::default()));
        let command = service.auth_status_command(&repository());
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("--show-token"))
        );
        for value in [
            "http://github.com/owner/repo/pull/1",
            "https://user@github.com/owner/repo/pull/1",
            "https://github.com/other/repo/pull/1",
            "https://github.com/owner/repo/pull/0",
            "https://github.com/owner/repo/pull/1?x=1",
        ] {
            let url = Url::parse(value).unwrap();
            assert!(pull_request_number(&url, &repository()).is_err(), "{value}");
        }
    }

    #[test]
    fn create_normalizes_the_returned_url_with_pr_view_json() {
        let created = b"https://github.com/owner/repo/pull/7\n";
        let detail = br#"{"number":7,"title":"Title","body":"Body","state":"OPEN","isDraft":false,"headRefName":"feature","headRefOid":"abc","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","author":null,"createdAt":"now","updatedAt":"now","url":"https://github.com/owner/repo/pull/7","labels":[]}"#;
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(created),
            success(detail),
        ]));
        let service = GitHubService::new("gh", fake.clone());
        let request = CreatePullRequestRequest {
            repository: repository(),
            base: "main".into(),
            head: "feature".into(),
            title: "Title".into(),
            body: "Body".into(),
            draft: false,
            labels: Vec::new(),
            reviewers: Vec::new(),
        };
        let outcome = service.create(&request, &CancellationToken::new()).unwrap();
        assert!(matches!(
            outcome,
            PullRequestCreationOutcome::Created { ref pull_request, .. } if pull_request.is_some()
        ));
        let commands = fake.commands.lock().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].arguments[0], OsString::from("pr"));
        assert_eq!(commands[1].arguments[1], OsString::from("view"));
        assert!(commands[1].arguments.contains(&OsString::from("--json")));
    }

    #[test]
    fn started_cancellation_is_an_unknown_create_outcome() {
        let failure = ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "gh".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: b"cancelled".into(),
            message: "cancelled".into(),
            started: true,
        };
        let fake = Arc::new(FakeExecutor::with_outputs(vec![Err(failure.clone())]));
        let service = GitHubService::new("gh", fake);
        let request = CreatePullRequestRequest {
            repository: repository(),
            base: "main".into(),
            head: "feature".into(),
            title: "Title".into(),
            body: String::new(),
            draft: false,
            labels: Vec::new(),
            reviewers: Vec::new(),
        };
        assert_eq!(
            service.create(&request, &CancellationToken::new()).unwrap(),
            PullRequestCreationOutcome::OutcomeUnknown { failure }
        );
    }

    #[test]
    fn nonzero_create_after_launch_is_conservatively_unknown() {
        let output = Ok(ProcessOutput {
            exit_code: Some(1),
            signal: None,
            success: false,
            stdout: Vec::new(),
            stderr: b"connection closed after request".into(),
        });
        let fake = Arc::new(FakeExecutor::with_outputs(vec![output]));
        let service = GitHubService::new("gh", fake);
        let request = CreatePullRequestRequest {
            repository: repository(),
            base: "main".into(),
            head: "feature".into(),
            title: "Title".into(),
            body: String::new(),
            draft: false,
            labels: Vec::new(),
            reviewers: Vec::new(),
        };
        assert!(matches!(
            service.create(&request, &CancellationToken::new()).unwrap(),
            PullRequestCreationOutcome::OutcomeUnknown { .. }
        ));
    }

    #[test]
    fn a_pr_without_checks_is_empty_but_other_stderr_is_an_error() {
        let output = |stderr: &[u8]| {
            Ok(ProcessOutput {
                exit_code: Some(1),
                signal: None,
                success: false,
                stdout: Vec::new(),
                stderr: stderr.to_vec(),
            })
        };
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            output(b"no checks reported on the 'feature' branch\n"),
            output(b"error connecting to api.github.com\n"),
        ]));
        let service = GitHubService::new("gh", fake);
        let key = PullRequestKey::new(repository(), 7).unwrap();
        assert!(
            service
                .checks(&key, &CancellationToken::new())
                .unwrap()
                .is_empty()
        );
        assert!(service.checks(&key, &CancellationToken::new()).is_err());
    }

    #[test]
    fn failed_checks_are_data_but_command_errors_are_not_empty_checks() {
        let output = |stdout: &[u8]| {
            Ok(ProcessOutput {
                exit_code: Some(1),
                signal: None,
                success: false,
                stdout: stdout.to_vec(),
                stderr: b"failed".to_vec(),
            })
        };
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            output(br#"[{"bucket":"fail","state":"FAILURE","name":"CI","link":"https://github.com/owner/repo/actions/runs/1"}]"#),
            output(br#"{"message":"API unavailable"}"#),
        ]));
        let service = GitHubService::new("gh", fake);
        let key = PullRequestKey::new(repository(), 7).unwrap();
        let checks = service.checks(&key, &CancellationToken::new()).unwrap();
        assert_eq!(checks[0].bucket.0, "fail");
        assert_eq!(checks[0].state.0, "FAILURE");
        assert!(service.checks(&key, &CancellationToken::new()).is_err());
    }

    #[test]
    fn graphql_partial_errors_and_broken_pagination_do_not_look_complete() {
        assert!(
            parse_graphql(br#"{"data":{"repository":null},"errors":[{"message":"Forbidden"}]}"#)
                .is_err()
        );
        assert!(parse_graphql(br#"{"data":null}"#).is_err());
        let info = PageInfo {
            has_next_page: true,
            end_cursor: None,
        };
        assert!(next_cursor(&info, None).is_err());
        let info = PageInfo {
            has_next_page: true,
            end_cursor: Some("same".into()),
        };
        assert!(next_cursor(&info, Some("same")).is_err());
    }

    #[test]
    fn local_head_identity_uses_git_configuration_and_push_url() {
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/CarsonBurke/cleanrl.git",
        ]);
        git(&[
            "remote",
            "add",
            "upstream",
            "https://github.com/vwxyzjn/cleanrl.git",
        ]);
        git(&["config", "branch.Feature/Case.remote", "upstream"]);
        git(&["config", "branch.Feature/Case.pushRemote", "origin"]);
        git(&[
            "config",
            "remote.origin.pushurl",
            "git@github.com:ActualPublisher/cleanrl.git",
        ]);
        let key = worktree_head_repository(temp.path(), "Feature/Case", &CancellationToken::new())
            .unwrap();
        assert_eq!(key.owner, "ActualPublisher");
        assert_eq!(key.name, "cleanrl");
        git(&["config", "branch.Feature/Case.pushRemote", "unknown"]);
        assert!(
            worktree_head_repository(temp.path(), "Feature/Case", &CancellationToken::new())
                .is_none()
        );
    }

    #[test]
    fn branch_lookup_requires_source_repository_even_for_identical_commits() {
        let own = repository();
        let pr: PullRequest = serde_json::from_value(serde_json::json!({
            "number":466,"title":"Unrelated master","state":"OPEN",
            "headRefName":"master","headRefOid":"abc","baseRefName":"master",
            "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"another-fork"},
            "updatedAt":"now","url":"https://github.com/owner/repo/pull/466"
        }))
        .unwrap();
        assert!(
            select_branch_pull_request(&[pr.clone()], &own, "master", Some("abc"))
                .unwrap()
                .is_none()
        );
        let mut ours = pr.clone();
        ours.head_repository_owner.as_mut().unwrap().login = own.owner.to_ascii_uppercase();
        ours.number = 7;
        assert_eq!(
            select_branch_pull_request(&[pr, ours.clone()], &own, "master", Some("abc"))
                .unwrap()
                .unwrap()
                .number,
            7
        );
        ours.head_repository = None;
        assert!(
            select_branch_pull_request(&[ours], &own, "master", Some("abc"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn branch_lookup_never_chooses_an_ambiguous_fork_by_list_order() {
        let first = parse_pull_request(
            br#"{"number":7,"title":"Title","state":"OPEN","headRefName":"feature","headRefOid":"abc","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"}"#,
            &repository(),
        ).unwrap();
        let mut second = first.clone();
        second.number = 8;
        second.head_ref_oid = "def".into();
        let candidates = [first, second];
        assert!(select_branch_pull_request(&candidates, &repository(), "feature", None).is_err());
        assert_eq!(
            select_branch_pull_request(&candidates, &repository(), "feature", Some("def"))
                .unwrap()
                .unwrap()
                .number,
            8
        );
        assert!(
            select_branch_pull_request(&candidates, &repository(), "unrelated", Some("def"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_head_lookup_finds_older_merged_pr_on_reused_branch() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![success(
            br#"[{"number":9,"title":"New","state":"OPEN","headRefName":"feature","headRefOid":"new","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"2026-09-01","url":"https://github.com/owner/repo/pull/9"},{"number":7,"title":"Old","state":"MERGED","headRefName":"feature","headRefOid":"old","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"2026-01-01","url":"https://github.com/owner/repo/pull/7"}]"#,
        )]));
        let service = GitHubService::new("gh", fake);
        let pr = service
            .resolve_branch_head(
                &repository(),
                &repository(),
                "feature",
                "old",
                &CancellationToken::new(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.state.0, "MERGED");
    }

    #[test]
    fn unique_same_repository_branch_survives_a_moved_local_tip() {
        let same_repo = br#"[{"number":7,"title":"Old","state":"MERGED","headRefName":"feature","headRefOid":"old","headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"owner"},"baseRefName":"main","updatedAt":"2026-01-01","url":"https://github.com/owner/repo/pull/7"}]"#;
        let fork = br#"[{"number":8,"title":"Fork","state":"MERGED","headRefName":"feature","headRefOid":"old","headRepository":{"name":"fork"},"headRepositoryOwner":{"login":"someone"},"baseRefName":"main","updatedAt":"2026-01-01","url":"https://github.com/owner/repo/pull/8"}]"#;
        let service = GitHubService::new(
            "gh",
            Arc::new(FakeExecutor::with_outputs(vec![
                success(same_repo),
                success(fork),
            ])),
        );
        assert_eq!(
            service
                .resolve_branch_head(
                    &repository(),
                    &repository(),
                    "feature",
                    "local-tip",
                    &CancellationToken::new()
                )
                .unwrap()
                .unwrap()
                .number,
            7
        );
        assert!(
            service
                .resolve_branch_head(
                    &repository(),
                    &repository(),
                    "feature",
                    "local-tip",
                    &CancellationToken::new()
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn conversation_paginates_replies_and_issue_comments_independently() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(br#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"id":"thread","path":"src/lib.rs","line":null,"originalLine":12,"isOutdated":true,"isResolved":false,"comments":{"nodes":[{"author":null,"body":"Root","createdAt":"now","url":"https://github.com/owner/repo/pull/7#discussion_r1"}],"pageInfo":{"hasNextPage":true,"endCursor":"reply-cursor"}}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
            success(br#"{"data":{"node":{"comments":{"nodes":[{"author":{"login":"reviewer"},"body":"Reply","createdAt":"now","url":"https://github.com/owner/repo/pull/7#discussion_r2"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"comments":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"issue-cursor"}}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"comments":{"nodes":[{"author":{"login":"author"},"body":"Issue comment","createdAt":"now","url":"https://github.com/owner/repo/pull/7#issuecomment-1"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"reviews":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
        ]));
        let service = GitHubService::new("gh", fake);
        let conversation = service
            .conversation(
                &PullRequestKey::new(repository(), 7).unwrap(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(conversation.threads[0].line, Some(12));
        assert!(conversation.threads[0].outdated);
        assert_eq!(conversation.threads[0].comments[1].body, "Reply");
        assert_eq!(conversation.comments[0].body, "Issue comment");
        assert!(!conversation.truncated);
        assert!(!conversation.threads[0].truncated);
    }
    fn pull_request_key() -> PullRequestKey {
        PullRequestKey::new(repository(), 7).unwrap()
    }

    fn argument_strings(command: &CommandSpec) -> Vec<String> {
        command
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn started_failure(kind: ProcessFailureKind, started: bool) -> ProcessFailure {
        ProcessFailure {
            kind,
            program: "gh".into(),
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: b"boom".into(),
            message: "boom".into(),
            started,
        }
    }

    fn failed(exit_code: i32, stdout: &[u8]) -> Result<ProcessOutput, ProcessFailure> {
        Ok(ProcessOutput {
            exit_code: Some(exit_code),
            signal: None,
            success: false,
            stdout: stdout.into(),
            stderr: Vec::new(),
        })
    }

    const THREAD_JSON: &str = r#"{"id":"PRRT_1","path":"src/lib.rs","line":12,"originalLine":12,"startLine":10,"originalStartLine":10,"diffSide":"RIGHT","startDiffSide":"RIGHT","isOutdated":false,"isResolved":false,"isCollapsed":false,"viewerCanResolve":true,"viewerCanUnresolve":false,"comments":{"nodes":[{"id":"PRRC_1","author":{"login":"me"},"body":"Draft","createdAt":"now","url":"https://github.com/owner/repo/pull/7#discussion_r1","viewerDidAuthor":true,"viewerCanUpdate":true,"viewerCanDelete":true,"commentState":"PENDING"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}"#;

    #[test]
    fn graphql_sends_integers_typed_and_never_interpolates_the_number() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(br#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"comments":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"reviews":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
        ]));
        let service = GitHubService::new("gh", fake.clone());
        service
            .conversation(&pull_request_key(), &CancellationToken::new())
            .unwrap();
        let commands = fake.commands.lock().unwrap();
        for command in commands.iter() {
            let arguments = argument_strings(command);
            let query = arguments
                .iter()
                .find_map(|argument| argument.strip_prefix("query="))
                .expect("query argument");
            assert!(query.contains("$number:Int!"), "{query}");
            assert!(!query.contains("number:7"), "{query}");
            let position = arguments
                .iter()
                .position(|argument| argument == "number=7")
                .expect("number variable");
            assert_eq!(arguments[position - 1], "-F");
            let owner = arguments
                .iter()
                .position(|argument| argument == "owner=owner")
                .expect("owner variable");
            assert_eq!(arguments[owner - 1], "-f");
        }
    }

    #[test]
    fn review_context_adopts_only_the_viewers_pending_review() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(br#"{"data":{"viewer":{"login":"me"},"repository":{"pullRequest":{"id":"PR_1","headRefOid":"abc","baseRefOid":"def","viewerDidAuthor":false,"reviews":{"nodes":[{"id":"PRR_other","author":{"login":"someone"},"comments":{"totalCount":9}},{"id":"PRR_mine","author":{"login":"me"},"comments":{"totalCount":2}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
            success(br#"{"data":{"viewer":{"login":"me"},"repository":{"pullRequest":{"id":"PR_1","headRefOid":"abc","baseRefOid":"def","viewerDidAuthor":true,"reviews":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
        ]));
        let service = GitHubService::new("gh", fake);
        let context = service
            .review_context(&pull_request_key(), &CancellationToken::new())
            .unwrap();
        assert_eq!(context.pull_request_id, "PR_1");
        assert_eq!(context.head_oid, "abc");
        assert_eq!(context.base_oid, "def");
        assert_eq!(
            context.pending,
            Some(PendingReview {
                id: "PRR_mine".into(),
                comment_count: 2
            })
        );
        let context = service
            .review_context(&pull_request_key(), &CancellationToken::new())
            .unwrap();
        assert!(context.viewer_is_author);
        assert_eq!(context.pending, None);
    }

    #[test]
    fn files_paginate_and_keep_the_forges_viewed_states() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(br#"{"data":{"repository":{"pullRequest":{"files":{"nodes":[{"path":"a.rs","additions":1,"deletions":2,"changeType":"MODIFIED","viewerViewedState":"VIEWED"}],"pageInfo":{"hasNextPage":true,"endCursor":"c1"}}}}}}"#),
            success(br#"{"data":{"repository":{"pullRequest":{"files":{"nodes":[{"path":"b.rs","additions":3,"deletions":0,"changeType":"ADDED","viewerViewedState":"DISMISSED"},{"path":"c.rs","viewerViewedState":"SOMETHING_NEW"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#),
        ]));
        let service = GitHubService::new("gh", fake.clone());
        let files = service
            .pull_request_files(&pull_request_key(), &CancellationToken::new())
            .unwrap();
        assert!(!files.truncated);
        assert_eq!(files.files.len(), 3);
        assert_eq!(files.files[0].viewer_viewed_state, FileViewedState::Viewed);
        assert!(files.files[0].viewer_viewed_state.is_viewed());
        assert_eq!(
            files.files[1].viewer_viewed_state,
            FileViewedState::Dismissed
        );
        assert_eq!(files.files[2].viewer_viewed_state, FileViewedState::Unknown);
        assert_eq!(files.files[1].additions, 3);
        let commands = fake.commands.lock().unwrap();
        assert!(
            !argument_strings(&commands[0])
                .iter()
                .any(|a| a.starts_with("cursor="))
        );
        assert!(argument_strings(&commands[1]).contains(&"cursor=c1".to_owned()));
    }

    #[test]
    fn a_review_thread_carries_its_range_and_pending_state() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![success(
            format!(r#"{{"data":{{"addPullRequestReviewThread":{{"thread":{THREAD_JSON}}}}}}}"#)
                .as_bytes(),
        )]));
        let service = GitHubService::new("gh", fake.clone());
        let request = AddReviewThreadRequest {
            review_id: "PRR_1".into(),
            path: "src/lib.rs".into(),
            line: 12,
            side: ReviewDiffSide::Right,
            start_line: Some(10),
            start_side: None,
            body: "Draft".into(),
        };
        let outcome = service
            .add_review_thread(&repository(), &request, &CancellationToken::new())
            .unwrap();
        let ReviewMutationOutcome::Applied(thread) = outcome else {
            panic!("expected an applied thread");
        };
        assert_eq!(thread.id, "PRRT_1");
        assert_eq!(thread.start_line, Some(10));
        assert_eq!(thread.line, Some(12));
        assert_eq!(thread.side, ReviewDiffSide::Right);
        assert!(thread.viewer_can_resolve);
        assert!(thread.comments[0].is_pending());
        assert!(thread.comments[0].viewer_did_author);
        let arguments = argument_strings(&fake.commands.lock().unwrap()[0]);
        let line = arguments.iter().position(|a| a == "line=12").unwrap();
        assert_eq!(arguments[line - 1], "-F");
        assert!(arguments.contains(&"startLine=10".to_owned()));
        assert!(arguments.contains(&"startSide=RIGHT".to_owned()));
        assert!(arguments.contains(&"side=RIGHT".to_owned()));

        let single = AddReviewThreadRequest {
            start_line: None,
            ..request.clone()
        };
        let fake = Arc::new(FakeExecutor::with_outputs(vec![success(
            format!(r#"{{"data":{{"addPullRequestReviewThread":{{"thread":{THREAD_JSON}}}}}}}"#)
                .as_bytes(),
        )]));
        let service = GitHubService::new("gh", fake.clone());
        service
            .add_review_thread(&repository(), &single, &CancellationToken::new())
            .unwrap();
        let arguments = argument_strings(&fake.commands.lock().unwrap()[0]);
        assert!(!arguments.iter().any(|a| a.starts_with("startLine=")));
        assert!(!arguments.iter().any(|a| a.starts_with("startSide=")));
    }

    #[test]
    fn review_requests_refuse_inputs_the_forge_would_misread() {
        let service = GitHubService::new("gh", Arc::new(FakeExecutor::default()));
        let valid = AddReviewThreadRequest {
            review_id: "PRR_1".into(),
            path: "src/lib.rs".into(),
            line: 12,
            side: ReviewDiffSide::Right,
            start_line: None,
            start_side: None,
            body: "Draft".into(),
        };
        let refused = |request: AddReviewThreadRequest| {
            assert!(
                service
                    .add_review_thread(&repository(), &request, &CancellationToken::new())
                    .is_err()
            );
        };
        refused(AddReviewThreadRequest {
            line: 0,
            ..valid.clone()
        });
        refused(AddReviewThreadRequest {
            start_line: Some(13),
            ..valid.clone()
        });
        // A one-line range is legal: the selection was one line.
        let one_line = AddReviewThreadRequest {
            start_line: Some(12),
            ..valid.clone()
        };
        assert!(one_line.validate().is_ok());
        refused(AddReviewThreadRequest {
            start_side: Some(ReviewDiffSide::Left),
            ..valid.clone()
        });
        refused(AddReviewThreadRequest {
            path: "../secret".into(),
            ..valid.clone()
        });
        refused(AddReviewThreadRequest {
            path: "/abs".into(),
            ..valid.clone()
        });
        refused(AddReviewThreadRequest {
            body: "   ".into(),
            ..valid.clone()
        });
        refused(AddReviewThreadRequest {
            review_id: "not a node id".into(),
            ..valid.clone()
        });
        assert!(
            service
                .set_file_viewed(
                    &repository(),
                    "PR_1",
                    "a/../b",
                    true,
                    &CancellationToken::new()
                )
                .is_err()
        );
        assert!(
            service
                .update_comment(&repository(), "PRRC_1", "", &CancellationToken::new())
                .is_err()
        );
        // Nothing reached gh.
        assert!(service.executor.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_mutation_is_classified_by_what_the_forge_proved() {
        let repository = repository();
        let outcomes = |output: Result<ProcessOutput, ProcessFailure>| {
            let service =
                GitHubService::new("gh", Arc::new(FakeExecutor::with_outputs(vec![output])));
            service.set_file_viewed(
                &repository,
                "PR_1",
                "src/lib.rs",
                true,
                &CancellationToken::new(),
            )
        };
        assert!(matches!(
            outcomes(Err(started_failure(ProcessFailureKind::Spawn, false))),
            Ok(ReviewMutationOutcome::NotApplied { .. })
        ));
        assert!(matches!(
            outcomes(Err(started_failure(ProcessFailureKind::Cancelled, true))),
            Ok(ReviewMutationOutcome::OutcomeUnknown { .. })
        ));
        assert!(matches!(
            outcomes(success(
                br#"{"data":{"markFileAsViewed":{"clientMutationId":null}}}"#
            )),
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        // Errors beside a payload are field errors on an applied mutation.
        assert!(matches!(
            outcomes(failed(
                1,
                br#"{"data":{"markFileAsViewed":{"clientMutationId":null}},"errors":[{"message":"partial"}]}"#
            )),
            Ok(ReviewMutationOutcome::Applied(()))
        ));
        // A typed refusal is raised before the write, so it proves non-application.
        match outcomes(failed(
            1,
            br#"{"data":{"markFileAsViewed":null},"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a node"}]}"#,
        )) {
            Ok(ReviewMutationOutcome::NotApplied { failure }) => {
                assert!(failure.message.contains("Could not resolve"), "{failure}");
            }
            other => panic!("unexpected {other:?}"),
        }
        // So does a response that never reached execution: no `data` at all.
        assert!(matches!(
            outcomes(failed(
                1,
                br#"{"errors":[{"message":"Variable $path of type String! was provided invalid value"}]}"#
            )),
            Ok(ReviewMutationOutcome::NotApplied { .. })
        ));
        // An untyped error beside a null payload is how a timeout or internal
        // failure reports, and those can land after the write.
        match outcomes(failed(
            1,
            br#"{"data":{"markFileAsViewed":null},"errors":[{"message":"Something went wrong while executing your query."}]}"#,
        )) {
            Ok(ReviewMutationOutcome::OutcomeUnknown { failure }) => {
                assert!(failure.message.contains("did not confirm"), "{failure}");
            }
            other => panic!("unexpected {other:?}"),
        }
        // One typed refusal beside an untyped error does not prove anything.
        assert!(matches!(
            outcomes(failed(
                1,
                br#"{"data":{"markFileAsViewed":null},"errors":[{"type":"NOT_FOUND","message":"a"},{"message":"b"}]}"#
            )),
            Ok(ReviewMutationOutcome::OutcomeUnknown { .. })
        ));
        assert!(matches!(
            outcomes(failed(1, b"error connecting to api.github.com\n")),
            Ok(ReviewMutationOutcome::OutcomeUnknown { .. })
        ));
        assert!(matches!(
            outcomes(success(b"{}")),
            Ok(ReviewMutationOutcome::OutcomeUnknown { .. })
        ));
        assert!(matches!(
            outcomes(failed(4, b"")),
            Err(GitHubError::AuthenticationRequired { .. })
        ));
    }

    #[test]
    fn an_applied_payload_this_build_cannot_read_is_unknown_not_refused() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![success(
            br#"{"data":{"addPullRequestReview":{"pullRequestReview":{"id":42}}}}"#,
        )]));
        let service = GitHubService::new("gh", fake);
        let outcome = service
            .start_review(&repository(), "PR_1", &CancellationToken::new())
            .unwrap();
        match outcome {
            ReviewMutationOutcome::OutcomeUnknown { failure } => {
                assert!(failure.started);
                assert!(failure.message.contains("unreadable"), "{failure}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn submitting_picks_the_mutation_the_pending_state_needs() {
        let review_payload = br#"{"data":{"submitPullRequestReview":{"pullRequestReview":{"id":"PRR_1","state":"APPROVED","url":"https://github.com/owner/repo/pull/7#pullrequestreview-1"}}}}"#;
        let fresh_payload = br#"{"data":{"addPullRequestReview":{"pullRequestReview":{"id":"PRR_2","state":"COMMENTED","url":"https://github.com/owner/repo/pull/7#pullrequestreview-2"}}}}"#;
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(review_payload),
            success(fresh_payload),
        ]));
        let service = GitHubService::new("gh", fake.clone());
        let submitted = service
            .submit_review(
                &repository(),
                &SubmitReviewRequest {
                    pull_request_id: "PR_1".into(),
                    review_id: Some("PRR_1".into()),
                    event: ReviewEvent::Approve,
                    body: "LGTM".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        let ReviewMutationOutcome::Applied(review) = submitted else {
            panic!("expected an applied review");
        };
        assert_eq!(review.state.0, "APPROVED");
        let commented = service
            .submit_review(
                &repository(),
                &SubmitReviewRequest {
                    pull_request_id: "PR_1".into(),
                    review_id: None,
                    event: ReviewEvent::Comment,
                    body: "  ".into(),
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(matches!(commented, ReviewMutationOutcome::Applied(_)));
        let commands = fake.commands.lock().unwrap();
        let first = argument_strings(&commands[0]);
        assert!(first.iter().any(|a| a.contains("submitPullRequestReview")));
        assert!(first.contains(&"event=APPROVE".to_owned()));
        assert!(first.contains(&"body=LGTM".to_owned()));
        assert!(first.contains(&"review=PRR_1".to_owned()));
        let second = argument_strings(&commands[1]);
        assert!(second.iter().any(|a| a.contains("addPullRequestReview(")));
        assert!(second.contains(&"pullRequest=PR_1".to_owned()));
        assert!(!second.iter().any(|a| a.starts_with("body=")));
    }

    #[test]
    fn viewed_and_resolved_toggles_name_the_matching_mutation() {
        let fake = Arc::new(FakeExecutor::with_outputs(vec![
            success(br#"{"data":{"markFileAsViewed":{"clientMutationId":null}}}"#),
            success(br#"{"data":{"unmarkFileAsViewed":{"clientMutationId":null}}}"#),
            success(br#"{"data":{"resolveReviewThread":{"thread":{"id":"PRRT_1","isResolved":true}}}}"#),
            success(br#"{"data":{"unresolveReviewThread":{"thread":{"id":"PRRT_1","isResolved":false}}}}"#),
        ]));
        let service = GitHubService::new("gh", fake.clone());
        let token = CancellationToken::new();
        for viewed in [true, false] {
            assert!(matches!(
                service
                    .set_file_viewed(&repository(), "PR_1", "src/lib.rs", viewed, &token)
                    .unwrap(),
                ReviewMutationOutcome::Applied(())
            ));
        }
        for resolved in [true, false] {
            assert!(matches!(
                service
                    .set_thread_resolved(&repository(), "PRRT_1", resolved, &token)
                    .unwrap(),
                ReviewMutationOutcome::Applied(())
            ));
        }
        let commands = fake.commands.lock().unwrap();
        let mutation = |index: usize| {
            argument_strings(&commands[index])
                .into_iter()
                .find(|a| a.starts_with("query="))
                .unwrap()
        };
        assert!(mutation(0).contains("markFileAsViewed"));
        assert!(mutation(1).contains("unmarkFileAsViewed"));
        assert!(mutation(2).contains("resolveReviewThread"));
        assert!(mutation(3).contains("unresolveReviewThread"));
        assert!(argument_strings(&commands[0]).contains(&"path=src/lib.rs".to_owned()));
    }
}
