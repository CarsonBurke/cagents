//! `cagents work`: the daemon's board of every worktree and branch in
//! progress, newest activity first.
use super::*;
use crate::work::{
    Board, CheckOutcome, Column, ConversationPullRequests, Forge, Item, ItemKey, Keep,
    PullRequestState, Removal, RemovalPlan, Removed, Request, Response, Review, SavedSnapshot,
    Scope,
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
    /// List this project's worktrees and branches in progress, newest
    /// activity first. `cagents work` alone does this.
    #[command(visible_alias = "ls")]
    List(WorkListArgs),
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
    /// Remove a card's worktree, keeping its branch.
    ///
    /// Removal is refused while a session or process uses the worktree, and
    /// unless nothing unsaved would be lost or --snapshot or --discard says
    /// what happens to it. --dry-run shows the plan.
    #[command(visible_alias = "remove")]
    Rm(RemoveArgs),
    /// List the snapshots worktree removals kept, newest first.
    ///
    /// Restore one with `git stash apply --index REF` in any worktree of its
    /// project.
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
pub struct WorkListArgs {
    /// Only the open project with this name, or containing this path. The
    /// default is the project containing the current directory, or every
    /// project outside one.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Every open project's cards.
    #[arg(long, conflicts_with = "project")]
    all: bool,
    /// Only cards in this column: active, in-progress, in-review, landed or
    /// stale.
    #[arg(long)]
    column: Option<Column>,
    /// Rescan the projects and poll GitHub first instead of reading the
    /// daemon's latest facts.
    #[arg(long)]
    fresh: bool,
    /// The newest cards of each column to list, or 0 for all of them.
    /// [default: 10; every card with --json]
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
}

/// How many cards of each column a listing shows when no `--limit` says.
/// The rest are counted, and `--json` reports them all.
const DEFAULT_LIMIT: usize = 10;

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
                    });
                }
                _ => {}
            }
        }
    }
}

impl ControlClient {
    /// Subscribes this connection to the pull requests `conversation`
    /// worked on or mentioned, and returns them with the revision later
    /// lists from [`Self::next_conversation_pull_requests`] supersede.
    pub fn watch_conversation_pull_requests(
        &mut self,
        conversation: TerminalId,
    ) -> Result<(u64, ConversationPullRequests), ControlError> {
        match self.request_with_deadline(
            ClientRequest::WatchConversationPullRequests { conversation },
            Instant::now() + BOARD_TIMEOUT,
        )? {
            ServerResponse::Work(Response::WatchingConversation {
                revision,
                pull_requests,
            }) => Ok((revision, *pull_requests)),
            response => Err(unexpected_response(
                "a conversation's pull requests",
                &response,
            )),
        }
    }

    /// Blocks until the daemon sends a list newer than `revision`, as
    /// [`Self::next_work_board`] does for boards.
    pub fn next_conversation_pull_requests(
        &mut self,
        revision: u64,
    ) -> Result<(u64, ConversationPullRequests), ControlError> {
        if let Some((observed, pull_requests)) = self.observed_conversation.take()
            && observed > revision
        {
            return Ok((observed, *pull_requests));
        }
        self.stream
            .set_read_timeout(None)
            .map_err(ControlError::Configure)?;
        loop {
            let envelope = self.read_envelope()?;
            self.validate_pinned_envelope(&envelope)?;
            match envelope.message {
                ServerMessage::ConversationPullRequests {
                    revision: observed,
                    pull_requests,
                    ..
                } if observed > revision => return Ok((observed, *pull_requests)),
                ServerMessage::HostShuttingDown => return Err(ControlError::ConnectionClosed),
                ServerMessage::Error(error) => {
                    return Err(ControlError::Remote {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
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

fn send(
    context: &ServiceRequestContext,
    request: Request,
    timeout: Duration,
) -> Result<Response, AppError> {
    send_with(context, request, timeout, protocol_error)
}

fn send_with(
    context: &ServiceRequestContext,
    request: Request,
    timeout: Duration,
    failed: impl FnOnce(ControlError) -> AppError,
) -> Result<Response, AppError> {
    let mut client = context.connect()?;
    // Inside a hosted session the daemon lets this read the board but never
    // change it, as it can tell once the connection names its authority.
    if context.scoped() && matches!(request, Request::Pin { .. } | Request::Remove(_)) {
        client
            .authenticate(context.authority.clone())
            .map_err(protocol_error)?;
    }
    client.work(request, timeout).map_err(failed)
}

pub(super) fn execute(command: WorkCommand) -> Result<CommandOutput, AppError> {
    execute_with_context(command, &ServiceRequestContext::capture())
}

pub(super) fn execute_with_context(
    command: WorkCommand,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    context.admit_remote()?;
    match command {
        WorkCommand::List(arguments) => list(arguments, context),
        WorkCommand::Pin {
            item,
            column,
            project,
        } => pin(item, Some(column), project, context),
        WorkCommand::Unpin { item, project } => pin(item, None, project, context),
        WorkCommand::Rm(arguments) => remove(arguments, context),
        WorkCommand::Snapshots { project } => snapshots(project, context),
    }
}

fn remove(
    arguments: RemoveArgs,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
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
    let Response::RemovalPlan(plan) = send(context, request, timeout)? else {
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
    let Response::Removed(removed) = send_with(context, request, REMOVE_TIMEOUT, unanswered)?
    else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    Ok(CommandOutput {
        command: "work.rm",
        human: removed_human(&removed),
        data: json!({ "plan": plan, "removed": removed }),
    })
}

fn snapshots(
    project: Option<PathBuf>,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    let request = Request::Snapshots {
        scope: scope(project)?,
    };
    let Response::Snapshots(snapshots) = send(context, request, BOARD_TIMEOUT)? else {
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

fn list(
    arguments: WorkListArgs,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    let here = (arguments.project.is_none() && !arguments.all)
        .then(|| std::env::current_dir().and_then(std::fs::canonicalize).ok())
        .flatten();
    let mut base = "cagents work".to_owned();
    if let Some(project) = &arguments.project {
        base.push_str(&format!(
            " --project {}",
            shell_word(&project.display().to_string())
        ));
    }
    if arguments.all {
        base.push_str(" --all");
    }
    let request = Request::Board {
        scope: scope(arguments.project)?,
        fresh: arguments.fresh,
    };
    let timeout = if arguments.fresh {
        FRESH_BOARD_TIMEOUT
    } else {
        BOARD_TIMEOUT
    };
    let Response::Board(mut board) = send(context, request, timeout)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    let narrowed = here.as_deref().is_some_and(|here| narrow(&mut board, here));
    if let Some(column) = arguments.column {
        board.items.retain(|item| item.column == column);
    }
    let human = board_human(
        &board,
        &Listing {
            column: arguments.column,
            limit: arguments.limit.unwrap_or(DEFAULT_LIMIT),
            command: &base,
            narrowed,
        },
        crate::chat::now_ms().unwrap_or_default(),
    );
    if let Some(limit) = arguments.limit {
        limit_columns(&mut board.items, limit);
    }
    Ok(CommandOutput {
        command: "work.list",
        data: json!({ "board": board }),
        human,
    })
}

/// Narrows a board of every project to the one whose primary checkout or
/// one of whose worktrees contains `here`, the nearest where several do.
/// Returns whether any did.
fn narrow(board: &mut Board, here: &Path) -> bool {
    let roots = board
        .projects
        .iter()
        .filter_map(|project| Some((project.root.as_deref()?, &project.repository)));
    let worktrees = board
        .items
        .iter()
        .filter_map(|item| Some((item.worktree.as_ref()?.path.as_path(), &item.project)));
    let Some(repository) = roots
        .chain(worktrees)
        .filter(|(path, _)| here.starts_with(path))
        .max_by_key(|(path, _)| path.components().count())
        .map(|(_, repository)| repository.clone())
    else {
        return false;
    };
    board
        .projects
        .retain(|project| project.repository == repository);
    board.items.retain(|item| item.project == repository);
    true
}

/// Keeps the first `limit` cards of each column, in board order; 0 keeps all.
fn limit_columns(items: &mut Vec<Item>, limit: usize) {
    if limit == 0 {
        return;
    }
    let mut seen = std::collections::HashMap::<Column, usize>::new();
    items.retain(|item| {
        let count = seen.entry(item.column).or_default();
        *count += 1;
        *count <= limit
    });
}

/// `word` quoted for a POSIX shell when it needs to be.
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./~:@%+=,".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

fn pin(
    item: String,
    column: Option<Column>,
    project: Option<PathBuf>,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    let request = Request::Pin {
        scope: scope(project)?,
        item: item_argument(item),
        column,
    };
    let Response::Pinned(item) = send(context, request, BOARD_TIMEOUT)? else {
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

/// How a board is listed: the column it was filtered to, how many cards
/// of each column to show, the command that listed it, so a hint can extend
/// it, and whether it was narrowed to the current directory's project.
struct Listing<'a> {
    column: Option<Column>,
    limit: usize,
    command: &'a str,
    narrowed: bool,
}

const HEADERS: [&str; 9] = [
    "CARD",
    "PROJECT",
    "ACTIVITY",
    "BASE",
    "CHANGES",
    "PULL REQUEST",
    "TICKET",
    "SESSIONS",
    "WORKTREE",
];

/// One table per column, in board order, the newest `limit` cards of each,
/// then what kept any project from being read in full and what to run next.
/// A project every card shares is named once, after them, rather than on
/// every row.
fn board_human(board: &Board, listing: &Listing<'_>, now_ms: u64) -> String {
    let single = board
        .items
        .first()
        .filter(|first| board.items.iter().all(|item| item.project == first.project))
        .map(|first| first.project_name.as_str());
    let roots: std::collections::HashMap<&Path, &Path> = board
        .projects
        .iter()
        .filter_map(|project| Some((project.repository.as_path(), project.root.as_deref()?)))
        .collect();
    let shown = |items: &[&Item]| {
        items.len().min(match listing.limit {
            0 => usize::MAX,
            limit => limit,
        })
    };
    let columns: Vec<(Column, Vec<&Item>)> = Column::ALL
        .into_iter()
        .map(|column| {
            (
                column,
                board
                    .items
                    .iter()
                    .filter(|item| item.column == column)
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, items)| !items.is_empty())
        .collect();
    let rows: Vec<Vec<[String; 9]>> = columns
        .iter()
        .map(|(_, items)| {
            items[..shown(items)]
                .iter()
                .map(|item| {
                    let mut row = row(item, roots.get(item.project.as_path()).copied(), now_ms);
                    if single.is_some() {
                        row[1].clear();
                    }
                    row
                })
                .collect()
        })
        .collect();
    let kept = super::format::columns_in_use(&rows.concat());
    let mut sections = Vec::new();
    let mut truncated = Vec::new();
    for ((column, items), rows) in columns.iter().zip(rows) {
        let count = if rows.len() < items.len() {
            truncated.push((*column, items.len()));
            format!("{} of {}", rows.len(), items.len())
        } else {
            items.len().to_string()
        };
        sections.push(format!(
            "{} ({count})\n{}",
            column.label().to_uppercase(),
            super::format::table_of(HEADERS, rows, kept, ""),
        ));
    }
    if sections.is_empty() {
        sections.push(match listing.column {
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
    let mut footer = Vec::new();
    if let Some(project) = single {
        footer.push(format!("(project {})", super::format::plain(project)));
    }
    let command = listing.command;
    let mut steps: Vec<(String, &str)> = Vec::new();
    match truncated.as_slice() {
        [] => {}
        [(column, _)] => steps.push((
            format!(
                "{command} --column {} --limit 0",
                column.as_str().replace('_', "-")
            ),
            "lists the rest",
        )),
        _ => steps.push((
            format!("{command} --column COLUMN --limit 0"),
            "lists a whole column",
        )),
    }
    if listing.narrowed {
        steps.push(("cagents work --all".into(), "lists every project"));
    }
    if columns.iter().any(|(column, _)| *column == Column::Landed) {
        steps.push((
            "cagents work rm CARD --dry-run".into(),
            "shows what removing a landed worktree loses",
        ));
    }
    if !steps.is_empty() {
        footer.push(super::format::hint(&steps));
    }
    let mut human = sections.join("\n\n");
    for line in footer {
        human.push('\n');
        human.push_str(&line);
    }
    human
}

/// A card's cells. `root` is its project's primary checkout, which its
/// worktree is shown relative to.
fn row(item: &Item, root: Option<&Path>, now_ms: u64) -> [String; 9] {
    // A detached worktree's card is named by its path, which `~` keeps
    // runnable from anywhere.
    let mut card = match &item.key {
        ItemKey::Worktree(path) => super::format::location(path, None),
        ItemKey::Branch(_) => item.label(),
    };
    if item.pinned {
        card.push_str(" (pinned)");
    }
    let activity = item.last_activity.map_or_else(
        || "-".to_owned(),
        |activity| super::format::relative_time(activity.at_unix_ms, now_ms),
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
                        CheckOutcome::Cancelled => "checks cancelled",
                        CheckOutcome::Skipped => "checks skipped",
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
    [
        card,
        item.project_name.clone(),
        activity,
        base,
        changes,
        pull_request,
        ticket,
        sessions(&item.sessions),
        item.worktree.as_ref().map_or_else(
            || "-".to_owned(),
            |worktree| super::format::location(&worktree.path, root),
        ),
    ]
}

/// A card's sessions counted by what they are doing, busiest first:
/// "2 working, 3 idle". `cagents ls` lists them.
fn sessions(sessions: &[crate::work::Session]) -> String {
    use crate::agent::ActivityState;
    let counts = [
        (ActivityState::Working, "working"),
        (ActivityState::Waiting, "waiting"),
        (ActivityState::Idle, "idle"),
        (ActivityState::Unknown, "running"),
    ]
    .into_iter()
    .filter_map(|(state, label)| {
        let count = sessions
            .iter()
            .filter(|session| session.state == state)
            .count();
        (count > 0).then(|| format!("{count} {label}"))
    })
    .collect::<Vec<_>>();
    if counts.is_empty() {
        "-".into()
    } else {
        counts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{
        Activity, ActivitySource, Checks, Comparison, Dirty, LinearTicket, PullRequest, Session,
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
                    ..Default::default()
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

    fn project(name: &str) -> crate::work::Project {
        crate::work::Project {
            repository: format!("/{name}/.git").into(),
            name: name.into(),
            root: Some(format!("/{name}").into()),
            base: Some("origin/main".into()),
            scanned_at_unix_ms: Some(NOW),
            scan_error: None,
            forge: Forge::NotGitHub,
        }
    }

    fn board(projects: Vec<crate::work::Project>, items: Vec<Item>) -> Board {
        Board {
            generated_at_unix_ms: NOW,
            stale_after_days: 7,
            projects,
            items,
            linear_unavailable: None,
        }
    }

    fn card(branch: &str, column: Column) -> Item {
        let mut item = item();
        item.key = ItemKey::Branch(branch.into());
        item.branch = Some(branch.into());
        item.column = column;
        item.pinned = false;
        item.linear = None;
        item.worktree = None;
        item
    }

    fn listing(limit: usize, narrowed: bool) -> Listing<'static> {
        Listing {
            column: None,
            limit,
            command: "cagents work",
            narrowed,
        }
    }

    #[test]
    fn rows_name_every_fact_a_card_carries() {
        assert_eq!(
            row(&item(), Some(Path::new("/repo")), NOW),
            [
                "fix/main-12-login (pinned)".to_owned(),
                "repo".into(),
                "1 min ago".into(),
                "+3 -1".into(),
                "4 (1 conflicted)".into(),
                "#42 open, checks fail, approved".into(),
                "MAIN-12".into(),
                "1 working".into(),
                ".worktrees/login".into(),
            ]
        );
        // Outside its project's checkout, a worktree keeps its whole path.
        assert_eq!(row(&item(), None, NOW)[8], "/repo/.worktrees/login");
        let mut detached = item();
        detached.key = ItemKey::Worktree("/repo/.worktrees/login".into());
        detached.pinned = false;
        assert_eq!(row(&detached, None, NOW)[0], "/repo/.worktrees/login");
    }

    #[test]
    fn a_cards_sessions_are_counted_busiest_first() {
        use crate::agent::ActivityState::{Idle, Unknown, Waiting, Working};
        let session = |state| Session {
            id: crate::model::TerminalId::new(),
            provider: Some(crate::agent::Provider::Claude),
            title: None,
            state,
        };
        assert_eq!(sessions(&[]), "-");
        assert_eq!(
            sessions(&[
                session(Idle),
                session(Working),
                session(Idle),
                session(Unknown),
                session(Waiting),
                session(Working),
            ]),
            "2 working, 1 waiting, 2 idle, 1 running"
        );
    }

    #[test]
    fn boards_group_by_column_and_report_what_could_not_be_read() {
        let mut stale = item();
        stale.column = Column::Stale;
        stale.key = ItemKey::Branch("old".into());
        stale.branch = Some("old".into());
        let mut repo = project("repo");
        repo.forge = Forge::Unavailable {
            reason: "gh is not\ninstalled".into(),
        };
        let mut board = board(vec![repo], vec![item(), stale]);
        board.linear_unavailable = Some("Add a Linear key".into());
        let human = board_human(&board, &listing(DEFAULT_LIMIT, false), NOW);
        let active = human.find("ACTIVE (1)").unwrap();
        let stale = human.find("STALE (1)").unwrap();
        assert!(active < stale);
        assert!(
            human.ends_with(
                "repo: GitHub is unavailable: gh is not installed\n\
                 Linear tickets could not be looked up: Add a Linear key\n\
                 (project repo)"
            ),
            "{human}"
        );
        let empty = Board {
            items: Vec::new(),
            projects: Vec::new(),
            linear_unavailable: None,
            ..board
        };
        assert_eq!(
            board_human(&empty, &listing(DEFAULT_LIMIT, false), NOW),
            "No cards"
        );
        let landed = Listing {
            column: Some(Column::Landed),
            ..listing(DEFAULT_LIMIT, false)
        };
        assert_eq!(board_human(&empty, &landed, NOW), "No cards in Landed");
    }

    #[test]
    fn one_projects_board_names_it_once_and_drops_columns_no_card_has() {
        let single = board(vec![project("repo")], vec![card("a", Column::Active)]);
        let human = board_human(&single, &listing(DEFAULT_LIMIT, true), NOW);
        let header = human.lines().nth(1).unwrap();
        assert_eq!(
            header.split_whitespace().collect::<Vec<_>>(),
            [
                "CARD", "ACTIVITY", "BASE", "CHANGES", "PULL", "REQUEST", "SESSIONS"
            ],
            "{human}"
        );
        assert!(
            human.ends_with("\n(project repo)\nhint: `cagents work --all` lists every project"),
            "{human}"
        );

        let mut other = card("b", Column::Stale);
        other.project = "/other/.git".into();
        other.project_name = "other".into();
        let both = board(
            vec![project("repo"), project("other")],
            vec![card("a", Column::Active), other],
        );
        let human = board_human(&both, &listing(DEFAULT_LIMIT, false), NOW);
        // Every section keeps the same columns, so they read as one table.
        let headers: Vec<&str> = human
            .lines()
            .filter(|line| line.starts_with("CARD"))
            .collect();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], headers[1]);
        assert!(headers[0].contains("PROJECT"), "{human}");
        assert!(!human.contains("(project"), "{human}");
        assert!(!human.contains("hint:"), "{human}");
    }

    #[test]
    fn a_listing_shows_the_newest_cards_of_each_column_and_how_to_see_the_rest() {
        let items = (0..12)
            .map(|index| card(&format!("work-{index}"), Column::InProgress))
            .chain([card("done", Column::Landed)])
            .collect();
        let board = board(vec![project("repo")], items);
        let human = board_human(&board, &listing(DEFAULT_LIMIT, true), NOW);
        assert!(human.contains("IN PROGRESS (10 of 12)\n"), "{human}");
        assert!(human.contains("\nwork-9 "), "{human}");
        assert!(!human.contains("work-10"), "{human}");
        assert!(human.contains("LANDED (1)\n"), "{human}");
        assert!(
            human.ends_with(
                "hint: `cagents work --column in-progress --limit 0` lists the rest; \
                 `cagents work --all` lists every project; \
                 `cagents work rm CARD --dry-run` shows what removing a landed worktree loses"
            ),
            "{human}"
        );

        let all = board_human(&board, &listing(0, false), NOW);
        assert!(all.contains("IN PROGRESS (12)\n"), "{all}");
        assert!(all.contains("work-11"), "{all}");

        let mut items = board.items.clone();
        limit_columns(&mut items, 2);
        assert_eq!(
            items.iter().map(Item::label).collect::<Vec<_>>(),
            ["work-0", "work-1", "done"]
        );
        limit_columns(&mut items, 0);
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn a_board_narrows_to_the_nearest_project_containing_the_directory() {
        let mut outer = card("outer", Column::Active);
        outer.project = "/code/.git".into();
        let mut inner = card("inner", Column::Active);
        inner.project = "/code/app/.git".into();
        let mut linked = card("linked", Column::Active);
        linked.project = "/code/app/.git".into();
        linked.worktree = Some(Worktree {
            path: "/elsewhere/linked".into(),
            primary: false,
            locked: false,
            created_at_unix_ms: None,
        });
        let mut projects = vec![project("code"), project("code/app")];
        projects[1].name = "app".into();
        let full = board(projects, vec![outer, inner, linked]);
        let labels = |board: &Board| board.items.iter().map(Item::label).collect::<Vec<_>>();

        let mut board = full.clone();
        assert!(narrow(&mut board, Path::new("/code/app/src")));
        assert_eq!(labels(&board), ["inner", "linked"]);
        assert_eq!(board.projects.len(), 1);

        let mut board = full.clone();
        assert!(narrow(&mut board, Path::new("/elsewhere/linked/src")));
        assert_eq!(labels(&board), ["inner", "linked"]);

        let mut board = full.clone();
        assert!(narrow(&mut board, Path::new("/code/docs")));
        assert_eq!(labels(&board), ["outer"]);

        let mut board = full.clone();
        assert!(!narrow(&mut board, Path::new("/tmp")));
        assert_eq!(board, full);
    }

    #[test]
    fn work_alone_lists_and_takes_the_list_options() {
        let parse = |arguments: &[&str]| {
            Cli::try_parse_from(std::iter::once("cagents").chain(arguments.iter().copied()))
        };
        let Ok(Cli {
            command:
                TopCommand::Work {
                    command: None,
                    list,
                },
            ..
        }) = parse(&["work", "--column", "in-review", "--limit", "3"])
        else {
            panic!("`cagents work` alone should list");
        };
        assert_eq!(list.column, Some(Column::InReview));
        assert_eq!(list.limit, Some(3));
        assert!(matches!(
            parse(&["work", "ls", "--all"]),
            Ok(Cli {
                command: TopCommand::Work {
                    command: Some(WorkCommand::List(WorkListArgs { all: true, .. })),
                    ..
                },
                ..
            })
        ));
        assert!(parse(&["work", "--all", "rm", "x"]).is_err());
        assert!(parse(&["work", "--all", "--project", "x"]).is_err());
    }

    #[test]
    fn hinted_project_names_are_quoted_for_the_shell() {
        assert_eq!(shell_word("redact"), "redact");
        assert_eq!(shell_word("/a/b-c"), "/a/b-c");
        assert_eq!(shell_word("Just Make Money"), "'Just Make Money'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
    }
}
