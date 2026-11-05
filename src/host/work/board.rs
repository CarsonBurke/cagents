//! Turns local scans, forge listings, pins and live sessions into cards. Pure,
//! so a board costs no process and is assembled fresh for every request.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

use super::{forge, scan, store::Ticket};
use crate::{
    agent::{ActivityState, Provider},
    model::TerminalId,
    work::{
        Activity, ActivitySource, Board, Checks, Column, Forge, Head, Item, ItemKey, LinearTicket,
        Project, PullRequest, PullRequestState, Review, Session, TicketSource, Worktree,
    },
};

/// A live session and the directory it runs in, canonicalized by the caller.
#[derive(Debug, Clone)]
pub(super) struct LiveSession {
    pub id: TerminalId,
    pub provider: Option<Provider>,
    pub title: Option<String>,
    pub state: ActivityState,
    pub directory: PathBuf,
}

pub(super) struct ProjectView<'a> {
    pub repository: &'a Path,
    pub name: &'a str,
    pub local: Option<&'a scan::Local>,
    pub scan_error: Option<&'a str>,
    pub forge: &'a Forge,
    pub snapshot: Option<&'a forge::Snapshot>,
}

pub(super) struct Context<'a> {
    pub now_unix_ms: u64,
    pub stale_after_days: u16,
    pub pins: &'a BTreeMap<(PathBuf, String), Column>,
    pub session_activity: &'a HashMap<PathBuf, u64>,
    pub sessions: &'a [LiveSession],
    pub tickets: &'a BTreeMap<String, Ticket>,
    pub linear_unavailable: Option<&'a str>,
}

pub(super) fn assemble(projects: &[ProjectView<'_>], context: &Context<'_>) -> Board {
    let worktrees = projects
        .iter()
        .filter_map(|project| project.local)
        .flat_map(|local| {
            local
                .worktrees
                .iter()
                .map(|worktree| worktree.path.as_path())
        })
        .collect::<Vec<_>>();
    let sessions = sessions_by_worktree(context.sessions, &worktrees);
    let mut items = projects
        .iter()
        .flat_map(|project| project_items(project, &sessions, context))
        .collect::<Vec<_>>();
    crate::work::sort(&mut items);
    Board {
        generated_at_unix_ms: context.now_unix_ms,
        stale_after_days: context.stale_after_days,
        projects: projects
            .iter()
            .map(|project| Project {
                repository: project.repository.to_owned(),
                name: project.name.to_owned(),
                root: project.local.and_then(|local| local.root.clone()),
                base: project
                    .local
                    .and_then(|local| local.base.as_ref())
                    .map(|base| short_ref(&base.refname).to_owned()),
                scanned_at_unix_ms: project.local.map(|local| local.scanned_at_unix_ms),
                scan_error: project.scan_error.map(str::to_owned),
                forge: project.forge.clone(),
            })
            .collect(),
        items,
        linear_unavailable: context.linear_unavailable.map(str::to_owned),
    }
}

/// Each session belongs to the innermost worktree that contains its
/// directory, so a linked worktree nested in its primary checkout keeps its
/// own sessions.
fn sessions_by_worktree<'a>(
    sessions: &'a [LiveSession],
    worktrees: &[&Path],
) -> HashMap<PathBuf, Vec<&'a LiveSession>> {
    let mut placed: HashMap<PathBuf, Vec<&LiveSession>> = HashMap::new();
    for session in sessions {
        if let Some(worktree) = worktrees
            .iter()
            .filter(|worktree| session.directory.starts_with(worktree))
            .max_by_key(|worktree| worktree.components().count())
        {
            placed
                .entry(worktree.to_path_buf())
                .or_default()
                .push(session);
        }
    }
    placed
}

fn project_items(
    project: &ProjectView<'_>,
    sessions: &HashMap<PathBuf, Vec<&LiveSession>>,
    context: &Context<'_>,
) -> Vec<Item> {
    let mut items = Vec::new();
    let mut carded = BTreeSet::new();
    let pin = |key: &ItemKey| {
        context
            .pins
            .get(&(project.repository.to_owned(), key.storage_key()))
            .copied()
    };
    let new_item = |key: ItemKey, branch: Option<String>| Item {
        project: project.repository.to_owned(),
        project_name: project.name.to_owned(),
        key,
        branch,
        worktree: None,
        head: None,
        comparison: None,
        dirty: None,
        pull_request: None,
        linear: None,
        sessions: Vec::new(),
        last_activity: None,
        error: None,
        column: Column::InProgress,
        derived: Column::InProgress,
        pinned: false,
    };

    if let Some(local) = project.local {
        for worktree in &local.worktrees {
            let attached = sessions
                .get(&worktree.path)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let ahead = worktree
                .comparison
                .as_ref()
                .is_some_and(|comparison| comparison.ahead > 0);
            let dirty = worktree.dirty.as_ref().is_some_and(|dirty| dirty.paths > 0);
            // The primary checkout usually sits on the default branch; it is
            // work only while something happens there. An idle shell open in
            // it is not something happening.
            let agent = attached.iter().any(|session| session.provider.is_some());
            if worktree.primary && !ahead && !dirty && !agent {
                continue;
            }
            // `git worktree add --force` can check one branch out twice.
            let key = match &worktree.branch {
                Some(branch) if !carded.contains(branch) => ItemKey::Branch(branch.clone()),
                _ => ItemKey::Worktree(worktree.path.clone()),
            };
            if let Some(branch) = &worktree.branch {
                carded.insert(branch.clone());
            }
            let mut item = new_item(key, worktree.branch.clone());
            item.worktree = Some(Worktree {
                path: worktree.path.clone(),
                primary: worktree.primary,
                locked: worktree.locked,
                created_at_unix_ms: worktree.created_at_unix_ms,
            });
            item.head = worktree.head.clone();
            item.comparison = worktree.comparison.clone();
            item.dirty = worktree.dirty.clone();
            item.error = worktree.error.clone();
            item.pull_request = worktree.branch.as_deref().and_then(|branch| {
                pull_request_for(project, local, branch, worktree.head.as_ref())
            });
            item.linear = match &worktree.link {
                Some(link) => Some(linked_ticket(&link.issue_id, context.tickets)),
                None => worktree
                    .branch
                    .as_deref()
                    .and_then(|branch| branch_ticket(branch, context.tickets)),
            };
            item.sessions = attached
                .iter()
                .map(|session| Session {
                    id: session.id,
                    provider: session.provider,
                    title: session.title.clone(),
                    state: session.state,
                })
                .collect();
            item.last_activity = crate::work::last_activity(
                item.head.as_ref(),
                item.comparison.as_ref(),
                item.dirty.as_ref(),
                context.session_activity.get(&worktree.path).copied(),
                worktree.created_at_unix_ms,
            )
            .or_else(|| pull_request_activity(item.pull_request.as_ref()));
            let pinned = pin(&item.key);
            crate::work::place(
                &mut item,
                pinned,
                context.now_unix_ms,
                context.stale_after_days,
            );
            items.push(item);
        }

        // Branches without a worktree: open pull requests and closed drafts
        // of the user's. A branch whose pull request merged or closed is
        // history, and one without a pull request is not work in progress:
        // agents leave hundreds behind.
        for (name, branch) in &local.branches {
            if carded.contains(name) {
                continue;
            }
            let head = Head {
                oid: branch.oid.clone(),
                committed_at_unix_ms: branch.committed_at_unix_ms,
            };
            let pull_request = pull_request_for(project, local, name, Some(&head));
            let keep = pull_request.as_ref().is_some_and(|pull_request| {
                pull_request.state == PullRequestState::Open
                    || (pull_request.state == PullRequestState::Closed && pull_request.draft)
            });
            if !keep {
                continue;
            }
            carded.insert(name.clone());
            let mut item = new_item(ItemKey::Branch(name.clone()), Some(name.clone()));
            item.comparison = branch.comparison.clone();
            item.head = Some(head);
            item.pull_request = pull_request;
            item.linear = branch_ticket(name, context.tickets);
            item.last_activity = crate::work::last_activity(
                item.head.as_ref(),
                item.comparison.as_ref(),
                None,
                None,
                None,
            )
            .or_else(|| pull_request_activity(item.pull_request.as_ref()));
            let pinned = pin(&item.key);
            crate::work::place(
                &mut item,
                pinned,
                context.now_unix_ms,
                context.stale_after_days,
            );
            items.push(item);
        }
    }

    // The user's open pull requests whose branch exists only on the forge.
    if let Some(snapshot) = project.snapshot {
        let local_names = project.local.map(|local| &local.branch_names);
        for pull_request in &snapshot.pull_requests {
            let name = &pull_request.head_ref_name;
            if pull_request.state.0 != "OPEN"
                || carded.contains(name)
                || local_names.is_some_and(|names| names.contains(name))
            {
                continue;
            }
            carded.insert(name.clone());
            let mut item = new_item(ItemKey::Branch(name.clone()), Some(name.clone()));
            item.pull_request = Some(convert(pull_request, false));
            item.linear = branch_ticket(name, context.tickets);
            item.last_activity = pull_request_activity(item.pull_request.as_ref());
            let pinned = pin(&item.key);
            crate::work::place(
                &mut item,
                pinned,
                context.now_unix_ms,
                context.stale_after_days,
            );
            items.push(item);
        }
    }
    items
}

fn pull_request_for(
    project: &ProjectView<'_>,
    local: &scan::Local,
    branch: &str,
    head: Option<&Head>,
) -> Option<PullRequest> {
    let oid = head.map(|head| head.oid.as_str());
    let pull_request = project.snapshot?.for_branch(branch, oid)?;
    let contains = oid.is_some_and(|oid| {
        oid == pull_request.head_ref_oid
            || local
                .ancestry
                .get(&(oid.to_owned(), pull_request.head_ref_oid.clone()))
                .copied()
                .unwrap_or(false)
    });
    Some(convert(pull_request, contains))
}

fn convert(pull_request: &crate::github::PullRequest, contains_local_head: bool) -> PullRequest {
    let state = match pull_request.state.0.as_str() {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        _ => PullRequestState::Closed,
    };
    // Only the open listing reads rollups; elsewhere checks are unknown,
    // not absent.
    let checks = pull_request.status_check_rollup.as_ref().map(|_| {
        let mut checks = Checks::default();
        for check in pull_request.check_summaries() {
            match check.bucket.0.as_str() {
                "pass" => checks.passed += 1,
                "fail" => checks.failed += 1,
                "pending" => checks.pending += 1,
                _ => {}
            }
        }
        checks
    });
    let review =
        pull_request
            .review_decision
            .as_ref()
            .and_then(|decision| match decision.0.as_str() {
                "APPROVED" => Some(Review::Approved),
                "CHANGES_REQUESTED" => Some(Review::ChangesRequested),
                "REVIEW_REQUIRED" => Some(Review::Required),
                _ => None,
            });
    PullRequest {
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.to_string(),
        state,
        draft: pull_request.is_draft,
        head_oid: pull_request.head_ref_oid.clone(),
        contains_local_head,
        checks,
        review,
        updated_at: pull_request.updated_at.clone(),
    }
}

/// A card with no local activity at all is dated by its pull request.
fn pull_request_activity(pull_request: Option<&PullRequest>) -> Option<Activity> {
    let updated = pull_request?.updated_at.parse::<jiff::Timestamp>().ok()?;
    Some(Activity {
        at_unix_ms: u64::try_from(updated.as_millisecond()).ok()?,
        source: ActivitySource::PullRequest,
    })
}

fn linked_ticket(issue_id: &str, tickets: &BTreeMap<String, Ticket>) -> LinearTicket {
    let ticket = tickets.get(issue_id);
    LinearTicket {
        identifier: ticket.map(|ticket| ticket.identifier.clone()),
        title: ticket.map(|ticket| ticket.title.clone()),
        url: ticket.map(|ticket| ticket.url.clone()),
        source: TicketSource::Link,
    }
}

/// The ticket a Linear-style branch name names, once Linear confirmed it:
/// `fix/discard-1117-x` looks like one and is not. Linear also answers a
/// former team key with the issue's current identifier, so `fix/issue-1403`
/// about GitHub issue 1403 could come back as `TEAM-1403`; only an answer
/// under the same identifier confirms a guess.
fn branch_ticket(branch: &str, tickets: &BTreeMap<String, Ticket>) -> Option<LinearTicket> {
    let identifier = crate::work::ticket_from_branch(branch)?;
    let ticket = tickets
        .get(&identifier)
        .filter(|ticket| ticket.identifier.eq_ignore_ascii_case(&identifier))?;
    Some(LinearTicket {
        identifier: Some(ticket.identifier.clone()),
        title: Some(ticket.title.clone()),
        url: Some(ticket.url.clone()),
        source: TicketSource::BranchName,
    })
}

fn short_ref(refname: &str) -> &str {
    refname
        .strip_prefix("refs/remotes/")
        .or_else(|| refname.strip_prefix("refs/heads/"))
        .unwrap_or(refname)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{github::GitHubRepoKey, work::Comparison};

    #[test]
    fn branch_tickets_need_linear_to_answer_under_the_same_identifier() {
        let ticket = |identifier: &str| Ticket {
            identifier: identifier.into(),
            title: "Title".into(),
            url: format!("https://linear.app/team/issue/{identifier}"),
        };
        let tickets = BTreeMap::from([
            ("TEAM-123".to_owned(), ticket("TEAM-123")),
            ("ISSUE-1403".to_owned(), ticket("TEAM-1403")),
        ]);
        let confirmed = branch_ticket("fix/team-123-display", &tickets).unwrap();
        assert_eq!(confirmed.identifier.as_deref(), Some("TEAM-123"));
        assert_eq!(confirmed.source, TicketSource::BranchName);
        assert_eq!(branch_ticket("fix/issue-1403-compact", &tickets), None);
        assert_eq!(branch_ticket("fix/main-1-unasked", &tickets), None);
    }

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 60 * 60 * 1000;

    fn worktree(path: &str, branch: Option<&str>, primary: bool) -> scan::Worktree {
        scan::Worktree {
            id: crate::repository::stable_worktree_id(Path::new(path)),
            path: path.into(),
            primary,
            locked: false,
            created_at_unix_ms: Some(NOW - 100 * HOUR),
            branch: branch.map(str::to_owned),
            head: Some(Head {
                oid: format!("{}-oid", branch.unwrap_or("detached")),
                committed_at_unix_ms: Some(NOW - 30 * HOUR),
            }),
            comparison: Some(Comparison {
                base: "origin/main".into(),
                ahead: u32::from(!primary),
                behind: 0,
            }),
            dirty: Some(crate::work::Dirty::default()),
            link: None,
            error: None,
        }
    }

    fn pull_request(
        branch: &str,
        oid: &str,
        state: &str,
        draft: bool,
    ) -> crate::github::PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": 7,
            "title": "Seven",
            "state": state,
            "isDraft": draft,
            "headRefName": branch,
            "headRefOid": oid,
            "headRepository": {"name": "repo"},
            "headRepositoryOwner": {"login": "me"},
            "baseRefName": "main",
            "updatedAt": "2026-10-01T00:00:00Z",
            "url": "https://github.com/me/repo/pull/7",
        }))
        .unwrap()
    }

    fn snapshot(pull_requests: Vec<crate::github::PullRequest>) -> forge::Snapshot {
        let key = GitHubRepoKey::new("github.com", None, "me", "repo").unwrap();
        forge::Snapshot {
            repository: key.clone(),
            remotes: vec![key],
            pull_requests,
            others: BTreeMap::new(),
        }
    }

    struct Fixture {
        local: scan::Local,
        snapshot: forge::Snapshot,
        pins: BTreeMap<(PathBuf, String), Column>,
        activity: HashMap<PathBuf, u64>,
        sessions: Vec<LiveSession>,
    }

    impl Fixture {
        fn board(&self) -> Board {
            let forge = Forge::Synced {
                at_unix_ms: NOW,
                error: None,
            };
            assemble(
                &[ProjectView {
                    repository: Path::new("/repo/.git"),
                    name: "repo",
                    local: Some(&self.local),
                    scan_error: None,
                    forge: &forge,
                    snapshot: Some(&self.snapshot),
                }],
                &Context {
                    now_unix_ms: NOW,
                    stale_after_days: 7,
                    pins: &self.pins,
                    session_activity: &self.activity,
                    sessions: &self.sessions,
                    tickets: &BTreeMap::new(),
                    linear_unavailable: None,
                },
            )
        }
    }

    fn fixture() -> Fixture {
        let mut local = scan::Local::default();
        local.worktrees = vec![
            worktree("/repo", Some("main"), true),
            worktree("/repo/.worktrees/feature", Some("feature"), false),
            worktree("/repo/.worktrees/landed", Some("landed"), false),
        ];
        local.branches.insert(
            "draft".into(),
            scan::Branch {
                oid: "draft-oid".into(),
                committed_at_unix_ms: Some(NOW - 50 * HOUR),
                worktree: None,
                comparison: Some(Comparison {
                    base: "origin/main".into(),
                    ahead: 2,
                    behind: 0,
                }),
            },
        );
        local.branches.insert(
            "merged-plain".into(),
            scan::Branch {
                oid: "merged-plain-oid".into(),
                committed_at_unix_ms: Some(NOW - 50 * HOUR),
                worktree: None,
                comparison: Some(Comparison {
                    base: "origin/main".into(),
                    ahead: 1,
                    behind: 0,
                }),
            },
        );
        local.branches.insert(
            "unshared".into(),
            scan::Branch {
                oid: "unshared-oid".into(),
                committed_at_unix_ms: Some(NOW - HOUR),
                worktree: None,
                comparison: Some(Comparison {
                    base: "origin/main".into(),
                    ahead: 3,
                    behind: 0,
                }),
            },
        );
        local.branch_names = [
            "main",
            "feature",
            "landed",
            "draft",
            "merged-plain",
            "unshared",
        ]
        .map(str::to_owned)
        .into();
        let mut merged = pull_request("landed", "landed-oid", "MERGED", false);
        merged.number = 8;
        let mut merged_plain = pull_request("merged-plain", "merged-plain-oid", "MERGED", false);
        merged_plain.number = 9;
        let mut remote = pull_request("remote-only", "r", "OPEN", false);
        remote.number = 10;
        Fixture {
            local,
            snapshot: snapshot(vec![
                pull_request("feature", "feature-oid", "OPEN", false),
                merged,
                pull_request("draft", "old", "CLOSED", true),
                merged_plain,
                remote,
            ]),
            pins: BTreeMap::new(),
            activity: HashMap::new(),
            sessions: Vec::new(),
        }
    }

    fn column(board: &Board, label: &str) -> Option<Column> {
        board
            .items
            .iter()
            .find(|item| item.branch.as_deref() == Some(label))
            .map(|item| item.column)
    }

    #[test]
    fn cards_cover_worktrees_drafts_and_remote_branches_but_not_history() {
        let board = fixture().board();
        assert_eq!(column(&board, "feature"), Some(Column::InReview));
        assert_eq!(column(&board, "landed"), Some(Column::Landed));
        assert_eq!(column(&board, "draft"), Some(Column::InProgress));
        assert_eq!(column(&board, "remote-only"), Some(Column::Stale));
        assert_eq!(column(&board, "main"), None, "an idle primary checkout");
        assert_eq!(
            column(&board, "merged-plain"),
            None,
            "a merged plain branch"
        );
        assert_eq!(
            column(&board, "unshared"),
            None,
            "no worktree and no pull request"
        );
        assert_eq!(board.projects[0].name, "repo");
    }

    #[test]
    fn sessions_attach_to_their_innermost_worktree_and_make_it_active() {
        let mut fixture = fixture();
        fixture.sessions.push(LiveSession {
            id: TerminalId::new(),
            provider: Some(Provider::Claude),
            title: Some("fix".into()),
            state: ActivityState::Working,
            directory: "/repo/.worktrees/landed/src".into(),
        });
        fixture.sessions.push(LiveSession {
            id: TerminalId::new(),
            provider: None,
            title: None,
            state: ActivityState::Unknown,
            directory: "/repo/docs".into(),
        });
        let board = fixture.board();
        let landed = board
            .items
            .iter()
            .find(|item| item.branch.as_deref() == Some("landed"))
            .unwrap();
        assert_eq!(landed.sessions.len(), 1);
        assert_eq!(landed.column, Column::Active);
        assert_eq!(
            column(&board, "main"),
            None,
            "a plain shell in the primary checkout is not work"
        );

        fixture.sessions.push(LiveSession {
            id: TerminalId::new(),
            provider: Some(Provider::Codex),
            title: None,
            state: ActivityState::Idle,
            directory: "/repo".into(),
        });
        assert_eq!(
            column(&fixture.board(), "main"),
            Some(Column::InProgress),
            "an agent in the primary checkout gives it a card"
        );
    }

    #[test]
    fn a_branch_checked_out_twice_gets_a_card_per_worktree() {
        let mut fixture = fixture();
        fixture
            .local
            .worktrees
            .push(worktree("/repo/.worktrees/again", Some("feature"), false));
        let board = fixture.board();
        let keys = board
            .items
            .iter()
            .filter(|item| item.branch.as_deref() == Some("feature"))
            .map(|item| item.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&ItemKey::Branch("feature".into())));
        assert!(keys.contains(&ItemKey::Worktree("/repo/.worktrees/again".into())));
    }

    #[test]
    fn pins_override_the_derived_column_and_activity_orders_cards() {
        let mut fixture = fixture();
        fixture.pins.insert(
            (PathBuf::from("/repo/.git"), "branch:landed".into()),
            Column::InProgress,
        );
        fixture
            .activity
            .insert("/repo/.worktrees/feature".into(), NOW - HOUR);
        let board = fixture.board();
        let landed = board
            .items
            .iter()
            .find(|item| item.branch.as_deref() == Some("landed"))
            .unwrap();
        assert_eq!(
            (landed.column, landed.derived, landed.pinned),
            (Column::InProgress, Column::Landed, true)
        );
        assert_eq!(board.items[0].branch.as_deref(), Some("feature"));
        assert_eq!(board.items[0].column, Column::Active);
        assert_eq!(
            board.items[0].last_activity.unwrap().source,
            ActivitySource::Session
        );
    }

    #[test]
    fn checks_and_reviews_are_read_only_where_the_listing_has_them() {
        let mut open = pull_request("feature", "feature-oid", "OPEN", false);
        assert_eq!(convert(&open, true).checks, None);
        open.status_check_rollup = Some(Vec::new());
        open.review_decision = Some(crate::github::RemoteValue("CHANGES_REQUESTED".into()));
        let converted = convert(&open, true);
        assert_eq!(converted.checks, Some(Checks::default()));
        assert_eq!(converted.review, Some(Review::ChangesRequested));
    }
}
