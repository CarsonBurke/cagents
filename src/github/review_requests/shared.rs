//! A resumable repository scan shared by Cagents and Ceditor processes.
//!
//! A caller advances at most one page, under a cancellable process lock. Other
//! windows adopt its snapshot and due time instead of starting their own scan.
use super::{HistoryPage, Page, scan::Scan, scan::Work};
use crate::notifications::review::{Census, MonitorUpdate, ScanPoint};
use crate::{
    github::{
        GitHubError, GitHubRepoKey, GitHubService, PullRequestKey, Revalidation, polling, pr_status,
    },
    repository::{
        CancellationToken, CommandSpec, ProcessExecutor, ProcessFailure, ProcessFailureKind,
        ProcessOutput, SystemProcessExecutor,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, path::Path, sync::Arc};

const REFRESH: u64 = 60;
const PACE: u64 = 6;
const MAX_BACKOFF: u64 = 30 * 60;
const OBSERVATION_AGE: u64 = 5 * 60;
const OBSERVATION_PAGES: usize = 50;
/// A settled census starts again only once GitHub says, for free, that one
/// of the repository's issues or pull requests changed, as a review request
/// does, or once the last is this old: a request to a team the viewer just
/// joined changes no pull request.
const CENSUS_AFTER: u64 = 5 * 60;
/// A probe GitHub refused, such as for a repository with issues disabled, is
/// asked again after this.
const PROBE_REFUSED_FOR: u64 = 10 * 60;

pub(super) fn backoff(failures: u32) -> u64 {
    REFRESH
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(MAX_BACKOFF)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub revision: u64,
    pub at: u64,
    pub viewer: String,
    pub requests: Vec<(u64, String)>,
}

#[derive(Debug, Clone)]
pub struct PollResult {
    pub scan: Scan,
    pub error: Option<String>,
    pub due: u64,
    /// A durable incarnation distinguishes a recreated cache from its old one.
    pub observation: (String, u64),
    /// Each follower consumes the bounded pages it has not yet observed.
    pub observed: Vec<Observation>,
    pub census: Option<Census>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    incarnation: String,
    revision: u64,
    scan: Scan,
    error: Option<String>,
    due: u64,
    failures: u32,
    #[serde(default)]
    page_due: u64,
    observed: Vec<Observation>,
    /// The probe's validator from when the last complete census started,
    /// and when that was.
    #[serde(default)]
    validator: Option<String>,
    #[serde(default)]
    census_at: u64,
    /// The validator and start of the census under way, which it becomes
    /// once that census completes.
    #[serde(default)]
    pending: Option<(Option<String>, u64)>,
    #[serde(default)]
    probe_refused_at: Option<u64>,
    #[serde(default)]
    attention_scan_start: Option<ScanPoint>,
    #[serde(default)]
    attention_completed: Option<(u64, Census)>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            version: 1,
            incarnation: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            scan: Scan::default(),
            error: None,
            due: 0,
            failures: 0,
            page_due: 0,
            observed: Vec::new(),
            validator: None,
            census_at: 0,
            pending: None,
            probe_refused_at: None,
            attention_scan_start: None,
            attention_completed: None,
        }
    }
}

impl Stored {
    fn schedule(&mut self, completed: u64) {
        self.due = self
            .scan
            .history_due()
            .map_or(self.page_due, |history| self.page_due.min(history))
            .max(completed.saturating_add(PACE));
    }

    fn result(self, repository: &GitHubRepoKey, now: u64) -> PollResult {
        let observed = if self.error.is_none() {
            self.observed
                .into_iter()
                .filter(|page| now.saturating_sub(page.at) < OBSERVATION_AGE)
                .collect()
        } else {
            Vec::new()
        };
        let census = self
            .attention_completed
            .filter(|(at, _)| now.saturating_sub(*at) < OBSERVATION_AGE)
            .map(|(_, census)| census)
            .or_else(|| {
                // An older process writing the shared cache removes newer
                // fields. Require its successful settled census and use only
                // the recorded beginning time, not the latest page revision.
                if self.scan.settled()
                    && self.pending.is_none()
                    && self.census_at > 0
                    && now.saturating_sub(self.census_at) < OBSERVATION_AGE
                {
                    return census_for(
                        repository,
                        &self.scan,
                        ScanPoint::legacy_census(self.census_at),
                    );
                }
                None
            })
            .filter(|census| {
                self.error.is_none()
                    && self
                        .scan
                        .viewer
                        .as_ref()
                        .is_some_and(|viewer| viewer.id == census.viewer)
            });
        PollResult {
            scan: self.scan,
            error: self.error,
            due: self.due,
            observation: (self.incarnation, self.revision),
            observed,
            census,
        }
    }
}

fn census_for(repository: &GitHubRepoKey, scan: &Scan, start: ScanPoint) -> Option<Census> {
    Some(Census {
        repository: repository.clone(),
        start,
        viewer: scan.viewer.as_ref()?.id.clone(),
        current: scan
            .requested
            .iter()
            .map(|(&number, request)| (number, request.current.id.clone()))
            .collect(),
    })
}

pub enum Reply {
    Page(Page),
    History(HistoryPage),
}

pub fn poll(
    repository: &GitHubRepoKey,
    cancellation: &CancellationToken,
) -> Result<PollResult, String> {
    poll_observing(repository, cancellation, |_| Ok(()))
}

pub fn poll_observing(
    repository: &GitHubRepoKey,
    cancellation: &CancellationToken,
    report: impl Fn(MonitorUpdate) -> Result<(), String>,
) -> Result<PollResult, String> {
    let credentials = pr_status::credential_fingerprint()?;
    polling::bind_scope(&repository.authority(), &credentials);
    let root = pr_status::state_root()?;
    let result = poll_with(
        &root,
        &credentials,
        repository,
        cancellation,
        polling::now,
        |work, lease| {
            let service = GitHubService::new("gh", Arc::new(FlightExecutor(lease)));
            match work {
                Work::Page { cursor } => service
                    .review_request_page(repository, cursor.as_deref(), cancellation)
                    .map(Reply::Page),
                Work::History { number, before, .. } => service
                    .review_request_history_page(
                        &PullRequestKey {
                            repository: repository.clone(),
                            number: *number,
                        },
                        before.as_deref(),
                        cancellation,
                    )
                    .map(Reply::History),
            }
        },
        || {
            if pr_status::credential_fingerprint()? == credentials {
                Ok(())
            } else {
                Err(
                    "The GitHub account changed while reading review requests. Refresh to retry."
                        .into(),
                )
            }
        },
        |validator, lease| {
            GitHubService::new("gh", Arc::new(FlightExecutor(lease))).revalidate(
                repository,
                &crate::github::sidebar::repository_probe_path(repository),
                validator,
                cancellation,
            )
        },
    )?;
    // Each interested daemon can adopt a vetted census from the shared cache;
    // the process that happened to fetch the last page is not its only owner.
    if let Some(census) = &result.census
        && let Err(error) = report(MonitorUpdate::Completed(census.clone()))
    {
        tracing::debug!(%error, "completed review census was not reported");
    }
    Ok(result)
}

/// The child retains the lease if its window dies while gh is still running.
struct FlightExecutor(Arc<File>);

impl ProcessExecutor for FlightExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let command = command
            .clone()
            .inherit_file(Arc::clone(&self.0))
            .map_err(|error| {
                crate::process::process_failure(
                    command,
                    ProcessFailureKind::Spawn,
                    &format!("Cannot inherit the GitHub review flight: {error}"),
                    false,
                )
            })?;
        SystemProcessExecutor.execute(&command, cancellation)
    }
}

fn name(credentials: &str, repository: &GitHubRepoKey) -> String {
    let mut digest = Sha256::new();
    for part in [
        "review-requests-v1",
        credentials,
        &repository.authority().to_ascii_lowercase(),
        &repository.owner.to_ascii_lowercase(),
        &repository.name.to_ascii_lowercase(),
    ] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn poll_with(
    root: &Path,
    credentials: &str,
    repository: &GitHubRepoKey,
    cancellation: &CancellationToken,
    clock: impl Fn() -> u64,
    read: impl FnOnce(&Work, Arc<File>) -> Result<Reply, GitHubError>,
    validate_context: impl Fn() -> Result<(), String>,
    probe: impl FnOnce(Option<&str>, Arc<File>) -> Result<Revalidation, GitHubError>,
) -> Result<PollResult, String> {
    repository.validate().map_err(|error| error.to_string())?;
    pr_status::private_directory(root).map_err(|error| error.to_string())?;
    let name = name(credentials, repository);
    pr_status::prune_cache(root, &name);
    let flight = pr_status::lock(&root.join(format!("{name}.lock")), cancellation)?;
    validate_context()?;
    let path = root.join(format!("{name}.json"));
    let mut stored = pr_status::load::<Stored>(&path)
        .map_err(|error| error.to_string())?
        .filter(|stored| stored.version == 1)
        .unwrap_or_default();
    let now = clock();
    if stored.due > now {
        return Ok(stored.result(repository, now));
    }
    if let Some(limit) = pr_status::shared_cooldown(
        root,
        credentials,
        &repository.authority(),
        "graphql",
        cancellation,
    )? {
        stored.error = Some(limit.to_string());
        stored.due = limit.retry_at;
        stored.observed.clear();
        return Ok(stored.result(repository, now));
    }
    if cancellation.is_cancelled() {
        return Err("Review-request read cancelled".into());
    }
    // Preserve this exclusive description in gh, while the scoped owner still
    // releases unrelated fork copies after the executor has reaped the child.
    let lease = || {
        flight
            .try_clone()
            .map(Arc::new)
            .map_err(|error| format!("Cannot inherit the GitHub review flight: {error}"))
    };
    let work = stored.scan.next_due_work(now, stored.page_due <= now);
    if matches!(work, Work::Page { cursor: None })
        && stored.scan.discovery_complete()
        && stored
            .probe_refused_at
            .is_none_or(|at| now.saturating_sub(at) >= PROBE_REFUSED_FOR)
    {
        // Asked before the census reads, so a change landing during it shows
        // as a change next time rather than hiding under this validator.
        let probe_lease = lease()?;
        let probed = polling::with_scope(credentials, || {
            probe(stored.validator.as_deref(), probe_lease)
        });
        let validator = match probed {
            Ok(Revalidation::NotModified)
                if stored.failures == 0 && now.saturating_sub(stored.census_at) < CENSUS_AFTER =>
            {
                validate_context()?;
                stored.probe_refused_at = None;
                let completed = clock();
                stored.page_due = completed
                    .saturating_add(REFRESH)
                    .min(stored.census_at.saturating_add(CENSUS_AFTER));
                stored.schedule(completed);
                pr_status::store(&path, &stored).map_err(|error| error.to_string())?;
                return Ok(stored.result(repository, now));
            }
            Ok(Revalidation::Modified { etag, .. }) => {
                stored.probe_refused_at = None;
                etag
            }
            Ok(Revalidation::Refused) => {
                stored.probe_refused_at = Some(now);
                None
            }
            // Unchanged but due, or out of quota, deferred or unreachable:
            // the validator is older than the census about to read.
            Ok(Revalidation::NotModified) | Err(_) => stored.validator.clone(),
        };
        stored.pending = Some((validator, now));
    }
    if matches!(work, Work::Page { cursor: None }) {
        stored.attention_scan_start = Some(ScanPoint {
            incarnation: stored.incarnation.clone(),
            revision: stored.revision,
            at: now,
        });
    }
    let previous_viewer = stored.scan.viewer.as_ref().map(|viewer| viewer.id.clone());
    let lease = lease()?;
    let reply = polling::with_scope(credentials, || read(&work, lease));
    // A gh auth switch can race a subprocess. Never publish the response under
    // the credential context captured before it started.
    validate_context()?;
    let completed = clock();
    let rate_limit = reply
        .as_ref()
        .err()
        .and_then(GitHubError::rate_limit)
        .or_else(|| {
            polling::with_scope(credentials, || polling::rate_limit(&repository.authority()))
        });
    if cancellation.is_cancelled() {
        if let Some(limit) = &rate_limit {
            // Cancellation must not discard a Retry-After already received.
            // The repository lease still protects publication, while the host
            // cooldown's own short transaction is independent of the caller.
            pr_status::record_cooldown(
                root,
                credentials,
                &repository.authority(),
                limit,
                &CancellationToken::new(),
            )?;
        }
        return Err("Review-request read cancelled".into());
    }
    let mut observed = Vec::new();
    let mut finished_census = false;
    let applied = reply.map_err(|error| error.to_string()).and_then(|reply| {
        let viewer = match &reply {
            Reply::Page(page) => &page.viewer,
            Reply::History(page) => &page.viewer,
        }
        .id
        .clone();
        observed = match &reply {
            Reply::Page(page) => page.entries.iter().map(|entry| entry.number).collect(),
            Reply::History(_) => match &work {
                Work::History { number, .. } => vec![*number],
                _ => Vec::new(),
            },
        };
        let same_viewer = previous_viewer.as_deref() == Some(viewer.as_str());
        let applied = match reply {
            Reply::Page(page) => {
                let first = matches!(work, Work::Page { cursor: None });
                if !first && !same_viewer {
                    stored.attention_scan_start = None;
                }
                finished_census = page.next_cursor.is_none() && (first || same_viewer);
                stored.scan.page(&work, page)
            }
            Reply::History(page) => {
                if !same_viewer {
                    stored.attention_scan_start = None;
                    stored.page_due = completed;
                }
                stored.scan.history(&work, page)
            }
        };
        if stored
            .scan
            .viewer
            .as_ref()
            .is_none_or(|current| current.id != viewer)
        {
            observed.clear();
        }
        applied
    });
    if matches!(work, Work::Page { .. }) {
        stored.error = applied.err();
        if stored.error.is_some() {
            stored.attention_scan_start = None;
            observed.clear();
            stored.failures = stored.failures.saturating_add(1);
            stored.page_due = completed.saturating_add(backoff(stored.failures));
        } else {
            stored.failures = 0;
            if finished_census && let Some((validator, at)) = stored.pending.take() {
                stored.validator = validator;
                stored.census_at = at;
            }
            stored.page_due =
                completed.saturating_add(if finished_census { REFRESH } else { PACE });
        }
    } else {
        if stored.failures == 0 {
            stored.error = None;
        }
        if let Err(error) = applied {
            observed.clear();
            stored.scan.failed_history(&work, completed);
            if rate_limit.is_some() {
                stored.error = Some(error);
            } else {
                tracing::debug!(?work, %error, "review timeline unavailable");
            }
        }
    }
    stored.schedule(completed);
    if let Some(limit) = rate_limit {
        stored.due = stored.due.max(limit.retry_at);
        pr_status::record_cooldown(
            root,
            credentials,
            &repository.authority(),
            &limit,
            cancellation,
        )?;
    }
    stored.revision = stored.revision.wrapping_add(1);
    stored
        .observed
        .retain(|page| completed.saturating_sub(page.at) < OBSERVATION_AGE);
    if let Some(viewer) = &stored.scan.viewer {
        let requests = observed
            .into_iter()
            .filter_map(|number| {
                stored
                    .scan
                    .requested
                    .get(&number)
                    .map(|request| (number, request.current.id.clone()))
            })
            .collect::<Vec<_>>();
        if !requests.is_empty() {
            stored.observed.push(Observation {
                revision: stored.revision,
                at: completed,
                viewer: viewer.id.clone(),
                requests,
            });
        }
    }
    if stored.observed.len() > OBSERVATION_PAGES {
        stored
            .observed
            .drain(..stored.observed.len() - OBSERVATION_PAGES);
    }
    if finished_census
        && stored.error.is_none()
        && let Some(census) = stored
            .attention_scan_start
            .take()
            .and_then(|start| census_for(repository, &stored.scan, start))
    {
        stored.attention_completed = Some((completed, census));
    }
    pr_status::store(&path, &stored).map_err(|error| error.to_string())?;
    Ok(stored.result(repository, completed))
}

/// Census fixtures, including publisher processes, shared with the
/// application's tests.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod fixture {
    use super::*;
    use crate::github::review_requests::{Current, Entry, Viewer};
    use std::{fs::OpenOptions, io::Write, process::Command, thread, time::Duration};

    /// A probe that can't answer, so every census reads as before probes.
    pub fn unprobed(_: Option<&str>, _: Arc<File>) -> Result<Revalidation, GitHubError> {
        Err(GitHubError::Inspection("unprobed".into()))
    }

    pub fn repository() -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, "owner", "repository").unwrap()
    }

    pub fn page(viewer: &str, numbers: &[u64], next: Option<&str>) -> Reply {
        Reply::Page(Page {
            viewer: Viewer {
                id: viewer.into(),
                login: viewer.into(),
            },
            entries: numbers
                .iter()
                .map(|&number| Entry {
                    number,
                    title: format!("PR {number}"),
                    url: format!("https://github.com/owner/repository/pull/{number}")
                        .parse()
                        .unwrap(),
                    viewer_latest_review_request: Some(Current {
                        id: format!("request-{number}"),
                        as_code_owner: false,
                        requested_reviewer: None,
                    }),
                })
                .collect(),
            next_cursor: next.map(str::to_owned),
        })
    }

    /// One publisher process. It does nothing unless [`published_fixture`]
    /// started it, so the test that calls it passes in an ordinary run.
    pub fn publisher_process() {
        let Some(root) = std::env::var_os("CAGENTS_REVIEW_POLL_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let tick = std::env::var("CAGENTS_REVIEW_POLL_TEST_TICK")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1000);
        let count = std::env::var("CAGENTS_REVIEW_POLL_TEST_COUNT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let result = poll_with(
            &root,
            "account",
            &repository(),
            &CancellationToken::new(),
            || tick,
            |work, _| {
                let mut calls = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(root.join("calls"))
                    .unwrap();
                writeln!(calls, "read").unwrap();
                thread::sleep(Duration::from_millis(50));
                Ok(if let Some(count) = count {
                    let start = match work {
                        Work::Page { cursor: None } => 1,
                        Work::Page {
                            cursor: Some(cursor),
                        } => cursor
                            .strip_prefix("page-")
                            .unwrap()
                            .parse::<u64>()
                            .unwrap(),
                        _ => panic!("unexpected shared continuation"),
                    };
                    let end = (start + 49).min(count);
                    let next = (end < count).then(|| format!("page-{}", end + 1));
                    page(
                        "viewer",
                        &(start..=end).collect::<Vec<_>>(),
                        next.as_deref(),
                    )
                } else {
                    page("viewer", &[1], None)
                })
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert!(result.scan.requested.contains_key(&1));
    }

    /// A publisher with no notification consumer exits between pages. A later
    /// follower must still see every validated page, not just the last one.
    ///
    /// `publisher` is the exact name of a test in the running test binary that
    /// calls [`publisher_process`].
    pub fn published_fixture(root: &Path, now: u64, count: u64, publisher: &str) -> PollResult {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pages = count.div_ceil(50);
        for tick in (0..pages).map(|page| now + page * PACE) {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", publisher])
                .env("CAGENTS_REVIEW_POLL_TEST_ROOT", root)
                .env("CAGENTS_REVIEW_POLL_TEST_TICK", tick.to_string())
                .env("CAGENTS_REVIEW_POLL_TEST_COUNT", count.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        poll_with(
            root,
            "account",
            &repository(),
            &CancellationToken::new(),
            || now + (pages - 1) * PACE + 1,
            |_, _| panic!("the follower must read both published pages without a network request"),
            || Ok(()),
            unprobed,
        )
        .unwrap()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::fixture::{page, published_fixture, publisher_process, repository, unprobed};
    use super::*;
    use std::{
        fs,
        process::{Command, Stdio},
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::{Duration, Instant},
    };

    fn private_root() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn a_settled_census_starts_again_only_when_the_repository_changed_or_is_due() {
        let root = private_root();
        let cancellation = CancellationToken::new();
        let etag = std::cell::RefCell::new("v1".to_owned());
        let reads = std::cell::Cell::new(0);
        let fails = std::cell::Cell::new(false);
        let poll = |at: u64| {
            let result = poll_with(
                root.path(),
                "census-probe-test",
                &repository(),
                &cancellation,
                || at,
                |_, _| {
                    reads.set(reads.get() + 1);
                    if fails.get() {
                        Err(GitHubError::Inspection("unavailable".into()))
                    } else {
                        Ok(page("viewer", &[1], None))
                    }
                },
                || Ok(()),
                |validator, _| {
                    let etag = etag.borrow().clone();
                    Ok(if validator == Some(etag.as_str()) {
                        Revalidation::NotModified
                    } else {
                        Revalidation::Modified {
                            etag: Some(etag),
                            body: Vec::new(),
                            date: None,
                        }
                    })
                },
            )
            .unwrap();
            (reads.get(), result.due)
        };
        // The first census, then the first with a validator.
        assert_eq!(poll(1000), (1, 1000 + REFRESH));
        assert_eq!(poll(1060), (2, 1060 + REFRESH));
        // Unchanged: no census until the last is CENSUS_AFTER old.
        assert_eq!(poll(1120), (2, 1120 + REFRESH));
        assert_eq!(
            poll(1060 + CENSUS_AFTER - REFRESH),
            (2, 1060 + CENSUS_AFTER)
        );
        let due = 1060 + CENSUS_AFTER;
        assert_eq!(poll(due), (3, due + REFRESH));
        assert_eq!(poll(due + 60), (3, due + 60 + REFRESH));
        // A change: a census, which failing leaves the change to find again.
        *etag.borrow_mut() = "v2".into();
        fails.set(true);
        assert_eq!(poll(due + 120).0, 4);
        fails.set(false);
        assert_eq!(poll(due + 240), (5, due + 240 + REFRESH));
        assert_eq!(poll(due + 300), (5, due + 300 + REFRESH));
        // A census that failed is read again though nothing changed.
        let due = due + 240 + CENSUS_AFTER;
        fails.set(true);
        assert_eq!(poll(due).0, 6);
        fails.set(false);
        assert_eq!(poll(due + 600), (7, due + 600 + REFRESH));
        assert_eq!(poll(due + 660), (7, due + 660 + REFRESH));
        // A repository GitHub refuses to probe is censused as before probes,
        // and asked again only after a while.
        let probes = std::cell::Cell::new(0);
        let censuses = std::cell::Cell::new(0);
        let refused = |at: u64| {
            poll_with(
                root.path(),
                "census-refused-test",
                &repository(),
                &cancellation,
                || at,
                |_, _| {
                    censuses.set(censuses.get() + 1);
                    Ok(page("viewer", &[1], None))
                },
                || Ok(()),
                |_, _| {
                    probes.set(probes.get() + 1);
                    Ok(Revalidation::Refused)
                },
            )
            .unwrap();
            (probes.get(), censuses.get())
        };
        assert_eq!(refused(1000), (0, 1));
        assert_eq!(refused(1060), (1, 2));
        assert_eq!(refused(1120), (1, 3));
        assert_eq!(refused(1060 + PROBE_REFUSED_FOR), (2, 4));
    }

    #[test]
    fn an_exclusive_scan_flight_allows_its_actual_query_child_to_run() {
        let root = private_root();
        let cancellation = CancellationToken::new();
        let result = poll_with(
            root.path(),
            "actual-flight-test",
            &repository(),
            &cancellation,
            || 1000,
            |_, lease| {
                let output = FlightExecutor(lease)
                    .execute(
                        &CommandSpec::new("/bin/sh").args(["-c", "printf query"]),
                        &cancellation,
                    )
                    .expect("the scan's real child must start while its flight is locked");
                assert!(output.success);
                assert_eq!(output.stdout, b"query");
                Ok(page("viewer", &[1], None))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(result.scan.requested.contains_key(&1));
        let cached = poll_with(
            root.path(),
            "actual-flight-test",
            &repository(),
            &cancellation,
            || 1001,
            |_, _| panic!("the completed real query must be cached"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(cached.observation, result.observation);
    }

    #[test]
    fn process_worker() {
        publisher_process();
    }

    #[test]
    fn slow_follower_sees_every_page_published_by_other_processes() {
        let root = private_root();
        let result = published_fixture(
            root.path(),
            1000,
            60,
            "github::review_requests::shared::tests::process_worker",
        );
        assert_eq!(
            fs::read_to_string(root.path().join("calls")).unwrap(),
            "read\nread\n"
        );
        assert_eq!(result.scan.requested.len(), 60);
        assert_eq!(
            result
                .observed
                .iter()
                .flat_map(|page| page.requests.iter())
                .count(),
            60
        );
    }

    #[test]
    fn concurrent_processes_share_one_poll_and_restarting_reuses_it() {
        let root = private_root();
        let mut children = (0..8)
            .map(|_| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "github::review_requests::shared::tests::process_worker",
                    ])
                    .env("CAGENTS_REVIEW_POLL_TEST_ROOT", root.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for child in children.drain(..) {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(
            fs::read_to_string(root.path().join("calls")).unwrap(),
            "read\n"
        );
        let cached = poll_with(
            root.path(),
            "account",
            &repository(),
            &CancellationToken::new(),
            || 1001,
            |_, _| panic!("a restarted window must reuse the same scan"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(cached.scan.viewer.unwrap().id, "viewer");
        assert_eq!(cached.due, 1060);
    }

    #[test]
    fn discovery_cursor_survives_window_handoff_and_completes_the_census() {
        let root = private_root();
        let cancel = CancellationToken::new();
        let first = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1000,
            |work, _| {
                assert_eq!(*work, Work::Page { cursor: None });
                Ok(page("viewer", &[1], Some("second")))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(first.due, 1006);
        poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1005,
            |_, _| panic!("continuation must obey the shared pace"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        let second = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1006,
            |work, _| {
                assert_eq!(
                    *work,
                    Work::Page {
                        cursor: Some("second".into())
                    }
                );
                Ok(page("viewer", &[51], None))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert!(second.scan.settled());
        assert_eq!(
            second.scan.requested.keys().copied().collect::<Vec<_>>(),
            [1, 51]
        );
        let fresh = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1066,
            |work, _| {
                assert_eq!(*work, Work::Page { cursor: None });
                Ok(page("viewer", &[51], None))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(
            fresh.scan.requested.keys().copied().collect::<Vec<_>>(),
            [51]
        );
    }

    #[test]
    fn credential_contexts_never_adopt_each_others_viewer_or_cursor() {
        let root = private_root();
        let cancel = CancellationToken::new();
        poll_with(
            root.path(),
            "alice",
            &repository(),
            &cancel,
            || 1000,
            |_, _| Ok(page("alice", &[1], Some("alice-cursor"))),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        let bob = poll_with(
            root.path(),
            "bob",
            &repository(),
            &cancel,
            || 1001,
            |work, _| {
                assert_eq!(*work, Work::Page { cursor: None });
                Ok(page("bob", &[2], None))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(bob.scan.viewer.unwrap().id, "bob");
        assert_eq!(bob.scan.requested.keys().copied().collect::<Vec<_>>(), [2]);
        let alice = poll_with(
            root.path(),
            "alice",
            &repository(),
            &cancel,
            || 1001,
            |_, _| panic!("Alice's cached cursor remains separate"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(alice.scan.viewer.unwrap().id, "alice");
    }

    #[test]
    fn account_switch_during_fetch_does_not_publish_under_old_credentials() {
        let root = private_root();
        let checks = AtomicUsize::new(0);
        let result = poll_with(
            root.path(),
            "alice",
            &repository(),
            &CancellationToken::new(),
            || 1000,
            |_, _| Ok(page("bob", &[2], None)),
            || {
                if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err("account changed".into())
                }
            },
            unprobed,
        );
        assert_eq!(result.unwrap_err(), "account changed");
        let next = poll_with(
            root.path(),
            "alice",
            &repository(),
            &CancellationToken::new(),
            || 1001,
            |work, _| {
                assert_eq!(*work, Work::Page { cursor: None });
                Ok(page("alice", &[1], None))
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(next.scan.viewer.unwrap().id, "alice");
    }

    #[test]
    fn failures_share_backoff_and_rate_limit_pauses_other_repositories() {
        let root = private_root();
        let cancel = CancellationToken::new();
        let failed = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1000,
            |_, _| Err(GitHubError::Inspection("offline".into())),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(failed.due, 1060);
        assert_eq!(failed.error.as_deref(), Some("offline"));
        poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1059,
            |_, _| panic!("another window cannot bypass the failure backoff"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        let retry_at = polling::now() + 180;
        let limited = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1060,
            |_, _| {
                Err(GitHubError::RateLimited {
                    limit: polling::RateLimit::secondary(retry_at),
                })
            },
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(limited.due, retry_at);
        let other = GitHubRepoKey::new("github.com", None, "owner", "other").unwrap();
        let paused = poll_with(
            root.path(),
            "account",
            &other,
            &cancel,
            polling::now,
            |_, _| panic!("the host cooldown must cover every repository"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(paused.due, retry_at);
        assert!(paused.error.unwrap().contains("rate limit"));
    }

    #[test]
    fn cancelled_lock_waiter_exits_and_does_not_start_a_read() {
        let root = private_root();
        let lock = pr_status::lock(
            &root
                .path()
                .join(format!("{}.lock", name("account", &repository()))),
            &CancellationToken::new(),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let root_path = root.path().to_owned();
        let worker = thread::spawn(move || {
            poll_with(
                &root_path,
                "account",
                &repository(),
                &cancel,
                || 1000,
                |_, _| panic!("a cancelled waiter cannot fetch"),
                || Ok(()),
                unprobed,
            )
        });
        thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        cancellation.cancel();
        assert!(worker.join().unwrap().unwrap_err().contains("cancelled"));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(lock);
        poll_with(
            root.path(),
            "account",
            &repository(),
            &CancellationToken::new(),
            || 1000,
            |_, _| Ok(page("viewer", &[1], None)),
            || Ok(()),
            unprobed,
        )
        .unwrap();
    }

    #[test]
    fn cancellation_preserves_a_received_cooldown_without_publishing_a_snapshot() {
        let root = private_root();
        let cancellation = CancellationToken::new();
        let retry_at = polling::now() + 180;
        let result = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancellation,
            polling::now,
            |_, _| {
                cancellation.cancel();
                Err(GitHubError::RateLimited {
                    limit: polling::RateLimit::secondary(retry_at),
                })
            },
            || Ok(()),
            unprobed,
        );
        assert!(result.unwrap_err().contains("cancelled"));
        let snapshot = root
            .path()
            .join(format!("{}.json", name("account", &repository())));
        assert!(!snapshot.exists());
        let other = GitHubRepoKey::new("github.com", None, "owner", "other").unwrap();
        let paused = poll_with(
            root.path(),
            "account",
            &other,
            &CancellationToken::new(),
            polling::now,
            |_, _| panic!("a cancelled caller must not discard the host's Retry-After"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(paused.due, retry_at);
    }

    #[test]
    fn only_a_complete_successful_provider_scan_reports_absence() {
        let root = private_root();
        let cancel = CancellationToken::new();
        let repository = repository();
        let run = |now, reply: Option<Result<Reply, GitHubError>>| {
            poll_with(
                root.path(),
                "account",
                &repository,
                &cancel,
                || now,
                |_, _| reply.expect("cache should not fetch another page"),
                || Ok(()),
                unprobed,
            )
            .unwrap()
        };
        assert!(
            run(1000, Some(Ok(page("viewer", &[1], Some("next")))))
                .census
                .is_none()
        );
        assert!(run(1001, None).census.is_none());
        let completed = run(1006, Some(Ok(page("viewer", &[], None))));
        let census = completed.census.as_ref().unwrap();
        assert_eq!(census.start.at, 1000);
        assert_eq!(census.start.revision, 0);
        assert_eq!(census.viewer, "viewer");
        assert_eq!(
            census.current.get(&1).map(String::as_str),
            Some("request-1")
        );
        let follower = run(1007, None);
        assert_eq!(follower.census.as_ref(), Some(census));
        let failed = run(1067, Some(Err(GitHubError::Inspection("offline".into()))));
        assert!(failed.error.is_some());
        assert!(failed.census.is_none());
    }

    #[test]
    fn failed_review_history_does_not_invalidate_a_complete_repository_census() {
        use crate::github::review_requests::Reviewer;
        let root = private_root();
        let cancel = CancellationToken::new();
        let repository = repository();
        let run = |now, expected: &Work, reply| {
            poll_with(
                root.path(),
                "account",
                &repository,
                &cancel,
                || now,
                |actual, _| {
                    assert_eq!(actual, expected);
                    reply
                },
                || Ok(()),
                unprobed,
            )
            .unwrap()
        };
        let Reply::Page(mut first_page) = page("viewer", &[1], Some("next")) else {
            unreachable!()
        };
        let request = first_page.entries[0]
            .viewer_latest_review_request
            .as_mut()
            .unwrap();
        request.requested_reviewer = Some(Reviewer {
            kind: "User".into(),
            id: Some("viewer".into()),
        });
        let history = Work::History {
            number: 1,
            request: request.clone(),
            before: None,
        };
        let first = run(
            1000,
            &Work::Page { cursor: None },
            Ok(Reply::Page(first_page)),
        );
        assert!(first.census.is_none());
        let failed = run(
            1006,
            &history,
            Err(GitHubError::Inspection("timeline unavailable".into())),
        );
        assert!(failed.error.is_none());
        assert!(failed.census.is_none());
        let completed = run(
            failed.due,
            &Work::Page {
                cursor: Some("next".into()),
            },
            Ok(page("viewer", &[2], None)),
        );
        assert!(completed.error.is_none());
        assert!(
            !completed.scan.settled(),
            "the request history is still unread"
        );
        let census = completed.census.unwrap();
        assert_eq!(census.repository, repository);
        assert_eq!(census.viewer, "viewer");
        assert_eq!(
            census.start,
            ScanPoint {
                incarnation: first.observation.0,
                revision: 0,
                at: 1000
            }
        );
        assert_eq!(
            census.current,
            [(1, "request-1".into()), (2, "request-2".into())].into()
        );
    }

    #[test]
    fn failing_timeline_retries_without_slowing_repository_pages() {
        use crate::github::review_requests::{Reviewer, scan::History};

        let root = private_root();
        let cancel = CancellationToken::new();
        let repository = repository();
        let mut tick = 1000;
        let mut next_page = 1;
        let mut attempts = Vec::new();
        let result = loop {
            let result = poll_with(
                root.path(),
                "account",
                &repository,
                &cancel,
                || tick,
                |work, _| match work {
                    Work::Page { cursor } => {
                        let expected = (next_page != 1).then(|| format!("page-{next_page}"));
                        assert_eq!(cursor, &expected);
                        let next = (next_page < 13).then(|| format!("page-{}", next_page + 1));
                        let Reply::Page(mut page) = page("viewer", &[next_page], next.as_deref())
                        else {
                            unreachable!()
                        };
                        if next_page == 1 {
                            page.entries[0]
                                .viewer_latest_review_request
                                .as_mut()
                                .unwrap()
                                .requested_reviewer = Some(Reviewer {
                                kind: "User".into(),
                                id: Some("viewer".into()),
                            });
                        }
                        next_page += 1;
                        Ok(Reply::Page(page))
                    }
                    Work::History { number: 1, .. } => {
                        attempts.push(tick);
                        Err(GitHubError::Inspection("timeline unavailable".into()))
                    }
                    other => panic!("unexpected work {other:?}"),
                },
                || Ok(()),
                unprobed,
            )
            .unwrap();
            if next_page == 14 {
                break result;
            }
            assert_eq!(result.due, tick + PACE);
            tick = result.due;
        };
        assert_eq!(attempts, [1006, 1066]);
        assert_eq!(result.scan.requested[&1].history, History::Unknown);
        assert_eq!(result.census.unwrap().current.len(), 13);
    }

    #[test]
    fn a_follower_recovers_a_complete_census_after_an_older_cache_writer() {
        let root = private_root();
        let cancel = CancellationToken::new();
        let repository = repository();
        let fetch = |now| {
            poll_with(
                root.path(),
                "account",
                &repository,
                &cancel,
                || now,
                |_, _| Ok(page("viewer", &[], None)),
                || Ok(()),
                unprobed,
            )
            .unwrap()
        };
        fetch(1000);
        let first = fetch(1060);
        let recent = first.census.unwrap();
        let path = root
            .path()
            .join(format!("{}.json", name("account", &repository)));
        let mut legacy = pr_status::load::<serde_json::Value>(&path)
            .unwrap()
            .unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("attention_scan_start");
        object.remove("attention_completed");
        assert_eq!(object["census_at"].as_u64(), Some(recent.start.at));
        pr_status::store(&path, &legacy).unwrap();
        let follower = |now| {
            poll_with(
                root.path(),
                "account",
                &repository,
                &cancel,
                || now,
                |_, _| panic!("shared scan evidence must not fetch GitHub again"),
                || Ok(()),
                unprobed,
            )
            .unwrap()
        };
        let fallback = follower(1061).census.unwrap();
        assert_eq!(fallback.viewer, recent.viewer);
        assert_eq!(fallback.current, recent.current);
        assert_eq!(fallback.start, ScanPoint::legacy_census(recent.start.at));
        assert!(!recent.start.precedes_census(&fallback.start));
        MonitorUpdate::Completed(fallback).validate().unwrap();
        let mut legacy_obj = legacy.as_object_mut().unwrap().clone();
        legacy_obj.insert("error".into(), "offline".into());
        pr_status::store(&path, &legacy_obj).unwrap();
        assert!(follower(1061).census.is_none());
    }

    #[test]
    fn cached_observation_expires_without_revoking_the_displayed_snapshot() {
        let root = private_root();
        let cancel = CancellationToken::new();
        let observed = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1000,
            |_, _| Ok(page("viewer", &[1], None)),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert_eq!(observed.observed.len(), 1);
        assert_eq!(observed.observed[0].requests, [(1, "request-1".into())]);
        let path = root
            .path()
            .join(format!("{}.json", name("account", &repository())));
        let mut stored = pr_status::load::<Stored>(&path).unwrap().unwrap();
        stored.due = 1400;
        pr_status::store(&path, &stored).unwrap();
        let expired = poll_with(
            root.path(),
            "account",
            &repository(),
            &cancel,
            || 1300,
            |_, _| panic!("a cached observation expires without another request"),
            || Ok(()),
            unprobed,
        )
        .unwrap();
        assert!(expired.observed.is_empty());
        assert_eq!(expired.observation, observed.observation);
        assert!(expired.scan.requested.contains_key(&1));
    }
}
