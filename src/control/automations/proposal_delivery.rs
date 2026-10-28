//! Explicit user commands; inherited authority is never replaced or cleared.
use super::*;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List pending metadata; continue with the returned registry and clock.
    Proposals {
        #[arg(long, requires_all = ["registry", "sequence"])]
        after: Option<Uuid>,
        #[arg(long, requires = "sequence")]
        registry: Option<Uuid>,
        #[arg(long, requires = "registry", value_parser = clap::value_parser!(u64).range(1..))]
        sequence: Option<u64>,
        #[arg(long, default_value_t = 50, value_parser = page_limit)]
        limit: usize,
    },
    /// Read full immutable consent from the exact registry shown by proposals.
    ReviewProposal {
        id: Uuid,
        #[arg(long)]
        registry: Uuid,
    },
    /// Approve the exact reviewed proposal (user only).
    Approve {
        id: Uuid,
        #[arg(long)]
        registry: Uuid,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        revision: u64,
        #[arg(long)]
        confirm: bool,
    },
    /// Explicitly decline the exact reviewed revision (user only).
    Decline {
        id: Uuid,
        #[arg(long)]
        registry: Uuid,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        revision: u64,
    },
}

fn page_limit(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|limit| (1..=crate::automations::PROPOSAL_PAGE_SIZE).contains(limit))
        .ok_or_else(|| "Choose a proposal page size from 1 through 64".into())
}

struct Plan {
    name: &'static str,
    request: Request,
    decision: Option<(Uuid, Uuid, u64, bool)>,
}

fn plan(command: Command) -> Result<Plan, AppError> {
    Ok(match command {
        Command::Proposals {
            after,
            registry,
            sequence,
            limit,
        } => Plan {
            name: "automation.proposals",
            request: Request::AllPendingProposals {
                identity: registry,
                sequence,
                after,
                limit,
            },
            decision: None,
        },
        Command::ReviewProposal { id, registry } => Plan {
            name: "automation.review_proposal",
            request: Request::ReviewProposal {
                identity: registry,
                id,
            },
            decision: None,
        },
        Command::Approve {
            id,
            registry,
            revision,
            confirm,
        } => {
            if !confirm {
                return Err(AppError::new(
                    "automation_confirmation",
                    "Review the immutable proposal, then pass its exact --registry, --revision and --confirm to approve unattended execution",
                ));
            }
            Plan {
                name: "automation.approve",
                request: Request::ReviewProposal {
                    identity: registry,
                    id,
                },
                decision: Some((registry, id, revision, true)),
            }
        }
        Command::Decline {
            id,
            registry,
            revision,
        } => Plan {
            name: "automation.decline",
            request: Request::ReviewProposal {
                identity: registry,
                id,
            },
            decision: Some((registry, id, revision, false)),
        },
    })
}

pub(super) fn execute(
    command: Command,
    authority: crate::agent_execution::Authority,
) -> Result<CommandOutput, AppError> {
    if authority.token().is_some() {
        return Err(AppError::new(
            "automation_authority",
            "An inherited session capability cannot inspect or decide global approval requests",
        ));
    }
    let Plan {
        name,
        request,
        decision,
    } = plan(command)?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    let mut reply = client
        .automation(authority.clone(), request)
        .map_err(protocol_error)?;
    if let Some((identity, id, revision, approved)) = decision {
        let Response::ProposalReview(review) = reply else {
            return Err(AppError::new(
                "automation_proposal",
                "The daemon did not return the exact proposal review",
            ));
        };
        if review.identity != identity
            || review.proposal.id != id
            || review.proposal.revision != revision
        {
            return Err(AppError::new(
                "automation_proposal",
                "The proposal registry or revision changed; review the current definition",
            ));
        }
        // Even a terminal receipt is sent through the scoped resolver: final
        // identity/admission is checked again and winning decisions stay terminal.
        reply = client
            .automation(
                authority,
                Request::ResolveReviewedProposal {
                    identity,
                    proposal: review.proposal,
                    approved,
                },
            )
            .map_err(protocol_error)?;
    }
    super::response_output(name, reply, None)
}

pub(super) fn list_human(proposals: &[crate::automations::ProposalSummary]) -> String {
    super::super::format::table(
        ["PROPOSAL", "REVISION", "NAME", "PROJECT"],
        proposals
            .iter()
            .map(|row| {
                [
                    row.id.to_string(),
                    row.revision.to_string(),
                    row.name.clone(),
                    row.repository.display().to_string(),
                ]
            })
            .collect(),
        "No pending automation requests",
    )
}

pub(super) fn page_human(page: &crate::automations::ProposalPage) -> String {
    let mut text = format!(
        "Registry {} · proposal clock {}\n{}",
        page.identity,
        page.automation_proposal_sequence,
        list_human(&page.proposals)
    );
    if let Some(last) = page.proposals.last() {
        text.push_str(&format!("\nContinue with --after {} --registry {} --sequence {}; review full settings with automation review-proposal ID --registry {}", last.id,page.identity,page.automation_proposal_sequence,page.identity));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        match Cli::try_parse_from(args).unwrap().command {
            TopCommand::Automation {
                command: AutomationCommand::ProposalDelivery(command),
            } => command,
            _ => panic!("expected user proposal command"),
        }
    }

    #[test]
    fn proposal_decisions_require_registry_revision_confirmation_and_never_infer_user_authority() {
        let id = Uuid::new_v4().to_string();
        let identity = Uuid::new_v4().to_string();
        assert!(
            Cli::try_parse_from(["cagents", "automation", "approve", &id, "--confirm"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["cagents", "automation", "decline", &id, "--revision", "7"])
                .is_err()
        );
        assert!(
            plan(parse(&[
                "cagents",
                "automation",
                "approve",
                &id,
                "--registry",
                &identity,
                "--revision",
                "7"
            ]))
            .is_err()
        );
        for (verb, approved) in [("approve", true), ("decline", false)] {
            let mut args = vec![
                "cagents",
                "automation",
                verb,
                &id,
                "--registry",
                &identity,
                "--revision",
                "7",
            ];
            if approved {
                args.push("--confirm");
            }
            let operation = plan(parse(&args)).unwrap();
            assert_eq!(
                operation.request,
                Request::ReviewProposal {
                    identity: identity.parse().unwrap(),
                    id: id.parse().unwrap()
                }
            );
            assert_eq!(
                operation.decision,
                Some((identity.parse().unwrap(), id.parse().unwrap(), 7, approved))
            );
            assert!(!operation.request.allowed_from_hosted_conversation());
        }
        for limit in ["0", "65"] {
            assert!(
                Cli::try_parse_from(["cagents", "automation", "proposals", "--limit", limit])
                    .is_err()
            );
        }
        assert!(
            Cli::try_parse_from(["cagents", "automation", "proposals", "--after", &id]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "cagents",
                "automation",
                "proposals",
                "--registry",
                &identity
            ])
            .is_err()
        );
        let operation = plan(parse(&[
            "cagents",
            "automation",
            "proposals",
            "--after",
            &id,
            "--registry",
            &identity,
            "--sequence",
            "2",
            "--limit",
            "64",
        ]))
        .unwrap();
        assert_eq!(
            operation.request,
            Request::AllPendingProposals {
                identity: Some(identity.parse().unwrap()),
                sequence: Some(2),
                after: Some(id.parse().unwrap()),
                limit: 64
            }
        );
        assert!(!operation.request.allowed_from_hosted_conversation());
    }
}
