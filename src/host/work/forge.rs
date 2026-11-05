//! The user's pull requests in each GitHub repository, read by bounded `gh`
//! calls on a thread of their own, so a slow or failing GitHub never delays a
//! local scan. Two listings serve every card of a repository; a checked-out
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
        AuthoredPullRequestsRequest, GitHubRepoKey, GitHubService, PullRequest,
        PullRequestStateFilter, remote_repository_key,
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
    cancellation: &CancellationToken,
) -> Outcome {
    match read(target, memory, cancellation) {
        Ok(Some(snapshot)) => Outcome::Synced(snapshot),
        Ok(None) => Outcome::NotGitHub,
        Err(error) => Outcome::Failed(error),
    }
}

fn read(
    target: &Target,
    memory: &mut Memory,
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
                .resolve_repository(from, cancellation)
                .map_err(|error| error.to_string())?;
            memory.repository = Some((repository.clone(), Instant::now()));
            repository
        }
    };
    if !keys.iter().any(|key| key.same_repository(&repository)) {
        keys.push(repository.clone());
    }

    let request = |state, limit, review| AuthoredPullRequestsRequest {
        repository: repository.clone(),
        state,
        limit,
        review,
    };
    let (authored, open) = std::thread::scope(|scope| {
        let open = scope.spawn(|| {
            gh.list_authored(
                &request(PullRequestStateFilter::Open, OPEN, true),
                cancellation,
            )
        });
        let authored = gh.list_authored(
            &request(PullRequestStateFilter::All, AUTHORED, false),
            cancellation,
        );
        (
            authored,
            open.join().unwrap_or_else(|_| {
                Err(crate::github::GitHubError::Inspection(
                    "the open pull request listing panicked".into(),
                ))
            }),
        )
    });
    let authored = authored.map_err(|error| error.to_string())?;
    let open = open.map_err(|error| error.to_string())?;
    let pull_requests = merge(authored, open);

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

/// One list, newest first, in which an open pull request's entry carries the
/// review and check state its second listing read.
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

/// How stale a listing may be while someone reads the board.
pub(super) const WATCHED_INTERVAL: Duration = Duration::from_secs(30);
/// Unread, a listing only keeps merge state and pins current.
const UNWATCHED_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// A project without a GitHub remote is asked again this rarely; adding one
/// is rare and the next read of the board notices it within the hour.
pub(super) const NOT_GITHUB_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// When the next poll is due: sooner while someone is reading the board,
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

    use super::*;
    use crate::github::RemoteValue;

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

    #[test]
    fn polling_backs_off_and_quickens_while_watched() {
        assert_eq!(interval(true, 0), Duration::from_secs(30));
        assert_eq!(interval(true, 1), Duration::from_secs(60));
        assert_eq!(interval(false, 0), Duration::from_secs(10 * 60));
        assert_eq!(interval(false, 40), Duration::from_secs(30 * 60));
    }
}
