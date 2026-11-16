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
    /// List this project's worktrees and branches in progress.
    #[command(
        visible_alias = "ls",
        after_help = "Newest activity first. `cagents work` alone does this."
    )]
    List(WorkListArgs),
    /// Pin a card to a column, overriding the column its state implies.
    Pin {
        /// The card's branch, or its worktree's path.
        item: String,
        /// active, in-progress, in-review, landed or stale.
        column: Column,
        /// The open project the card belongs to, by name or path. The
        /// default is the project containing the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Remove a card's pin, returning it to the column its state implies.
    Unpin {
        /// The card's branch, or its worktree's path.
        item: String,
        /// The open project the card belongs to, by name or path. The
        /// default is the project containing the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Remove a card's worktree, keeping its branch.
    #[command(
        visible_alias = "remove",
        after_help = "Removal is refused while a session or process uses the worktree, and unless \
            nothing unsaved would be lost or --snapshot or --discard says what happens to it. \
            --dry-run shows the plan."
    )]
    Rm(RemoveArgs),
    /// List the snapshots worktree removals kept, newest first.
    #[command(
        after_help = "Restore one with `git stash apply --index REF` in any worktree of its project."
    )]
    Snapshots {
        /// Only the open project with this name, or containing this path.
        /// The default is the project containing the current directory, or
        /// every project outside one.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

#[derive(Debug, clap::Args)]
pub struct RemoveArgs {
    /// The card's branch, or its worktree's path.
    item: String,
    /// The open project the card belongs to, by name or path. The default
    /// is the project containing the current directory.
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

#[derive(Debug, Default, PartialEq, clap::Args)]
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
    /// [default: 10]
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    #[arg(long, value_name = "FIELDS", value_delimiter = ',', help = super::fields::HELP,
        value_parser = clap::builder::PossibleValuesParser::new(FIELDS))]
    fields: Vec<String>,
}

impl WorkListArgs {
    pub(super) fn selects_fields(&self) -> bool {
        !self.fields.is_empty()
    }
}

/// How many cards of each column a listing shows when no `--limit` says.
/// The rest are counted, in text and in `--json`.
const DEFAULT_LIMIT: usize = 10;

/// The fields `--json` reports for a card, which `--fields` selects from.
const FIELDS: &[&str] = &[
    "card",
    "project",
    "column",
    "derived",
    "pinned",
    "branch",
    "worktree",
    "primary",
    "locked",
    "head",
    "base",
    "ahead",
    "behind",
    "changed_paths",
    "conflicted_paths",
    "pull_request",
    "ticket",
    "sessions",
    "active_at",
    "error",
];

/// The hexadecimal digits of a head commit a card shows, as Git
/// abbreviates object names in its own listings.
const HEAD_DIGITS: usize = 12;

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

/// One connection to the daemon for a work command, which reads what the
/// command covers and then asks for it. Inside a hosted session the daemon
/// lets a connection read the board but change it only once it names its
/// authority, which a changing command's connection does first.
struct Daemon<'a> {
    context: &'a ServiceRequestContext,
    client: ControlClient,
}

impl<'a> Daemon<'a> {
    fn connect(context: &'a ServiceRequestContext, changes: bool) -> Result<Self, AppError> {
        let mut client = context.connect()?;
        if changes && context.scoped() {
            client
                .authenticate(context.authority.clone())
                .map_err(protocol_error)?;
        }
        Ok(Self { context, client })
    }

    /// The projects a command covers: the open project `project` names, or
    /// else the one [`Self::here`] finds, or else every project.
    fn scope(&mut self, project: Option<PathBuf>) -> Result<Scope, AppError> {
        Ok(match project {
            Some(project) => Scope::Project(super::project_argument(project)?),
            None => self.here()?.map_or(Scope::All, Scope::Project),
        })
    }

    /// The open Git project containing the current directory, placed among
    /// the daemon's open projects as the daemon places a session's
    /// directory; none outside every one. A remote device's projects are not
    /// where this shell runs, so there it finds none.
    fn here(&mut self) -> Result<Option<PathBuf>, AppError> {
        if self.context.remote {
            return Ok(None);
        }
        let Ok(directory) = std::env::current_dir() else {
            return Ok(None);
        };
        let (_, projects) = self
            .client
            .read_workspace_projects()
            .map_err(protocol_error)?;
        Ok(containing(&directory, &projects))
    }

    fn send(&mut self, request: Request, timeout: Duration) -> Result<Response, AppError> {
        self.send_with(request, timeout, protocol_error)
    }

    fn send_with(
        &mut self,
        request: Request,
        timeout: Duration,
        failed: impl FnOnce(ControlError) -> AppError,
    ) -> Result<Response, AppError> {
        self.client.work(request, timeout).map_err(failed)
    }
}

/// The repository key of the open Git project `directory` is in. An open
/// folder has no worktrees, so it widens the listing like no project.
fn containing(
    directory: &Path,
    projects: &[crate::workspace_model::ModelProject],
) -> Option<PathBuf> {
    let repository = crate::project_location::place_among(directory, projects).project?;
    projects
        .iter()
        .any(|project| {
            project.repository == repository && project.kind == crate::model::ProjectKind::Git
        })
        .then_some(repository)
}

/// A card named by its worktree's path is sent as that path made canonical,
/// as the daemon knows it; anything else is a branch name, or a path the
/// daemon reads relative to the project's primary checkout. A leading `~`
/// is the home directory, as a listing shows it, even when quoted.
fn item_argument(item: String) -> String {
    item_in(item, std::env::var_os("HOME").as_deref().map(Path::new))
}

fn item_in(item: String, home: Option<&Path>) -> String {
    let expanded = match (item.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(&item),
    };
    if expanded.join(".git").exists()
        && let Ok(canonical) = std::fs::canonicalize(&expanded)
    {
        return canonical.to_string_lossy().into_owned();
    }
    item
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
    let mut daemon = Daemon::connect(context, false)?;
    let scope = daemon.scope(arguments.project)?;
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
    let Response::RemovalPlan(plan) = daemon.send(request, timeout)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    // The removal, which may take minutes, asks on a connection of its own.
    drop(daemon);
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
    let Response::Removed(removed) =
        Daemon::connect(context, true)?.send_with(request, REMOVE_TIMEOUT, unanswered)?
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
    let mut daemon = Daemon::connect(context, false)?;
    let request = Request::Snapshots {
        scope: daemon.scope(project)?,
    };
    let Response::Snapshots(snapshots) = daemon.send(request, BOARD_TIMEOUT)? else {
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
    let mut daemon = Daemon::connect(context, false)?;
    let placed = match (&arguments.project, arguments.all) {
        (None, false) => daemon.here()?,
        _ => None,
    };
    let program = super::format::program();
    let named = arguments
        .project
        .as_ref()
        .map(|project| super::format::shell_word(&project.display().to_string()));
    let scope = match (arguments.project, placed.clone()) {
        (Some(project), _) => Scope::Project(super::project_argument(project)?),
        (None, Some(repository)) => Scope::Project(repository),
        (None, None) => Scope::All,
    };
    let request = Request::Board {
        scope: scope.clone(),
        fresh: arguments.fresh,
    };
    let timeout = if arguments.fresh {
        FRESH_BOARD_TIMEOUT
    } else {
        BOARD_TIMEOUT
    };
    let Response::Board(mut board) = daemon.send(request, timeout)? else {
        return Err(AppError::new("work", "Unexpected Work board response"));
    };
    if let Some(column) = arguments.column {
        board.items.retain(|item| item.column == column);
    }
    // The card of the worktree this shell is in is always listed.
    let current = (!context.remote)
        .then(|| std::env::current_dir().and_then(std::fs::canonicalize).ok())
        .flatten()
        .and_then(|directory| current_worktree(&board.items, &directory));
    let listing = Listing {
        column: arguments.column,
        limit: arguments.limit.unwrap_or(DEFAULT_LIMIT),
        program: &program,
        project: named.as_deref(),
        all: arguments.all,
        placed: placed.is_some(),
        current: current.as_deref(),
    };
    let human = board_human(&board, &listing, crate::chat::now_ms().unwrap_or_default());
    let omitted = listing.omit(&mut board.items, listing.limit);
    let data = board_json(&board, &scope, &omitted, &arguments.fields);
    Ok(CommandOutput {
        command: "work.list",
        data,
        human,
    })
}

fn pin(
    item: String,
    column: Option<Column>,
    project: Option<PathBuf>,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    let mut daemon = Daemon::connect(context, true)?;
    let request = Request::Pin {
        scope: daemon.scope(project)?,
        item: item_argument(item),
        column,
    };
    let Response::Pinned(item) = daemon.send(request, BOARD_TIMEOUT)? else {
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
/// of each column to show, how the command that listed it named this
/// program and its projects, so a hint can extend it, whether its project
/// was the current directory's, and that directory.
struct Listing<'a> {
    column: Option<Column>,
    limit: usize,
    program: &'a str,
    /// `--project`'s value, quoted for the shell.
    project: Option<&'a str>,
    all: bool,
    placed: bool,
    /// The worktree this shell is in, if the board has its card.
    current: Option<&'a Path>,
}

impl Listing<'_> {
    /// Whether `item` is listed when it is the `index`th card of its column:
    /// one of the newest `limit`, or the card of the worktree this shell is in.
    fn shows(&self, item: &Item, index: usize, limit: usize) -> bool {
        limit == 0
            || index < limit
            || self.current.is_some_and(|current| {
                item.worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.path == current)
            })
    }

    /// Keeps the cards of each column [`Self::shows`] lists, in board order,
    /// and counts the rest by column.
    fn omit(
        &self,
        items: &mut Vec<Item>,
        limit: usize,
    ) -> std::collections::BTreeMap<Column, usize> {
        let mut seen = std::collections::HashMap::<Column, usize>::new();
        let mut omitted = std::collections::BTreeMap::new();
        items.retain(|item| {
            let index = seen.entry(item.column).or_default();
            let shown = self.shows(item, *index, limit);
            *index += 1;
            if !shown {
                *omitted.entry(item.column).or_default() += 1;
            }
            shown
        });
        omitted
    }
}

/// The listed board as `--json` reports it: the scope it was asked for,
/// each project once, with the default branch its cards compare against,
/// and the cards the listing shows, newest activity first, as compact rows.
/// `omitted` counts the cards of each column the limit left out. With
/// `selected` fields, only the cards, with only those fields.
fn board_json(
    board: &Board,
    scope: &Scope,
    omitted: &std::collections::BTreeMap<Column, usize>,
    selected: &[String],
) -> Value {
    let bases: std::collections::HashMap<&Path, Option<&str>> = board
        .projects
        .iter()
        .map(|project| (project.repository.as_path(), project.base.as_deref()))
        .collect();
    let cards: Vec<Value> = board
        .items
        .iter()
        .map(|item| card_json(item, bases.get(item.project.as_path()).copied().flatten()))
        .collect();
    if !selected.is_empty() {
        return json!({ "cards": super::fields::select(cards, selected) });
    }
    let projects: Vec<Value> = board
        .projects
        .iter()
        .map(|project| {
            let (forge, forge_error) = match &project.forge {
                Forge::Pending => ("pending", None),
                Forge::NotGitHub => ("not_github", None),
                Forge::Unavailable { reason } => ("unavailable", Some(reason)),
                Forge::Synced { error, .. } => ("synced", error.as_ref()),
            };
            super::fields::Row::new()
                .put("name", &project.name)
                .put("root", &project.root)
                .put("base", &project.base)
                .put("forge", forge)
                .put("forge_error", forge_error)
                .put("scan_error", &project.scan_error)
                .into_value()
        })
        .collect();
    super::fields::Row::new()
        .put("scope", scope)
        .put("projects", projects)
        .put("stale_after_days", board.stale_after_days)
        .put("linear_unavailable", &board.linear_unavailable)
        .put("cards", cards)
        .put("omitted", omitted)
        .into_value()
}

/// A card as a compact row. `card` is how commands name it: its branch, or
/// its worktree's path when the branch is another card's or there is none,
/// and then `branch` names the branch it has. `base` appears only where it
/// is not the project's. Its pull request carries its number, title, state
/// unless open, check outcome and review, and `unpushed` when the local
/// head is not in it; its sessions their ID and state.
fn card_json(item: &Item, project_base: Option<&str>) -> Value {
    use super::fields::{Row, word};
    let card = item.label();
    let comparison = item.comparison.as_ref();
    let dirty = item.dirty.as_ref();
    let pull_request = item.pull_request.as_ref().map(|pull| {
        Row::new()
            .put("number", pull.number)
            .put("title", &pull.title)
            .put(
                "state",
                (pull.state != crate::work::PullRequestState::Open).then(|| word(pull.state)),
            )
            .put("draft", pull.draft)
            .put("checks", pull.checks.and_then(|checks| checks.outcome()))
            .put("review", pull.review)
            .put("unpushed", !pull.contains_local_head)
            .into_value()
    });
    let ticket = item.linear.as_ref().map(|ticket| {
        Row::new()
            .put("identifier", &ticket.identifier)
            .put("title", &ticket.title)
            .into_value()
    });
    let sessions: Vec<Value> = item
        .sessions
        .iter()
        .map(|session| json!({ "id": session.id, "state": word(session.state) }))
        .collect();
    Row::new()
        .put("card", &card)
        .put("project", &item.project_name)
        .put("column", item.column)
        .put(
            "derived",
            (item.derived != item.column).then_some(item.derived),
        )
        .put("pinned", item.pinned)
        .put(
            "branch",
            item.branch.as_ref().filter(|branch| **branch != card),
        )
        .put(
            "worktree",
            item.worktree.as_ref().map(|worktree| &worktree.path),
        )
        .put(
            "primary",
            item.worktree
                .as_ref()
                .is_some_and(|worktree| worktree.primary),
        )
        .put(
            "locked",
            item.worktree
                .as_ref()
                .is_some_and(|worktree| worktree.locked),
        )
        .put(
            "head",
            item.head
                .as_ref()
                .map(|head| super::format::prefix(&head.oid, HEAD_DIGITS)),
        )
        .put(
            "base",
            comparison
                .map(|comparison| comparison.base.as_str())
                .filter(|base| Some(*base) != project_base),
        )
        .count("ahead", comparison.map_or(0, |comparison| comparison.ahead))
        .count(
            "behind",
            comparison.map_or(0, |comparison| comparison.behind),
        )
        .count("changed_paths", dirty.map_or(0, |dirty| dirty.paths))
        .count(
            "conflicted_paths",
            dirty.map_or(0, |dirty| dirty.conflicted),
        )
        .put("pull_request", pull_request)
        .put("ticket", ticket)
        .put("sessions", sessions)
        .put(
            "active_at",
            item.last_activity.map(|activity| activity.at_unix_ms),
        )
        .put("error", &item.error)
        .into_value()
}

/// The worktree of `items` that holds `directory`: the innermost, since a
/// project's linked worktrees usually sit inside its primary checkout.
fn current_worktree(items: &[Item], directory: &Path) -> Option<PathBuf> {
    items
        .iter()
        .filter_map(|item| item.worktree.as_ref())
        .map(|worktree| &worktree.path)
        .filter(|path| directory.starts_with(path))
        .max_by_key(|path| path.components().count())
        .cloned()
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
/// A project every card shares is named once, with where it is, rather than
/// on every row, and its worktrees are shown relative to it; across projects
/// each row names its own and shows its worktree in full.
fn board_human(board: &Board, listing: &Listing<'_>, now_ms: u64) -> String {
    let single = match board.projects.as_slice() {
        [project] => Some(project),
        _ => board
            .items
            .first()
            .filter(|first| board.items.iter().all(|item| item.project == first.project))
            .and_then(|first| {
                board
                    .projects
                    .iter()
                    .find(|project| project.repository == first.project)
            }),
    };
    let root = single.and_then(|project| project.root.as_deref());
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
            items
                .iter()
                .enumerate()
                .filter(|(index, item)| listing.shows(item, *index, listing.limit))
                .map(|(_, item)| {
                    let mut row = row(item, root, now_ms);
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
            truncated.push(*column);
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
        let mut empty = match listing.column {
            Some(column) => format!("No cards in {}", column.label()),
            None => "No cards".into(),
        };
        if let Some(project) = single {
            empty.push_str(&format!(
                " in project {}",
                super::format::plain(&project.name)
            ));
        }
        sections.push(empty);
    }
    let notes = board
        .projects
        .iter()
        .flat_map(|project| {
            let name = super::format::plain(&project.name);
            let scan = project.scan_error.as_deref().map(|error| {
                format!(
                    "{name}: Git could not be read: {}",
                    super::format::plain(&super::format::home_shortened(error))
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
            let name = match &item.key {
                ItemKey::Worktree(path) => super::format::location(path, root),
                ItemKey::Branch(_) => item.label(),
            };
            Some(format!(
                "{}: its changes could not be read: {}",
                super::format::plain(&name),
                super::format::plain(&super::format::home_shortened(error))
            ))
        }))
        .chain(board.linear_unavailable.as_deref().map(|reason| {
            format!(
                "Linear tickets could not be looked up: {}",
                super::format::plain(reason)
            )
        }))
        .collect::<Vec<_>>();
    let mut footer = Vec::new();
    if let Some(project) = single.filter(|_| !board.items.is_empty()) {
        let name = super::format::plain(&project.name);
        footer.push(match root {
            Some(root) => format!(
                "(project {name} at {})",
                super::format::location(root, None)
            ),
            None => format!("(project {name})"),
        });
    }
    let program = listing.program;
    let projects = match (listing.project, listing.all) {
        (Some(project), _) => format!(" --project {project}"),
        (None, true) => " --all".into(),
        (None, false) => String::new(),
    };
    let command = format!("{program} work{projects}");
    let mut steps: Vec<(String, &str)> = Vec::new();
    // One column to finish: the one listed, or the only one cut short.
    let rest = listing.column.or(match truncated.as_slice() {
        [only] => Some(*only),
        _ => None,
    });
    match rest {
        _ if truncated.is_empty() => {}
        Some(column) => steps.push((
            format!(
                "{command} --column {} --limit 0",
                column.as_str().replace('_', "-")
            ),
            "lists the rest",
        )),
        None => steps.push((
            format!("{command} --column COLUMN --limit 0"),
            "lists a whole column",
        )),
    }
    if listing.placed {
        steps.push((format!("{program} work --all"), "lists every project"));
    }
    if columns.iter().any(|(column, _)| *column == Column::Landed) {
        // Removal looks in this directory's project unless told otherwise.
        let project = match (single, listing.project) {
            (_, Some(project)) => format!(" --project {project}"),
            (Some(project), None) if listing.all => {
                format!(" --project {}", super::format::shell_word(&project.name))
            }
            (Some(_), None) => String::new(),
            (None, None) => " --project PROJECT".into(),
        };
        steps.push((
            format!("{program} work rm CARD{project} --dry-run"),
            "shows what removing a landed worktree loses",
        ));
    }
    if !notes.is_empty() {
        sections.push(notes.join("\n"));
    }
    let mut human = sections.join("\n\n");
    for line in footer {
        human.push('\n');
        human.push_str(&line);
    }
    if !steps.is_empty() {
        human.push('\n');
        human.push_str(&super::format::hint(&steps));
    }
    human
}

/// A card's cells. `root` is the primary checkout of the one project a
/// listing shows, which its worktree is shown relative to; across projects
/// the worktree is shown in full.
fn row(item: &Item, root: Option<&Path>, now_ms: u64) -> [String; 9] {
    // A detached worktree's card is named by its path, which its WORKTREE
    // cell already shows.
    let mut card = match &item.key {
        ItemKey::Worktree(_) => "(detached)".to_owned(),
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
        Activity, ActivitySource, Checks, Comparison, Dirty, Head, LinearTicket, PullRequest,
        Session, TicketSource, Worktree,
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

    fn listing(limit: usize, placed: bool) -> Listing<'static> {
        Listing {
            column: None,
            limit,
            program: "cagents",
            project: None,
            all: false,
            placed,
            current: None,
        }
    }

    #[test]
    fn card_json_states_each_fact_once() {
        let mut card = item();
        card.head = Some(Head {
            oid: "0123456789abcdef0123456789abcdef01234567".into(),
            committed_at_unix_ms: Some(NOW),
        });
        assert_eq!(
            card_json(&card, Some("origin/main")),
            json!({
                "card": "fix/main-12-login",
                "project": "repo",
                "column": "active",
                "pinned": true,
                "worktree": "/repo/.worktrees/login",
                "head": "0123456789ab",
                "ahead": 3,
                "behind": 1,
                "changed_paths": 4,
                "conflicted_paths": 1,
                "pull_request": {
                    "number": 42,
                    "title": "Fix login",
                    "checks": "failing",
                    "review": "approved",
                },
                "ticket": {"identifier": "MAIN-12"},
                "sessions": [{"id": "00000000-0000-4000-8000-000000000001", "state": "working"}],
                "active_at": NOW - 60_000,
            })
        );
        // A card named by its worktree names its branch too, and a base
        // other than the project's is given.
        card.key = ItemKey::Worktree("/repo/.worktrees/login".into());
        card.pull_request.as_mut().unwrap().contains_local_head = false;
        card.pull_request.as_mut().unwrap().state = PullRequestState::Merged;
        let value = card_json(&card, Some("origin/trunk"));
        assert_eq!(value["card"], "/repo/.worktrees/login");
        assert_eq!(value["branch"], "fix/main-12-login");
        assert_eq!(value["base"], "origin/main");
        assert_eq!(value["pull_request"]["state"], "merged");
        assert_eq!(value["pull_request"]["unpushed"], true);
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
        // Across projects, a worktree keeps its whole path.
        assert_eq!(row(&item(), None, NOW)[8], "/repo/.worktrees/login");
        // A detached worktree is named by that path, in its own cell.
        let mut detached = item();
        detached.key = ItemKey::Worktree("/repo/.worktrees/login".into());
        detached.pinned = false;
        let cells = row(&detached, Some(Path::new("/repo")), NOW);
        assert_eq!([&cells[0], &cells[8]], ["(detached)", ".worktrees/login"]);
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
                 (project repo at /repo)"
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
        // An empty board of one project says which.
        let scoped = Board {
            projects: vec![project("repo")],
            ..empty
        };
        assert_eq!(
            board_human(&scoped, &listing(DEFAULT_LIMIT, true), NOW),
            "No cards in project repo\nhint: `cagents work --all` lists every project"
        );
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
            human.ends_with(
                "\n(project repo at /repo)\nhint: `cagents work --all` lists every project"
            ),
            "{human}"
        );

        let mut other = card("b", Column::Stale);
        other.project = "/other/.git".into();
        other.project_name = "other".into();
        let both = board(
            vec![project("repo"), project("other")],
            vec![card("a", Column::Active), other],
        );
        let mut both = both;
        both.items[0].worktree = item().worktree;
        let human = board_human(&both, &listing(DEFAULT_LIMIT, false), NOW);
        // Every section keeps the same columns, so they read as one table.
        let headers: Vec<&str> = human
            .lines()
            .filter(|line| line.starts_with("CARD"))
            .collect();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], headers[1]);
        assert!(headers[0].contains("PROJECT"), "{human}");
        // With no one root to be relative to, a worktree is shown in full.
        assert!(human.contains(" /repo/.worktrees/login\n"), "{human}");
        assert!(!human.contains("(project"), "{human}");
        assert!(!human.contains("hint:"), "{human}");

        // A listing of every project names the card's project to remove it.
        both.items[1].column = Column::Landed;
        let all = Listing {
            all: true,
            ..listing(DEFAULT_LIMIT, false)
        };
        assert!(
            board_human(&both, &all, NOW).ends_with(
                "hint: `cagents work rm CARD --project PROJECT --dry-run` \
                 shows what removing a landed worktree loses"
            ),
            "{human}"
        );
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

        let listed = |items: &mut Vec<Item>, listing: &Listing<'_>, limit| {
            let omitted = listing.omit(items, limit);
            (items.iter().map(Item::label).collect::<Vec<_>>(), omitted)
        };
        let mut items = board.items.clone();
        let (labels, omitted) = listed(&mut items, &listing(2, false), 2);
        assert_eq!(labels, ["work-0", "work-1", "done"]);
        assert_eq!(omitted, [(Column::InProgress, 10)].into());
        let (labels, omitted) = listed(&mut items, &listing(0, false), 0);
        assert_eq!(labels.len(), 3);
        assert!(omitted.is_empty());
    }

    /// The card of the worktree this shell is in is listed however old, so
    /// an agent always finds its own branch.
    #[test]
    fn a_listing_keeps_the_card_of_the_current_worktree() {
        let mut items: Vec<Item> = (0..12)
            .map(|index| {
                let mut card = card(&format!("work-{index}"), Column::InProgress);
                card.worktree = Some(Worktree {
                    path: format!("/repo/.worktrees/{index}").into(),
                    primary: false,
                    locked: false,
                    created_at_unix_ms: None,
                });
                card
            })
            .collect();
        let mut primary = card("main", Column::InProgress);
        primary.worktree = Some(Worktree {
            path: "/repo".into(),
            primary: true,
            locked: false,
            created_at_unix_ms: None,
        });
        items.insert(3, primary);
        let board = board(vec![project("repo")], items);
        // The innermost worktree holding the directory is the current one,
        // not the primary checkout the linked worktrees sit inside.
        let current = current_worktree(&board.items, Path::new("/repo/.worktrees/11/src"));
        assert_eq!(current.as_deref(), Some(Path::new("/repo/.worktrees/11")));
        let inside = Listing {
            current: current.as_deref(),
            ..listing(DEFAULT_LIMIT, true)
        };
        let human = board_human(&board, &inside, NOW);
        assert!(human.contains("IN PROGRESS (11 of 13)\n"), "{human}");
        assert!(human.contains("\nwork-11 "), "{human}");
        assert!(!human.contains("work-10 "), "{human}");
        let mut items = board.items.clone();
        inside.omit(&mut items, 3);
        assert_eq!(
            items.iter().map(Item::label).collect::<Vec<_>>(),
            ["work-0", "work-1", "work-2", "work-11"]
        );
    }

    /// The listing's own options carry into its hints: the device, the
    /// project and the column.
    #[test]
    fn hints_extend_the_command_that_listed_the_board() {
        let items = (0..12)
            .map(|index| card(&format!("work-{index}"), Column::InProgress))
            .chain([card("done", Column::Landed)])
            .collect();
        let board = board(vec![project("repo")], items);
        let named = Listing {
            program: "cagents --device fw16",
            project: Some("'my repo'"),
            column: Some(Column::InProgress),
            ..listing(DEFAULT_LIMIT, false)
        };
        let mut filtered = board.clone();
        filtered
            .items
            .retain(|item| item.column == Column::InProgress);
        assert!(board_human(&filtered, &named, NOW).ends_with(
            "hint: `cagents --device fw16 work --project 'my repo' --column in-progress --limit 0` \
                 lists the rest"
        ));
        let named = Listing {
            column: None,
            ..named
        };
        assert!(board_human(&board, &named, NOW).ends_with(
            "`cagents --device fw16 work rm CARD --project 'my repo' --dry-run` \
                 shows what removing a landed worktree loses"
        ));
    }

    /// The current directory is placed as the daemon places a session's: a
    /// checkout or linked worktree belongs to its open repository; an open
    /// folder, or no open project, widens to every project.
    #[test]
    fn the_current_directory_selects_the_open_git_project_containing_it() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let repository = root.join("app/.git");
        std::fs::create_dir_all(repository.join("worktrees/linked")).unwrap();
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        let linked = root.join("linked");
        std::fs::create_dir_all(linked.join("src")).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!(
                "gitdir: {}\n",
                repository.join("worktrees/linked").display()
            ),
        )
        .unwrap();
        std::fs::write(repository.join("worktrees/linked/commondir"), "../..\n").unwrap();
        let folder = root.join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        let open = |repository: &Path, kind| crate::workspace_model::ModelProject {
            repository: repository.to_owned(),
            name: "fixture".into(),
            kind,
            ordinal: 0,
        };
        let projects = [
            open(&repository, crate::model::ProjectKind::Git),
            open(&folder, crate::model::ProjectKind::Folder),
        ];
        assert_eq!(
            containing(&root.join("app/src"), &projects),
            Some(repository.clone())
        );
        assert_eq!(containing(&linked.join("src"), &projects), Some(repository));
        assert_eq!(containing(&folder, &projects), None);
        assert_eq!(containing(&root, &projects), None);
    }

    #[test]
    fn work_alone_lists_and_takes_the_list_options() {
        let parse = |arguments: &[&str]| {
            Cli::try_parse_from(std::iter::once("cagents").chain(arguments.iter().copied()))
                .and_then(Cli::validated)
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
        // Global options still combine with a subcommand.
        assert!(matches!(
            parse(&["work", "--json", "rm", "x", "--dry-run"]),
            Ok(Cli {
                global: GlobalOptions { json: true, .. },
                command: TopCommand::Work {
                    command: Some(WorkCommand::Rm(_)),
                    ..
                },
                ..
            })
        ));
        assert!(parse(&["work", "--all", "--project", "x"]).is_err());
    }

    /// A detached card is named by the path a listing shows, which a
    /// quoted `~` must not break.
    #[test]
    fn a_card_named_by_a_home_relative_path_is_sent_whole() {
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        std::fs::create_dir_all(home.join("linked")).unwrap();
        std::fs::write(home.join("linked/.git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(
            item_in("~/linked".into(), Some(&home)),
            home.join("linked").to_string_lossy()
        );
        // A branch, or a path only the daemon can place, is sent as given.
        assert_eq!(item_in("fix/login".into(), Some(&home)), "fix/login");
        assert_eq!(item_in("~/absent".into(), Some(&home)), "~/absent");
        assert_eq!(
            item_in(".worktrees/linked".into(), Some(&home)),
            ".worktrees/linked"
        );
    }
}
