//! Explicit user commands; inherited authority is never replaced or cleared.
use super::*;
use crate::automations::ProposalState;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List pending metadata; continue with the returned registry and clock.
    Proposals {
        #[arg(long, requires_all = ["registry", "sequence"])]
        #[arg(help = "Continue after this proposal, as returned by the previous page.")]
        after: Option<Uuid>,
        #[arg(long, requires = "sequence")]
        #[arg(help = "Registry identity returned by the previous page.")]
        registry: Option<Uuid>,
        #[arg(long, requires = "registry", value_parser = clap::value_parser!(u64).range(1..))]
        #[arg(help = "Delivery clock returned by the previous page.")]
        sequence: Option<u64>,
        #[arg(long, default_value_t = 50, value_parser = page_limit)]
        #[arg(help = "Requests per page (1 through 64).")]
        limit: usize,
    },
    /// Read full immutable consent from the exact registry shown by proposals.
    ReviewProposal {
        #[arg(help = "The stable resource ID.")]
        id: Uuid,
        #[arg(long)]
        #[arg(help = "Registry identity shown by automation proposals.")]
        registry: Uuid,
    },
    /// Approve the exact reviewed proposal (user only).
    Approve {
        #[arg(help = "The stable resource ID.")]
        id: Uuid,
        #[arg(long)]
        #[arg(help = "Registry identity shown by automation proposals.")]
        registry: Uuid,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        #[arg(help = "Revision shown by automation review-proposal.")]
        revision: u64,
        #[arg(long)]
        #[arg(help = "Confirm approving and enabling this exact request.")]
        confirm: bool,
    },
    /// Explicitly decline the exact reviewed revision (user only).
    Decline {
        #[arg(help = "The stable resource ID.")]
        id: Uuid,
        #[arg(long)]
        #[arg(help = "Registry identity shown by automation proposals.")]
        registry: Uuid,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        #[arg(help = "Revision shown by automation review-proposal.")]
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
    let Some((_, _, _, approved)) = decision else {
        return super::response_output(name, reply, None);
    };
    decision_output(name, approved, reply)
}

/// An already-resolved request answers with its terminal state. Only the same
/// decision is a success: the other was not carried out.
fn decision_output(
    name: &'static str,
    approved: bool,
    reply: Response,
) -> Result<CommandOutput, AppError> {
    let Response::ProposalReview(review) = &reply else {
        return Err(AppError::new(
            "automation_proposal",
            "The daemon did not return the exact proposal decision",
        ));
    };
    let human = super::proposal_human(&review.proposal);
    match (&review.proposal.state, approved) {
        (ProposalState::Pending, _) => {
            return Err(AppError::new(
                "automation_proposal",
                "The daemon did not resolve this request; review it again",
            ));
        }
        (ProposalState::Declined, true) => {
            return Err(AppError::new(
                "automation_proposal_declined",
                "This request was already declined, so it was not approved and no schedule was created",
            ));
        }
        (ProposalState::Approved { automation, .. }, false) => {
            return Err(AppError::new(
                "automation_proposal_approved",
                format!(
                    "This request was already approved as automation {automation}, so it was not declined"
                ),
            ));
        }
        _ => {}
    }
    let mut output = super::response_output(name, reply, None)?;
    output.human = human;
    Ok(output)
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
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Surface, Target},
        automations::{
            Definition, DirectoryIdentity, ProposalPage, ProposalReview, Schedule, SchedulePattern,
            SessionMode,
        },
    };

    fn proposal(state: ProposalState) -> crate::automations::Proposal {
        let definition = Definition {
            name: "Weekday recap".into(),
            prompt: "Review only the approved project".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: None,
                target: Target {
                    repository: "/fixture".into(),
                    directory: "/fixture".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                reasoning_effort: None,
                permission_mode: None,
            },
            schedule: Schedule {
                timezone: "UTC".into(),
                pattern: SchedulePattern::Weekdays {
                    hour: 9,
                    minute: 30,
                },
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        };
        crate::automations::Proposal {
            id: Uuid::from_u128(7),
            revision: 3,
            owner: Origin::User,
            requested: definition.clone(),
            definition,
            target_identity: DirectoryIdentity {
                device: 1,
                inode: 2,
            },
            channel_title: None,
            created_ms: 1,
            state,
        }
    }

    fn review(state: ProposalState) -> Response {
        Response::ProposalReview(ProposalReview {
            identity: Uuid::from_u128(1),
            proposal: Box::new(proposal(state)),
        })
    }

    #[test]
    fn listing_and_decisions_keep_their_json_shapes() {
        let page = Response::ProposalPage(ProposalPage {
            identity: Uuid::from_u128(1),
            automation_proposal_sequence: 9,
            after: None,
            proposals: vec![(&proposal(ProposalState::Pending)).into()],
        });
        let listed = super::super::response_output("automation.proposals", page, None).unwrap();
        let page = &listed.data["proposal_page"];
        assert_eq!(page["automation_proposal_sequence"], 9);
        assert_eq!(page["proposals"][0]["name"], "Weekday recap");
        assert_eq!(page["proposals"][0]["revision"], 3);

        let approved = ProposalState::Approved {
            automation: Uuid::from_u128(8),
            execution_revision: 1,
        };
        for (name, is_approval, state) in [
            ("automation.approve", true, approved),
            ("automation.decline", false, ProposalState::Declined),
        ] {
            let output = decision_output(name, is_approval, review(state)).unwrap();
            assert_eq!(output.command, name);
            assert_eq!(output.data["registry"], Uuid::from_u128(1).to_string());
            assert_eq!(output.data["proposal"]["revision"], 3);
            assert!(!output.human.contains('{'), "{}", output.human);
        }
    }

    #[test]
    fn a_decision_that_lost_to_the_other_one_is_a_failure_in_plain_terms() {
        let approved = ProposalState::Approved {
            automation: Uuid::from_u128(8),
            execution_revision: 1,
        };
        for (is_approval, state, code, says) in [
            (
                true,
                ProposalState::Declined,
                "automation_proposal_declined",
                "already declined",
            ),
            (
                false,
                approved,
                "automation_proposal_approved",
                "already approved",
            ),
        ] {
            let error = decision_output("automation.decide", is_approval, review(state))
                .err()
                .expect("the other decision was not carried out");
            assert_eq!(error.kind, code);
            assert!(error.message.contains(says), "{}", error.message);
        }
        assert!(
            decision_output("automation.decide", true, review(ProposalState::Pending)).is_err(),
            "a pending reply is not a decision"
        );
    }

    #[test]
    fn an_inherited_session_capability_is_refused_before_any_daemon_is_contacted() {
        for command in [
            Command::Proposals {
                after: None,
                registry: None,
                sequence: None,
                limit: 50,
            },
            Command::ReviewProposal {
                id: Uuid::from_u128(7),
                registry: Uuid::from_u128(1),
            },
            Command::Approve {
                id: Uuid::from_u128(7),
                registry: Uuid::from_u128(1),
                revision: 3,
                confirm: true,
            },
            Command::Decline {
                id: Uuid::from_u128(7),
                registry: Uuid::from_u128(1),
                revision: 3,
            },
        ] {
            let error = execute(
                command,
                crate::agent_execution::Authority::session("unit-only-scope".into()),
            )
            .err()
            .expect("a session capability cannot decide global requests");
            assert_eq!(error.kind, "automation_authority");
        }
    }

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
