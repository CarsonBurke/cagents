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

/// A successful result and a failed attempt both suppress repeated reads for
/// two minutes. An explicit server cooldown can extend that interval.
pub const MAX_AGE: u64 = 120;
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
const QUERY: &str = "query CagentsPrStatus($owner:String!,$name:String!,$number:Int!){rateLimit{cost} repository(owner:$owner,name:$name){pullRequest(number:$number){number title url state isDraft headRefName headRefOid baseRefName mergeable mergeStateStatus reviewDecision updatedAt commits(last:1){nodes{commit{oid statusCheckRollup{state contexts(first:100){nodes{__typename ... on CheckRun{name status conclusion detailsUrl startedAt completedAt} ... on StatusContext{context state targetUrl description}} pageInfo{hasNextPage endCursor}}}}}}}}}";

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

    fn fetch(
        &self,
        command: &CommandSpec,
        key: &PullRequestKey,
        cancellation: &CancellationToken,
    ) -> Result<(Snapshot, Option<RateLimit>), StatusError> {
        let output = self
            .executor
            .execute(command, cancellation)
            .map_err(|error| {
                classify_failure(&error.stdout, &error.stderr)
                    .map(StatusError::RateLimited)
                    .unwrap_or_else(|| StatusError::Message(error.reported_detail()))
            })?;
        if !output.success {
            if let Some(limit) = classify_failure(&output.stdout, &output.stderr) {
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
        let headers = String::from_utf8_lossy(&output.stdout[..output.stdout.len() - body.len()]);
        let mut envelope: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
            StatusError::Message(format!("Invalid GitHub status JSON: {error}"))
        })?;
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

fn classify_failure(stdout: &[u8], stderr: &[u8]) -> Option<RateLimit> {
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
        "graphql",
    )
}

fn status_command(gh: &Path, key: &PullRequestKey) -> CommandSpec {
    CommandSpec::new(gh)
        .args([
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub pull_request: PullRequestStatus,
    /// GitHub's aggregate includes every context, even when the list is paged.
    /// None means GitHub returned no check rollup, not a passing check.
    pub checks_state: Option<String>,
    pub checks: Vec<serde_json::Value>,
    pub checks_complete: bool,
    pub primary_cost: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub target: PullRequestKey,
    pub fetched_at: Option<u64>,
    pub age_seconds: Option<u64>,
    pub stale: bool,
    pub source: &'static str,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub retry_at: Option<u64>,
    pub outcome: &'static str,
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
        Report {
            target: target.clone(),
            fetched_at: self.fetched_at,
            age_seconds: self.fetched_at.map(|at| now.saturating_sub(at)),
            stale: failed
                || self
                    .fetched_at
                    .is_none_or(|at| now.saturating_sub(at) >= MAX_AGE),
            source,
            snapshot: self.snapshot,
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
    let _flight = lock(&root.join(format!("{name}.lock")), cancellation)?;
    let path = root.join(format!("{name}.json"));
    let mut cached = load::<Cached>(&path)
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let current = now();
    if cached.next_attempt > current {
        verify_credentials()?;
        return Ok(cached.report(key, "cache", current));
    }
    if let Some(limit) = shared_cooldown(
        root,
        credentials,
        &key.repository.authority(),
        "graphql",
        cancellation,
    )? {
        cached.error = Some(limit.to_string());
        cached.next_attempt = limit.retry_at;
        verify_credentials()?;
        return Ok(cached.report(key, "cache", current));
    }
    // An exclusive flight must retain its original open description: an
    // independently opened shared lease would conflict with this lock. Keep
    // the scoped owner until publication so normal return unlocks unrelated
    // fork copies; abrupt owner death leaves gh holding the original EX lock.
    let lease = Arc::new(
        _flight
            .try_clone()
            .map_err(|error| format!("Cannot inherit the GitHub status flight: {error}"))?,
    );
    let command = status_command(&service.gh, key)
        .inherit_file(lease)
        .map_err(|error| format!("Cannot inherit the GitHub status flight: {error}"))?;
    let result = service.fetch(&command, key, cancellation);
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
        record_cooldown(
            root,
            credentials,
            &key.repository.authority(),
            &limit,
            cancellation,
        )?;
        if result.is_err() {
            cached.next_attempt = cached.next_attempt.max(limit.retry_at);
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

fn parse(data: &serde_json::Value, key: &PullRequestKey) -> Result<Snapshot, StatusError> {
    let pr = data
        .pointer("/repository/pullRequest")
        .filter(|value| value.is_object())
        .ok_or_else(|| StatusError::Message("GitHub returned no pull request".into()))?;
    let pull_request: PullRequestStatus = serde_json::from_value(pr.clone()).map_err(|error| {
        StatusError::Message(format!("Invalid GitHub status response: {error}"))
    })?;
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
    })
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
        let mut another = key();
        another.number = 43;
        let repeated = read_fixture(root.path(), "account", &another, &service, &cancellation);
        assert_eq!(repeated.source, "cache");
        assert_eq!(repeated.outcome, "failed");
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
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
    }
    impl Fixture {
        fn new(results: Vec<ProcessOutput>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                results: Mutex::new(results.into()),
            })
        }
    }
    impl ProcessExecutor for Fixture {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert!(
                command
                    .arguments
                    .iter()
                    .any(|argument| argument.to_string_lossy().contains("query CagentsPrStatus"))
            );
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
        let stale = read_fixture(root.path(), "account", &key(), &service, &cancellation);
        assert!(stale.stale);
        assert_eq!(stale.outcome, "failed");
        assert_eq!(stale.retry_at, Some(reset));
        assert!(stale.snapshot.is_some());
        assert_eq!(fixture.calls.load(Ordering::Relaxed), 1);
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
