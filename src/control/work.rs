//! `cagents work`: the daemon's board of every worktree and branch in
//! progress, newest activity first.
use super::*;
use crate::work::{
    ActivitySource, Board, CheckOutcome, Column, Forge, Item, Keep, PullRequestState, Removal,
    RemovalPlan, Removed, Request, Response, Review, SavedSnapshot, Scope,
};

/// How long a board may wait on its first scan or a fresh rescan, on top of
/// the daemon's own bound for each.
const BOARD_TIMEOUT: Duration = Duration::from_secs(15);
const FRESH_BOARD_TIMEOUT: Duration = Duration::from_secs(30);
/// An inspection reads every file of a large worktree; a fetch may add the
/// daemon's own minute.
const PLAN_TIMEOUT: Duration = Duration::from_secs(90);
const FETCHED_PLAN_TIMEOUT: Duration = Duration::from_secs(150);
/// Removal inspects twice, or inspects and snapshots, then gives Git up to
/// ten minutes to delete the files.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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
    /// Remove a card's worktree, keeping its branch. Removal is refused while
    /// a session or process uses the worktree, and unless nothing unsaved
    /// would be lost or --snapshot or --discard says what happens to it.
    #[command(visible_alias = "remove")]
    Rm(RemoveArgs),
    /// List the snapshots worktree removals kept, newest first. Restore one
    /// with `git stash apply --index REF` in any worktree of its project.
    Snapshots {
        /// Only the open project with this name, or containing this path.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// The card's branch, or its worktree's path.
    item: String,
    /// The open project the card belongs to, by name or path.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Keep unsaved changes as a snapshot before removing the worktree.
    #[arg(long, conflicts_with = "discard")]
    snapshot: bool,
    /// Delete unsaved changes with the worktree.
    #[arg(long)]
    discard: bool,
    /// Delete the branch too; allowed only when its work landed.
    #[arg(long)]
    delete_branch: bool,
    /// Show what removal would keep and lose, and remove nothing.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Fetch the project's remotes first, so work that landed upstream
    /// counts as landed.
    #[arg(long)]
    fetch: bool,
    /// Remove only if the worktree is still as the plan with this token,
    /// from `--dry-run --json`, found it.
    #[arg(long)]
    token: Option<String>,
    /// Confirm the removal.
    #[arg(long, short)]
    yes: bool,
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

    /// Rescans every open project and polls GitHub, waiting within the
    /// daemon's bound for the board those produce. A watching connection is
    /// sent that board as well.
    pub fn refresh_work_board(&mut self) -> Result<(), ControlError> {
        self.work(
            Request::Board {
                scope: Scope::All,
                fresh: true,
            },
            FRESH_BOARD_TIMEOUT,
        )
        .map(drop)
    }

    /// Pins the card `item` (its storage key, a branch name or a worktree
    /// path) of the project `repository` to `column`, or unpins it.
    pub fn pin_work_card(
        &mut self,
        repository: PathBuf,
        item: String,
        column: Option<Column>,
    ) -> Result<(), ControlError> {
        self.work(
            Request::Pin {
                scope: Scope::Project(repository),
                item,
                column,
            },
            BOARD_TIMEOUT,
        )
        .map(drop)
    }

    /// Subscribes this connection to the Work board and returns it, with the
    /// revision later boards from [`Self::next_work_board`] supersede.
    pub fn watch_work(&mut self) -> Result<(u64, Board), ControlError> {
        match self
            .request_with_deadline(ClientRequest::WatchWork, Instant::now() + BOARD_TIMEOUT)?
        {
            ServerResponse::Work(Response::Watching { revision, board }) => Ok((revision, *board)),
            response => Err(unexpected_response("a Work board subscription", &response)),
        }
    }

    /// Blocks until the daemon sends a board newer than `revision`.
    ///
    /// A board change has no deadline, so this drops the read timeout rather
    /// than polling: a timeout expiring mid-frame would desynchronize the
    /// stream, and a subscribed connection issues no further requests.
    pub fn next_work_board(&mut self, revision: u64) -> Result<(u64, Board), ControlError> {
        if let Some((observed, board)) = self.observed_work.take()
            && observed > revision
        {
            return Ok((observed, *board));
        }
        self.stream
            .set_read_timeout(None)
            .map_err(ControlError::Configure)?;
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.message {
                ServerMessage::WorkBoard {
                    revision: observed,
                    board,
                } if observed > revision => return Ok((observed, *board)),
                // The daemon is about to close this connection. Reporting it as
                // a closed connection is what makes a watcher reconnect and
                // resubscribe to the successor's board.
                ServerMessage::HostShuttingDown => return Err(ControlError::ConnectionClosed),
                ServerMessage::Error(error) => {
                    return Err(ControlError::Remote {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                        details: error.details,
                    });
                }
                _ => {}
            }
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
    send_with(request, timeout, protocol_error)
}

fn send_with(
    request: Request,
    timeout: Duration,
    failed: impl FnOnce(ControlError) -> AppError,
) -> Result<Response, AppError> {
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    // Inside a hosted session the daemon lets this read the board but never
    // change it, as it can tell once the connection names its authority.
    let authority = Authority::from_environment();
    if authority.token().is_some() && matches!(request, Request::Pin { .. } | Request::Remove(_)) {
        client.authenticate(authority).map_err(protocol_error)?;
    }
    client.work(request, timeout).map_err(failed)
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
        WorkCommand::Rm(arguments) => remove(arguments),
        WorkCommand::Snapshots { project } => snapshots(project),
    }
}

fn remove(arguments: RemoveArgs) -> Result<CommandOutput, AppError> {
    let scope = scope(arguments.project)?;
    let item = item_argument(arguments.item);
    let request = Request::RemovalPlan {
        scope: scope.clone(),
        item: item.clone(),
        fetch: arguments.fetch,
    };
    let timeout = if arguments.fetch {
        FETCHED_PLAN_TIMEOUT
    } else {
        PLAN_TIMEOUT
    };
    let Response::RemovalPlan(plan) = send(request, timeout)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    let described = plan_human(&plan);
    if arguments.dry_run {
        return Ok(CommandOutput {
            command: "work.rm",
            data: json!({ "plan": plan, "removed": null }),
            human: described,
        });
    }
    let keep = if arguments.snapshot {
        Keep::Snapshot
    } else if arguments.discard {
        Keep::Discard
    } else {
        Keep::Nothing
    };
    plan.permits(keep, arguments.delete_branch)
        .map_err(|reason| {
            AppError::new("work_removal_refused", format!("{described}\n\n{reason}"))
        })?;
    let token = match (arguments.token, plan.inspection.token.clone()) {
        (Some(expected), Some(current)) if expected != current => {
            return Err(AppError::new(
                "work_removal_changed",
                format!(
                    "{described}\n\nThe worktree changed since the plan with that token; review this plan and retry"
                ),
            ));
        }
        (_, Some(current)) => current,
        (_, None) => return Err(AppError::new("work_removal_refused", described)),
    };
    // This command's own working directory would refuse the removal.
    if std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .is_ok_and(|directory| directory.starts_with(&plan.inspection.path))
    {
        return Err(AppError::new(
            "work_removal_refused",
            format!("{described}\n\nThis shell is inside the worktree; leave it and retry"),
        ));
    }
    if !arguments.yes {
        return Err(AppError::new(
            "work_removal_unconfirmed",
            format!("{described}\n\nPass --yes to remove it, or --dry-run to only see this plan"),
        ));
    }
    let request = Request::Remove(Box::new(Removal {
        scope,
        item,
        token,
        keep,
        delete_branch: arguments.delete_branch,
    }));
    let unanswered = |error: ControlError| match error {
        ControlError::ReplyTimeout { .. } | ControlError::ConnectionClosed => AppError::new(
            "work_removal_unanswered",
            format!(
                "{error}. The removal may still finish: see `cagents work list`, and `cagents work snapshots` for unsaved work it kept"
            ),
        ),
        error => protocol_error(error),
    };
    let Response::Removed(removed) = send_with(request, REMOVE_TIMEOUT, unanswered)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    Ok(CommandOutput {
        command: "work.rm",
        human: removed_human(&removed),
        data: json!({ "plan": plan, "removed": removed }),
    })
}

fn snapshots(project: Option<PathBuf>) -> Result<CommandOutput, AppError> {
    let request = Request::Snapshots {
        scope: scope(project)?,
    };
    let Response::Snapshots(snapshots) = send(request, BOARD_TIMEOUT)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    let human = snapshots_human(&snapshots, crate::chat::now_ms().unwrap_or_default());
    Ok(CommandOutput {
        command: "work.snapshots",
        data: json!({ "snapshots": snapshots }),
        human,
    })
}

/// The plan's verdict, then what it counted and what removal deletes.
fn plan_human(plan: &RemovalPlan) -> String {
    let inspection = &plan.inspection;
    let mut lines = vec![
        format!(
            "{} in {}: {}",
            super::format::plain(&plan.item),
            super::format::plain(&plan.project_name),
            inspection.path.display()
        ),
        plan.summary(),
    ];
    if let Some(number) = plan.merged_pull_request {
        lines.push(format!("Pull request #{number} merged at this commit."));
    }
    if inspection.staged + inspection.modified + inspection.untracked > 0 {
        lines.push(format!(
            "Changes: {} staged, {} modified, {} untracked ({})",
            inspection.staged,
            inspection.modified,
            inspection.untracked,
            super::format::size(inspection.untracked_bytes)
        ));
    }
    let listed = |title: &str, paths: &[String]| {
        (!paths.is_empty()).then(|| {
            format!(
                "{title}:\n{}",
                paths
                    .iter()
                    .map(|path| format!("  {}", super::format::plain(path)))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
    };
    lines.extend(listed(
        "Changed on their way into Git, so no snapshot keeps them",
        &inspection.filtered,
    ));
    lines.extend(listed(
        "Repositories inside it, deleted whole",
        &inspection.nested,
    ));
    if inspection.ignored_total > 0 {
        let shown = inspection
            .ignored
            .iter()
            .map(|entry| {
                if entry.directory {
                    format!("{}/", entry.path.trim_end_matches('/'))
                } else {
                    entry.path.clone()
                }
            })
            .collect::<Vec<_>>();
        let mut section = listed(
            &format!("Ignored, deleted with it ({})", inspection.ignored_total),
            &shown,
        )
        .unwrap_or_default();
        let hidden = (inspection.ignored_total as usize).saturating_sub(shown.len());
        if hidden > 0 {
            section.push_str(&format!("\n  and {hidden} more"));
        }
        lines.push(section);
    }
    lines.join("\n")
}

fn removed_human(removed: &Removed) -> String {
    let mut lines = vec![format!("Removed {}", removed.path.display())];
    if let Some(snapshot) = &removed.snapshot {
        lines.push(format!(
            "Unsaved work is in {}; restore it with `git stash apply --index {}`",
            snapshot.refname, snapshot.refname
        ));
    }
    if let Some(branch) = &removed.deleted_branch {
        lines.push(format!("Deleted branch {}", super::format::plain(branch)));
    }
    if let Some(error) = &removed.branch_error {
        lines.push(format!(
            "The branch was kept: {}",
            super::format::plain(error)
        ));
    }
    lines.join("\n")
}

fn snapshots_human(snapshots: &[SavedSnapshot], now_ms: u64) -> String {
    super::format::table(
        ["SNAPSHOT", "PROJECT", "TAKEN", "OF", "WORKTREE"],
        snapshots
            .iter()
            .map(|snapshot| {
                [
                    snapshot.refname.clone(),
                    snapshot.project_name.clone(),
                    snapshot.created_at_unix_ms.map_or_else(
                        || "-".to_owned(),
                        |at| super::format::relative_time(at, now_ms),
                    ),
                    snapshot
                        .subject
                        .strip_prefix("WIP on ")
                        .unwrap_or(&snapshot.subject)
                        .to_owned(),
                    snapshot
                        .worktree
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), |path| path.display().to_string()),
                ]
            })
            .collect(),
        "No snapshots",
    )
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

fn pin(
    item: String,
    column: Option<Column>,
    project: Option<PathBuf>,
) -> Result<CommandOutput, AppError> {
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
        command: if column.is_some() {
            "work.pin"
        } else {
            "work.unpin"
        },
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
                [
                    "CARD",
                    "PROJECT",
                    "ACTIVITY",
                    "BASE",
                    "CHANGES",
                    "PULL REQUEST",
                    "TICKET",
                    "SESSIONS",
                    "WORKTREE"
                ],
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
            let scan = project.scan_error.as_deref().map(|error| {
                format!(
                    "{name}: Git could not be read: {}",
                    super::format::plain(error)
                )
            });
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
                    session
                        .provider
                        .map_or("shell", |provider| provider.executable()),
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
        item.worktree.as_ref().map_or_else(
            || "-".to_owned(),
            |worktree| worktree.path.display().to_string(),
        ),
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
