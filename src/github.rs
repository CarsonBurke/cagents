//! Typed GitHub CLI integration.
//!
//! Authentication remains owned by `gh`.  The service neither requests nor
//! persists tokens, and every repository selector and user value is passed as
//! a distinct process argument.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
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

const PR_LIST_FIELDS: &str = "number,title,state,isDraft,headRefName,headRefOid,baseRefName,author,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels";
const PR_VIEW_FIELDS: &str = "number,title,body,state,isDraft,headRefName,headRefOid,baseRefName,author,createdAt,updatedAt,url,mergeable,mergeStateStatus,reviewDecision,labels,additions,deletions,changedFiles";
const PR_CHECK_FIELDS: &str =
    "bucket,completedAt,description,event,link,name,startedAt,state,workflow";

/// How many `gh pr checks` invocations one refresh may have in flight.
///
/// Each one is a network round trip, and a refresh needs one per open pull
/// request, so running them serially made refresh latency scale with the pull
/// request count.
const CHECK_CONCURRENCY: usize = 6;

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

/// Permits shared by every concurrent refresh; see [`CHECK_PROCESS_CONCURRENCY`].
static CHECK_PERMITS: LazyLock<CheckPermits> =
    LazyLock::new(|| CheckPermits::new(CHECK_PROCESS_CONCURRENCY));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChecksCacheKey {
    pull_request: PullRequestKey,
    /// `gh` reports this for every pull request it lists, and it changes whenever
    /// the pull request record does, so it is the version half of the key.
    updated_at: String,
}

#[derive(Debug, Clone)]
struct CachedChecks {
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
        Self::new(gh, Arc::new(SystemProcessExecutor::default()))
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
            Some(4) => Err(GitHubError::AuthenticationRequired {
                host: key.repository.host.clone(),
                failure: Box::new(output.require_success(&self.gh).unwrap_err()),
            }),
            _ => Err(output.require_success(&self.gh).unwrap_err().into()),
        }
    }

    /// Fetches the checks of every pull request in `pull_requests`, all of which
    /// must belong to `repository`.
    ///
    /// Results keep the order of the input, and a pull request whose checks could
    /// not be fetched yields an empty list — exactly what a caller asking one
    /// pull request at a time observes. Two things make this cheaper than that
    /// serial loop: a completed result is reused while the pull request is
    /// unchanged and the result is fresh, and the remaining fetches run
    /// concurrently up to [`CHECK_CONCURRENCY`].
    pub fn checks_for_pull_requests(
        &self,
        repository: &GitHubRepoKey,
        pull_requests: &[PullRequest],
        cancellation: &CancellationToken,
    ) -> Vec<Vec<PullRequestCheck>> {
        let keys = pull_requests
            .iter()
            .map(|pull_request| ChecksCacheKey {
                pull_request: PullRequestKey {
                    repository: repository.clone(),
                    number: pull_request.number,
                },
                updated_at: pull_request.updated_at.clone(),
            })
            .collect::<Vec<_>>();
        let mut results = vec![Vec::new(); pull_requests.len()];
        let mut missing = Vec::new();
        {
            let now = Instant::now();
            let mut cache = lock_checks_cache();
            // Two rules bound the cache. An entry expires, which is what keeps a
            // repository nobody refreshes any more from living forever; and a new
            // version of a pull request supersedes the entry cached for the old
            // one. Eviction is deliberately not scoped to "everything for this
            // repository that the caller did not list", because that would make a
            // caller refreshing a single pull request wipe the rest.
            let versions = keys
                .iter()
                .map(|key| (&key.pull_request, &key.updated_at))
                .collect::<HashMap<_, _>>();
            cache.retain(|key, entry| {
                now.duration_since(entry.fetched_at) < CHECKS_CACHE_TTL
                    && versions
                        .get(&key.pull_request)
                        .is_none_or(|updated_at| **updated_at == key.updated_at)
            });
            for (index, key) in keys.iter().enumerate() {
                match cache.get(key) {
                    Some(entry) => results[index] = entry.checks.clone(),
                    None => missing.push(index),
                }
            }
        }
        if missing.is_empty() {
            return results;
        }

        let next = AtomicUsize::new(0);
        let fetched = Mutex::new(Vec::with_capacity(missing.len()));
        thread::scope(|scope| {
            for _ in 0..CHECK_CONCURRENCY.min(missing.len()) {
                scope.spawn(|| {
                    loop {
                        let position = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&index) = missing.get(position) else {
                            break;
                        };
                        // The executor already fails a cancelled command before
                        // launch; stopping here only avoids the pointless spawn.
                        if cancellation.is_cancelled() {
                            break;
                        }
                        let (checks, complete) = {
                            // Held only around the subprocess, so a refresh
                            // waiting for a permit holds no other resource.
                            let _permit = CHECK_PERMITS.acquire();
                            match self.checks(&keys[index].pull_request, cancellation) {
                                Ok(checks) => (checks, true),
                                // Pending checks are worth showing, but caching
                                // them would pin the display to a state that
                                // changes without the pull request changing.
                                Err(GitHubError::ChecksPending { checks, .. }) => (checks, false),
                                Err(_) => (Vec::new(), false),
                            }
                        };
                        fetched
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((index, checks, complete));
                    }
                });
            }
        });

        let fetched_at = Instant::now();
        let mut cache = lock_checks_cache();
        for (index, checks, complete) in fetched
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            if complete {
                cache.insert(
                    keys[index].clone(),
                    CachedChecks {
                        fetched_at,
                        checks: checks.clone(),
                    },
                );
            }
            results[index] = checks;
        }
        results
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
}
