use super::*;
use crate::agent_execution::{Compaction, CompactionState, Operation, Reply};

#[derive(Debug, clap::Args)]
pub struct CompactArgs {
    /// Session, lineage or agent IDs, or at least four characters of each.
    #[arg(required = true, value_name = "SESSION")]
    sessions: Vec<String>,
}

pub(super) fn execute(arguments: CompactArgs) -> Result<CommandOutput, AppError> {
    let mut client = control_daemon_client()?;
    let authority = Authority::from_environment();
    let mut transport_failed = false;
    let mut rows = Vec::with_capacity(arguments.sessions.len());
    let mut lines = Vec::with_capacity(arguments.sessions.len());
    let mut failed = false;
    for target in arguments.sessions {
        let result = if transport_failed {
            Err((
                "failed",
                "The daemon connection was lost before this request".to_owned(),
            ))
        } else {
            match client.agent(authority.clone(), Operation::Compact(target.clone())) {
                Ok(Reply::Compact(compaction)) => Ok(compaction),
                Ok(_) => Err(("unconfirmed", "Unexpected reply from the daemon".into())),
                Err(ControlError::Remote { mut message, .. }) => {
                    format::qualify_diagnostic(&mut message);
                    Err(("failed", message))
                }
                Err(error) => {
                    transport_failed = true;
                    Err(("unconfirmed", protocol_error(error).message))
                }
            }
        };
        let (session, state, reason) = match result {
            Ok(Compaction { session, state }) => {
                let (state, reason) = match state {
                    CompactionState::Submitted => ("submitted", None),
                    CompactionState::Queued => ("queued", None),
                    CompactionState::Unsupported => ("unsupported", None),
                    CompactionState::Unconfirmed(reason) => ("unconfirmed", Some(reason)),
                };
                (Some(session), state, reason)
            }
            Err((state, reason)) => (None, state, Some(reason)),
        };
        failed |= !matches!(state, "submitted" | "queued");
        let detail = reason.as_deref().map(format::plain);
        lines.push(match detail {
            Some(detail) => format!("{}  {state} · {detail}", format::plain(&target)),
            None => format!("{}  {state}", format::plain(&target)),
        });
        rows.push(json!({
            "target": target,
            "session": session,
            "state": state,
            "reason": reason,
        }));
    }
    Ok(CommandOutput {
        command: "compact",
        human: lines.join("\n"),
        data: json!({
            "sessions": rows,
            "outcome": if failed { "failed" } else { "succeeded" },
        }),
    })
}
