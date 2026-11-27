//! `cagents resume`: reopen the conversations of sessions whose provider
//! stopped, as each pane's Resume does in the app, from a shell that has no
//! window, such as a headless machine's after a reboot.
use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::Scope as Sessions,
    resume::{Resumed, Scope, Stopped},
};

#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("named")
        .args(["sessions", "channel", "all", "project"])
        .required(true)
))]
pub struct ResumeArgs {
    /// Sessions to resume: a session, lineage or agent ID, or at least four
    /// characters of one, as `cagents send` takes.
    #[arg(value_name = "SESSION", conflicts_with_all = ["channel", "all", "project"])]
    sessions: Vec<String>,
    /// Resume every stopped session in this channel, or in yours when no
    /// UUID is given.
    #[arg(long, value_name = "CHANNEL", num_args = 0..=1, default_missing_value = "",
        conflicts_with_all = ["all", "project"])]
    channel: Option<String>,
    /// Resume every stopped session in every project.
    #[arg(long, conflicts_with = "project")]
    all: bool,
    /// Resume every stopped session in the open project with this name, or
    /// containing this path.
    #[arg(long)]
    project: Option<PathBuf>,
}

/// What became of one named session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Resumed(Resumed),
    Failed { session: String, reason: String },
}

pub(super) fn resume(arguments: ResumeArgs) -> Result<CommandOutput, AppError> {
    let mut client = control_daemon_client()?;
    let mut request = |operation| client.agent(Authority::from_environment(), operation);
    let named: Vec<String> = match scope(&arguments)? {
        None => arguments.sessions,
        Some(scope) => match request(Operation::Stopped(scope)).map_err(protocol_error)? {
            Reply::Stopped(stopped) => stopped
                .into_iter()
                .map(|Stopped { id, .. }| id.to_string())
                .collect(),
            _ => return Err(AppError::new("resume", "Unexpected reply from the daemon")),
        },
    };
    // One at a time, in order: each start is admitted against the daemon's
    // capacity as the one before it settles. A refusal fails only its own
    // session; a lost connection or timeout leaves the rest untried, and
    // what already resumed is still reported.
    let mut results = Vec::with_capacity(named.len());
    let mut lost: Option<String> = None;
    for session in named {
        if let Some(reason) = &lost {
            results.push(Outcome::Failed {
                session,
                reason: format!("Not tried: {reason}"),
            });
            continue;
        }
        results.push(match request(Operation::Resume(session.clone())) {
            Ok(Reply::Resumed(resumed)) => Outcome::Resumed(resumed),
            Ok(_) => Outcome::Failed {
                session,
                reason: "Unexpected reply from the daemon".into(),
            },
            Err(ControlError::Remote { message, .. }) => Outcome::Failed {
                session,
                reason: message,
            },
            Err(error) => {
                let error = protocol_error(error);
                lost = Some(error.message.clone());
                Outcome::Failed {
                    session,
                    reason: format!(
                        "{}; the daemon may still be starting it, so check with `cagents ls --all` before trying again",
                        error.message
                    ),
                }
            }
        });
    }
    Ok(output(&results, &format::program()))
}

/// The scope a flag names, or `None` when sessions are named one by one.
fn scope(arguments: &ResumeArgs) -> Result<Option<Scope>, AppError> {
    if let Some(channel) = &arguments.channel {
        let channel =
            if channel.is_empty() {
                None
            } else {
                Some(channel.parse().map_err(|_| {
                    AppError::new("target", format!("{channel} is not a channel UUID"))
                })?)
            };
        return Ok(Some(Scope::Channel(channel)));
    }
    if arguments.all {
        return Ok(Some(Scope::Sessions(Sessions::All)));
    }
    match &arguments.project {
        Some(project) => Ok(Some(Scope::Sessions(Sessions::Project(
            super::project_argument(project.clone())?,
        )))),
        None => Ok(None),
    }
}

fn output(results: &[Outcome], program: &str) -> CommandOutput {
    let failed = results
        .iter()
        .any(|result| matches!(result, Outcome::Failed { .. }));
    let rows: Vec<Value> = results.iter().map(row).collect();
    CommandOutput {
        command: "resume",
        human: if super::human_output() {
            human(results, program)
        } else {
            String::new()
        },
        data: json!({
            "sessions": rows,
            "outcome": if failed { "failed" } else { "succeeded" },
        }),
    }
}

fn row(result: &Outcome) -> Value {
    match result {
        Outcome::Resumed(resumed) => json!({
            "session": resumed.stopped,
            "running": resumed.session,
            "provider": resumed.provider.executable(),
            "title": resumed.title,
            "agent": resumed.agent,
            "state": if resumed.already_running {
                "already_running"
            } else if resumed.starting {
                "starting"
            } else {
                "resumed"
            },
            "note": resumed.note,
        }),
        Outcome::Failed { session, reason } => json!({
            "session": session,
            "state": "failed",
            "error": reason,
        }),
    }
}

fn human(results: &[Outcome], program: &str) -> String {
    if results.is_empty() {
        return "No stopped sessions to resume.".into();
    }
    let mut lines = Vec::new();
    for result in results {
        match result {
            Outcome::Resumed(resumed) => {
                let name = match &resumed.title {
                    Some(title) => format!(
                        "{} ({}, {})",
                        resumed.stopped,
                        resumed.provider.label(),
                        format::plain(title)
                    ),
                    None => format!("{} ({})", resumed.stopped, resumed.provider.label()),
                };
                let mut line = if resumed.already_running {
                    format!("Already running: {name}")
                } else if resumed.starting {
                    format!(
                        "Resumed {name}; its provider is still starting, and `{program} ls --all` shows when it is ready or why it stopped"
                    )
                } else {
                    format!("Resumed {name}")
                };
                if resumed.session != resumed.stopped {
                    line.push_str(&format!(" as {}", resumed.session));
                }
                lines.push(line);
                if let Some(note) = &resumed.note {
                    lines.push(format!("  {}", format::plain(note)));
                }
            }
            Outcome::Failed { session, reason } => {
                lines.push(format!(
                    "Could not resume {session}: {}",
                    format::plain(reason)
                ));
            }
        }
    }
    let resumed = results
        .iter()
        .filter(|result| matches!(result, Outcome::Resumed(resumed) if !resumed.already_running))
        .count();
    if resumed > 0 {
        lines.push(format!(
            "Each waits idle for its next message; send one with `{program} send SESSION`."
        ));
    }
    lines.join("\n")
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use crate::{agent::Provider, model::TerminalId};

    fn resumed(already_running: bool, moved: bool) -> Resumed {
        let stopped = TerminalId::new();
        Resumed {
            stopped,
            session: if moved { TerminalId::new() } else { stopped },
            provider: Provider::Codex,
            title: Some("Fix the build".into()),
            agent: None,
            already_running,
            starting: false,
            note: None,
        }
    }

    fn parse(arguments: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("cagents").chain(arguments.iter().copied()))
    }

    #[test]
    fn resume_takes_sessions_or_one_scope_and_never_nothing() {
        assert!(parse(&["resume", "c4477d33", "4518d661"]).is_ok());
        assert!(parse(&["resume", "--channel"]).is_ok());
        assert!(parse(&["resume", "--all"]).is_ok());
        assert!(parse(&["resume", "--project", "cagents"]).is_ok());
        // Nothing named resumes nothing rather than guessing a scope.
        assert!(parse(&["resume"]).is_err());
        assert!(parse(&["resume", "c4477d33", "--all"]).is_err());
        assert!(parse(&["resume", "--channel", "--all"]).is_err());
    }

    #[test]
    fn a_failed_resume_says_why_and_fails_the_command() {
        let reason = "Antigravity sign-in needs attention";
        let results = [
            Outcome::Resumed(resumed(false, false)),
            Outcome::Failed {
                session: "a3d7ff73".into(),
                reason: reason.into(),
            },
        ];
        let output = output(&results, "cagents");
        assert_eq!(output.data["outcome"], "failed");
        assert_eq!(output.exit_code(), ExitCode::from(4));
        assert_eq!(output.data["sessions"][1]["error"], reason);
        let text = human(&results, "cagents");
        assert!(
            text.contains(&format!("Could not resume a3d7ff73: {reason}")),
            "{text}"
        );
    }

    #[test]
    fn the_listing_names_a_successor_and_what_was_already_running() {
        let moved = resumed(false, true);
        let running = resumed(true, false);
        let results = [
            Outcome::Resumed(moved.clone()),
            Outcome::Resumed(running.clone()),
        ];
        let output = output(&results, "cagents");
        assert_eq!(output.data["outcome"], "succeeded");
        assert_eq!(output.data["sessions"][1]["state"], "already_running");
        let text = human(&results, "cagents");
        assert!(
            text.contains(&format!(
                "Resumed {} (Codex, Fix the build) as {}",
                moved.stopped, moved.session
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("Already running: {}", running.stopped)),
            "{text}"
        );
        assert!(text.contains("cagents send SESSION"), "{text}");
        assert_eq!(human(&[], "cagents"), "No stopped sessions to resume.");
    }
}
