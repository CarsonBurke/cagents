use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::{Directory, Entry, Relation, Scope},
};

#[derive(Debug, clap::Args)]
pub struct SessionsArgs {
    /// List sessions in every project, not just the current one.
    #[arg(long, conflicts_with_all = ["project", "channel"])]
    all: bool,
    /// List sessions in the open project with this name, or containing
    /// this path.
    #[arg(long, conflicts_with = "channel")]
    project: Option<PathBuf>,
    /// List the sessions in this channel, or in yours when no UUID is given,
    /// with how recently each was active. Includes chats whose provider exited.
    #[arg(long, value_name = "CHANNEL", num_args = 0..=1, default_missing_value = "")]
    channel: Option<String>,
}

/// Who is calling, and which other sessions share its channel or checkout.
pub(super) fn whoami() -> Result<CommandOutput, AppError> {
    let directory = read(Scope::Containing(current_directory()?))?;
    let channel = directory.caller.as_ref().and_then(|me| me.channel);
    let now = now_ms();
    let mut human = match &directory.caller {
        Some(me) => list_human(std::iter::once(me), channel, now),
        None => "Not running inside a cagents session".to_owned(),
    };
    let peers: Vec<_> = directory
        .sessions
        .iter()
        .filter(|entry| {
            directory.caller.is_none()
                || entry.relation != Relation::Peer
                || channel.is_some() && entry.channel == channel
                || shares_checkout(&directory, entry)
        })
        .collect();
    if !peers.is_empty() {
        human.push_str("\n\nRelated sessions:\n");
        human.push_str(&list_human(peers.iter().copied(), channel, now));
    }
    if channel.is_some() {
        human.push_str(
            "\n\n`cagents ls --channel` lists your channel, including ended sessions; `cagents transcript SESSION` reads one.",
        );
    }
    Ok(CommandOutput {
        command: "whoami",
        data: json!({ "me": directory.caller, "scope": directory.scope, "sessions": peers }),
        human,
    })
}

pub(super) fn list(arguments: SessionsArgs) -> Result<CommandOutput, AppError> {
    let scope =
        if arguments.all {
            Scope::All
        } else if let Some(channel) = arguments.channel {
            Scope::Channel(if channel.is_empty() {
                None
            } else {
                Some(channel.parse().map_err(|_| {
                    AppError::new("target", format!("{channel} is not a channel UUID"))
                })?)
            })
        } else if let Some(project) = arguments.project {
            Scope::Project(super::project_argument(project)?)
        } else {
            Scope::Containing(current_directory()?)
        };
    let directory = read(scope)?;
    let channel = directory.caller.as_ref().and_then(|me| me.channel);
    let mut human = list_human(
        directory.caller.iter().chain(directory.sessions.iter()),
        channel,
        now_ms(),
    );
    match &directory.scope {
        Scope::Project(project) => human.push_str(&format!(
            "\n(project {}; --all for every project, --channel for yours)",
            super::format::plain(&project.display().to_string())
        )),
        Scope::Channel(Some(channel)) => human.push_str(&format!(
            "\n(channel {channel}; read a conversation with `cagents transcript SESSION`)"
        )),
        _ => {}
    }
    Ok(CommandOutput {
        command: "sessions",
        data: json!({ "me": directory.caller, "scope": directory.scope, "sessions": directory.sessions }),
        human,
    })
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `channel` is the caller's, so a session sharing it is marked.
pub(super) fn list_human<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    channel: Option<crate::model::ChannelId>,
    now: u64,
) -> String {
    use crate::agent::ActivityState;
    use crate::agent_execution::directory::Owner;
    super::format::table(
        ["SESSION", "PROVIDER", "STATE", "CONTEXT"],
        entries
            .into_iter()
            .map(|entry| {
                let mut context = Vec::new();
                if let Some(project) = &entry.project_name {
                    context.push(format!("[{project}]"));
                }
                if let Some(directory) = &entry.directory {
                    context.push(directory.display().to_string());
                }
                if let Some(title) = entry.title.as_deref().filter(|title| !title.is_empty()) {
                    context.push(format!("\"{title}\""));
                }
                if let Some(crate::agent_execution::Placement::Worktree { path, .. }) =
                    &entry.view_scope
                {
                    context.push(format!("worktree view {}", path.display()));
                }
                if entry.relation != Relation::Myself
                    && channel.is_some()
                    && entry.channel == channel
                {
                    context.push("my channel".into());
                }
                match (entry.relation, entry.delegation.as_ref().map(|d| &d.owner)) {
                    (Relation::Myself, _) => context.push("me".into()),
                    (Relation::Parent, _) => context.push("my parent".into()),
                    (Relation::Child, Some(Owner::User)) => context.push("user agent".into()),
                    (Relation::Child, _) => context.push("my agent".into()),
                    (Relation::Peer, _) => {}
                }
                if entry.lineage.0 != entry.id {
                    context.push(format!("lineage {}", entry.lineage));
                }
                if let Some(delegation) = &entry.delegation {
                    context.push(format!("agent {}", delegation.agent));
                    if let Some(state) = delegation.run_state {
                        context.push(format!("latest run {}", super::agents::state_label(state)));
                    }
                }
                if let Some(at) = entry.active_at {
                    context.push(format!("active {}", super::format::relative_time(at, now)));
                }
                if let Some(failure) = &entry.failure {
                    context.push(format!("failed: {failure}"));
                }
                [
                    entry.id.to_string(),
                    entry.provider.map_or("shell", |p| p.executable()).into(),
                    match entry.state {
                        _ if entry.exited => "exited",
                        ActivityState::Unknown => "unknown",
                        ActivityState::Idle => "idle",
                        ActivityState::Working => "working",
                        ActivityState::Waiting => "waiting",
                    }
                    .into(),
                    context.join("; "),
                ]
            })
            .collect(),
        "No live sessions",
    )
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
    let socket = control_daemon_socket()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ActivityState;
    use crate::agent_execution::{
        Lineage, Placement, State,
        directory::{Delegation, Owner, Surface},
    };

    fn entry() -> Entry {
        let id = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        Entry {
            id,
            surface: Surface::Chat,
            provider: Some(crate::agent::Provider::Claude),
            title: Some("review\n\u{1b}title".into()),
            channel: None,
            view_scope: Some(Placement::Worktree {
                repository: "/fixture/.git".into(),
                path: "/fixture/linked".into(),
            }),
            channel_repository: None,
            state: ActivityState::Waiting,
            directory: Some("/fixture/linked".into()),
            worktree: Some("/fixture/linked".into()),
            project: Some("/fixture/.git".into()),
            project_name: Some("Fixture".into()),
            delegation: Some(Delegation {
                agent: "00000000-0000-4000-8000-000000000002".parse().unwrap(),
                owner: Owner::User,
                run: Some("00000000-0000-4000-8000-000000000004".parse().unwrap()),
                run_state: Some(State::Interrupted),
            }),
            relation: Relation::Child,
            lineage: Lineage("00000000-0000-4000-8000-000000000003".parse().unwrap()),
            exited: false,
            failure: None,
            active_at: None,
        }
    }

    #[test]
    fn readable_session_rows_keep_original_identity_and_delegation_context() {
        let entry = entry();
        let before = serde_json::to_value(&entry).unwrap();
        let human = list_human(std::iter::once(&entry), None, 0);
        assert_eq!(human.lines().count(), 2);
        assert_eq!(
            human
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["SESSION", "PROVIDER", "STATE", "CONTEXT"]
        );
        for expected in [
            "00000000-0000-4000-8000-000000000001",
            "claude",
            "waiting",
            "[Fixture]",
            "\"review  title\"",
            "worktree view /fixture/linked",
            "user agent",
            "lineage 00000000-0000-4000-8000-000000000003",
            "agent 00000000-0000-4000-8000-000000000002",
            "latest run Outcome uncertain",
        ] {
            assert!(human.contains(expected), "{human}");
        }
        for debug in ["Some(", "None", "Interrupted", "{ ", "_ms"] {
            assert!(!human.contains(debug));
        }
        assert!(!human.contains('\u{1b}'));
        assert_eq!(serde_json::to_value(&entry).unwrap(), before);
    }

    #[test]
    fn readable_sessions_empty_and_plain_shell_rows_are_explicit() {
        assert_eq!(
            list_human(std::iter::empty::<&Entry>(), None, 0),
            "No live sessions"
        );
        let mut entry = entry();
        entry.provider = None;
        entry.surface = Surface::Terminal;
        entry.state = ActivityState::Unknown;
        entry.relation = Relation::Myself;
        entry.delegation = None;
        entry.title = None;
        entry.directory = None;
        entry.worktree = None;
        entry.project = None;
        entry.project_name = None;
        entry.view_scope = None;
        entry.lineage = Lineage(entry.id);
        assert_eq!(
            list_human(std::iter::once(&entry), None, 0),
            "SESSION                               PROVIDER  STATE    CONTEXT\n00000000-0000-4000-8000-000000000001  shell     unknown  me"
        );
    }
}
