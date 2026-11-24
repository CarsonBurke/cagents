//! On-demand PR status, shared by local callers rather than one watcher per agent.
use crate::{
    github_identity::PullRequestKey,
    github_rate_limit::{RateLimit, now},
    process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use url::Url;

mod retention;
pub(crate) use retention::prune_cache;

/// A successful result and a failed attempt both suppress repeated reads for
/// two minutes. An explicit server cooldown can extend that interval.
pub const MAX_AGE: u64 = 120;
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
/// GitHub's largest page; a longer list is reported incomplete, never passing.
const REST_PAGE: usize = 100;
const QUERY: &str = "query CagentsPrStatus($owner:String!,$name:String!,$number:Int!){rateLimit{cost} repository(owner:$owner,name:$name){pullRequest(number:$number){number title url state isDraft headRefName headRefOid baseRefName mergeable mergeStateStatus reviewDecision updatedAt commits(last:1){nodes{commit{oid statusCheckRollup{state contexts(first:100){nodes{__typename ... on CheckRun{name status conclusion detailsUrl} ... on StatusContext{context state targetUrl description}} pageInfo{hasNextPage endCursor}}}}}}}}}";

struct StatusService<E = SystemProcessExecutor> {
    gh: PathBuf,
    executor: Arc<E>,
}

impl StatusService<SystemProcessExecutor> {
    fn system(gh: impl Into<PathBuf>) -> Self {
        Self::new(gh, Arc::new(SystemProcessExecutor))
    }
}

impl<E: ProcessExecutor> StatusService<E> {
    fn new(gh: impl Into<PathBuf>, executor: Arc<E>) -> Self {
        Self {
            gh: gh.into(),
            executor,
        }
    }

    /// Runs one read and returns its JSON body and response headers. A rate
    /// limit in a failure is bound to `resource`, the API the request used.
    fn execute(
        &self,
        command: &CommandSpec,
        resource: &str,
        cancellation: &CancellationToken,
    ) -> Result<(serde_json::Value, String), StatusError> {
        let output = self
            .executor
            .execute(command, cancellation)
            .map_err(|error| {
                classify_failure(&error.stdout, &error.stderr, resource)
                    .map(StatusError::RateLimited)
                    .unwrap_or_else(|| StatusError::Message(error.reported_detail()))
            })?;
        if !output.success {
            if let Some(limit) = classify_failure(&output.stdout, &output.stderr, resource) {
                return Err(StatusError::RateLimited(limit));
            }
            return Err(StatusError::Message(
                output
                    .require_success(&self.gh)
                    .unwrap_err()
                    .reported_detail(),
            ));
        }
        let body = response_body(&output.stdout);
        let headers = String::from_utf8_lossy(&output.stdout[..output.stdout.len() - body.len()])
            .into_owned();
        let body = serde_json::from_slice(body).map_err(|error| {
            StatusError::Message(format!("Invalid GitHub status JSON: {error}"))
        })?;
        Ok((body, headers))
    }

    fn fetch(
        &self,
        transport: Transport,
        key: &PullRequestKey,
        lease: &Arc<fs::File>,
        cancellation: &CancellationToken,
    ) -> Result<(Snapshot, Option<RateLimit>), StatusError> {
        match transport {
            Transport::Graphql => self.fetch_graphql(key, lease, cancellation),
            Transport::Rest => self.fetch_rest(key, lease, cancellation),
        }
    }

    fn fetch_graphql(
        &self,
        key: &PullRequestKey,
        lease: &Arc<fs::File>,
        cancellation: &CancellationToken,
    ) -> Result<(Snapshot, Option<RateLimit>), StatusError> {
        let command = inherit(status_command(&self.gh, key), lease)?;
        let (mut envelope, headers) = self.execute(&command, "graphql", cancellation)?;
        if let Some(errors) = envelope
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .filter(|errors| !errors.is_empty())
        {
            let text = format!("{headers}\n{}", serde_json::Value::Array(errors.clone()));
            if let Some(limit) = RateLimit::classify_for(&text, now(), "graphql") {
                return Err(StatusError::RateLimited(limit));
            }
            let messages = errors
                .iter()
                .filter_map(|error| error["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(StatusError::Message(format!(
                "GitHub GraphQL error: {messages}"
            )));
        }
        let data = envelope
            .as_object_mut()
            .and_then(|envelope| envelope.remove("data"))
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| StatusError::Message("GitHub returned no status data".into()))?;
        Ok((
            parse(&data, key)?,
            RateLimit::classify_for(&headers, now(), "graphql"),
        ))
    }

    /// Reads the PR, then CI for its head, through REST (`core`) while GraphQL
    /// is in a cooldown of its own. Four requests in sequence; one that fails
    /// or exhausts the quota ends the read, without retrying anything.
    fn fetch_rest(
        &self,
        key: &PullRequestKey,
        lease: &Arc<fs::File>,
        cancellation: &CancellationToken,
    ) -> Result<(Snapshot, Option<RateLimit>), StatusError> {
        let repository = format!("repos/{}/{}", key.repository.owner, key.repository.name);
        let get = |endpoint: String| {
            if cancellation.is_cancelled() {
                return Err(StatusError::Message("GitHub status read cancelled".into()));
            }
            let command = inherit(rest_command(&self.gh, key, &endpoint), lease)?;
            let (body, headers) = self.execute(&command, "core", cancellation)?;
            Ok::<_, StatusError>((body, RateLimit::classify_for(&headers, now(), "core")))
        };
        let (pull, mut limit) = get(format!("{repository}/pulls/{}", key.number))?;
        let pull_request = rest_pull_request(&pull, key)?;
        let head = pull_request.head_ref_oid.clone();
        if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StatusError::Message(
                "GitHub returned an invalid head commit".into(),
            ));
        }
        let mut pages = Vec::with_capacity(3);
        for endpoint in ["check-runs", "check-suites", "status"] {
            // A response that spent the quota or asked for a pause ends the
            // read; the next request would only be refused.
            if let Some(limit) = limit {
                return Err(StatusError::RateLimited(limit));
            }
            let (page, next) = get(format!(
                "{repository}/commits/{head}/{endpoint}?per_page={REST_PAGE}"
            ))?;
            pages.push(page);
            limit = next;
        }
        let (checks_state, checks, checks_complete) =
            rest_rollup(&pages[0], &pages[1], &pages[2], &head)?;
        Ok((
            Snapshot {
                pull_request,
                checks_state,
                checks,
                checks_complete,
                primary_cost: None,
                transport: Transport::Rest,
            },
            limit,
        ))
    }
}

fn inherit(command: CommandSpec, lease: &Arc<fs::File>) -> Result<CommandSpec, StatusError> {
    command.inherit_file(Arc::clone(lease)).map_err(|error| {
        StatusError::Message(format!("Cannot inherit the GitHub status flight: {error}"))
    })
}

#[derive(Debug, thiserror::Error)]
enum StatusError {
    #[error("{0}")]
    RateLimited(RateLimit),
    #[error("{0}")]
    Message(String),
}

impl StatusError {
    fn rate_limit(&self) -> Option<RateLimit> {
        match self {
            Self::RateLimited(limit) => Some(limit.clone()),
            Self::Message(_) => None,
        }
    }
}

fn classify_failure(stdout: &[u8], stderr: &[u8], resource: &str) -> Option<RateLimit> {
    let body = response_body(stdout);
    let headers = String::from_utf8_lossy(&stdout[..stdout.len() - body.len()]);
    // Partial GraphQL data may contain arbitrary titles and check names. Only
    // error fields, transport diagnostics and real headers establish pacing.
    let detail = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => format!(
            "{}\n{}",
            value.get("message").unwrap_or(&serde_json::Value::Null),
            value.get("errors").unwrap_or(&serde_json::Value::Null)
        ),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    };
    RateLimit::classify_for(
        &format!("{headers}\n{detail}\n{}", String::from_utf8_lossy(stderr)),
        now(),
        resource,
    )
}

fn status_command(gh: &Path, key: &PullRequestKey) -> CommandSpec {
    gh_command(gh).args([
        "api".into(),
        "graphql".into(),
        "--include".into(),
        "--hostname".into(),
        key.repository.authority(),
        "-f".into(),
        format!("query={QUERY}"),
        "-f".into(),
        format!("owner={}", key.repository.owner),
        "-f".into(),
        format!("name={}", key.repository.name),
        "-F".into(),
        format!("number={}", key.number),
    ])
}

fn rest_command(gh: &Path, key: &PullRequestKey, endpoint: &str) -> CommandSpec {
    gh_command(gh).args([
        "api".into(),
        "--method".into(),
        "GET".into(),
        "--include".into(),
        "--hostname".into(),
        key.repository.authority(),
        endpoint.into(),
    ])
}

fn gh_command(gh: &Path) -> CommandSpec {
    CommandSpec::new(gh)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("CAGENTS_GH_NONBLOCKING", "1")
        .env_remove("GH_FORCE_TTY")
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .timeout(Duration::from_secs(30))
}

fn response_body(mut bytes: &[u8]) -> &[u8] {
    while bytes.starts_with(b"HTTP/") {
        let boundary = bytes
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .map(|at| at + 4)
            .or_else(|| {
                bytes
                    .windows(2)
                    .position(|part| part == b"\n\n")
                    .map(|at| at + 2)
            });
        let Some(boundary) = boundary else {
            break;
        };
        bytes = &bytes[boundary..];
    }
    bytes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStatus {
    pub number: u64,
    pub title: String,
    pub url: Url,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub mergeable: String,
    pub merge_state_status: Option<String>,
    pub review_decision: Option<String>,
    pub updated_at: String,
}

/// The API a snapshot was read through. Snapshots saved before REST reads
/// existed came from GraphQL, or are the bootstrap script's REST shape,
/// which [`Snapshot::is_rest_bootstrap`] recognizes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Graphql,
    /// Used while GraphQL is in a cooldown of its own. REST has no review
    /// decision, and its aggregate is derived from the listed checks.
    Rest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub pull_request: PullRequestStatus,
    /// Through GraphQL, GitHub's aggregate includes every context, even when
    /// the list is paged. Through REST it is derived from the lists read, and
    /// is never SUCCESS when one of them is incomplete. None means no check
    /// was reported, not a passing check.
    pub checks_state: Option<String>,
    pub checks: Vec<serde_json::Value>,
    pub checks_complete: bool,
    pub primary_cost: Option<u64>,
    #[serde(default)]
    pub transport: Transport,
}

/// Whether a check context has passed, by the rule `scripts/land-pr` applies.
fn check_passed(check: &serde_json::Value) -> bool {
    if check["__typename"] == "StatusContext" {
        return check["state"] == "SUCCESS";
    }
    check["status"] == "COMPLETED"
        && matches!(
            check["conclusion"].as_str(),
            Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
        )
}

/// Whether a check context has failed, by the rule `scripts/land-pr` applies.
fn check_failed(check: &serde_json::Value) -> bool {
    if check["__typename"] == "StatusContext" {
        return matches!(check["state"].as_str(), Some("FAILURE" | "ERROR"));
    }
    matches!(
        check["conclusion"].as_str(),
        Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
    )
}

/// A check context that has not passed, as a report shows it, without the
/// timestamps that snapshots saved before the query stopped asking for them
/// still carry. Callers decide on its name and state, and follow its link.
fn reported_check(mut check: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = check.as_object_mut() {
        fields.remove("startedAt");
        fields.remove("completedAt");
    }
    check
}

/// A snapshot as a report shows it. A passing check needs nothing from its
/// reader, so passing checks are counted rather than listed; `checks` holds
/// those still running or not passed, which is all `scripts/land-pr` and an
/// agent act on. The aggregate, `checks_state`, still covers every check.
#[derive(Debug, Serialize)]
pub struct ReportedSnapshot {
    #[serde(flatten)]
    snapshot: Snapshot,
    pub checks_passed: usize,
}

impl ReportedSnapshot {
    pub fn new(mut snapshot: Snapshot) -> Self {
        let listed = snapshot.checks.len();
        snapshot.checks = std::mem::take(&mut snapshot.checks)
            .into_iter()
            .filter(|check| !check_passed(check))
            .map(reported_check)
            .collect();
        Self {
            checks_passed: listed - snapshot.checks.len(),
            snapshot,
        }
    }

    /// Whether the bootstrap script's REST read wrote this snapshot, as
    /// [`Snapshot::is_rest_bootstrap`] decides from every check it listed.
    pub fn is_rest_bootstrap(&self) -> bool {
        self.checks_passed == 0 && self.snapshot.is_rest_bootstrap()
    }
}

/// Reads see the snapshot's fields, with `checks` already filtered; judge a
/// whole snapshot, as [`Snapshot::is_rest_bootstrap`] does, before filtering.
impl std::ops::Deref for ReportedSnapshot {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        &self.snapshot
    }
}

impl Snapshot {
    /// Whether the bootstrap script's single REST read wrote this snapshot
    /// to the shared cache. REST carries no check rollup or review decision,
    /// so CI is unknown rather than absent; a GraphQL read that finds no
    /// rollup reports its context list complete.
    pub fn is_rest_bootstrap(&self) -> bool {
        self.checks_state.is_none() && !self.checks_complete && self.checks.is_empty()
    }

    /// What this snapshot cannot tell its reader.
    fn limitations(&self) -> Vec<&'static str> {
        if self.is_rest_bootstrap() {
            return vec!["Single REST bootstrap read; CI and review decision are unknown."];
        }
        let mut limitations = Vec::new();
        if self.transport == Transport::Rest {
            limitations.push(
                "Read through REST while GitHub GraphQL is rate limited; the review decision is unknown.",
            );
            if !self.checks_complete {
                limitations.push(
                    "More checks than one REST page holds; CI cannot pass until GraphQL reads them all.",
                );
            }
        }
        limitations
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub target: PullRequestKey,
    pub fetched_at: Option<u64>,
    pub age_seconds: Option<u64>,
    pub stale: bool,
    pub source: &'static str,
    pub snapshot: Option<ReportedSnapshot>,
    pub error: Option<String>,
    pub retry_at: Option<u64>,
    pub outcome: &'static str,
    /// What the snapshot cannot tell, such as an unknown review decision.
    /// Unlike the bootstrap script's top-level `limitations`, these do not
    /// by themselves mean CI is unknown: read `checks_state` for that.
    pub limitations: Vec<&'static str>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cached {
    fetched_at: Option<u64>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    next_attempt: u64,
}

impl Cached {
    fn report(self, target: &PullRequestKey, source: &'static str, now: u64) -> Report {
        let failed = self.error.is_some() || self.snapshot.is_none();
        let source = if self
            .snapshot
            .as_ref()
            .is_some_and(Snapshot::is_rest_bootstrap)
        {
            "rest-bootstrap"
        } else {
            source
        };
        let limitations = self
            .snapshot
            .as_ref()
            .map(Snapshot::limitations)
            .unwrap_or_default();
        Report {
            target: target.clone(),
            fetched_at: self.fetched_at,
            age_seconds: self.fetched_at.map(|at| now.saturating_sub(at)),
            stale: failed
                || self
                    .fetched_at
                    .is_none_or(|at| now.saturating_sub(at) >= MAX_AGE),
            source,
            snapshot: self.snapshot.map(ReportedSnapshot::new),
            limitations,
            error: self.error,
            retry_at: (self.next_attempt > now).then_some(self.next_attempt),
            outcome: if failed { "failed" } else { "succeeded" },
        }
    }
}

pub fn read(key: &PullRequestKey, cancellation: &CancellationToken) -> Result<Report, String> {
    let root = state_root()?;
    let credentials = credential_fingerprint()?;
    read_with(
        &root,
        &credentials,
        key,
        &StatusService::system("gh"),
        cancellation,
        || {
            if credential_fingerprint()? == credentials {
                Ok(())
            } else {
                Err("GitHub credentials changed while reading status; retry later".into())
            }
        },
    )
}

pub(crate) fn state_root() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or("Cannot locate the private GitHub status cache")?;
    Ok(base.join("cagents/github-status"))
}

/// Scope cached private data to gh's credential context, without probing the
/// network or retaining credentials. Stored-account switches change hosts.yml;
/// explicit token environments are distinct even with the same configuration.
pub(crate) fn credential_fingerprint() -> Result<String, String> {
    let configuration = std::env::var_os("GH_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .map(|path| path.join("gh"))
        })
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/gh")))
        .ok_or("Cannot locate gh's credential configuration")?;
    let configuration = std::path::absolute(configuration).map_err(|error| error.to_string())?;
    let hosts = match fs::read(configuration.join("hosts.yml")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "Cannot read gh's credential configuration: {error}"
            ));
        }
    };
    let mut digest = Sha256::new();
    add_digest(&mut digest, configuration.as_os_str().as_encoded_bytes());
    add_digest(&mut digest, &hosts);
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        add_digest(&mut digest, name.as_bytes());
        match std::env::var_os(name) {
            Some(value) => {
                add_digest(&mut digest, b"present");
                add_digest(&mut digest, value.as_encoded_bytes());
            }
            None => add_digest(&mut digest, b"absent"),
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn add_digest(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn digest(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        add_digest(&mut digest, part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn target_name(credentials: &str, key: &PullRequestKey) -> String {
    digest(&[
        "pr-status-v1",
        credentials,
        &key.repository.authority().to_ascii_lowercase(),
        &key.repository.owner.to_ascii_lowercase(),
        &key.repository.name.to_ascii_lowercase(),
        &key.number.to_string(),
    ])
}

pub(crate) fn private_directory(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    match builder.create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(root)?;
    // SAFETY: geteuid takes no arguments or pointers.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "GitHub cache directory must be private and owned by this user",
        ));
    }
    Ok(())
}

pub(crate) fn lock(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<crate::instance::OwnedLock, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if cancellation.is_cancelled() {
            return Err("GitHub status read cancelled".into());
        }
        match crate::instance::owned_lock(path, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("Cannot lock the GitHub cache: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("Another GitHub status read is still in progress; retry later".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn host_paths(root: &Path, credentials: &str, host: &str) -> (PathBuf, PathBuf) {
    let name = digest(&["host-v2", credentials, &host.to_ascii_lowercase()]);
    (
        root.join(format!("{name}.lock")),
        root.join(format!("{name}.cooldown")),
    )
}

/// Versioned, resource-aware pacing. host-v1 stored an ambiguous bare reset
/// timestamp, which cannot establish an all-resource secondary pause.
#[derive(Default, Serialize, Deserialize)]
struct HostBudget {
    // Compatibility name: resource-local pauses also include ambiguous rate
    // errors and do not by themselves diagnose primary quota exhaustion.
    primary: std::collections::BTreeMap<String, u64>,
    secondary: u64,
}

impl HostBudget {
    fn limit(&self, resource: &str, at: u64) -> Option<RateLimit> {
        let secondary = (self.secondary > at).then_some(self.secondary);
        if let Some(&due) = self.primary.get(resource).filter(|&&due| due > at) {
            Some(RateLimit {
                retry_at: due.max(secondary.unwrap_or(0)),
                resource: Some(resource.into()),
                secondary_retry_at: secondary,
            })
        } else {
            secondary.map(RateLimit::secondary)
        }
    }
}

pub(crate) fn shared_cooldown(
    root: &Path,
    credentials: &str,
    host: &str,
    resource: &str,
    cancellation: &CancellationToken,
) -> Result<Option<RateLimit>, String> {
    let (lock_path, path) = host_paths(root, credentials, host);
    let _host = lock(&lock_path, cancellation)?;
    Ok(load::<HostBudget>(&path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default()
        .limit(resource, now()))
}

pub(crate) fn record_cooldown(
    root: &Path,
    credentials: &str,
    host: &str,
    limit: &RateLimit,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let (lock_path, path) = host_paths(root, credentials, host);
    let _host = lock(&lock_path, cancellation)?;
    let mut budget = load::<HostBudget>(&path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    budget.primary.retain(|_, due| *due > now());
    match limit.resource.as_deref() {
        Some("unknown") => {
            return Err("Cannot persist a primary GitHub limit without its resource".into());
        }
        Some(resource) => {
            let due = budget.primary.entry(resource.into()).or_default();
            *due = (*due).max(limit.retry_at);
            if let Some(due) = limit.secondary_retry_at {
                budget.secondary = budget.secondary.max(due);
            }
        }
        None => budget.secondary = budget.secondary.max(limit.retry_at),
    }
    store(&path, &budget).map_err(|error| error.to_string())
}

pub(crate) fn load<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("GitHub cache entry is not a regular file"));
    }
    if metadata.len() > MAX_CACHE_BYTES {
        return Err(io::Error::other("GitHub cache entry is too large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::other("GitHub cache entry is too large"));
    }
    // An interrupted/older cache is a miss, never evidence of a passing status.
    Ok(serde_json::from_slice(&bytes).ok())
}

pub(crate) fn store(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::other("GitHub cache entry is too large"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| io::Error::other("Missing cache directory"))?,
    )?;
    temporary.write_all(&bytes)?;
    temporary.flush()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_with<E: ProcessExecutor>(
    root: &Path,
    credentials: &str,
    key: &PullRequestKey,
    service: &StatusService<E>,
    cancellation: &CancellationToken,
    verify_credentials: impl FnOnce() -> Result<(), String>,
) -> Result<Report, String> {
    key.validate().map_err(|error| error.to_string())?;
    i32::try_from(key.number).map_err(|_| "The pull request number exceeds GitHub's Int range")?;
    private_directory(root).map_err(|error| error.to_string())?;
    let name = target_name(credentials, key);
    prune_cache(root, &name);
    let _flight = lock(&root.join(format!("{name}.lock")), cancellation)?;
    let path = root.join(format!("{name}.json"));
    let mut cached = load::<Cached>(&path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let current = now();
    // CLIs from before REST reads stored a failed read's GraphQL reset as the
    // PR's next attempt. Once this PR's shared interval has passed, such a
    // deadline gives way to the per-resource choice below, which still waits
    // when every resource is paused.
    let earlier_cli_pause = cached.error.is_some()
        && cached.next_attempt > current.saturating_add(MAX_AGE)
        && published_at(&path).is_some_and(|at| current.saturating_sub(at) >= MAX_AGE);
    if cached.next_attempt > current && !earlier_cli_pause {
        verify_credentials()?;
        return Ok(cached.report(key, "cache", current));
    }
    let host = key.repository.authority();
    let transport = match shared_cooldown(root, credentials, &host, "graphql", cancellation)? {
        None => Transport::Graphql,
        // A primary reset belongs to its resource: exhausted GraphQL leaves
        // REST quota to read with. A secondary pause covers both.
        Some(graphql) => match shared_cooldown(root, credentials, &host, "core", cancellation)?
            .or_else(|| bootstrap_rest_pause(root, &name, current))
        {
            None => Transport::Rest,
            Some(core) => {
                let earlier = if core.retry_at < graphql.retry_at {
                    core
                } else {
                    graphql
                };
                cached.error = Some(earlier.to_string());
                cached.next_attempt = earlier.retry_at;
                verify_credentials()?;
                return Ok(cached.report(key, "cache", current));
            }
        },
    };
    // An exclusive flight must retain its original open description: an
    // independently opened shared lease would conflict with this lock. Keep
    // the scoped owner until publication so normal return unlocks unrelated
    // fork copies; abrupt owner death leaves gh holding the original EX lock.
    let lease = Arc::new(
        _flight
            .try_clone()
            .map_err(|error| format!("Cannot inherit the GitHub status flight: {error}"))?,
    );
    let result = service.fetch(transport, key, &lease, cancellation);
    if cancellation.is_cancelled() {
        return Err("GitHub status read cancelled".into());
    }
    verify_credentials()?;
    let completed = now();
    cached.next_attempt = completed.saturating_add(MAX_AGE);
    let server_limit = match &result {
        Ok((_, limit)) => limit.clone(),
        Err(error) => error.rate_limit(),
    };
    if let Some(limit) = server_limit {
        record_cooldown(root, credentials, &host, &limit, cancellation)?;
        if result.is_err() {
            // The PR's next attempt is shared by both transports, so only a
            // pause that covers every resource extends it. A resource's own
            // reset stays in the host budget, where the next read finds it
            // and chooses the other transport.
            let shared = match limit.resource {
                None => limit.retry_at,
                Some(_) => limit.secondary_retry_at.unwrap_or(0),
            };
            cached.next_attempt = cached.next_attempt.max(shared);
        }
    }
    match result {
        Ok((snapshot, _)) => {
            cached.snapshot = Some(snapshot);
            cached.fetched_at = Some(completed);
            cached.error = None;
        }
        Err(error) => cached.error = Some(error.to_string()),
    }
    store(&path, &cached).map_err(|error| error.to_string())?;
    Ok(cached.report(key, "network", completed))
}

/// When the cache entry was last written, which a read always does.
fn published_at(path: &Path) -> Option<u64> {
    let modified = fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

/// A REST pause the bootstrap script kept for this PR alone, because the
/// host budget's lock was busy when it tried to share it.
fn bootstrap_rest_pause(root: &Path, name: &str, current: u64) -> Option<RateLimit> {
    #[derive(Deserialize)]
    struct RestRetry {
        retry_at: u64,
    }
    let retry = load::<RestRetry>(&root.join(format!("{name}.rest-retry")))
        .ok()
        .flatten()?;
    (retry.retry_at > current).then(|| RateLimit {
        retry_at: retry.retry_at,
        resource: Some("core".into()),
        secondary_retry_at: None,
    })
}

fn parse(data: &serde_json::Value, key: &PullRequestKey) -> Result<Snapshot, StatusError> {
    let pr = data
        .pointer("/repository/pullRequest")
        .filter(|value| value.is_object())
        .ok_or_else(|| StatusError::Message("GitHub returned no pull request".into()))?;
    let pull_request: PullRequestStatus = serde_json::from_value(pr.clone()).map_err(|error| {
        StatusError::Message(format!("Invalid GitHub status response: {error}"))
    })?;
    same_pull_request(&pull_request, key)?;
    let commit = pr
        .pointer("/commits/nodes/0/commit")
        .filter(|commit| commit["oid"].as_str() == Some(pull_request.head_ref_oid.as_str()))
        .ok_or_else(|| {
            StatusError::Message(
                "The check rollup did not match the pull request head; retry later".into(),
            )
        })?;
    let rollup = commit
        .get("statusCheckRollup")
        .ok_or_else(|| StatusError::Message("GitHub returned no check rollup field".into()))?;
    let (checks_state, checks, checks_complete) = if rollup.is_null() {
        (None, Vec::new(), true)
    } else {
        let state = rollup["state"].as_str().ok_or_else(|| {
            StatusError::Message("GitHub returned no aggregate check state".into())
        })?;
        let checks = rollup
            .pointer("/contexts/nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| StatusError::Message("GitHub returned no check contexts".into()))?;
        let more = rollup
            .pointer("/contexts/pageInfo/hasNextPage")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                StatusError::Message("GitHub returned no check pagination state".into())
            })?;
        (Some(state.into()), checks.clone(), !more)
    };
    Ok(Snapshot {
        pull_request,
        checks_state,
        checks,
        checks_complete,
        primary_cost: data
            .pointer("/rateLimit/cost")
            .and_then(serde_json::Value::as_u64),
        transport: Transport::Graphql,
    })
}

fn same_pull_request(
    pull_request: &PullRequestStatus,
    key: &PullRequestKey,
) -> Result<(), StatusError> {
    let identity =
        PullRequestKey::from_url(pull_request.url.as_str()).map_err(StatusError::Message)?;
    if identity.number != key.number
        || pull_request.number != key.number
        || !identity.repository.same_repository(&key.repository)
    {
        return Err(StatusError::Message(
            "GitHub returned a different pull request".into(),
        ));
    }
    Ok(())
}

/// The REST pull request in the shape GraphQL reports it. REST has no review
/// decision, and a null `mergeable` means GitHub is still computing it.
fn rest_pull_request(
    pull: &serde_json::Value,
    key: &PullRequestKey,
) -> Result<PullRequestStatus, StatusError> {
    let invalid =
        |field: &str| StatusError::Message(format!("GitHub returned no pull request {field}"));
    let text = |pointer: &str| {
        pull.pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(pointer))
    };
    let flag = |field: &str| pull[field].as_bool().ok_or_else(|| invalid(field));
    let url = text("/html_url")?;
    let pull_request = PullRequestStatus {
        number: pull["number"].as_u64().ok_or_else(|| invalid("number"))?,
        title: text("/title")?,
        url: Url::parse(&url).map_err(|_| invalid("URL"))?,
        state: if flag("merged")? {
            "MERGED".into()
        } else {
            text("/state")?.to_ascii_uppercase()
        },
        is_draft: flag("draft")?,
        head_ref_name: text("/head/ref")?,
        head_ref_oid: text("/head/sha")?,
        base_ref_name: text("/base/ref")?,
        mergeable: match pull["mergeable"].as_bool() {
            Some(true) => "MERGEABLE",
            Some(false) => "CONFLICTING",
            None => "UNKNOWN",
        }
        .into(),
        merge_state_status: Some(
            pull["mergeable_state"]
                .as_str()
                .unwrap_or("unknown")
                .to_ascii_uppercase(),
        ),
        review_decision: None,
        updated_at: text("/updated_at")?,
    };
    same_pull_request(&pull_request, key)?;
    Ok(pull_request)
}

/// Check suites that say whether a head's check runs exist yet. Every app
/// that can write checks gets a suite on each push, and GitHub leaves it
/// queued forever when the app never runs, as GitHub's own pages ignore. A
/// GitHub Actions suite is a workflow run, which exists before its jobs'
/// check runs do, so an unfinished one means CI is still being created.
fn suite_counts(suite: &serde_json::Value) -> bool {
    suite
        .pointer("/app/slug")
        .and_then(serde_json::Value::as_str)
        == Some("github-actions")
        || suite["latest_check_runs_count"].as_u64() != Some(0)
}

/// The aggregate, contexts and completeness that GraphQL's check rollup
/// reports, derived from REST's check runs, check suites and combined
/// commit status for `head`. SUCCESS needs every list complete, every
/// counted suite finished and every context passed; a failure anywhere is
/// FAILURE; nothing at all is None, as GraphQL reports a missing rollup.
fn rest_rollup(
    runs: &serde_json::Value,
    suites: &serde_json::Value,
    status: &serde_json::Value,
    head: &str,
) -> Result<(Option<String>, Vec<serde_json::Value>, bool), StatusError> {
    /// A page's items, and whether they are all there are.
    fn list<'a>(
        page: &'a serde_json::Value,
        field: &str,
    ) -> Result<(&'a Vec<serde_json::Value>, bool), StatusError> {
        let items = page[field]
            .as_array()
            .ok_or_else(|| StatusError::Message(format!("GitHub returned no {field} list")))?;
        let total = page["total_count"]
            .as_u64()
            .ok_or_else(|| StatusError::Message(format!("GitHub returned no {field} count")))?;
        Ok((items, items.len() as u64 == total))
    }
    let (runs, runs_complete) = list(runs, "check_runs")?;
    let (suites, suites_complete) = list(suites, "check_suites")?;
    let (statuses, statuses_complete) = list(status, "statuses")?;
    if runs
        .iter()
        .chain(suites)
        .any(|item| item["head_sha"].as_str() != Some(head))
        || status["sha"].as_str() != Some(head)
    {
        return Err(StatusError::Message(
            "The checks did not match the pull request head; retry later".into(),
        ));
    }
    let upper = |value: &serde_json::Value| {
        value.as_str().map_or(serde_json::Value::Null, |text| {
            text.to_ascii_uppercase().into()
        })
    };
    let checks = runs
        .iter()
        .map(|run| {
            serde_json::json!({"__typename":"CheckRun","name":run["name"],
                "status":upper(&run["status"]),"conclusion":upper(&run["conclusion"]),
                "detailsUrl":run["details_url"]})
        })
        .chain(statuses.iter().map(|context| {
            serde_json::json!({"__typename":"StatusContext","context":context["context"],
                "state":upper(&context["state"]),"targetUrl":context["target_url"],
                "description":context["description"]})
        }))
        .collect::<Vec<_>>();
    let counted = suites
        .iter()
        .filter(|suite| suite_counts(suite))
        .collect::<Vec<_>>();
    let unfinished = counted.iter().any(|suite| suite["status"] != "completed");
    // A suite with check runs is judged by them: the list holds each check's
    // latest run, so a cancelled suite a rerun superseded does not fail CI.
    // A workflow that failed before creating any job shows only in its suite.
    let suite_failed = counted.iter().any(|suite| {
        suite["status"] == "completed"
            && suite["latest_check_runs_count"].as_u64() == Some(0)
            && matches!(
                suite["conclusion"].as_str(),
                Some("failure" | "timed_out" | "action_required" | "startup_failure")
            )
    });
    let complete = runs_complete && suites_complete && statuses_complete;
    let state = if suite_failed || checks.iter().any(check_failed) {
        Some("FAILURE")
    } else if complete && checks.is_empty() && !unfinished {
        None
    } else if complete && !unfinished && !checks.is_empty() && checks.iter().all(check_passed) {
        Some("SUCCESS")
    } else {
        Some("PENDING")
    };
    Ok((state.map(str::to_owned), checks, complete))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github_identity::GitHubRepoKey,
        process::{ProcessFailure, ProcessOutput},
    };
    use std::{collections::VecDeque, sync::Mutex};
    use std::{
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn private_tempdir() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn persisted_cooldowns_preserve_resource_and_credential_boundaries() {
        let root = private_tempdir();
        let cancellation = CancellationToken::new();
        let due = now() + 3600;
        record_cooldown(
            root.path(),
            "first",
            "example.invalid",
            &RateLimit::primary("graphql", due),
            &cancellation,
        )
        .unwrap();
        let graphql = shared_cooldown(
            root.path(),
            "first",
            "example.invalid",
            "graphql",
            &cancellation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(graphql.resource.as_deref(), Some("graphql"));
        assert_eq!(graphql.retry_at, due);
        assert!(
            shared_cooldown(
                root.path(),
                "first",
                "example.invalid",
                "core",
                &cancellation
            )
            .unwrap()
            .is_none()
        );
        assert!(
            shared_cooldown(
                root.path(),
                "second",
                "example.invalid",
                "graphql",
                &cancellation
            )
            .unwrap()
            .is_none()
        );
        record_cooldown(
            root.path(),
            "first",
            "example.invalid",
            &RateLimit::secondary(now() + 60),
            &cancellation,
        )
        .unwrap();
        let core = shared_cooldown(
            root.path(),
            "first",
            "example.invalid",
            "core",
            &cancellation,
        )
        .unwrap()
        .unwrap();
        assert!(core.resource.is_none());
        assert!(core.retry_at < due);
        let graphql = shared_cooldown(
            root.path(),
            "first",
            "example.invalid",
            "graphql",
            &cancellation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(graphql.resource.as_deref(), Some("graphql"));
        assert_eq!(graphql.retry_at, due);
        assert_eq!(graphql.secondary_retry_at, Some(core.retry_at));
    }

    #[test]
    fn headerless_graphql_failure_persists_only_a_resource_local_host_v2_pause() {
        let root = private_tempdir();
        let fixture = Fixture::new(vec![ProcessOutput {
            success: false,
            exit_code: Some(1),
            signal: None,
            stdout: vec![],
            stderr: b"GraphQL: API rate limit already exceeded\n".to_vec(),
        }]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        let before = now();
        let first = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(first.outcome, "failed");
        assert!(first.stale);
        let (_, path) = host_paths(root.path(), "account", &key().repository.authority());
        let budget = load::<HostBudget>(&path).unwrap().unwrap();
        assert_eq!(budget.secondary, 0);
        assert_eq!(budget.primary.len(), 1);
        assert!((before + 60..=now() + 60).contains(&budget.primary["graphql"]));
        for resource in ["core", "search"] {
            assert!(
                shared_cooldown(
                    root.path(),
                    "account",
                    &key().repository.authority(),
                    resource,
                    &cancellation
                )
                .unwrap()
                .is_none()
            );
        }
        let graphql = shared_cooldown(
            root.path(),
            "account",
            &key().repository.authority(),
            "graphql",
            &cancellation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(graphql.resource.as_deref(), Some("graphql"));
        assert_eq!(graphql.secondary_retry_at, None);
        // The pause is GraphQL's own: the PR keeps only the shared interval,
        // and another PR reads through REST rather than waiting it out.
        assert!(first.retry_at.unwrap() <= now() + MAX_AGE);
        let mut another = key();
        another.number = 43;
        fixture.results.lock().unwrap().extend(rest_read(
            43,
            vec![rest_run("test", "completed", Some("success"))],
            vec![],
            vec![],
        ));
        let repeated = read_fixture(root.path(), "account", &another, &service, &cancellation);
        assert_eq!(repeated.source, "network");
        assert_eq!(repeated.outcome, "succeeded");
        let snapshot = repeated.snapshot.unwrap();
        assert_eq!(snapshot.transport, Transport::Rest);
        assert_eq!(snapshot.checks_state.as_deref(), Some("SUCCESS"));
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn legacy_unscoped_cooldowns_do_not_become_secondary_pauses() {
        let root = private_tempdir();
        let name = digest(&["host-v1", "account", "example.invalid"]);
        store(
            &root.path().join(format!("{name}.cooldown")),
            &(now() + 3600),
        )
        .unwrap();
        for resource in ["graphql", "core", "search"] {
            assert!(
                shared_cooldown(
                    root.path(),
                    "account",
                    "example.invalid",
                    resource,
                    &CancellationToken::new()
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn expired_secondary_pause_does_not_inherit_another_resources_primary_reset() {
        let budget = HostBudget {
            primary: std::collections::BTreeMap::from([("graphql".into(), 3600)]),
            secondary: 120,
        };
        assert_eq!(budget.limit("core", 110), Some(RateLimit::secondary(120)));
        assert!(budget.limit("core", 121).is_none());
        let graphql = budget.limit("graphql", 121).unwrap();
        assert_eq!(graphql.resource.as_deref(), Some("graphql"));
        assert_eq!(graphql.secondary_retry_at, None);
    }

    fn key() -> PullRequestKey {
        PullRequestKey::new(
            GitHubRepoKey::new("status-test.invalid", None, "Owner", "Repo").unwrap(),
            42,
        )
        .unwrap()
    }
    fn data() -> serde_json::Value {
        serde_json::json!({"rateLimit":{"cost":1},"repository":{"pullRequest":{
            "number":42,"title":"A pull request","url":"https://status-test.invalid/Owner/Repo/pull/42",
            "state":"OPEN","isDraft":false,"headRefName":"feature","headRefOid":"a".repeat(40),"baseRefName":"main",
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":null,"updatedAt":"2026-11-09T00:00:00Z",
            "commits":{"nodes":[{"commit":{"oid":"a".repeat(40),"statusCheckRollup":null}}]}
        }}})
    }
    fn response(data: serde_json::Value) -> ProcessOutput {
        ProcessOutput {
            success: true,
            exit_code: Some(0),
            signal: None,
            stdout: format!(
                "HTTP/2.0 200 OK\r\nX-RateLimit-Remaining: 4000\r\n\r\n{}",
                serde_json::json!({"data":data})
            )
            .into_bytes(),
            stderr: Vec::new(),
        }
    }
    struct Fixture {
        calls: AtomicUsize,
        results: Mutex<VecDeque<ProcessOutput>>,
        /// "graphql", or the REST endpoint, of each read in order.
        endpoints: Mutex<Vec<String>>,
    }
    impl Fixture {
        fn new(results: Vec<ProcessOutput>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                results: Mutex::new(results.into()),
                endpoints: Mutex::new(Vec::new()),
            })
        }
        fn endpoints(&self) -> Vec<String> {
            self.endpoints.lock().unwrap().clone()
        }
    }
    impl ProcessExecutor for Fixture {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let arguments = command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let endpoint = if arguments[1] == "graphql" {
                assert!(
                    arguments
                        .iter()
                        .any(|argument| argument.contains("query CagentsPrStatus"))
                );
                "graphql".to_owned()
            } else {
                assert_eq!(arguments[..3], ["api", "--method", "GET"]);
                arguments.last().unwrap().clone()
            };
            self.endpoints.lock().unwrap().push(endpoint);
            assert!(
                command
                    .arguments
                    .iter()
                    .any(|argument| argument == "--include")
            );
            assert_eq!(command.timeout, Some(Duration::from_secs(30)));
            assert_eq!(
                command
                    .environment
                    .get(std::ffi::OsStr::new("CAGENTS_GH_NONBLOCKING")),
                Some(&std::ffi::OsString::from("1")),
            );
            Ok(self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected network read"))
        }
    }
    fn read_fixture(
        root: &Path,
        credentials: &str,
        key: &PullRequestKey,
        service: &StatusService<Fixture>,
        cancellation: &CancellationToken,
    ) -> Report {
        read_with(root, credentials, key, service, cancellation, || Ok(())).unwrap()
    }

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn rest(status: &str, headers: &str, body: serde_json::Value) -> ProcessOutput {
        ProcessOutput {
            success: status.starts_with('2'),
            exit_code: Some(if status.starts_with('2') { 0 } else { 1 }),
            signal: None,
            stdout: format!(
                "HTTP/2.0 {status}\r\nX-RateLimit-Resource: core\r\n{headers}\r\n{body}"
            )
            .into_bytes(),
            stderr: Vec::new(),
        }
    }
    fn rest_ok(body: serde_json::Value) -> ProcessOutput {
        rest("200 OK", "X-RateLimit-Remaining: 4000\r\n", body)
    }
    fn rest_pull(number: u64) -> serde_json::Value {
        serde_json::json!({"number":number,"title":"A pull request",
            "html_url":format!("https://status-test.invalid/Owner/Repo/pull/{number}"),
            "state":"open","merged":false,"draft":false,"head":{"ref":"feature","sha":HEAD},
            "base":{"ref":"main"},"mergeable":true,"mergeable_state":"clean",
            "updated_at":"2026-11-09T00:00:00Z"})
    }
    fn rest_run(name: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
        serde_json::json!({"name":name,"head_sha":HEAD,"status":status,"conclusion":conclusion,
            "details_url":"https://status-test.invalid/Owner/Repo/actions/runs/1/job/2"})
    }
    fn rest_suite(
        app: &str,
        status: &str,
        conclusion: Option<&str>,
        runs: u64,
    ) -> serde_json::Value {
        serde_json::json!({"head_sha":HEAD,"app":{"slug":app},"status":status,
            "conclusion":conclusion,"latest_check_runs_count":runs})
    }
    fn rest_context(context: &str, state: &str) -> serde_json::Value {
        serde_json::json!({"context":context,"state":state,
            "target_url":"https://status-test.invalid/deploy","description":"Deploy"})
    }
    fn runs_page(runs: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"total_count":runs.len(),"check_runs":runs})
    }
    fn suites_page(suites: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"total_count":suites.len(),"check_suites":suites})
    }
    fn status_page(statuses: Vec<serde_json::Value>) -> serde_json::Value {
        // GitHub reports "pending" for a commit with no statuses at all.
        serde_json::json!({"sha":HEAD,"state":"pending","total_count":statuses.len(),"statuses":statuses})
    }
    /// The four responses of one REST read. With no suites given, the head
    /// has one finished GitHub Actions suite.
    fn rest_read(
        number: u64,
        runs: Vec<serde_json::Value>,
        suites: Vec<serde_json::Value>,
        statuses: Vec<serde_json::Value>,
    ) -> Vec<ProcessOutput> {
        let suites = if suites.is_empty() {
            vec![rest_suite(
                "github-actions",
                "completed",
                Some("success"),
                runs.len() as u64,
            )]
        } else {
            suites
        };
        vec![
            rest_ok(rest_pull(number)),
            rest_ok(runs_page(runs)),
            rest_ok(suites_page(suites)),
            rest_ok(status_page(statuses)),
        ]
    }
    fn exhaust_graphql(root: &Path, credentials: &str) -> u64 {
        let reset = now() + 3600;
        record_cooldown(
            root,
            credentials,
            &key().repository.authority(),
            &RateLimit::primary("graphql", reset),
            &CancellationToken::new(),
        )
        .unwrap();
        reset
    }

    #[test]
    fn exhausted_graphql_reads_ci_through_rest_and_shares_that_read() {
        let root = private_tempdir();
        exhaust_graphql(root.path(), "account");
        let runs = (0..11)
            .map(|index| rest_run(&format!("check {index}"), "completed", Some("success")))
            .collect();
        // What #2918's head had: the Actions suite, and empty suites GitHub
        // keeps queued for installed apps that never run.
        let suites = vec![
            rest_suite("github-actions", "completed", Some("success"), 11),
            rest_suite("netlify", "queued", None, 0),
            rest_suite("vercel", "queued", None, 0),
        ];
        let fixture = Fixture::new(rest_read(42, runs, suites, vec![]));
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        let report = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(report.source, "network");
        assert_eq!(report.outcome, "succeeded");
        assert!(!report.stale);
        assert_eq!(report.retry_at, Some(report.fetched_at.unwrap() + MAX_AGE));
        assert_eq!(
            report.limitations,
            [
                "Read through REST while GitHub GraphQL is rate limited; the review decision is unknown."
            ]
        );
        let snapshot = report.snapshot.unwrap();
        assert_eq!(snapshot.transport, Transport::Rest);
        assert_eq!(snapshot.checks_state.as_deref(), Some("SUCCESS"));
        assert!(snapshot.checks_complete);
        // Every REST run reads as a passing check, so the report counts them all.
        assert_eq!(snapshot.checks_passed, 11);
        assert!(snapshot.checks.is_empty());
        let pr = &snapshot.pull_request;
        assert_eq!(
            (pr.state.as_str(), pr.head_ref_oid.as_str()),
            ("OPEN", HEAD)
        );
        assert_eq!(pr.mergeable, "MERGEABLE");
        assert_eq!(pr.merge_state_status.as_deref(), Some("CLEAN"));
        assert_eq!(pr.review_decision, None);
        assert_eq!(snapshot.primary_cost, None);
        assert_eq!(
            fixture.endpoints(),
            [
                "repos/Owner/Repo/pulls/42".to_owned(),
                format!("repos/Owner/Repo/commits/{HEAD}/check-runs?per_page=100"),
                format!("repos/Owner/Repo/commits/{HEAD}/check-suites?per_page=100"),
                format!("repos/Owner/Repo/commits/{HEAD}/status?per_page=100"),
            ]
        );
        // The REST read is this interval's shared read, like a GraphQL one.
        let again = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(again.source, "cache");
        assert_eq!(again.snapshot.unwrap().transport, Transport::Rest);
        assert!(!again.limitations.is_empty());
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 4);
        // Another credential context has its own GraphQL budget.
        fixture.results.lock().unwrap().push_back(response(data()));
        let other = read_fixture(root.path(), "other", &key(), &service, &cancellation);
        assert_eq!(other.snapshot.unwrap().transport, Transport::Graphql);
        assert_eq!(fixture.endpoints().last().unwrap(), "graphql");
    }

    #[test]
    fn a_rest_rate_limit_ends_the_read_and_pauses_only_rest() {
        let root = private_tempdir();
        let graphql_reset = exhaust_graphql(root.path(), "account");
        let core_reset = now() + 1800;
        let fixture = Fixture::new(vec![rest(
            "200 OK",
            &format!("X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {core_reset}\r\n"),
            rest_pull(42),
        )]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        let first = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        // The PR read spent the quota, so no check read follows it.
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
        assert_eq!(first.outcome, "failed");
        assert!(first.snapshot.is_none());
        let core = shared_cooldown(
            root.path(),
            "account",
            &key().repository.authority(),
            "core",
            &cancellation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (core.resource.as_deref(), core.retry_at),
            (Some("core"), core_reset)
        );
        // With both resources paused, a read waits for the earlier reset.
        let mut another = key();
        another.number = 43;
        let paused = read_fixture(root.path(), "account", &another, &service, &cancellation);
        assert_eq!(paused.source, "cache");
        assert_eq!(paused.retry_at, Some(core_reset.min(graphql_reset)));
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_failed_check_read_publishes_no_partial_snapshot_and_honors_retry_after() {
        let root = private_tempdir();
        exhaust_graphql(root.path(), "account");
        let fixture = Fixture::new(vec![
            rest_ok(rest_pull(42)),
            rest(
                "403 Forbidden",
                "Retry-After: 300\r\n",
                serde_json::json!({"message":"You have exceeded a secondary rate limit"}),
            ),
        ]);
        let service = StatusService::new("gh", fixture.clone());
        let before = now();
        let report = read_fixture(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
        assert_eq!(report.outcome, "failed");
        assert!(
            report.snapshot.is_none(),
            "a PR read alone is not published"
        );
        assert!(report.retry_at.unwrap() >= before + 300);
        // A secondary pause covers GraphQL as well.
        let graphql = shared_cooldown(
            root.path(),
            "account",
            &key().repository.authority(),
            "graphql",
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert!(graphql.secondary_retry_at.unwrap() >= before + 300);
    }

    #[test]
    fn an_earlier_clis_graphql_reset_gives_way_to_rest_after_the_shared_interval() {
        let root = private_tempdir();
        let reset = exhaust_graphql(root.path(), "account");
        let path = root
            .path()
            .join(format!("{}.json", target_name("account", &key())));
        // What a CLI before REST reads saved after GraphQL ran out.
        let publish = |age: u64| {
            store(
                &path,
                &Cached {
                    fetched_at: None,
                    snapshot: None,
                    error: Some("GitHub rate limit reached".into()),
                    next_attempt: reset,
                },
            )
            .unwrap();
            let at = std::time::UNIX_EPOCH + Duration::from_secs(now() - age);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        };
        let fixture = Fixture::new(rest_read(
            42,
            vec![rest_run("test", "completed", Some("success"))],
            vec![],
            vec![],
        ));
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        publish(30);
        let waiting = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(waiting.source, "cache");
        assert_eq!(
            fixture.calls.load(Ordering::Relaxed),
            0,
            "the interval is still shared"
        );
        publish(MAX_AGE + 1);
        let read = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(read.source, "network");
        assert_eq!(read.snapshot.unwrap().transport, Transport::Rest);
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn the_bootstrap_scripts_own_rest_pause_is_honored() {
        let root = private_tempdir();
        let reset = exhaust_graphql(root.path(), "account");
        let due = now() + 600;
        let name = target_name("account", &key());
        store(
            &root.path().join(format!("{name}.rest-retry")),
            &serde_json::json!({"retry_at": due}),
        )
        .unwrap();
        let fixture = Fixture::new(Vec::new());
        let service = StatusService::new("gh", fixture.clone());
        let report = read_fixture(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
        );
        assert_eq!(report.source, "cache");
        assert_eq!(report.retry_at, Some(due.min(reset)));
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rest_ci_never_passes_an_incomplete_or_not_yet_created_list() {
        let pass = || rest_run("test", "completed", Some("success"));
        let actions =
            |status, conclusion, runs| rest_suite("github-actions", status, conclusion, runs);
        let finished = || vec![actions("completed", Some("success"), 1)];
        let rollup = |runs: serde_json::Value,
                      suites: Vec<serde_json::Value>,
                      statuses: serde_json::Value| {
            let (state, _, complete) =
                rest_rollup(&runs, &suites_page(suites), &statuses, HEAD).unwrap();
            (state, complete)
        };
        let success = (Some("SUCCESS".to_owned()), true);
        let pending = |complete| (Some("PENDING".to_owned()), complete);
        assert_eq!(
            rollup(runs_page(vec![pass()]), finished(), status_page(vec![])),
            success
        );
        // A second page of check runs is unread.
        let mut paged = runs_page((0..100).map(|_| pass()).collect());
        paged["total_count"] = 101.into();
        assert_eq!(
            rollup(paged, finished(), status_page(vec![])),
            pending(false)
        );
        // So is a second page of check suites or of statuses.
        let mut suites = suites_page(finished());
        suites["total_count"] = 2.into();
        assert_eq!(
            rest_rollup(
                &runs_page(vec![pass()]),
                &suites,
                &status_page(vec![]),
                HEAD
            )
            .unwrap()
            .0
            .as_deref(),
            Some("PENDING")
        );
        let mut statuses = status_page(vec![rest_context("deploy", "success")]);
        statuses["total_count"] = 2.into();
        assert_eq!(
            rollup(runs_page(vec![pass()]), finished(), statuses),
            pending(false)
        );
        // The workflow run exists but its jobs' check runs do not, yet.
        assert_eq!(
            rollup(
                runs_page(vec![]),
                vec![actions("queued", None, 0)],
                status_page(vec![])
            ),
            pending(true)
        );
        // One workflow passed while another has not created its runs.
        assert_eq!(
            rollup(
                runs_page(vec![pass()]),
                vec![
                    actions("completed", Some("success"), 1),
                    actions("queued", None, 0)
                ],
                status_page(vec![]),
            ),
            pending(true)
        );
        // Every listed run passed, but the workflow run is still going.
        assert_eq!(
            rollup(
                runs_page(vec![pass()]),
                vec![actions("in_progress", None, 1)],
                status_page(vec![])
            ),
            pending(true)
        );
        // Nothing reported at all, apart from apps that never run, is no
        // rollup, as GraphQL reports it: land-pr waits for checks.
        assert_eq!(
            rollup(
                runs_page(vec![]),
                vec![rest_suite("netlify", "queued", None, 0)],
                status_page(vec![]),
            ),
            (None, true)
        );
        // Unfinished, stale and pending contexts do not pass.
        for run in [
            rest_run("test", "in_progress", None),
            rest_run("test", "completed", None),
            rest_run("test", "completed", Some("stale")),
        ] {
            assert_eq!(
                rollup(runs_page(vec![run]), finished(), status_page(vec![])),
                pending(true)
            );
        }
        assert_eq!(
            rollup(
                runs_page(vec![pass()]),
                finished(),
                status_page(vec![rest_context("deploy", "pending")])
            ),
            pending(true)
        );
        // A failure is final even on an incomplete list.
        let mut failing = runs_page(vec![rest_run("test", "completed", Some("failure"))]);
        failing["total_count"] = 200.into();
        assert_eq!(
            rollup(failing, finished(), status_page(vec![])),
            (Some("FAILURE".to_owned()), false)
        );
        assert_eq!(
            rollup(
                runs_page(vec![pass()]),
                finished(),
                status_page(vec![rest_context("deploy", "error")])
            )
            .0,
            Some("FAILURE".to_owned())
        );
        // A run that was cancelled, then rerun, gives way to its rerun, and
        // a workflow cancelled before it created any job does not fail CI.
        assert_eq!(
            rollup(
                runs_page(vec![pass()]),
                vec![
                    actions("completed", Some("cancelled"), 1),
                    actions("completed", Some("success"), 1),
                    actions("completed", Some("cancelled"), 0),
                ],
                status_page(vec![]),
            ),
            success
        );
        // A workflow that failed before creating any job.
        assert_eq!(
            rollup(
                runs_page(vec![]),
                vec![actions("completed", Some("failure"), 0)],
                status_page(vec![])
            )
            .0,
            Some("FAILURE".to_owned())
        );
        // Checks for another commit are refused, not counted.
        let mut other = rest_run("test", "completed", Some("success"));
        other["head_sha"] = "b".repeat(40).into();
        assert!(
            rest_rollup(
                &runs_page(vec![other]),
                &suites_page(finished()),
                &status_page(vec![]),
                HEAD
            )
            .is_err()
        );
        let mut status = status_page(vec![]);
        status["sha"] = "b".repeat(40).into();
        assert!(
            rest_rollup(
                &runs_page(vec![pass()]),
                &suites_page(finished()),
                &status,
                HEAD
            )
            .is_err()
        );
        assert!(
            rest_rollup(
                &serde_json::json!({}),
                &suites_page(finished()),
                &status_page(vec![]),
                HEAD
            )
            .is_err()
        );
    }

    #[test]
    fn rest_pull_requests_report_merge_state_and_refuse_another_target() {
        let mut pull = rest_pull(42);
        pull["mergeable"] = serde_json::Value::Null;
        pull["mergeable_state"] = "unknown".into();
        let status = rest_pull_request(&pull, &key()).unwrap();
        assert_eq!(status.mergeable, "UNKNOWN");
        pull["mergeable"] = false.into();
        pull["mergeable_state"] = "dirty".into();
        pull["merged"] = true.into();
        pull["state"] = "closed".into();
        let status = rest_pull_request(&pull, &key()).unwrap();
        assert_eq!(
            (
                status.mergeable.as_str(),
                status.merge_state_status.as_deref(),
                status.state.as_str()
            ),
            ("CONFLICTING", Some("DIRTY"), "MERGED")
        );
        pull["html_url"] = "https://status-test.invalid/Other/Repo/pull/42".into();
        assert!(rest_pull_request(&pull, &key()).is_err());
        // A malformed head never becomes part of a request path.
        let root = private_tempdir();
        exhaust_graphql(root.path(), "account");
        let mut pull = rest_pull(42);
        pull["head"]["sha"] = "../../pulls/1".into();
        let fixture = Fixture::new(vec![rest_ok(pull)]);
        let service = StatusService::new("gh", fixture.clone());
        let report = read_fixture(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
        );
        assert_eq!(report.outcome, "failed");
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_callers_share_one_read_and_empty_checks_are_explicit() {
        let root = private_tempdir();
        let fixture = Fixture::new(vec![response(data())]);
        let service = StatusService::new("gh", fixture.clone());
        let barrier = Barrier::new(12);
        thread::scope(|scope| {
            let workers = (0..12)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        read_fixture(
                            root.path(),
                            "one-account",
                            &key(),
                            &service,
                            &CancellationToken::new(),
                        )
                    })
                })
                .collect::<Vec<_>>();
            let reports = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                reports
                    .iter()
                    .filter(|report| report.source == "network")
                    .count(),
                1
            );
            for report in reports {
                assert!(!report.stale);
                assert_eq!(report.outcome, "succeeded");
                let snapshot = report.snapshot.unwrap();
                assert_eq!(snapshot.primary_cost, Some(1));
                assert!(snapshot.checks_state.is_none());
                assert!(snapshot.checks.is_empty());
                assert!(snapshot.checks_complete);
            }
        });
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn case_variants_share_but_credential_contexts_do_not() {
        let root = private_tempdir();
        let fixture = Fixture::new(vec![response(data()), response(data())]);
        let service = StatusService::new("gh", fixture.clone());
        let mut upper = key();
        upper.repository.owner.make_ascii_uppercase();
        upper.repository.name.make_ascii_uppercase();
        assert_eq!(
            read_fixture(
                root.path(),
                "account-a",
                &key(),
                &service,
                &CancellationToken::new()
            )
            .source,
            "network"
        );
        assert_eq!(
            read_fixture(
                root.path(),
                "account-a",
                &upper,
                &service,
                &CancellationToken::new()
            )
            .source,
            "cache"
        );
        assert_eq!(
            read_fixture(
                root.path(),
                "account-b",
                &upper,
                &service,
                &CancellationToken::new()
            )
            .source,
            "network"
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
        assert_ne!(
            target_name("credential-a", &key()),
            target_name("credential-b", &key())
        );
    }

    #[test]
    fn failed_refresh_retains_old_snapshot_as_stale_and_paces_other_targets() {
        let root = private_tempdir();
        let mut target = key();
        target.repository.host = "status-backoff-test.invalid".into();
        let mut value = data();
        value["repository"]["pullRequest"]["url"] =
            serde_json::json!("https://status-backoff-test.invalid/Owner/Repo/pull/42");
        let failure = ProcessOutput {success:false,exit_code:Some(1),signal:None,
            stdout:b"HTTP/2.0 403 Forbidden\r\nRetry-After: 300\r\nX-RateLimit-Remaining: 4000\r\n\r\n{\"message\":\"secondary rate limit\"}".to_vec(),stderr:Vec::new()};
        let fixture = Fixture::new(vec![response(value), failure]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        read_fixture(root.path(), "account", &target, &service, &cancellation);
        let path = root
            .path()
            .join(format!("{}.json", target_name("account", &target)));
        let mut cached = load::<Cached>(&path).unwrap().unwrap();
        let fetched = now().saturating_sub(MAX_AGE + 1);
        cached.fetched_at = Some(fetched);
        cached.next_attempt = 0;
        store(&path, &cached).unwrap();
        let report = read_fixture(root.path(), "account", &target, &service, &cancellation);
        assert!(report.stale);
        assert_eq!(report.outcome, "failed");
        assert_eq!(report.fetched_at, Some(fetched));
        assert!(report.snapshot.is_some());
        assert!(report.retry_at.unwrap() >= now() + 299);
        assert_eq!(
            read_fixture(root.path(), "account", &target, &service, &cancellation).source,
            "cache"
        );
        let mut another = target.clone();
        another.number = 43;
        let paused = read_fixture(root.path(), "account", &another, &service, &cancellation);
        assert_eq!(paused.source, "cache");
        assert!(paused.stale);
        assert!(paused.snapshot.is_none());
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_shared_rest_bootstrap_snapshot_is_not_reported_as_a_ci_answer() {
        let root = private_tempdir();
        let fixture = Fixture::new(Vec::new());
        let service = StatusService::new("gh", fixture.clone());
        let mut snapshot = parse(&data(), &key()).unwrap();
        // The shape the bootstrap script's REST read stores for this PR.
        snapshot.checks_complete = false;
        snapshot.primary_cost = None;
        snapshot.pull_request.mergeable = "UNKNOWN".into();
        let path = root
            .path()
            .join(format!("{}.json", target_name("account", &key())));
        store(
            &path,
            &Cached {
                fetched_at: Some(now()),
                snapshot: Some(snapshot),
                error: None,
                next_attempt: now() + MAX_AGE,
            },
        )
        .unwrap();
        let report = read_fixture(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
        );
        assert_eq!(report.source, "rest-bootstrap");
        assert!(report.snapshot.unwrap().checks_state.is_none());
        // It still counts as this interval's shared read.
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reports_count_passing_checks_and_list_the_rest_with_links() {
        let mut snapshot = parse(&data(), &key()).unwrap();
        let link = "https://status-test.invalid/Owner/Repo/actions/runs/1/job/2";
        let run = |name: &str, status: &str, conclusion: Option<&str>| {
            serde_json::json!({"__typename":"CheckRun","name":name,"status":status,"conclusion":conclusion,
                "detailsUrl":link,"startedAt":"2026-11-09T00:00:00Z","completedAt":"2026-11-09T00:01:00Z"})
        };
        snapshot.checks = vec![
            run("passed", "COMPLETED", Some("SUCCESS")),
            run("skipped", "COMPLETED", Some("SKIPPED")),
            run("failed", "COMPLETED", Some("FAILURE")),
            run("running", "IN_PROGRESS", None),
            serde_json::json!({"__typename":"StatusContext","context":"deploy","state":"SUCCESS",
                "targetUrl":link,"description":"Deployed"}),
            serde_json::json!({"__typename":"StatusContext","context":"lint","state":"ERROR",
                "targetUrl":link,"description":"Lint crashed"}),
        ];
        let report = Cached {
            fetched_at: Some(now()),
            snapshot: Some(snapshot),
            error: None,
            next_attempt: 0,
        }
        .report(&key(), "cache", now());
        let snapshot = report.snapshot.unwrap();
        assert_eq!(snapshot.checks_passed, 3);
        let shown = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(shown["checks_passed"], 3);
        assert_eq!(shown["checks"].as_array().map(Vec::len), Some(3));
        assert_eq!(
            shown["pull_request"]["headRefOid"],
            snapshot.pull_request.head_ref_oid
        );
        assert_eq!(
            snapshot.checks,
            vec![
                serde_json::json!({"__typename":"CheckRun","name":"failed","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":link}),
                serde_json::json!({"__typename":"CheckRun","name":"running","status":"IN_PROGRESS","conclusion":null,"detailsUrl":link}),
                serde_json::json!({"__typename":"StatusContext","context":"lint","state":"ERROR","targetUrl":link,"description":"Lint crashed"}),
            ]
        );
        assert!(!QUERY.contains("startedAt") && !QUERY.contains("completedAt"));
    }

    #[test]
    fn rollups_bind_to_the_head_and_report_truncated_details() {
        let mut value = data();
        value["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = serde_json::json!({"state":"FAILURE","contexts":{"nodes":[{"__typename":"CheckRun","name":"test","conclusion":"SUCCESS"}],"pageInfo":{"hasNextPage":true,"endCursor":"more"}}});
        let snapshot = parse(&value, &key()).unwrap();
        assert_eq!(snapshot.checks_state.as_deref(), Some("FAILURE"));
        assert!(!snapshot.checks_complete);
        value["repository"]["pullRequest"]["headRefOid"] = serde_json::json!("b".repeat(40));
        assert!(parse(&value, &key()).is_err());
        value["repository"]["pullRequest"]["url"] =
            serde_json::json!("https://status-test.invalid/other/repo/pull/42");
        assert!(parse(&value, &key()).is_err());
        assert!(classify_failure(
            br#"{"data":{"repository":{"pullRequest":{"title":"Fix API rate limit exceeded"}}},"errors":[{"message":"Forbidden","type":"FORBIDDEN"}]}"#,
            b"GraphQL: Forbidden",
            "graphql",
        ).is_none());
    }

    #[test]
    fn an_old_accounts_rate_limit_does_not_block_the_new_account() {
        let root = private_tempdir();
        let mut target = key();
        target.repository.host = "status-account-switch.invalid".into();
        let mut value = data();
        value["repository"]["pullRequest"]["url"] =
            serde_json::json!("https://status-account-switch.invalid/Owner/Repo/pull/42");
        let failure = ProcessOutput {
            success: false, exit_code: Some(1), signal: None,
            stdout: b"HTTP/2.0 403 Forbidden\r\nRetry-After: 300\r\n\r\n{\"message\":\"secondary rate limit\"}".to_vec(), stderr: Vec::new(),
        };
        let fixture = Fixture::new(vec![failure, response(value)]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        assert_eq!(
            read_fixture(root.path(), "old-account", &target, &service, &cancellation).outcome,
            "failed"
        );
        assert_eq!(
            read_fixture(root.path(), "new-account", &target, &service, &cancellation).outcome,
            "succeeded"
        );
        assert_eq!(
            read_fixture(root.path(), "old-account", &target, &service, &cancellation).outcome,
            "failed"
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_last_successful_quota_point_does_not_keep_a_snapshot_fresh_until_reset() {
        let root = private_tempdir();
        let reset = now() + 3600;
        let mut last = response(data());
        last.stdout = format!(
            "HTTP/2.0 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{}",
            serde_json::json!({"data":data()})
        )
        .into_bytes();
        let fixture = Fixture::new(vec![last]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        let fresh = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert!(!fresh.stale);
        assert_eq!(fresh.outcome, "succeeded");
        let path = root
            .path()
            .join(format!("{}.json", target_name("account", &key())));
        let mut cached = load::<Cached>(&path).unwrap().unwrap();
        cached.fetched_at = Some(now().saturating_sub(MAX_AGE + 1));
        cached.next_attempt = 0;
        store(&path, &cached).unwrap();
        // GraphQL waits for its reset; the refresh reads REST instead.
        fixture.results.lock().unwrap().extend(rest_read(
            42,
            vec![rest_run("test", "completed", Some("success"))],
            vec![],
            vec![],
        ));
        let refreshed = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert!(!refreshed.stale);
        assert_eq!(refreshed.outcome, "succeeded");
        assert_eq!(refreshed.snapshot.unwrap().transport, Transport::Rest);
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 5);
        assert_eq!(
            shared_cooldown(
                root.path(),
                "account",
                &key().repository.authority(),
                "graphql",
                &cancellation
            )
            .unwrap()
            .unwrap()
            .retry_at,
            reset
        );
    }

    #[test]
    fn shim_deferral_is_a_shared_cooldown_without_retrying() {
        let root = private_tempdir();
        let fixture = Fixture::new(vec![ProcessOutput {
            success: false,
            exit_code: Some(75),
            signal: None,
            stdout: Vec::new(),
            stderr: b"cagents-gh: deferred shared GitHub cooldown\nRetry-After: 300\n".to_vec(),
        }]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        let before = now();
        let first = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(first.outcome, "failed");
        assert!(first.stale);
        assert!(first.snapshot.is_none());
        assert!(first.retry_at.unwrap() >= before + 300);
        let cached = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert_eq!(cached.source, "cache");
        assert_eq!(cached.retry_at, first.retry_at);
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancellation_and_changed_credentials_release_without_publishing() {
        let root = private_tempdir();
        let fixture = Fixture::new(vec![response(data()), response(data())]);
        let service = StatusService::new("gh", fixture.clone());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            read_with(
                root.path(),
                "account",
                &key(),
                &service,
                &cancellation,
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 0);
        assert!(
            read_with(
                root.path(),
                "account",
                &key(),
                &service,
                &CancellationToken::new(),
                || Err("credentials changed".into())
            )
            .is_err()
        );
        assert_eq!(
            read_fixture(
                root.path(),
                "account",
                &key(),
                &service,
                &CancellationToken::new()
            )
            .source,
            "network"
        );
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn credential_fingerprints_change_with_tokens_accounts_and_configuration() {
        const PROBE: &str = "CAGENTS_PR_STATUS_TEST_AUTH_PROBE";
        if std::env::var_os(PROBE).is_some() {
            println!("fingerprint:{}", credential_fingerprint().unwrap());
            return;
        }
        let root = private_tempdir();
        let hosts = root.path().join("hosts.yml");
        fs::write(&hosts, "github.com:\n    user: first-account\n").unwrap();
        let probe = |token: &str, config: &Path| {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "github_status::tests::credential_fingerprints_change_with_tokens_accounts_and_configuration", "--nocapture"])
                .env(PROBE, "1")
                .env("GH_CONFIG_DIR", config)
                .env("GH_TOKEN", token)
                .output().unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("fingerprint:").map(str::to_owned))
                .unwrap()
        };
        let first = probe("fixture-token-a", root.path());
        assert_ne!(first, probe("fixture-token-b", root.path()));
        assert_eq!(first, probe("fixture-token-a", root.path()));
        fs::write(&hosts, "github.com:\n    user: second-account\n").unwrap();
        assert_ne!(first, probe("fixture-token-a", root.path()));
        assert_ne!(
            first,
            probe("fixture-token-a", &root.path().join("other-config"))
        );
    }

    #[test]
    fn separate_processes_share_one_actual_gh_child() {
        const CHILD_ROOT: &str = "CAGENTS_PR_STATUS_TEST_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let service = StatusService::system(root.join("gh"));
            let report = read_with(
                &root,
                "account",
                &key(),
                &service,
                &CancellationToken::new(),
                || Ok(()),
            )
            .unwrap();
            assert_eq!(report.outcome, "succeeded");
            return;
        }
        let root = private_tempdir();
        let script = root.path().join("gh");
        let count = root.path().join("count");
        let count_quoted = format!("'{}'", count.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(&script, format!("#!/bin/sh\nprintf x >> {count_quoted}\nsleep 0.1\ncat <<'RESPONSE'\n{}\nRESPONSE\n", String::from_utf8(response(data()).stdout).unwrap())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let mut children = (0..8)
            .map(|_| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "github_status::tests::separate_processes_share_one_actual_gh_child",
                        "--nocapture",
                    ])
                    .env(CHILD_ROOT, root.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let outputs = children
            .drain(..)
            .map(|child| child.wait_with_output().unwrap())
            .collect::<Vec<_>>();
        for output in outputs {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(fs::read(count).unwrap(), b"x");
    }

    #[test]
    fn cancellation_during_a_query_releases_the_flight_for_the_next_caller() {
        let root = private_tempdir();
        let script = root.path().join("gh");
        let started = root.path().join("started");
        let started_quoted = format!("'{}'", started.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf started > {started_quoted}\nsleep 20\n"),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let service = StatusService::system(&script);
        let cancellation = CancellationToken::new();
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                read_with(
                    root.path(),
                    "account",
                    &key(),
                    &service,
                    &cancellation,
                    || Ok(()),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !started.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(started.exists(), "fixture query did not start");
            cancellation.cancel();
            assert!(worker.join().unwrap().is_err());
        });
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat <<'RESPONSE'\n{}\nRESPONSE\n",
                String::from_utf8(response(data()).stdout).unwrap()
            ),
        )
        .unwrap();
        let report = read_with(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(report.source, "network");
        assert_eq!(report.outcome, "succeeded");
    }

    #[test]
    fn completed_flight_unlocks_even_when_a_command_copy_survives() {
        struct RememberingExecutor(Mutex<Option<CommandSpec>>);
        impl ProcessExecutor for RememberingExecutor {
            fn execute(
                &self,
                command: &CommandSpec,
                cancellation: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                // A retained command owns a copy of the exported description,
                // just as an unrelated fork can retain it before reaching exec.
                *self.0.lock().unwrap() = Some(command.clone());
                SystemProcessExecutor.execute(command, cancellation)
            }
        }
        let root = private_tempdir();
        let script = root.path().join("gh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat <<'RESPONSE'\n{}\nRESPONSE\n",
                String::from_utf8(response(data()).stdout).unwrap()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let executor = Arc::new(RememberingExecutor(Mutex::new(None)));
        let service = StatusService::new(script, Arc::clone(&executor));
        let report = read_with(
            root.path(),
            "account",
            &key(),
            &service,
            &CancellationToken::new(),
            || Ok(()),
        )
        .unwrap();
        assert_eq!(report.outcome, "succeeded", "{report:?}");
        assert!(executor.0.lock().unwrap().is_some());
        let path = root
            .path()
            .join(format!("{}.lock", target_name("account", &key())));
        let next = crate::instance::owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB);
        assert!(
            next.is_ok(),
            "scoped flight was retained by a copy: {next:?}"
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_live_query_keeps_the_original_flight_after_its_owner_is_killed() {
        use std::ffi::CString;
        const ROOT: &str = "CAGENTS_PR_STATUS_TEST_KILLED_OWNER_ROOT";
        const TEST: &str = "github_status::tests::a_live_query_keeps_the_original_flight_after_its_owner_is_killed";
        if crate::retained_fixture::run_isolated() {
            return;
        }
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let service = StatusService::system(root.join("gh"));
            read_with(
                &root,
                "account",
                &key(),
                &service,
                &CancellationToken::new(),
                || Ok(()),
            )
            .unwrap();
            panic!("the query owner must remain blocked until killed");
        }
        // This exact worker is already beneath the fixture guardian. Make it
        // the nearer subreaper so it can reap gh after killing its producer;
        // no shared test harness changes process-wide child ownership.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        // Retain the small private fixture if any assertion fails; the outer
        // guardian settles descendants before the evidence can be removed.
        let root = private_tempdir().keep();
        eprintln!("GitHub killed-owner fixture: {}", root.display());
        let fifo = root.join("release");
        let fifo_name = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let ready = root.join("started");
        let quoted = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(
            root.join("gh"),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nread release < {}\n",
                quoted(&ready),
                quoted(&fifo)
            ),
        )
        .unwrap();
        fs::set_permissions(root.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut producer = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(ROOT, &root)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = fs::read_to_string(&ready)
                .ok()
                .and_then(|value| value.trim().parse::<libc::pid_t>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "fixture query did not start");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(pid > 0 && pid as u32 != producer.id());
        // This owned Child is still blocked in read_with. Kill only the
        // producer, leaving the actual query and its inherited flight alive.
        producer.kill().unwrap();
        assert!(!producer.wait().unwrap().success());
        let path = root.join(format!("{}.lock", target_name("account", &key())));
        let busy = crate::instance::owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB)
            .err()
            .map(|error| error.kind());
        let mut release = loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(file) => break file,
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture did not reach its barrier"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cannot release fixture: {error}"),
            }
        };
        release.write_all(b"finish\n").unwrap();
        drop(release);
        let mut status = 0;
        loop {
            // The private producer's gh was adopted by this private subreaper.
            // waitpid only reaps that exact child; no file-derived PID is signalled.
            let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if reaped == pid {
                break;
            }
            assert_eq!(reaped, 0, "fixture query was not an owned child");
            assert!(Instant::now() < deadline, "fixture query did not exit");
            thread::sleep(Duration::from_millis(10));
        }
        let next = crate::instance::owned_lock(&path, libc::LOCK_EX | libc::LOCK_NB);
        assert_eq!(busy, Some(io::ErrorKind::WouldBlock));
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(next.is_ok(), "query exit retained its flight: {next:?}");
        drop(next);
        fs::remove_dir_all(root).unwrap();
    }
}
