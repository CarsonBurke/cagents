//! Explicit user management of daemon-owned schedules. Revision and request
//! identities make confirmations and retries independent of CLI process life.
use super::*;
use crate::automations::{Definition, HistoryCursor, Request, Response};
use uuid::Uuid;

const MAX_DEFINITION_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum AutomationCommand {
    /// Create a disabled automation from a JSON definition (use - for stdin).
    Create {
        #[arg(long)]
        definition: PathBuf,
    },
    /// List definitions, with a cursor for the next page.
    List {
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Show {
        id: Uuid,
    },
    /// Replace a definition at the exact revision shown by `show`.
    Edit {
        id: Uuid,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        definition: PathBuf,
    },
    /// Confirm unattended execution of the displayed definition revision.
    Enable {
        id: Uuid,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        confirm: bool,
    },
    Disable {
        id: Uuid,
        #[arg(long)]
        revision: u64,
    },
    /// Remove a definition. Past runs remain in history.
    Remove {
        id: Uuid,
        #[arg(long)]
        revision: u64,
    },
    /// Run the exact execution revision once; reuse request-id for safe retry.
    RunNow {
        id: Uuid,
        #[arg(long)]
        execution_revision: u64,
        #[arg(long)]
        request_id: Uuid,
        #[arg(long)]
        confirm: bool,
    },
    Cancel {
        id: Uuid,
        #[arg(long)]
        firing: Uuid,
    },
    History {
        id: Uuid,
        #[arg(long, requires = "before_id")]
        before_ms: Option<u64>,
        #[arg(long, requires = "before_ms")]
        before_id: Option<Uuid>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Read one immutable firing, including its bounded execution result.
    Run {
        firing: Uuid,
    },
}

impl ControlClient {
    pub fn automation(
        &mut self,
        authority: crate::agent_execution::Authority,
        operation: Request,
    ) -> Result<Response, ControlError> {
        // Editing launch settings and removal can await an exact idle close.
        let response = self.request_with_deadline(
            ClientRequest::Automation {
                authority,
                operation,
            },
            Instant::now() + CHAT_CLOSE_TIMEOUT + Duration::from_secs(35),
        )?;
        match response {
            ServerResponse::Automation(response) => Ok(response),
            response => Err(unexpected_response("automation management", &response)),
        }
    }
}

pub(super) fn execute(command: AutomationCommand) -> Result<CommandOutput, AppError> {
    let authority = crate::agent_execution::Authority::from_environment();
    if authority.token().is_some() {
        return Err(AppError::new(
            "automation_authority",
            "An inherited agent capability cannot manage automations; use an explicit user session",
        ));
    }
    let (name, request) = request(command)?;
    let socket = terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let reply = client
        .automation(authority, request)
        .map_err(protocol_error)?;
    let (data, human) = match reply {
        Response::Listed(entries) => {
            let human = entries
                .iter()
                .map(|entry| {
                    format!(
                        "{}\t{}\t{}",
                        entry.id,
                        if entry.enabled { "enabled" } else { "paused" },
                        entry.name
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            (json!({"automations": entries}), human)
        }
        Response::Automation(automation) => {
            let human = format!(
                "{}\t{}\trevision {}\n{}",
                automation.id,
                if automation.enabled {
                    "enabled"
                } else {
                    "paused"
                },
                automation.revision,
                automation.definition.name
            );
            (json!({"automation": automation}), human)
        }
        Response::Firing(firing) => {
            let human = format!("{}\t{:?}", firing.id, firing.state);
            (json!({"firing": firing}), human)
        }
        Response::History(history) => {
            let human = history
                .firings
                .iter()
                .map(|run| format!("{}\t{:?}", run.id, run.state))
                .collect::<Vec<_>>()
                .join("\n");
            (json!({"history": history}), human)
        }
        Response::Detail(detail) => {
            let human = format!(
                "{}\t{:?}\n{}",
                detail.firing.id,
                detail.firing.state,
                detail
                    .run
                    .as_ref()
                    .map(|run| run.result.text.as_str())
                    .or(detail.firing.diagnostic.as_deref())
                    .unwrap_or_default()
            );
            (json!({"run": detail}), human)
        }
        Response::Removed => (
            json!({"removed": true}),
            "Automation removed. Past runs remain in history.".into(),
        ),
        Response::CancellationRequested => (
            json!({"cancellation_requested": true}),
            "Cancellation requested; inspect the run for its confirmed outcome.".into(),
        ),
    };
    Ok(CommandOutput {
        command: name,
        data,
        human,
    })
}

fn request(command: AutomationCommand) -> Result<(&'static str, Request), AppError> {
    Ok(match command {
        AutomationCommand::Create { definition } => (
            "automation.create",
            Request::Create {
                definition: read_definition(&definition)?,
            },
        ),
        AutomationCommand::List { after, limit } => {
            ("automation.list", Request::List { after, limit })
        }
        AutomationCommand::Show { id } => ("automation.show", Request::Show { id }),
        AutomationCommand::Edit {
            id,
            revision,
            definition,
        } => (
            "automation.edit",
            Request::Edit {
                id,
                revision,
                definition: read_definition(&definition)?,
            },
        ),
        AutomationCommand::Enable {
            id,
            revision,
            confirm,
        } => (
            "automation.enable",
            Request::SetEnabled {
                id,
                revision,
                enabled: true,
                confirmed: confirm,
            },
        ),
        AutomationCommand::Disable { id, revision } => (
            "automation.disable",
            Request::SetEnabled {
                id,
                revision,
                enabled: false,
                confirmed: false,
            },
        ),
        AutomationCommand::Remove { id, revision } => {
            ("automation.remove", Request::Remove { id, revision })
        }
        AutomationCommand::RunNow {
            id,
            execution_revision,
            request_id,
            confirm,
        } => (
            "automation.run_now",
            Request::RunNow {
                id,
                execution_revision,
                request: request_id,
                confirmed: confirm,
            },
        ),
        AutomationCommand::Cancel { id, firing } => {
            ("automation.cancel", Request::Cancel { id, firing })
        }
        AutomationCommand::History {
            id,
            before_ms,
            before_id,
            limit,
        } => (
            "automation.history",
            Request::History {
                id,
                before: before_ms
                    .zip(before_id)
                    .map(|(created_ms, id)| HistoryCursor { created_ms, id }),
                limit,
            },
        ),
        AutomationCommand::Run { firing } => ("automation.run", Request::ShowFiring { id: firing }),
    })
}

fn read_definition(path: &Path) -> Result<Definition, AppError> {
    let reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin())
    } else {
        Box::new(std::fs::File::open(path).map_err(|e| AppError::new("automation_definition", e))?)
    };
    let mut bytes = Vec::new();
    reader
        .take(MAX_DEFINITION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| AppError::new("automation_definition", e))?;
    if bytes.len() as u64 > MAX_DEFINITION_BYTES {
        return Err(AppError::new(
            "automation_definition",
            "The automation definition exceeds 2 MiB",
        ));
    }
    let definition: Definition =
        serde_json::from_slice(&bytes).map_err(|e| AppError::new("automation_definition", e))?;
    definition
        .validate()
        .map_err(|e| AppError::new("automation_definition", e))?;
    Ok(definition)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<AutomationCommand, clap::Error> {
        match Cli::try_parse_from(arguments)?.command {
            TopCommand::Automation { command } => Ok(command),
            _ => panic!("expected automation command"),
        }
    }

    #[test]
    fn manual_automation_run_requires_retry_identity_and_preserves_consent_revision() {
        let id = Uuid::new_v4().to_string();
        let retry = Uuid::new_v4().to_string();
        let base = [
            "cagents",
            "automation",
            "run-now",
            &id,
            "--execution-revision",
            "7",
        ];
        assert!(
            parse(&base).is_err(),
            "a retry key cannot be silently regenerated"
        );
        let mut arguments = base.to_vec();
        arguments.extend(["--request-id", &retry]);
        let (_, unconfirmed) = request(parse(&arguments).unwrap()).unwrap();
        assert!(matches!(
            unconfirmed,
            Request::RunNow {
                confirmed: false,
                ..
            }
        ));
        arguments.push("--confirm");
        let (_, confirmed) = request(parse(&arguments).unwrap()).unwrap();
        assert_eq!(
            confirmed,
            Request::RunNow {
                id: id.parse().unwrap(),
                execution_revision: 7,
                request: retry.parse().unwrap(),
                confirmed: true,
            }
        );
    }

    #[test]
    fn automation_history_requires_the_complete_stable_cursor() {
        let id = Uuid::new_v4().to_string();
        let entry = Uuid::new_v4().to_string();
        assert!(
            parse(&[
                "cagents",
                "automation",
                "history",
                &id,
                "--before-ms",
                "100"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "cagents",
                "automation",
                "history",
                &id,
                "--before-id",
                &entry
            ])
            .is_err()
        );
        let (_, request) = request(
            parse(&[
                "cagents",
                "automation",
                "history",
                &id,
                "--before-ms",
                "100",
                "--before-id",
                &entry,
            ])
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(request, Request::History {
            before: Some(HistoryCursor { created_ms: 100, id: cursor }), ..
        } if cursor.to_string() == entry));
    }

    #[test]
    fn automation_definition_input_is_bounded_before_deserialization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("definition.json");
        std::fs::write(&path, vec![b' '; MAX_DEFINITION_BYTES as usize + 1]).unwrap();
        assert!(
            read_definition(&path)
                .unwrap_err()
                .message
                .contains("exceeds 2 MiB")
        );
    }
}
