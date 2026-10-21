//! `cagents work`: the daemon's board of every worktree and branch in
//! progress, newest activity first.
use super::*;
use crate::work::{
    ActivitySource, Board, CheckOutcome, Column, Forge, Item, PullRequestState, Request, Response,
    Review, Scope,
};

/// How long a board may wait on its first scan or a fresh rescan, on top of
/// the daemon's own bound for each.
const BOARD_TIMEOUT: Duration = Duration::from_secs(15);
const FRESH_BOARD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// List every worktree and branch in progress, newest activity first.
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Pin a card to a column, overriding the column its state implies.
    Pin {
        /// The card's branch, or its worktree's path.
        item: String,
        /// active, in-progress, in-review, landed or stale.
        column: Column,
        /// The open project the card belongs to, by name or path.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Remove a card's pin, returning it to the column its state implies.
    Unpin {
        /// The card's branch, or its worktree's path.
        item: String,
        /// The open project the card belongs to, by name or path.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Only the open project with this name, or containing this path.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Only cards in this column: active, in-progress, in-review, landed or
    /// stale.
    #[arg(long)]
    column: Option<Column>,
    /// Rescan the projects and poll GitHub first instead of reading the
    /// daemon's latest facts.
    #[arg(long)]
    fresh: bool,
}

impl ControlClient {
    fn work(&mut self, request: Request, timeout: Duration) -> Result<Response, ControlError> {
        match self.request_with_deadline(ClientRequest::Work(request), Instant::now() + timeout)? {
            ServerResponse::Work(response) => Ok(response),
            response => Err(unexpected_response("Work board", &response)),
        }
    }
}

fn scope(project: Option<PathBuf>) -> Result<Scope, AppError> {
    Ok(match project {
        Some(project) => Scope::Project(super::project_argument(project)?),
        None => Scope::All,
    })
}

/// A card named by its worktree's path is sent as that path made canonical,
/// as the daemon knows it; anything else is a branch name.
fn item_argument(item: String) -> String {
    let path = Path::new(&item);
    if path.join(".git").exists()
        && let Ok(canonical) = std::fs::canonicalize(path)
    {
        return canonical.to_string_lossy().into_owned();
    }
    item
}

fn send(request: Request, timeout: Duration) -> Result<Response, AppError> {
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    client.work(request, timeout).map_err(protocol_error)
}

pub(super) fn execute(command: WorkCommand) -> Result<CommandOutput, AppError> {
    match command {
        WorkCommand::List(arguments) => list(arguments),
        WorkCommand::Pin {
            item,
            column,
            project,
        } => pin(item, Some(column), project),
        WorkCommand::Unpin { item, project } => pin(item, None, project),
    }
}

fn list(arguments: ListArgs) -> Result<CommandOutput, AppError> {
    let request = Request::Board {
        scope: scope(arguments.project)?,
        fresh: arguments.fresh,
    };
    let timeout = if arguments.fresh {
        FRESH_BOARD_TIMEOUT
    } else {
        BOARD_TIMEOUT
    };
    let Response::Board(mut board) = send(request, timeout)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    if let Some(column) = arguments.column {
        board.items.retain(|item| item.column == column);
    }
    let human = board_human(
        &board,
        arguments.column,
        crate::chat::now_ms().unwrap_or_default(),
    );
    Ok(CommandOutput {
        command: "work.list",
        data: json!({ "board": board }),
        human,
    })
}

fn pin(item: String, column: Option<Column>, project: Option<PathBuf>) -> Result<CommandOutput, AppError> {
    let request = Request::Pin {
        scope: scope(project)?,
        item: item_argument(item),
        column,
    };
    let Response::Pinned(item) = send(request, BOARD_TIMEOUT)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    let human = match column {
        Some(column) => format!("Pinned {} to {}", item.label(), column.label()),
        None => format!(
            "Unpinned {}; it is in {}",
            item.label(),
            item.column.label()
        ),
    };
    Ok(CommandOutput {
        command: if column.is_some() { "work.pin" } else { "work.unpin" },
        data: json!({ "item": item }),
        human,
    })
}

/// One table per column, in board order, then what kept any project from
/// being read in full. `filtered_to` names the column the cards were
/// filtered to.
fn board_human(board: &Board, filtered_to: Option<Column>, now_ms: u64) -> String {
    let mut sections = Vec::new();
    for column in Column::ALL {
        let items = board
            .items
            .iter()
            .filter(|item| item.column == column)
            .collect::<Vec<_>>();
        if items.is_empty() {
            continue;
        }
        let rows = items
            .iter()
            .map(|item| row(item, now_ms))
            .collect::<Vec<_>>();
        sections.push(format!(
            "{} ({})\n{}",
            column.label().to_uppercase(),
            items.len(),
            super::format::table(
                ["CARD", "PROJECT", "ACTIVITY", "BASE", "CHANGES", "PULL REQUEST", "TICKET", "SESSIONS", "WORKTREE"],
                rows,
                "",
            )
        ));
    }
    if sections.is_empty() {
        sections.push(match filtered_to {
            Some(column) => format!("No cards in {}", column.label()),
            None => "No cards".into(),
        });
    }
    let notes = board
        .projects
        .iter()
        .flat_map(|project| {
            let name = super::format::plain(&project.name);
            let scan = project
                .scan_error
                .as_deref()
                .map(|error| format!("{name}: Git could not be read: {}", super::format::plain(error)));
            let forge = match &project.forge {
                Forge::Unavailable { reason } => Some(format!(
                    "{name}: GitHub is unavailable: {}",
                    super::format::plain(reason)
                )),
                Forge::Synced {
                    at_unix_ms,
                    error: Some(error),
                } => Some(format!(
                    "{name}: pull requests are from {}; GitHub failed since: {}",
                    super::format::relative_time(*at_unix_ms, now_ms),
                    super::format::plain(error)
                )),
                Forge::Pending | Forge::NotGitHub | Forge::Synced { error: None, .. } => None,
            };
            scan.into_iter().chain(forge)
        })
        .chain(board.items.iter().filter_map(|item| {
            let error = item.error.as_deref()?;
            Some(format!(
                "{}: its changes could not be read: {}",
                super::format::plain(&item.label()),
                super::format::plain(error)
            ))
        }))
        .chain(board.linear_unavailable.as_deref().map(|reason| {
            format!(
                "Linear tickets could not be looked up: {}",
                super::format::plain(reason)
            )
        }))
        .collect::<Vec<_>>();
    if !notes.is_empty() {
        sections.push(notes.join("\n"));
    }
    sections.join("\n\n")
}

fn row(item: &Item, now_ms: u64) -> [String; 9] {
    let mut card = item.label();
    if item.pinned {
        card.push_str(" (pinned)");
    }
    let activity = item.last_activity.map_or_else(
        || "-".to_owned(),
        |activity| {
            format!(
                "{} ({})",
                super::format::relative_time(activity.at_unix_ms, now_ms),
                match activity.source {
                    ActivitySource::Commit => "commit",
                    ActivitySource::Files => "files",
                    ActivitySource::Session => "agent",
                    ActivitySource::Created => "created",
                    ActivitySource::PullRequest => "pull request",
                }
            )
        },
    );
    let base = item.comparison.as_ref().map_or_else(
        || "-".to_owned(),
        |comparison| format!("+{} -{}", comparison.ahead, comparison.behind),
    );
    let changes = match &item.dirty {
        Some(dirty) if dirty.conflicted > 0 => {
            format!("{} ({} conflicted)", dirty.paths, dirty.conflicted)
        }
        Some(dirty) if dirty.paths > 0 => dirty.paths.to_string(),
        Some(_) => "clean".into(),
        None => "-".into(),
    };
    let pull_request = item.pull_request.as_ref().map_or_else(
        || "-".to_owned(),
        |pull_request| {
            let mut parts = vec![format!(
                "#{} {}",
                pull_request.number,
                match (pull_request.state, pull_request.draft) {
                    (PullRequestState::Open, true) => "draft",
                    (PullRequestState::Open, false) => "open",
                    (PullRequestState::Closed, true) => "closed draft",
                    (PullRequestState::Closed, false) => "closed",
                    (PullRequestState::Merged, _) => "merged",
                }
            )];
            if let Some(outcome) = pull_request.checks.and_then(|checks| checks.outcome()) {
                parts.push(
                    match outcome {
                        CheckOutcome::Passing => "checks pass",
                        CheckOutcome::Pending => "checks pending",
                        CheckOutcome::Failing => "checks fail",
                    }
                    .into(),
                );
            }
            if let Some(review) = pull_request.review {
                parts.push(
                    match review {
                        Review::Approved => "approved",
                        Review::ChangesRequested => "changes requested",
                        Review::Required => "review required",
                    }
                    .into(),
                );
            }
            parts.join(", ")
        },
    );
    let ticket = item
        .linear
        .as_ref()
        .and_then(|ticket| ticket.identifier.clone())
        .unwrap_or_else(|| "-".into());
    let sessions = if item.sessions.is_empty() {
        "-".to_owned()
    } else {
        item.sessions
            .iter()
            .map(|session| {
                format!(
                    "{} {}",
                    session.provider.map_or("shell", |provider| provider.executable()),
                    match session.state {
                        crate::agent::ActivityState::Working => "working",
                        crate::agent::ActivityState::Waiting => "waiting",
                        crate::agent::ActivityState::Idle => "idle",
                        crate::agent::ActivityState::Unknown => "running",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    [
        card,
        item.project_name.clone(),
        activity,
        base,
        changes,
        pull_request,
        ticket,
        sessions,
        item.worktree
            .as_ref()
            .map_or_else(|| "-".to_owned(), |worktree| worktree.path.display().to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{
        Activity, Checks, Comparison, Dirty, ItemKey, LinearTicket, PullRequest, Session,
        TicketSource, Worktree,
    };

    const NOW: u64 = 1_800_000_000_000;

    fn item() -> Item {
        Item {
            project: "/repo/.git".into(),
            project_name: "repo".into(),
            key: ItemKey::Branch("fix/main-12-login".into()),
            branch: Some("fix/main-12-login".into()),
            worktree: Some(Worktree {
                path: "/repo/.worktrees/login".into(),
                primary: false,
                locked: false,
                created_at_unix_ms: None,
            }),
            head: None,
            comparison: Some(Comparison {
                base: "origin/main".into(),
                ahead: 3,
                behind: 1,
            }),
            dirty: Some(Dirty {
                paths: 4,
                conflicted: 1,
                newest_modified_at_unix_ms: Some(NOW - 60_000),
            }),
            pull_request: Some(PullRequest {
                number: 42,
                title: "Fix login".into(),
                url: "https://github.com/me/repo/pull/42".into(),
                state: PullRequestState::Open,
                draft: false,
                head_oid: "a".into(),
                contains_local_head: true,
                checks: Some(Checks {
                    passed: 3,
                    failed: 1,
                    pending: 0,
                }),
                review: Some(Review::Approved),
                updated_at: "2026-10-01T00:00:00Z".into(),
            }),
            linear: Some(LinearTicket {
                identifier: Some("MAIN-12".into()),
                title: None,
                url: None,
                source: TicketSource::BranchName,
            }),
            sessions: vec![Session {
                id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                provider: Some(crate::agent::Provider::Claude),
                title: None,
                state: crate::agent::ActivityState::Working,
            }],
            last_activity: Some(Activity {
                at_unix_ms: NOW - 60_000,
                source: ActivitySource::Files,
            }),
            error: None,
            column: Column::Active,
            derived: Column::Active,
            pinned: true,
        }
    }

    #[test]
    fn rows_name_every_fact_a_card_carries() {
        assert_eq!(
            row(&item(), NOW),
            [
                "fix/main-12-login (pinned)".to_owned(),
                "repo".into(),
                "1 min ago (files)".into(),
                "+3 -1".into(),
                "4 (1 conflicted)".into(),
                "#42 open, checks fail, approved".into(),
                "MAIN-12".into(),
                "claude working".into(),
                "/repo/.worktrees/login".into(),
            ]
        );
    }

    #[test]
    fn boards_group_by_column_and_report_what_could_not_be_read() {
        let mut stale = item();
        stale.column = Column::Stale;
        stale.key = ItemKey::Branch("old".into());
        stale.branch = Some("old".into());
        let board = Board {
            generated_at_unix_ms: NOW,
            stale_after_days: 7,
            projects: vec![crate::work::Project {
                repository: "/repo/.git".into(),
                name: "repo".into(),
                root: Some("/repo".into()),
                base: Some("origin/main".into()),
                scanned_at_unix_ms: Some(NOW),
                scan_error: None,
                forge: Forge::Unavailable {
                    reason: "gh is not\ninstalled".into(),
                },
            }],
            items: vec![item(), stale],
            linear_unavailable: Some("Add a Linear key".into()),
        };
        let human = board_human(&board, None, NOW);
        let active = human.find("ACTIVE (1)").unwrap();
        let stale = human.find("STALE (1)").unwrap();
        assert!(active < stale);
        assert!(human.ends_with(
            "repo: GitHub is unavailable: gh is not installed\n\
             Linear tickets could not be looked up: Add a Linear key"
        ));
        let empty = Board {
            items: Vec::new(),
            projects: Vec::new(),
            linear_unavailable: None,
            ..board
        };
        assert_eq!(board_human(&empty, None, NOW), "No cards");
        assert_eq!(
            board_human(&empty, Some(Column::Landed), NOW),
            "No cards in Landed"
        );
    }
}
