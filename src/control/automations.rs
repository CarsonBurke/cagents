//! Explicit user management of daemon-owned schedules. Revision and request
//! identities make confirmations and retries independent of CLI process life.
use super::*;
use crate::automations::{Definition, HistoryCursor, Request, Response};
use uuid::Uuid;

const MAX_DEFINITION_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum AutomationCommand {
    /// Save exact launch settings for approval in cagents, without creating a schedule.
    Propose {
        #[arg(long)]
        request_id: Uuid,
        #[arg(long)]
        definition: PathBuf,
        /// Wait for the user's decision so the requesting tool call resumes.
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=3600))]
        wait_seconds: u64,
    },
    /// Read only this conversation's immutable approval request.
    Proposal {
        id: Uuid,
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=3600))]
        wait_seconds: u64,
    },
    /// List pending requests from every project (user authority required).
    Proposals {
        /// Continue after this proposal UUID from the previous page.
        #[arg(long)]
        after: Option<Uuid>,
        /// Maximum number of pending requests to return.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Approve the immutable saved proposal (user authority required).
    Approve {
        /// Exact pending proposal UUID shown by automation proposals.
        id: Uuid,
        /// Immutable proposal revision being decided.
        #[arg(long, default_value_t = 1)]
        revision: u64,
    },
    /// Explicitly decline the immutable saved proposal.
    Decline {
        /// Exact pending proposal UUID shown by automation proposals.
        id: Uuid,
        /// Immutable proposal revision being decided.
        #[arg(long, default_value_t = 1)]
        revision: u64,
    },
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
        /// Omit to include all automations, including removed definitions.
        id: Option<Uuid>,
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
        self.automation_until(
            authority,
            operation,
            Instant::now() + CHAT_CLOSE_TIMEOUT + Duration::from_secs(35),
        )
    }

    fn automation_until(
        &mut self,
        authority: crate::agent_execution::Authority,
        operation: Request,
        deadline: Instant,
    ) -> Result<Response, ControlError> {
        let response = self.request_with_deadline(
            ClientRequest::Automation {
                authority,
                operation,
            },
            deadline,
        )?;
        match response {
            ServerResponse::Automation(response) => Ok(response),
            response => Err(unexpected_response("automation management", &response)),
        }
    }
}

pub(super) fn execute(command: AutomationCommand) -> Result<CommandOutput, AppError> {
    let authority = crate::agent_execution::Authority::from_environment();
    let wait = match &command {
        AutomationCommand::Propose {
            wait: true,
            wait_seconds,
            ..
        }
        | AutomationCommand::Proposal {
            wait: true,
            wait_seconds,
            ..
        } => Some(*wait_seconds),
        _ => None,
    };
    let (name, request) = request(command)?;
    if authority.token().is_some() && !request.allowed_from_hosted_conversation() {
        return Err(AppError::new(
            "automation_authority",
            "An inherited agent capability cannot alter unattended schedules; use automation propose for user approval",
        ));
    }
    let socket = terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let mut reply = client
        .automation(authority.clone(), request)
        .map_err(protocol_error)?;
    if let Some(seconds) = wait {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while let Response::Proposal(proposal) = &reply {
            if proposal.state != crate::automations::ProposalState::Pending
                || Instant::now() >= deadline
            {
                break;
            }
            let id = proposal.id;
            std::thread::sleep(
                Duration::from_secs(1).min(deadline.saturating_duration_since(Instant::now())),
            );
            if Instant::now() >= deadline {
                break;
            }
            reply = client
                .automation_until(
                    authority.clone(),
                    Request::Proposal { id },
                    deadline.min(Instant::now() + IO_TIMEOUT),
                )
                .map_err(protocol_error)?;
        }
    }
    Ok(response_output(name, reply))
}

fn response_output(name: &'static str, reply: Response) -> CommandOutput {
    let (data, human) = match reply {
        Response::Proposal(proposal) => {
            let human = match &proposal.state {
                crate::automations::ProposalState::Pending => format!(
                    "{}\tAwaiting user approval in cagents. No schedule is enabled.",
                    proposal.id
                ),
                crate::automations::ProposalState::Approved {
                    automation,
                    execution_revision,
                } => format!(
                    "{}\tApproved automation {automation}, execution revision {execution_revision}",
                    proposal.id
                ),
                crate::automations::ProposalState::Declined => {
                    format!("{}\tUser declined. No schedule was created.", proposal.id)
                }
            };
            (json!({"proposal":proposal}), human)
        }
        Response::Proposals(proposals) => {
            let human = if proposals.is_empty() {
                "No pending automation approval requests".into()
            } else {
                proposals
                    .iter()
                    .map(|proposal| {
                        format!(
                            "{}\t{}\t{}\trevision {}",
                            proposal.id,
                            proposal.name,
                            proposal.repository.display(),
                            proposal.revision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (json!({"proposals":proposals}), human)
        }
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
                .map(|run| {
                    if let Some(interval) = &run.missed_interval {
                        format!(
                            "{}\t{:?}\tmissed interval [{}..{}); occurrence count unknown",
                            run.id, run.state, interval.first_due_ms, interval.until_exclusive_ms
                        )
                    } else if let Some(summary) = &run.skipped_occurrences {
                        format!(
                            "{}\t{:?}\t{} occurrences\t{}..{}",
                            run.id,
                            run.state,
                            summary.count,
                            summary.first_occurrence_ms,
                            summary.last_occurrence_ms
                        )
                    } else {
                        format!("{}\t{:?}", run.id, run.state)
                    }
                })
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
        Response::ArchivedConversation(archive) => (
            json!({"archive":archive}),
            "Saved scheduled conversation".into(),
        ),
        Response::Removed => (
            json!({"removed": true}),
            "Automation removed. Past runs remain in history.".into(),
        ),
        Response::CancellationRequested => (
            json!({"cancellation_requested": true}),
            "Cancellation requested; inspect the run for its confirmed outcome.".into(),
        ),
    };
    CommandOutput {
        command: name,
        data,
        human,
    }
}

fn request(command: AutomationCommand) -> Result<(&'static str, Request), AppError> {
    Ok(match command {
        AutomationCommand::Propose {
            request_id,
            definition,
            ..
        } => (
            "automation.propose",
            Request::Propose {
                request: request_id,
                definition: read_definition(&definition)?,
            },
        ),
        AutomationCommand::Proposal { id, .. } => ("automation.proposal", Request::Proposal { id }),
        AutomationCommand::Proposals { after, limit } => (
            "automation.proposals",
            Request::AllPendingProposals { after, limit },
        ),
        AutomationCommand::Approve { id, revision } => (
            "automation.approve",
            Request::ResolveProposal {
                id,
                revision,
                approved: true,
            },
        ),
        AutomationCommand::Decline { id, revision } => (
            "automation.decline",
            Request::ResolveProposal {
                id,
                revision,
                approved: false,
            },
        ),
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
    fn proposal_management_is_explicit_and_never_granted_to_agent_tokens() {
        let id = Uuid::new_v4();
        let id_text = id.to_string();
        for (args, expected) in [
            (vec!["cagents", "automation", "approve", &id_text], true),
            (vec!["cagents", "automation", "decline", &id_text], false),
        ] {
            let (_, request) = request(parse(&args).unwrap()).unwrap();
            assert!(
                matches!(request, Request::ResolveProposal { id: actual, revision: 1, approved } if actual == id && approved == expected)
            );
            assert!(!request.allowed_from_hosted_conversation());
        }
        let (_, request) =
            request(parse(&["cagents", "automation", "proposals"]).unwrap()).unwrap();
        assert!(matches!(
            request,
            Request::AllPendingProposals {
                after: None,
                limit: 50
            }
        ));
        assert!(!request.allowed_from_hosted_conversation());
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
        let (_, all) = request(parse(&["cagents", "automation", "history"]).unwrap()).unwrap();
        assert!(matches!(
            all,
            Request::History {
                id: None,
                before: None,
                ..
            }
        ));
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

    #[test]
    fn automation_proposals_require_a_stable_retry_identity_and_bound_waits() {
        assert!(parse(&["cagents", "automation", "propose", "--definition", "-"]).is_err());
        let id = Uuid::new_v4().to_string();
        let parsed = parse(&[
            "cagents",
            "automation",
            "propose",
            "--request-id",
            &id,
            "--definition",
            "-",
            "--wait",
            "--wait-seconds",
            "30",
        ])
        .unwrap();
        assert!(
            matches!(parsed,AutomationCommand::Propose {request_id,wait:true,wait_seconds:30,..}
            if request_id.to_string()==id)
        );
        assert!(
            parse(&[
                "cagents",
                "automation",
                "proposal",
                &id,
                "--wait-seconds",
                "0"
            ])
            .is_err()
        );
        let (_, request) =
            request(parse(&["cagents", "automation", "proposal", &id, "--wait"]).unwrap()).unwrap();
        assert_eq!(
            request,
            Request::Proposal {
                id: id.parse().unwrap()
            }
        );
        assert!(request.allowed_from_hosted_conversation());
        assert!(
            !Request::ResolveProposal {
                id: id.parse().unwrap(),
                revision: 1,
                approved: true
            }
            .allowed_from_hosted_conversation()
        );
    }
    #[test]
    fn proposal_management_json_preserves_pending_list_and_immutable_decision_receipts() {
        use crate::automations::{Proposal, ProposalState, ProposalSummary};
        let definition: Definition = serde_json::from_value(json!({
            "name":"Requested fixture", "prompt":"Inspect project",
            "agent":{"provider":"Claude","account":null,
                "target":{"repository":"/project","directory":"/project","channel":null},
                "surface":"Chat","model":null,"permission_mode":null},
            "schedule":{"pattern":{"daily":{"hour":3,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        }))
        .unwrap();
        let mut proposal = Proposal {
            id: Uuid::new_v4(),
            revision: 1,
            owner: crate::agent_execution::Origin::Child {
                parent: crate::agent_execution::Lineage(crate::model::TerminalId::new()),
            },
            requested: definition.clone(),
            definition,
            target_identity: crate::automations::DirectoryIdentity {
                device: 1,
                inode: 1,
            },
            channel_title: None,
            created_ms: 100,
            state: ProposalState::Pending,
        };
        let list = response_output(
            "automation.proposals",
            Response::Proposals(vec![ProposalSummary::from(&proposal)]),
        );
        assert_eq!(list.command, "automation.proposals");
        assert_eq!(list.data["proposals"][0]["id"], proposal.id.to_string());
        assert_eq!(list.data["proposals"][0]["repository"], "/project");
        proposal.state = ProposalState::Declined;
        let declined = response_output(
            "automation.decline",
            Response::Proposal(Box::new(proposal.clone())),
        );
        assert_eq!(declined.data["proposal"]["state"], "Declined");
        let automation = Uuid::new_v4();
        proposal.state = ProposalState::Approved {
            automation,
            execution_revision: 7,
        };
        let approved =
            response_output("automation.approve", Response::Proposal(Box::new(proposal)));
        assert_eq!(
            approved.data["proposal"]["state"]["Approved"]["automation"],
            automation.to_string()
        );
        assert_eq!(
            approved.data["proposal"]["state"]["Approved"]["execution_revision"],
            7
        );
    }
}
