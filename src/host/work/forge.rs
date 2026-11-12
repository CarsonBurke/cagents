//! The user's pull requests in each GitHub repository, read by bounded `gh`
//! calls on a thread of their own, so a slow or failing GitHub never delays a
//! local scan. One GraphQL document serves every card of a repository, read
//! only when a free conditional probe says it may have changed; a checked-out
//! branch whose pull request someone else opened costs one cached lookup.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::scan::ForgeHints;
use crate::{
    github::{GitHubRepoKey, GitHubService, PullRequest, Revalidation, remote_repository_key},
    repository::{CancellationToken, RepositoryService},
};

/// The user's most recently updated pull requests, in any state.
const AUTHORED: u16 = 100;
/// Their open pull requests, read again with review and check state, which
/// GitHub computes per pull request and so is asked for only where it shows.
const OPEN: u16 = 50;
/// Lookups of someone else's pull request per poll; the rest wait their turn.
const OTHERS_PER_POLL: usize = 4;
const OTHERS_FOUND_FOR: Duration = Duration::from_secs(15 * 60);
const OTHERS_MISSING_FOR: Duration = Duration::from_secs(30 * 60);
const REPOSITORY_FOR: Duration = Duration::from_secs(60 * 60);
/// Checks GitHub doesn't date are read again at least this often while
/// someone reads the project, such as a rerun of a finished check.
const RELIST_WATCHED: Duration = Duration::from_secs(5 * 60);
const RELIST_UNWATCHED: Duration = Duration::from_secs(30 * 60);
/// How long after a push a pull request with no checks may still get some.
const CHECKS_START_WITHIN: Duration = Duration::from_secs(10 * 60);
/// A probe that failed for any reason but quota is retried after this.
const PROBE_UNAVAILABLE_FOR: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    pub repository: GitHubRepoKey,
    /// The repositories the project's remotes name. A pull request belongs
    /// to a local branch only when it comes from one of them.
    pub remotes: Vec<GitHubRepoKey>,
    /// The user's pull requests, newest first; open ones carry review and
    /// check state.
    pub pull_requests: Vec<PullRequest>,
    /// Pull requests for checked-out branches that someone else opened.
    pub others: BTreeMap<String, PullRequest>,
}

impl Snapshot {
    /// The pull request a local branch is for: the one at its exact commit,
    /// else an open one, else the most recently updated.
    pub fn for_branch(&self, branch: &str, local_oid: Option<&str>) -> Option<&PullRequest> {
        let candidates = || {
            self.pull_requests
                .iter()
                .chain(self.others.get(branch))
                .filter(move |pull_request| {
                    pull_request.head_ref_name == branch
                        && self
                            .remotes
                            .iter()
                            .any(|remote| pull_request.belongs_to_head(remote))
                })
        };
        candidates()
            .find(|pull_request| local_oid.is_some_and(|oid| pull_request.head_ref_oid == oid))
            .or_else(|| candidates().find(|pull_request| pull_request.state.0 == "OPEN"))
            .or_else(|| candidates().next())
    }

    pub fn hints(&self) -> ForgeHints {
        let mut hints = ForgeHints::default();
        for pull_request in self.pull_requests.iter().chain(self.others.values()) {
            hints.branches.insert(pull_request.head_ref_name.clone());
            if pull_request.state.0 == "MERGED" && !pull_request.head_ref_oid.is_empty() {
                hints
                    .merged
                    .entry(pull_request.head_ref_name.clone())
                    .or_default()
                    .insert(pull_request.head_ref_oid.clone());
            }
        }
        hints
    }
}

/// How current a poll must make the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freshness {
    /// Nobody reads the project: merges and pins only.
    Unwatched,
    /// Someone reads the project.
    Watched,
    /// Someone asked for a refresh: read the listing whatever the probe says.
    Fresh,
}

/// What a poll needs to know about the project.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub repository: PathBuf,
    /// A checkout `gh` can resolve the repository from.
    pub root: Option<PathBuf>,
    /// Checked-out branches: worktree, branch and head commit.
    pub checked_out: Vec<(PathBuf, String, String)>,
}

/// Facts a poll reuses from earlier ones.
#[derive(Debug, Clone, Default)]
pub(super) struct Memory {
    repository: Option<(GitHubRepoKey, Instant)>,
    /// Branch to when someone else's pull request was last looked up, at
    /// which head, and what was found.
    others: BTreeMap<String, (Instant, String, Option<PullRequest>)>,
    listing: Option<Listing>,
}

/// The last listing read, and how to ask whether it is still current.
#[derive(Debug, Clone)]
struct Listing {
    repository: GitHubRepoKey,
    at: Instant,
    pull_requests: Vec<PullRequest>,
    /// Who `@me` was, whose pull requests the probe asks about.
    viewer: String,
    probe: Probe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// No validator yet: the next probe establishes one.
    Unknown,
    Validator(String),
    /// The repository can't be probed, such as one with issues disabled;
    /// its listing is read every poll until then.
    Unavailable(Instant),
}

#[derive(Debug)]
pub(super) enum Outcome {
    NotGitHub,
    Synced(Snapshot),
    Failed(String),
}

pub(super) fn poll(
    target: &Target,
    memory: &mut Memory,
    freshness: Freshness,
    cancellation: &CancellationToken,
) -> Outcome {
    match read(target, memory, freshness, cancellation) {
        Ok(Some(snapshot)) => Outcome::Synced(snapshot),
        Ok(None) => Outcome::NotGitHub,
        Err(error) => Outcome::Failed(error),
    }
}

fn read(
    target: &Target,
    memory: &mut Memory,
    freshness: Freshness,
    cancellation: &CancellationToken,
) -> Result<Option<Snapshot>, String> {
    let git = RepositoryService::system("git");
    let gh = GitHubService::system("gh");
    let remotes = git
        .remotes(&target.repository, cancellation)
        .map_err(|error| error.to_string())?;
    if !gh
        .has_associated_remote(&remotes, cancellation)
        .map_err(|error| error.to_string())?
    {
        return Ok(None);
    }
    let mut keys = Vec::<GitHubRepoKey>::new();
    for key in remotes
        .iter()
        .flat_map(|remote| [remote.fetch_url.as_deref(), remote.push_url.as_deref()])
        .flatten()
        .filter_map(remote_repository_key)
    {
        if !keys.iter().any(|known| known.same_repository(&key)) {
            keys.push(key);
        }
    }
    let repository = match &memory.repository {
        Some((repository, at)) if at.elapsed() < REPOSITORY_FOR => repository.clone(),
        _ => {
            let from = target.root.as_deref().unwrap_or(&target.repository);
            let repository = gh
                .resolve_repository(from, &remotes, cancellation)
                .map_err(|error| error.to_string())?;
            memory.repository = Some((repository.clone(), Instant::now()));
            repository
        }
    };
    if !keys.iter().any(|key| key.same_repository(&repository)) {
        keys.push(repository.clone());
    }

    let pull_requests = listing(&gh, &repository, memory, freshness, cancellation)?;

    let mut snapshot = Snapshot {
        repository: repository.clone(),
        remotes: keys,
        pull_requests,
        others: BTreeMap::new(),
    };
    let mut looked_up = 0;
    memory
        .others
        .retain(|branch, _| target.checked_out.iter().any(|(_, name, _)| name == branch));
    for (worktree, branch, oid) in &target.checked_out {
        if snapshot.for_branch(branch, Some(oid)).is_some() {
            memory.others.remove(branch);
            continue;
        }
        let fresh = memory.others.get(branch).filter(|(at, head, found)| {
            head == oid
                && at.elapsed()
                    < if found.is_some() {
                        OTHERS_FOUND_FOR
                    } else {
                        OTHERS_MISSING_FOR
                    }
        });
        let found = match fresh {
            Some((_, _, found)) => found.clone(),
            None if looked_up < OTHERS_PER_POLL => {
                looked_up += 1;
                let Some(head) =
                    crate::github::worktree_head_repository(worktree, branch, cancellation)
                else {
                    memory
                        .others
                        .insert(branch.clone(), (Instant::now(), oid.clone(), None));
                    continue;
                };
                // A lookup that fails is retried next poll; the cards stand.
                let Ok(found) =
                    gh.resolve_branch_head(&repository, &head, branch, oid, cancellation)
                else {
                    continue;
                };
                memory
                    .others
                    .insert(branch.clone(), (Instant::now(), oid.clone(), found.clone()));
                found
            }
            None => continue,
        };
        if let Some(found) = found {
            snapshot.others.insert(branch.clone(), found);
        }
    }
    Ok(Some(snapshot))
}

/// The user's pull requests, read again only when GitHub says they may have
/// changed. A pull request's `updatedAt`, which the probe watches, moves with
/// pushes, reviews, comments, edits, merges and closes, but not with its
/// checks, so a listing whose checks are still settling is read every poll,
/// and any listing is read again once it is `relist_after` old.
fn listing<E: crate::repository::ProcessExecutor>(
    gh: &GitHubService<E>,
    repository: &GitHubRepoKey,
    memory: &mut Memory,
    freshness: Freshness,
    cancellation: &CancellationToken,
) -> Result<Vec<PullRequest>, String> {
    let previous = memory
        .listing
        .take()
        .filter(|listing| listing.repository == *repository);
    // Asked before the listing is read, so a change landing between the two
    // shows as a change next time rather than hiding under a new validator.
    let probed = previous.as_ref().and_then(|listing| {
        let validator = match &listing.probe {
            Probe::Unavailable(at) if at.elapsed() < PROBE_UNAVAILABLE_FOR => return None,
            Probe::Validator(etag) => Some(etag.as_str()),
            Probe::Unknown | Probe::Unavailable(_) => None,
        };
        Some(
            match gh.revalidate(
                repository,
                &probe_path(repository, &listing.viewer),
                validator,
                cancellation,
            ) {
                Ok(Revalidation::NotModified) => (true, listing.probe.clone()),
                Ok(Revalidation::Modified { etag }) => {
                    (false, etag.map_or(Probe::Unknown, Probe::Validator))
                }
                // Out of REST quota, GraphQL may still answer; the probe is
                // asked again next poll.
                Err(error) if error.rate_limit().is_some() => (false, Probe::Unknown),
                Err(_) => (false, Probe::Unavailable(Instant::now())),
            },
        )
    });
    let current = matches!(probed, Some((true, _)))
        && freshness != Freshness::Fresh
        && previous.as_ref().is_some_and(|listing| {
            listing.at.elapsed() < relist_after(freshness) && !settling(&listing.pull_requests)
        });
    let (previous, probed) = match (current, previous, probed) {
        (true, Some(mut listing), Some((_, probe))) => {
            listing.probe = probe;
            let pull_requests = listing.pull_requests.clone();
            memory.listing = Some(listing);
            return Ok(pull_requests);
        }
        (_, previous, probed) => (previous, probed),
    };
    let read = gh
        .authored_pull_requests(repository, OPEN, AUTHORED, cancellation)
        .map_err(|error| error.to_string())?;
    let probe = match (previous, probed) {
        // A validator is for one viewer's pull requests.
        (Some(previous), Some((_, probe))) if previous.viewer == read.viewer => probe,
        (Some(previous), None) if previous.viewer == read.viewer => previous.probe,
        _ => Probe::Unknown,
    };
    let pull_requests = merge(read.all, read.open);
    memory.listing = Some(Listing {
        repository: repository.clone(),
        at: Instant::now(),
        pull_requests: pull_requests.clone(),
        viewer: read.viewer,
        probe,
    });
    Ok(pull_requests)
}

/// The viewer's most recently updated issue or pull request in the
/// repository. Unlike a pull request listing, whose rows embed the
/// repository and change with any push to it, an issue row changes only
/// with the issue or pull request itself.
fn probe_path(repository: &GitHubRepoKey, viewer: &str) -> String {
    format!(
        "repos/{}/{}/issues?creator={viewer}&state=all&sort=updated&direction=desc&per_page=1",
        repository.owner, repository.name
    )
}

fn relist_after(freshness: Freshness) -> Duration {
    match freshness {
        Freshness::Unwatched => RELIST_UNWATCHED,
        Freshness::Watched | Freshness::Fresh => RELIST_WATCHED,
    }
}

/// Whether an open pull request's checks may still change without its
/// `updatedAt` moving: some are pending, or none have reported yet for a
/// pull request updated so lately that its CI may not have started.
fn settling(pull_requests: &[PullRequest]) -> bool {
    let recent = |pull_request: &PullRequest| {
        pull_request
            .updated_at
            .parse::<jiff::Timestamp>()
            .is_ok_and(|updated| {
                jiff::Timestamp::now().duration_since(updated)
                    < jiff::SignedDuration::try_from(CHECKS_START_WITHIN).unwrap_or_default()
            })
    };
    pull_requests
        .iter()
        .filter(|pull_request| pull_request.state.0 == "OPEN")
        .any(|pull_request| {
            let checks = pull_request.check_summaries();
            checks.iter().any(|check| check.bucket.0 == "pending")
                || (checks.is_empty() && recent(pull_request))
        })
}

/// One list, newest first, in which an open pull request's entry carries the
/// review and check state its second search read.
fn merge(authored: Vec<PullRequest>, open: Vec<PullRequest>) -> Vec<PullRequest> {
    let mut open = open
        .into_iter()
        .map(|pull_request| (pull_request.number, pull_request))
        .collect::<BTreeMap<_, _>>();
    let mut merged = authored
        .into_iter()
        .map(|pull_request| open.remove(&pull_request.number).unwrap_or(pull_request))
        .collect::<Vec<_>>();
    // An open pull request beyond the first hundred still has its card.
    merged.extend(open.into_values());
    merged.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    merged
}

/// Whether a pull request keeps its branch in flight: open, or a draft
/// closed without merging.
pub(super) fn in_flight(pull_request: &PullRequest) -> bool {
    match pull_request.state.0.as_str() {
        "OPEN" => true,
        "CLOSED" => pull_request.is_draft,
        _ => false,
    }
}

/// How stale a listing may be while someone reads its project.
pub(super) const WATCHED_INTERVAL: Duration = Duration::from_secs(30);
/// Unread, a listing only keeps merge state and pins current.
const UNWATCHED_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// A project without a GitHub remote is asked again this rarely; adding one
/// is rare and the next read of the board notices it within the hour.
pub(super) const NOT_GITHUB_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// When the next poll is due: sooner while someone is reading the project,
/// later after each consecutive failure.
pub(super) fn interval(watched: bool, failures: u32) -> Duration {
    let base = if watched {
        WATCHED_INTERVAL
    } else {
        UNWATCHED_INTERVAL
    };
    (base * 2u32.saturating_pow(failures.min(6))).min(Duration::from_secs(30 * 60))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::sync::Arc;

    use super::*;
    use crate::{
        github::RemoteValue,
        repository::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput},
    };

    fn pull_request(
        number: u64,
        branch: &str,
        oid: &str,
        state: &str,
        updated: &str,
    ) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": number,
            "title": format!("PR {number}"),
            "state": state,
            "isDraft": false,
            "headRefName": branch,
            "headRefOid": oid,
            "headRepository": {"name": "repo"},
            "headRepositoryOwner": {"login": "me"},
            "baseRefName": "main",
            "updatedAt": updated,
            "url": format!("https://github.com/me/repo/pull/{number}"),
        }))
        .unwrap()
    }

    fn key(owner: &str) -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, owner, "repo").unwrap()
    }

    #[test]
    fn a_branch_takes_its_exact_commit_then_an_open_pull_request() {
        let snapshot = Snapshot {
            repository: key("me"),
            remotes: vec![key("me")],
            pull_requests: vec![
                pull_request(3, "feature", "c", "OPEN", "2026-10-03T00:00:00Z"),
                pull_request(2, "feature", "b", "MERGED", "2026-10-02T00:00:00Z"),
                pull_request(1, "elsewhere", "a", "CLOSED", "2026-10-01T00:00:00Z"),
            ],
            others: BTreeMap::new(),
        };
        assert_eq!(snapshot.for_branch("feature", Some("b")).unwrap().number, 2);
        assert_eq!(snapshot.for_branch("feature", Some("z")).unwrap().number, 3);
        assert_eq!(snapshot.for_branch("elsewhere", None).unwrap().number, 1);
        assert!(snapshot.for_branch("missing", None).is_none());
        let hints = snapshot.hints();
        assert_eq!(
            hints.branches,
            BTreeSet::from(["elsewhere".to_owned(), "feature".to_owned()])
        );
        assert_eq!(
            hints.merged,
            BTreeMap::from([("feature".to_owned(), BTreeSet::from(["b".to_owned()]))])
        );

        let fork = Snapshot {
            remotes: vec![key("someone")],
            ..snapshot
        };
        assert!(
            fork.for_branch("feature", Some("b")).is_none(),
            "a same-named branch from an unrelated repository is not this one"
        );
    }

    #[test]
    fn open_listing_entries_replace_their_plain_ones_and_extend_the_list() {
        let mut reviewed = pull_request(2, "b", "b", "OPEN", "2026-10-02T00:00:00Z");
        reviewed.review_decision = Some(RemoteValue("APPROVED".into()));
        let merged = merge(
            vec![
                pull_request(2, "b", "b", "OPEN", "2026-10-02T00:00:00Z"),
                pull_request(1, "a", "a", "MERGED", "2026-10-01T00:00:00Z"),
            ],
            vec![
                reviewed.clone(),
                pull_request(9, "old", "o", "OPEN", "2025-01-01T00:00:00Z"),
            ],
        );
        assert_eq!(
            merged.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2, 1, 9]
        );
        assert_eq!(merged[0], reviewed);
    }

    /// A forge answering the probe and the authored listing, counting each.
    struct Forge {
        etag: std::sync::Mutex<String>,
        pending: std::sync::atomic::AtomicBool,
        probe_fails: std::sync::atomic::AtomicBool,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Forge {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                etag: std::sync::Mutex::new("W/\"1\"".into()),
                pending: false.into(),
                probe_fails: false.into(),
                calls: Default::default(),
            })
        }

        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }
    }

    impl ProcessExecutor for Forge {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            use std::sync::atomic::Ordering;
            let arguments = command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let output = |success, stdout: String| {
                Ok(ProcessOutput {
                    success,
                    exit_code: Some(if success { 0 } else { 1 }),
                    signal: None,
                    stdout: stdout.into_bytes(),
                    stderr: vec![],
                })
            };
            if arguments.iter().any(|argument| argument == "graphql") {
                self.calls.lock().unwrap().push("listing");
                let checks = if self.pending.load(Ordering::SeqCst) {
                    "IN_PROGRESS"
                } else {
                    "COMPLETED"
                };
                let mut open = serde_json::to_value(pull_request(
                    1,
                    "feature",
                    "a",
                    "OPEN",
                    "2026-01-01T00:00:00Z",
                ))
                .unwrap();
                open["commits"] = serde_json::json!({"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
                    {"__typename": "CheckRun", "name": "ci", "status": checks, "conclusion": "SUCCESS"}
                ]}}}}]});
                let body = serde_json::json!({"data": {
                    "viewer": {"login": "me"},
                    "open": {"nodes": [open]},
                    "all": {"nodes": []},
                }});
                return output(true, format!("HTTP/2.0 200 OK\r\n\r\n{body}"));
            }
            self.calls.lock().unwrap().push("probe");
            assert!(
                arguments
                    .last()
                    .is_some_and(|path| path.starts_with("repos/me/repo/issues?creator=me&"))
            );
            if self.probe_fails.load(Ordering::SeqCst) {
                return output(false, "HTTP/2.0 410 Gone\r\n\r\n{}".into());
            }
            let etag = self.etag.lock().unwrap().clone();
            if arguments.contains(&format!("If-None-Match: {etag}")) {
                return output(false, "HTTP/2.0 304 Not Modified\r\n\r\n".into());
            }
            output(true, format!("HTTP/2.0 200 OK\r\nEtag: {etag}\r\n\r\n[]"))
        }
    }

    #[test]
    fn an_unchanged_listing_costs_a_free_probe_until_its_checks_or_age_say_otherwise() {
        use std::sync::atomic::Ordering;
        let forge = Forge::new();
        let gh = GitHubService::new("gh", forge.clone());
        let repository = key("me");
        let cancellation = CancellationToken::new();
        let mut memory = Memory::default();
        let mut poll = |freshness| {
            listing(&gh, &repository, &mut memory, freshness, &cancellation).unwrap();
            forge.take()
        };

        // Nothing to compare with yet, then a validator to establish.
        assert_eq!(poll(Freshness::Watched), ["listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        // Unchanged: the probe's 304 costs no rate limit and no listing.
        assert_eq!(poll(Freshness::Watched), ["probe"]);
        assert_eq!(poll(Freshness::Unwatched), ["probe"]);
        // A refresh reads the listing whatever the probe says.
        assert_eq!(poll(Freshness::Fresh), ["probe", "listing"]);
        // A change GitHub dates.
        *forge.etag.lock().unwrap() = "W/\"2\"".into();
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe"]);

        // Pending checks change without the probe noticing.
        forge.pending.store(true, Ordering::SeqCst);
        assert_eq!(poll(Freshness::Fresh), ["probe", "listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        forge.pending.store(false, Ordering::SeqCst);
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe"]);
        drop(poll);

        // An old listing is read again: soon while watched, rarely unwatched.
        let age = |memory: &mut Memory, by: Duration| {
            let listing = memory.listing.as_mut().unwrap();
            listing.at = Instant::now().checked_sub(by).unwrap();
        };
        age(&mut memory, RELIST_WATCHED);
        listing(
            &gh,
            &repository,
            &mut memory,
            Freshness::Unwatched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["probe"]);
        age(&mut memory, RELIST_WATCHED);
        listing(
            &gh,
            &repository,
            &mut memory,
            Freshness::Watched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["probe", "listing"]);
        age(&mut memory, RELIST_UNWATCHED);
        listing(
            &gh,
            &repository,
            &mut memory,
            Freshness::Unwatched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["probe", "listing"]);

        // A repository that can't be probed is listed every poll, and asked
        // again only after a while.
        forge.probe_fails.store(true, Ordering::SeqCst);
        listing(
            &gh,
            &repository,
            &mut memory,
            Freshness::Watched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["probe", "listing"]);
        listing(
            &gh,
            &repository,
            &mut memory,
            Freshness::Watched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["listing"]);
    }

    #[test]
    fn a_pull_request_without_checks_settles_once_ci_has_had_time_to_start() {
        let mut fresh = pull_request(1, "a", "a", "OPEN", &jiff::Timestamp::now().to_string());
        fresh.status_check_rollup = Some(vec![]);
        assert!(settling(std::slice::from_ref(&fresh)));
        let mut quiet = pull_request(1, "a", "a", "OPEN", "2026-01-01T00:00:00Z");
        quiet.status_check_rollup = Some(vec![]);
        assert!(!settling(&[quiet]));
        let mut closed = fresh;
        closed.state = RemoteValue("MERGED".into());
        assert!(!settling(&[closed]));
    }

    #[test]
    fn polling_backs_off_and_quickens_while_watched() {
        assert_eq!(interval(true, 0), Duration::from_secs(30));
        assert_eq!(interval(true, 1), Duration::from_secs(60));
        assert_eq!(interval(false, 0), Duration::from_secs(10 * 60));
        assert_eq!(interval(false, 40), Duration::from_secs(30 * 60));
    }
}
