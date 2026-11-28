use super::*;
use crate::agent_execution::{CompactionState, Operation, Reply, TrackedCompaction};

#[derive(Debug, clap::Args)]
pub struct CompactArgs {
    /// Session, lineage or agent IDs, or at least four characters of each.
    #[arg(required = true, value_name = "SESSION")]
    sessions: Vec<String>,
    /// Wait for each provider to confirm its compaction.
    #[arg(long)]
    wait: bool,
    /// Seconds to wait for all queued and submitted requests.
    #[arg(long, requires = "wait", value_parser = clap::value_parser!(u64).range(1..=300))]
    timeout: Option<u64>,
}

struct Row {
    target: String,
    session: Option<TerminalId>,
    request: Option<uuid::Uuid>,
    state: CompactionState,
}

impl Row {
    fn update(&mut self, result: TrackedCompaction) {
        self.session = Some(result.compaction.session);
        self.request = result.request;
        self.state = result.compaction.state;
    }

    fn pending(&self) -> bool {
        self.request.is_some()
            && matches!(
                self.state,
                CompactionState::Queued | CompactionState::Submitted
            )
    }
}

pub(super) fn execute(arguments: CompactArgs) -> Result<CommandOutput, AppError> {
    use CompactionState::{Completed, Failed, Queued, Submitted, Unconfirmed, Unsupported};
    let mut client = control_daemon_client()?;
    let authority = Authority::from_environment();
    let mut transport_failed = false;
    let mut rows = Vec::with_capacity(arguments.sessions.len());
    for target in arguments.sessions {
        let mut row = Row {
            target,
            session: None,
            request: None,
            state: Failed("The daemon connection was lost before this request".into()),
        };
        if !transport_failed {
            let operation = if arguments.wait {
                Operation::CompactTracked(row.target.clone())
            } else {
                Operation::Compact(row.target.clone())
            };
            match client.agent(authority.clone(), operation) {
                Ok(Reply::Compact(compaction)) if !arguments.wait => {
                    row.update(TrackedCompaction {
                        request: None,
                        compaction,
                    });
                }
                Ok(Reply::TrackedCompaction(result)) if arguments.wait => row.update(result),
                Ok(_) => row.state = Unconfirmed("Unexpected reply from the daemon".into()),
                Err(ControlError::Remote { mut message, .. }) => {
                    format::qualify_diagnostic(&mut message);
                    row.state = Failed(message);
                }
                Err(error) => {
                    transport_failed = true;
                    row.state = Unconfirmed(protocol_error(error).message);
                }
            }
        }
        rows.push(row);
    }

    if arguments.wait {
        let deadline = Instant::now() + Duration::from_secs(arguments.timeout.unwrap_or(120));
        while !transport_failed && Instant::now() < deadline && rows.iter().any(Row::pending) {
            for row in rows.iter_mut().filter(|row| row.pending()) {
                let request = row.request.expect("pending rows have a receipt");
                if Instant::now() >= deadline {
                    break;
                }
                match client.agent_before(
                    authority.clone(),
                    Operation::CompactionStatus(request),
                    Some(deadline),
                ) {
                    Ok(Reply::TrackedCompaction(result))
                        if result.request == Some(request)
                            && row.session == Some(result.compaction.session) =>
                    {
                        row.update(result);
                    }
                    Ok(_) => {
                        row.state =
                            Unconfirmed("Unexpected compaction receipt from the daemon".into())
                    }
                    Err(ControlError::Remote { mut message, .. }) => {
                        format::qualify_diagnostic(&mut message);
                        row.state = Unconfirmed(message);
                    }
                    Err(error) => {
                        transport_failed = true;
                        if !is_timeout_error(&error) {
                            row.state = Unconfirmed(protocol_error(error).message);
                        }
                        break;
                    }
                }
            }
            if !transport_failed && rows.iter().any(Row::pending) {
                std::thread::sleep(
                    Duration::from_millis(500)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let reason = if Instant::now() >= deadline {
            "The wait ended before the provider confirmed compaction"
        } else {
            "The daemon connection was lost before compaction was confirmed"
        };
        for row in rows.iter_mut().filter(|row| row.pending()) {
            row.state = Unconfirmed(reason.into());
        }
    }

    let mut lines = Vec::with_capacity(rows.len());
    let mut failed = false;
    let rows = rows
        .into_iter()
        .map(|row| {
            failed |= match row.state {
                Completed => false,
                Queued | Submitted => arguments.wait,
                _ => true,
            };
            let (state, reason) = match row.state {
                Completed => ("completed", None),
                Failed(reason) => ("failed", Some(reason)),
                Submitted => ("submitted", None),
                Queued => ("queued", None),
                Unsupported => ("unsupported", None),
                Unconfirmed(reason) => ("unconfirmed", Some(reason)),
            };
            let target = format::plain(&row.target);
            lines.push(match reason.as_deref().map(format::plain) {
                Some(detail) => format!("{target}  {state} · {detail}"),
                None => format!("{target}  {state}"),
            });
            json!({
                "target": row.target,
                "session": row.session,
                "state": state,
                "reason": reason,
            })
        })
        .collect::<Vec<_>>();
    Ok(CommandOutput {
        command: "compact",
        human: lines.join("\n"),
        data: json!({
            "sessions": rows,
            "outcome": if failed { "failed" } else { "succeeded" },
        }),
    })
}
