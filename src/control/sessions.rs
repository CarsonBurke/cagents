use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::{Directory, Entry, Relation, Scope},
};

#[derive(Debug, clap::Args)]
pub struct SessionsArgs {
    /// List sessions in every project, not just the current one.
    #[arg(long, conflicts_with = "project")]
    all: bool,
    /// List sessions in the open project with this name, or containing
    /// this path.
    #[arg(long)]
    project: Option<PathBuf>,
}

/// Who is calling, and which other sessions share its project.
pub(super) fn whoami() -> Result<CommandOutput, AppError> {
    let directory = read(Scope::Containing(current_directory()?))?;
    let mut human = match &directory.caller {
        Some(me) => me.line(),
        None => "Not running inside a cagents session".to_owned(),
    };
    let peers: Vec<_> = directory
        .sessions
        .iter()
        .filter(|entry| {
            directory.caller.is_none()
                || entry.relation != Relation::Peer
                || shares_checkout(&directory, entry)
        })
        .collect();
    if !peers.is_empty() {
        human.push_str("\n\nRelated sessions:");
        for entry in &peers {
            human.push('\n');
            human.push_str(&entry.line());
        }
    }
    Ok(CommandOutput {
        command: "whoami",
        data: json!({ "me": directory.caller, "scope": directory.scope, "sessions": peers }),
        human,
    })
}

pub(super) fn list(arguments: SessionsArgs) -> Result<CommandOutput, AppError> {
    let scope = if arguments.all {
        Scope::All
    } else if let Some(project) = arguments.project {
        Scope::Project(super::project_argument(project)?)
    } else {
        Scope::Containing(current_directory()?)
    };
    let directory = read(scope)?;
    let mut lines: Vec<_> = directory.caller.iter().map(Entry::line).collect();
    lines.extend(directory.sessions.iter().map(Entry::line));
    if lines.is_empty() {
        lines.push("No live sessions".into());
    }
    if let Scope::Project(project) = &directory.scope {
        lines.push(format!(
            "(project {}; --all for every project)",
            project.display()
        ));
    }
    Ok(CommandOutput {
        command: "sessions",
        data: json!({ "me": directory.caller, "scope": directory.scope, "sessions": directory.sessions }),
        human: lines.join("\n"),
    })
}

/// Peers in the caller's own checkout are the ones most likely to collide.
fn shares_checkout(directory: &Directory, entry: &Entry) -> bool {
    directory
        .caller
        .as_ref()
        .and_then(|me| me.worktree.as_ref())
        .is_some_and(|worktree| entry.worktree.as_ref() == Some(worktree))
}

fn current_directory() -> Result<PathBuf, AppError> {
    std::env::current_dir().map_err(|e| AppError::new("target", e))
}

fn read(scope: Scope) -> Result<Directory, AppError> {
    let socket = terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    match client
        .agent(Authority::from_environment(), Operation::Directory(scope))
        .map_err(protocol_error)?
    {
        Reply::Directory(directory) => Ok(directory),
        _ => Err(AppError::new("sessions", "Unexpected directory response")),
    }
}
