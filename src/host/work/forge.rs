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
    github::{
        GitHubRepoKey, GitHubService, PullRequest, Revalidation, checks_settling,
        remote_repository_key,
    },
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
/// A probe GitHub refused, such as for a repository with issues disabled, is
/// asked again after this.
const PROBE_UNAVAILABLE_FOR: Duration = Duration::from_secs(10 * 60);
/// How long GitHub's search, which the listing reads, may take to show a
/// change its REST API, which the probe reads, already does.
const SEARCH_LAG: Duration = Duration::from_secs(60);
/// How many of the viewer's latest changes the probe names. Unchanged, a
/// larger answer is as free; changed, it costs the same one request.
const PROBE_PAGE: usize = 20;

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
    /// Whose credentials read it: after `gh auth switch` it is another
    /// account's listing.
    credential: Option<String>,
    at: Instant,
    /// When it was read by this clock, to compare with GitHub's dates.
    read_at: jiff::Timestamp,
    pull_requests: Vec<PullRequest>,
    /// Who `@me` was, whose pull requests the probe asks about.
    viewer: String,
    probe: Probe,
}

impl Listing {
    /// Whether this listing already shows the changes the probe saw. The
    /// probe's REST answer can be ahead of the search the listing reads,
    /// which fills in the fields of the pull requests it returns live but can
    /// be slow to return a new one at all. So every pull request the probe
    /// names must be listed as it is now, unless it changed so long before
    /// this listing was read that search has had time to show it, and an
    /// answer of only such recent changes may have more beyond it.
    fn reflects(&self, recent: &Recent) -> bool {
        // When this listing was read, by GitHub's clock, less the lag.
        let Some(settled) = jiff::SignedDuration::try_from(SEARCH_LAG)
            .ok()
            .and_then(|lag| {
                self.read_at
                    .checked_add(recent.skew)
                    .ok()?
                    .checked_sub(lag)
                    .ok()
            })
        else {
            return false;
        };
        if recent.rows.is_empty() {
            return self.pull_requests.is_empty();
        }
        let full = recent.rows.len() >= PROBE_PAGE;
        if full && recent.rows.iter().all(|row| row.updated >= settled) {
            return false;
        }
        recent.rows.iter().all(|row| {
            let Some(number) = row.pull_request else {
                return true;
            };
            // Search has had time to show an older change, however it dates
            // it.
            row.updated < settled
                || self.pull_requests.iter().any(|listed| {
                    listed.number == number
                        && listed.updated_at.parse::<jiff::Timestamp>().ok() == Some(row.updated)
                })
        })
    }
}

/// The viewer's most recently updated issues and pull requests in the
/// repository, newest first, as the probe's answer names them, and how far
/// GitHub's clock is ahead of this one's.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recent {
    rows: Vec<Row>,
    skew: jiff::SignedDuration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    /// The pull request's number; an issue has none.
    pull_request: Option<u64>,
    updated: jiff::Timestamp,
}

impl Recent {
    fn of(body: &[u8], skew: jiff::SignedDuration) -> Option<Self> {
        let rows: Vec<serde_json::Value> = serde_json::from_slice(body).ok()?;
        rows.iter()
            .map(|row| {
                Some(Row {
                    pull_request: match row.get("pull_request") {
                        Some(_) => Some(row.get("number")?.as_u64()?),
                        None => None,
                    },
                    updated: row.get("updated_at")?.as_str()?.parse().ok()?,
                })
            })
            .collect::<Option<_>>()
            .map(|rows| Self { rows, skew })
    }
}

/// What the probe said about the last listing.
enum Answer {
    /// It is still current; keep this validator with it.
    Current(Probe),
    /// It may not be: read it again, and keep this validator.
    Unknown(Probe),
    /// Something changed: read it again, and keep `etag` only if that read
    /// shows what the probe saw, so search lag can't hide behind a new
    /// validator.
    Changed {
        etag: Option<String>,
        recent: Recent,
    },
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
    let credential = crate::github_status::credential_fingerprint().ok();
    let Some(mut previous) = memory
        .listing
        .take()
        .filter(|listing| listing.repository == *repository && listing.credential == credential)
    else {
        let read = read_listing(
            gh,
            repository,
            credential.clone(),
            Probe::Unknown,
            cancellation,
        )?;
        let pull_requests = read.pull_requests.clone();
        if crate::github_status::credential_fingerprint().ok() == credential {
            memory.listing = Some(read);
        }
        return Ok(pull_requests);
    };
    // Asked before the listing is read, so a change landing between the two
    // shows as a change next time rather than hiding under a new validator.
    let answer = ask(gh, repository, &previous, cancellation);
    if let Answer::Current(probe) = &answer
        && freshness != Freshness::Fresh
        && previous.at.elapsed() < relist_after(freshness)
        && !checks_settling(&previous.pull_requests)
    {
        previous.probe = probe.clone();
        let pull_requests = previous.pull_requests.clone();
        memory.listing = Some(previous);
        return Ok(pull_requests);
    }
    let (probe, recent) = match answer {
        Answer::Current(probe) | Answer::Unknown(probe) => {
            // What the probe learned holds for the last listing too, such as
            // that GitHub refuses it, should this read fail.
            previous.probe = probe.clone();
            (probe, None)
        }
        Answer::Changed { etag, recent } => {
            (etag.map_or(Probe::Unknown, Probe::Validator), Some(recent))
        }
    };
    match read_listing(gh, repository, credential.clone(), probe, cancellation) {
        // Read as another account than it would be kept for.
        Ok(read) if crate::github_status::credential_fingerprint().ok() != credential => {
            Ok(read.pull_requests)
        }
        Ok(mut read) => {
            // A validator is for one viewer's pull requests, and one read
            // that shows what the probe saw.
            if read.viewer != previous.viewer
                || recent.is_some_and(|recent| !read.reflects(&recent))
            {
                read.probe = Probe::Unknown;
            }
            let pull_requests = read.pull_requests.clone();
            memory.listing = Some(read);
            Ok(pull_requests)
        }
        Err(error) => {
            // Its validator still matches it, for when GitHub answers again.
            memory.listing = Some(previous);
            Err(error)
        }
    }
}

fn ask<E: crate::repository::ProcessExecutor>(
    gh: &GitHubService<E>,
    repository: &GitHubRepoKey,
    listing: &Listing,
    cancellation: &CancellationToken,
) -> Answer {
    let validator = match &listing.probe {
        Probe::Unavailable(at) if at.elapsed() < PROBE_UNAVAILABLE_FOR => {
            return Answer::Unknown(listing.probe.clone());
        }
        Probe::Validator(etag) => Some(etag.as_str()),
        Probe::Unknown | Probe::Unavailable(_) => None,
    };
    let asked = jiff::Timestamp::now();
    match gh.revalidate(
        repository,
        &probe_path(repository, &listing.viewer),
        validator,
        cancellation,
    ) {
        Ok(Revalidation::NotModified) => Answer::Current(listing.probe.clone()),
        Ok(Revalidation::Modified { etag, body, date }) => match Recent::of(
            &body,
            date.map_or(jiff::SignedDuration::ZERO, |date| {
                date.duration_since(asked)
            }),
        ) {
            // Unchanged in effect, such as the first validator after a read.
            Some(recent) if listing.reflects(&recent) => {
                Answer::Current(etag.map_or(Probe::Unknown, Probe::Validator))
            }
            Some(recent) => Answer::Changed { etag, recent },
            None => Answer::Unknown(Probe::Unknown),
        },
        Ok(Revalidation::Refused) => Answer::Unknown(Probe::Unavailable(Instant::now())),
        // Out of quota, deferred or unreachable: the validator still matches
        // the listing it came with.
        Err(_) => Answer::Unknown(listing.probe.clone()),
    }
}

fn read_listing<E: crate::repository::ProcessExecutor>(
    gh: &GitHubService<E>,
    repository: &GitHubRepoKey,
    credential: Option<String>,
    probe: Probe,
    cancellation: &CancellationToken,
) -> Result<Listing, String> {
    let read_at = jiff::Timestamp::now();
    let read = gh
        .authored_pull_requests(repository, OPEN, AUTHORED, cancellation)
        .map_err(|error| error.to_string())?;
    Ok(Listing {
        repository: repository.clone(),
        credential,
        at: Instant::now(),
        read_at,
        pull_requests: merge(read.all, read.open),
        viewer: read.viewer,
        probe,
    })
}

/// The viewer's most recently updated issue or pull request in the
/// repository. Unlike a pull request listing, whose rows embed the
/// repository and change with any push to it, an issue row changes only
/// with the issue or pull request itself.
fn probe_path(repository: &GitHubRepoKey, viewer: &str) -> String {
    format!(
        "repos/{}/{}/issues?creator={viewer}&state=all&sort=updated&direction=desc&per_page={PROBE_PAGE}",
        repository.owner, repository.name
    )
}

fn relist_after(freshness: Freshness) -> Duration {
    match freshness {
        Freshness::Unwatched => RELIST_UNWATCHED,
        Freshness::Watched | Freshness::Fresh => RELIST_WATCHED,
    }
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
    /// The probe sees `updated` at once; the listing's search sees it unless
    /// it lags.
    struct Forge {
        etag: std::sync::Mutex<String>,
        updated: std::sync::Mutex<String>,
        lagging: std::sync::atomic::AtomicBool,
        pending: std::sync::atomic::AtomicBool,
        /// How the probe fails, if it does: a status line.
        probe_fails: std::sync::Mutex<Option<&'static str>>,
        listing_fails: std::sync::atomic::AtomicBool,
        listed: std::sync::Mutex<String>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    const FIRST: &str = "2026-01-01T00:00:00Z";
    const SECOND: &str = "2026-01-02T00:00:00Z";
    const THIRD: &str = "2026-01-03T00:00:00Z";

    impl Forge {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                etag: std::sync::Mutex::new("W/\"1\"".into()),
                updated: std::sync::Mutex::new(FIRST.into()),
                lagging: false.into(),
                pending: false.into(),
                probe_fails: Default::default(),
                listing_fails: false.into(),
                listed: std::sync::Mutex::new(FIRST.into()),
                calls: Default::default(),
            })
        }

        fn take(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }

        /// The pull request changes: GitHub dates it and its validator moves.
        fn change(&self, updated: &str, etag: &str) {
            *self.updated.lock().unwrap() = updated.into();
            *self.etag.lock().unwrap() = etag.into();
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
                if self.listing_fails.load(Ordering::SeqCst) {
                    return output(false, "HTTP/2.0 502 Bad Gateway\r\n\r\n".into());
                }
                if !self.lagging.load(Ordering::SeqCst) {
                    *self.listed.lock().unwrap() = self.updated.lock().unwrap().clone();
                }
                let checks = if self.pending.load(Ordering::SeqCst) {
                    "IN_PROGRESS"
                } else {
                    "COMPLETED"
                };
                let listed = self.listed.lock().unwrap().clone();
                let mut open =
                    serde_json::to_value(pull_request(1, "feature", "a", "OPEN", &listed)).unwrap();
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
            if let Some(status) = *self.probe_fails.lock().unwrap() {
                return output(false, format!("HTTP/2.0 {status}\r\n\r\n{{}}"));
            }
            let etag = self.etag.lock().unwrap().clone();
            if arguments.contains(&format!("If-None-Match: {etag}")) {
                return output(false, "HTTP/2.0 304 Not Modified\r\n\r\n".into());
            }
            let newest = serde_json::json!([{
                "number": 1,
                "updated_at": *self.updated.lock().unwrap(),
                "pull_request": {"url": "https://api.github.com/repos/me/repo/pulls/1"},
            }]);
            output(
                true,
                // GitHub's clock, just after the second change.
                format!(
                    "HTTP/2.0 200 OK\r\nDate: Fri, 02 Jan 2026 00:00:30 GMT\r\nEtag: {etag}\r\n\r\n{newest}"
                ),
            )
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

        // The first probe's validator is for the listing already read.
        assert_eq!(poll(Freshness::Watched), ["listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe"]);
        // Unchanged: the probe's 304 costs no rate limit and no listing.
        assert_eq!(poll(Freshness::Watched), ["probe"]);
        assert_eq!(poll(Freshness::Unwatched), ["probe"]);
        // A refresh reads the listing whatever the probe says.
        assert_eq!(poll(Freshness::Fresh), ["probe", "listing"]);
        // A change GitHub dates.
        forge.change(SECOND, "W/\"2\"");
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe"]);

        // Search not yet showing the change is read until it does.
        forge.lagging.store(true, Ordering::SeqCst);
        forge.change(THIRD, "W/\"3\"");
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        assert_eq!(poll(Freshness::Watched), ["probe", "listing"]);
        forge.lagging.store(false, Ordering::SeqCst);
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
        let mut poll = |memory: &mut Memory, freshness| {
            let read = listing(&gh, &repository, memory, freshness, &cancellation);
            (read.is_ok(), forge.take())
        };
        age(&mut memory, RELIST_WATCHED);
        assert_eq!(
            poll(&mut memory, Freshness::Unwatched),
            (true, vec!["probe"])
        );
        age(&mut memory, RELIST_WATCHED);
        assert_eq!(
            poll(&mut memory, Freshness::Watched),
            (true, vec!["probe", "listing"])
        );
        age(&mut memory, RELIST_UNWATCHED);
        assert_eq!(
            poll(&mut memory, Freshness::Unwatched),
            (true, vec!["probe", "listing"])
        );

        // A failed listing keeps the last one and its validator.
        forge.listing_fails.store(true, Ordering::SeqCst);
        assert_eq!(
            poll(&mut memory, Freshness::Fresh),
            (false, vec!["probe", "listing"])
        );
        forge.listing_fails.store(false, Ordering::SeqCst);
        assert_eq!(poll(&mut memory, Freshness::Watched), (true, vec!["probe"]));

        // A probe that fails in passing keeps its validator for the next.
        *forge.probe_fails.lock().unwrap() = Some("502 Bad Gateway");
        assert_eq!(
            poll(&mut memory, Freshness::Watched),
            (true, vec!["probe", "listing"])
        );
        *forge.probe_fails.lock().unwrap() = None;
        assert_eq!(poll(&mut memory, Freshness::Watched), (true, vec!["probe"]));

        // Out of REST quota, the board is listed over GraphQL every poll and
        // keeps its validator for when the quota resets.
        crate::github::polling::with_scope("forge-probe-out-of-quota", || {
            *forge.probe_fails.lock().unwrap() = Some(
                "403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Resource: core\r\nX-Ratelimit-Reset: 4102444800",
            );
            assert_eq!(
                poll(&mut memory, Freshness::Watched),
                (true, vec!["probe", "listing"])
            );
            assert_eq!(
                poll(&mut memory, Freshness::Watched),
                (true, vec!["listing"])
            );
        });
        assert!(matches!(
            memory.listing.as_ref().unwrap().probe,
            Probe::Validator(_)
        ));
        *forge.probe_fails.lock().unwrap() = None;
        assert_eq!(poll(&mut memory, Freshness::Watched), (true, vec!["probe"]));

        // A repository GitHub refuses to probe is listed every poll, and
        // asked again only after a while, even when that listing failed.
        *forge.probe_fails.lock().unwrap() = Some("410 Gone");
        forge.listing_fails.store(true, Ordering::SeqCst);
        assert_eq!(
            poll(&mut memory, Freshness::Watched),
            (false, vec!["probe", "listing"])
        );
        forge.listing_fails.store(false, Ordering::SeqCst);
        assert_eq!(
            poll(&mut memory, Freshness::Watched),
            (true, vec!["listing"])
        );
    }

    #[test]
    fn a_listing_is_one_accounts_and_one_repositorys() {
        let forge = Forge::new();
        let gh = GitHubService::new("gh", forge.clone());
        let cancellation = CancellationToken::new();
        let mut memory = Memory::default();
        listing(
            &gh,
            &key("me"),
            &mut memory,
            Freshness::Watched,
            &cancellation,
        )
        .unwrap();
        memory.listing.as_mut().unwrap().credential = Some("another account".into());
        listing(
            &gh,
            &key("me"),
            &mut memory,
            Freshness::Watched,
            &cancellation,
        )
        .unwrap();
        assert_eq!(forge.take(), ["listing", "listing"]);
    }

    #[test]
    fn a_listing_reflects_the_changes_search_has_had_time_to_show() {
        let listing = |listed: &[(u64, &str)], read_at: &str| Listing {
            repository: key("me"),
            credential: None,
            at: Instant::now(),
            read_at: read_at.parse().unwrap(),
            pull_requests: listed
                .iter()
                .map(|&(number, updated)| pull_request(number, "a", "a", "OPEN", updated))
                .collect(),
            viewer: "me".into(),
            probe: Probe::Unknown,
        };
        let ahead = |body: serde_json::Value, seconds: i64| {
            Recent::of(
                body.to_string().as_bytes(),
                jiff::SignedDuration::from_secs(seconds),
            )
            .unwrap()
        };
        let recent = |body: serde_json::Value| ahead(body, 0);
        let pull = |number: u64, updated: &str| serde_json::json!({"number": number, "updated_at": updated, "pull_request": {}});
        let issue = |updated: &str| serde_json::json!({"number": 99, "updated_at": updated});
        let late = "2026-01-02T00:00:30Z";
        let soon = "2026-01-02T00:01:00Z";

        // A change made just before the read, listed as it is now or not.
        let changed = recent(serde_json::json!([pull(1, SECOND)]));
        assert!(listing(&[(1, SECOND)], late).reflects(&changed));
        assert!(!listing(&[(1, FIRST)], late).reflects(&changed));
        assert!(!listing(&[], late).reflects(&changed));
        // Search has had time to show one long before, however it dates it.
        assert!(listing(&[(1, FIRST)], THIRD).reflects(&changed));
        // A change after the read isn't in it.
        assert!(!listing(&[(1, FIRST)], FIRST).reflects(&changed));
        // GitHub's clock, not this one, says how long search has had.
        assert!(listing(&[], late).reflects(&ahead(serde_json::json!([pull(1, SECOND)]), 60)));
        assert!(
            !listing(&[], THIRD)
                .reflects(&ahead(serde_json::json!([pull(1, SECOND)]), -24 * 60 * 60))
        );
        // An older new pull request search hadn't returned yet, though it
        // returned a newer one.
        let both = recent(serde_json::json!([pull(2, late), pull(1, SECOND)]));
        assert!(!listing(&[(2, late)], soon).reflects(&both));
        assert!(listing(&[(2, late), (1, SECOND)], soon).reflects(&both));
        // One search has had a while to return and never did isn't waited for.
        assert!(listing(&[(2, late)], THIRD).reflects(&both));
        // Issues aren't listed.
        let issues = recent(serde_json::json!([issue(late), pull(1, SECOND)]));
        assert!(listing(&[(1, SECOND)], soon).reflects(&issues));
        // A whole answer of recent changes may hide more beyond it.
        let busy = recent(serde_json::Value::Array(
            (0..PROBE_PAGE).map(|_| issue(THIRD)).collect(),
        ));
        assert!(!listing(&[], THIRD).reflects(&busy));
        // One whose oldest change search has had time for shows the rest.
        let mut rows = (1..PROBE_PAGE).map(|_| issue(THIRD)).collect::<Vec<_>>();
        rows.push(issue(FIRST));
        let ending_settled = recent(serde_json::Value::Array(rows));
        assert!(listing(&[], "2026-01-03T00:01:00Z").reflects(&ending_settled));
        assert!(!listing(&[(1, SECOND)], THIRD).reflects(&recent(serde_json::json!([]))));
        assert!(listing(&[], THIRD).reflects(&recent(serde_json::json!([]))));
        assert_eq!(
            Recent::of(b"{\"message\":\"no\"}", jiff::SignedDuration::ZERO),
            None
        );
    }

    #[test]
    fn polling_backs_off_and_quickens_while_watched() {
        assert_eq!(interval(true, 0), Duration::from_secs(30));
        assert_eq!(interval(true, 1), Duration::from_secs(60));
        assert_eq!(interval(false, 0), Duration::from_secs(10 * 60));
        assert_eq!(interval(false, 40), Duration::from_secs(30 * 60));
    }
}
