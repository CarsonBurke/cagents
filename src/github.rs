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

const PR_LIST_FIELDS: &str = "number,title,state,isDraft,headRefName,headRefOid,baseRefName,author,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels,statusCheckRollup";
const PR_VIEW_FIELDS: &str = "number,title,body,state,isDraft,headRefName,headRefOid,baseRefName,author,createdAt,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels,additions,deletions,changedFiles,statusCheckRollup";
const PR_CHECK_FIELDS: &str =
    "bucket,completedAt,description,event,link,name,startedAt,state,workflow";

/// How many may be in flight across the whole process.
///
/// Refreshes are issued one per project and run concurrently, so the per-refresh
/// bound alone would still multiply by the project count. This ceiling is above
/// the one-`gh`-per-project the serial code reached, so it bounds the burst
/// without making a many-project refresh slower than it used to be.
const CHECK_PROCESS_CONCURRENCY: usize = 12;

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

/// Permits shared by every concurrent refresh; see [`CHECK_PROCESS_CONCURRENCY`].
static CHECK_PERMITS: LazyLock<CheckPermits> =
    LazyLock::new(|| CheckPermits::new(CHECK_PROCESS_CONCURRENCY));

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubActor {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "is_bot", default)]
    pub is_bot: bool,
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
    pub base_ref_name: String,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestComment {
    pub author: Option<GitHubActor>,
    pub body: String,
    pub created_at: String,
    pub url: Url,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestReview {
    #[serde(flatten)]
    pub comment: PullRequestComment,
    pub state: RemoteValue,
}

#[derive(Debug, Clone)]
pub struct PullRequestReviewThread {
    pub path: String,
    pub line: Option<u64>,
    pub outdated: bool,
    pub resolved: bool,
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
    is_outdated: bool,
    is_resolved: bool,
    comments: Connection<PullRequestComment>,
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

/// Only a completed refresh grants authority. Cached presentation rows deliberately
/// live outside this value and cannot supply a repository to a mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubAvailability {
    MissingCli,
    Unauthenticated,
    NotAssociated,
    Loading,
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
            Self::Loading => "Refreshing GitHub; mutations are disabled.",
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
fn remote_repository_key(remote: &str) -> Option<GitHubRepoKey> {
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
        let output = self.executor.execute(
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
        let output = self.executor.execute(&command, cancellation)?;
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
        branch: &str,
        head_oid: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Option<PullRequest>, GitHubError> {
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
        let candidates =
            attach_process(parse_pull_request_list(&output.stdout, repository), &output)?;
        Ok(select_branch_pull_request(&candidates, branch, head_oid)?.cloned())
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

    fn graphql(
        &self,
        repository: &GitHubRepoKey,
        query: &str,
        variables: Vec<(&str, String)>,
        cancellation: &CancellationToken,
    ) -> Result<serde_json::Value, GitHubError> {
        let mut command = self.gh_command([
            OsString::from("api"),
            "graphql".into(),
            "--hostname".into(),
            repository.authority().into(),
            "-f".into(),
            format!("query={query}").into(),
        ]);
        for (name, value) in variables {
            command
                .arguments
                .extend([OsString::from("-f"), format!("{name}={value}").into()]);
        }
        let output = self.run_checked(&command, repository, cancellation)?;
        attach_process(parse_graphql(&output.stdout), &output)
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
                "reviewThreads" => {
                    "id path line originalLine isOutdated isResolved comments(first:50){nodes{author{login} body createdAt url} pageInfo{hasNextPage endCursor}}"
                }
                "reviews" => "author{login} body createdAt url state",
                _ => "author{login} body createdAt url",
            };
            let query = format!(
                "query($owner:String!,$name:String!,$cursor:String){{repository(owner:$owner,name:$name){{pullRequest(number:{}){{{section}(first:50,after:$cursor){{nodes{{{fields}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}",
                key.number
            );
            let mut cursor: Option<String> = None;
            for page in 0..4 {
                let mut variables = vec![
                    ("owner", key.repository.owner.clone()),
                    ("name", key.repository.name.clone()),
                ];
                if let Some(cursor) = &cursor {
                    variables.push(("cursor", cursor.clone()));
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
                    for thread in connection.nodes {
                        let mut comments = thread.comments.nodes;
                        let mut truncated = thread.comments.page_info.has_next_page;
                        if truncated {
                            let cursor = next_cursor(&thread.comments.page_info, None)?;
                            let data = self.graphql(
                                &key.repository,
                                "query($id:ID!,$cursor:String!){node(id:$id){... on PullRequestReviewThread{comments(first:50,after:$cursor){nodes{author{login} body createdAt url} pageInfo{hasNextPage endCursor}}}}}",
                                vec![("id", thread.id), ("cursor", cursor)],
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
                        result.threads.push(PullRequestReviewThread {
                            path: thread.path,
                            line: thread.line.or(thread.original_line),
                            outdated: thread.is_outdated,
                            resolved: thread.is_resolved,
                            comments,
                            truncated,
                        });
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
        let output = self.executor.execute(&command, cancellation)?;
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
        let _permit = CHECK_PERMITS.acquire();
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
        let output = match self.executor.execute(&command, cancellation) {
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
        let (pull_request, normalization_warning) =
            match self.executor.execute(&view, &normalization_token) {
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

    fn run_checked(
        &self,
        command: &CommandSpec,
        repository: &GitHubRepoKey,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, GitHubError> {
        repository.validate()?;
        let output = self.executor.execute(command, cancellation)?;
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
        .filter(|pr| pr.head_ref_name == branch)
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
        let output = br#"[{"number":7,"title":"Title","state":"FUTURE_STATE","isDraft":false,"headRefName":"feature","headRefOid":"abc","baseRefName":"main","author":null,"updatedAt":"2026-01-01T00:00:00Z","url":"https://github.com/owner/repo/pull/7","mergeable":"FUTURE","mergeStateStatus":null,"reviewDecision":null,"labels":[]}]"#;
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
        let detail = br#"{"number":7,"title":"Title","body":"Body","state":"OPEN","isDraft":false,"headRefName":"feature","headRefOid":"abc","baseRefName":"main","author":null,"createdAt":"now","updatedAt":"now","url":"https://github.com/owner/repo/pull/7","labels":[]}"#;
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
    fn branch_lookup_never_chooses_an_ambiguous_fork_by_list_order() {
        let first = parse_pull_request(
            br#"{"number":7,"title":"Title","state":"OPEN","headRefName":"feature","headRefOid":"abc","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"}"#,
            &repository(),
        ).unwrap();
        let mut second = first.clone();
        second.number = 8;
        second.head_ref_oid = "def".into();
        let candidates = [first, second];
        assert!(select_branch_pull_request(&candidates, "feature", None).is_err());
        assert_eq!(
            select_branch_pull_request(&candidates, "feature", Some("def"))
                .unwrap()
                .unwrap()
                .number,
            8
        );
        assert!(
            select_branch_pull_request(&candidates, "unrelated", Some("def"))
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
}
