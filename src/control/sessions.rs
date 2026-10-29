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
        Some(me) => list_human(std::iter::once(me)),
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
        human.push_str("\n\nRelated sessions:\n");
        human.push_str(&list_human(peers.iter().copied()));
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
    let mut human = list_human(directory.caller.iter().chain(directory.sessions.iter()));
    if let Scope::Project(project) = &directory.scope {
        human.push_str(&format!(
            "\n(project {}; --all for every project)",
            super::format::plain(&project.display().to_string())
        ));
    }
    Ok(CommandOutput {
        command: "sessions",
        data: json!({ "me": directory.caller, "scope": directory.scope, "sessions": directory.sessions }),
        human,
    })
}

fn list_human<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> String {
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
                    if entry.status.is_none()
                        && let Some(state) = delegation.run_state
                    {
                        context.push(format!("latest run {}", super::agents::state_label(state)));
                    }
                }
                if let Some(status) = &entry.status {
                    context.extend(status.explanation());
                }
                [
                    entry.id.to_string(),
                    entry.provider.map_or("shell", |p| p.executable()).into(),
                    match entry.state {
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
            status: None,
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
        }
    }

    #[test]
    fn readable_session_rows_keep_original_identity_and_delegation_context() {
        let entry = entry();
        let before = serde_json::to_value(&entry).unwrap();
        let human = list_human(std::iter::once(&entry));
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
        assert_eq!(list_human(std::iter::empty::<&Entry>()), "No live sessions");
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
            list_human(std::iter::once(&entry)),
            "SESSION                               PROVIDER  STATE    CONTEXT\n00000000-0000-4000-8000-000000000001  shell     unknown  me"
        );
    }

    #[test]
    fn current_request_failed_submission_and_queued_instruction_remain_distinct() {
        use crate::agent_execution::directory::{ExecutionStatus, RunStatus, Status};
        let mut entry = entry();
        let current = crate::model::RunId::new();
        let old = crate::model::RunId::new();
        entry.state = ActivityState::Working;
        entry.status = Some(Status {
            observed: Some(ExecutionStatus {
                activity: ActivityState::Working,
                generation: 12,
                turn_generation: 3,
                parent_busy: true,
                active_run: Some(current),
            }),
            latest_run: Some(RunStatus {
                id: old,
                state: State::Failed,
                request_recorded: false,
                diagnostic: Some(
                    "Selected model is at capacity. Please try a different model.".into(),
                ),
            }),
            pending_message: Some(true),
        });
        let original = serde_json::to_value(&entry).unwrap();
        let human = list_human(std::iter::once(&entry));
        assert!(human.contains("working"));
        assert!(human.contains(&format!("tracked request {current} (working)")));
        assert!(human.contains(&format!("latest tracked run {old} failed")));
        assert!(human.contains("provider request not recorded"));
        assert!(human.contains("Selected model is at capacity"));
        assert!(human.contains("instruction queued; not delivered"));
        assert!(!human.contains("provider failed") && !human.contains("retry accepted"));
        assert_eq!(entry.state, ActivityState::Working);
        assert_eq!(serde_json::to_value(&entry).unwrap(), original);
    }

    #[test]
    fn status_error_summary_is_bounded_single_line_without_losing_structured_detail() {
        use crate::agent_execution::directory::{RunStatus, Status};
        let mut entry = entry();
        let diagnostic = format!("capacity\n\t\u{1b}{}", "λ".repeat(400));
        entry.status = Some(Status {
            observed: None,
            latest_run: Some(RunStatus {
                id: crate::model::RunId::new(),
                state: State::Failed,
                request_recorded: true,
                diagnostic: Some(diagnostic.clone()),
            }),
            pending_message: Some(false),
        });
        let explanations = entry.status.as_ref().unwrap().explanation();
        let summary = explanations.last().unwrap();
        assert!(summary.starts_with("last run error: capacity"));
        assert!(!summary.chars().any(char::is_control));
        assert!(summary.len() <= "last run error: ".len() + 163);
        assert!(summary.ends_with('…'));
        let human = list_human(std::iter::once(&entry));
        assert_eq!(human.lines().count(), 2);
        assert!(!human.contains('\u{1b}'));
        assert!(
            !human.contains("instruction consumed") && !human.contains("provider start confirmed")
        );
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["status"]["latest_run"]["diagnostic"], diagnostic);
        let mut legacy = json;
        legacy.as_object_mut().unwrap().remove("status");
        let decoded: Entry = serde_json::from_value(legacy).unwrap();
        assert!(decoded.status.is_none());
        assert_eq!(decoded.state, entry.state);
    }

    #[test]
    fn status_distinguishes_background_work_from_a_parent_recorded_request() {
        use crate::agent_execution::directory::{ExecutionStatus, Status};
        let mut entry = entry();
        let request = crate::model::RunId::new();
        entry.state = ActivityState::Working;
        entry.status = Some(Status {
            observed: Some(ExecutionStatus {
                activity: ActivityState::Working,
                generation: 12,
                turn_generation: 3,
                parent_busy: false,
                active_run: Some(request),
            }),
            latest_run: None,
            pending_message: Some(false),
        });
        let human = list_human(std::iter::once(&entry));
        assert!(human.contains(&format!(
            "parent not busy; background work; tracked request {request}"
        )));
        assert!(!human.contains(&format!("tracked request {request} (working)")));
        assert!(!human.contains("provider idle") && !human.contains("provider execution"));
        assert_eq!(entry.state, ActivityState::Working);
    }
}
