//! Best-effort committed proposal hint without scheduler or SQLite work under
//! HostState. This is a model watch hint, not a durable desktop-notice ACK.
use crate::host::{agent_execution::SharedRegistry, state::SharedHost};

pub(super) fn publish(host: &SharedHost, registry: &SharedRegistry) {
    let sequence = {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execution_sequence()
    };
    if let Ok(sequence) = sequence {
        // Zero is the existing internal execution publisher, never an external client.
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .announce_model_edit(
                0,
                sequence,
                &crate::workspace_model::ModelEdit::AgentsChanged,
            );
    }
}

#[cfg(test)]
mod tests {
    use crate::{agent_execution::Authority, automations::Request};

    #[test]
    fn inherited_or_invalid_session_authority_cannot_read_global_proposal_delivery() {
        let host = crate::host::service::test_host();
        for authority in [
            Authority::session(String::new()),
            Authority::session("invalid fixture capability".into()),
        ] {
            for request in [
                Request::ProposalDelivery,
                Request::ReviewProposal {
                    identity: uuid::Uuid::new_v4(),
                    id: uuid::Uuid::new_v4(),
                },
                Request::AllPendingProposals {
                    identity: None,
                    sequence: None,
                    after: None,
                    limit: 64,
                },
                Request::ResolveProposal {
                    id: uuid::Uuid::new_v4(),
                    revision: 1,
                    approved: false,
                },
            ] {
                let error =
                    super::super::service::process_under_control(&host, authority.clone(), request)
                        .unwrap_err();
                assert!(error.contains("child-execution capability"), "{error}");
                assert!(
                    host.lock().unwrap().execution_registry.is_none(),
                    "no registry or User fallback was needed"
                );
            }
        }
    }
}

#[cfg(test)]
mod watch_edge_tests {
    use crate::{
        agent_execution::{Authority, Operation, Reply, State},
        automations::{
            Definition, ProposalState, Request, Response, Schedule, SchedulePattern, SessionMode,
        },
        host::{agent_execution, delivery::DeliveryQueue},
        terminal_ipc::ServerMessage,
        workspace_model::ModelEdit,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    // The real client writer consumes this same bounded queue. Closing after
    // the synchronous operation makes a missing publisher fail without an
    // unbounded pop, rather than replacing the publisher with a test signal.
    fn completed_watch(queue: &DeliveryQueue, expected: u64) {
        queue.close_after_drain();
        let mut exact = 0;
        while let Some(outbound) = queue.pop() {
            if matches!(outbound.message, ServerMessage::WorkspaceModelEdited {
                sequence,
                edit: ModelEdit::AgentsChanged,
            } if sequence == expected)
            {
                exact += 1;
            }
        }
        assert_eq!(
            exact, 1,
            "one committed proposal edge reaches this actual watcher"
        );
    }

    #[test]
    fn real_proposal_creation_and_reviewed_decline_publish_to_every_existing_model_watcher() {
        let name = "host::automations::proposal_delivery::watch_edge_tests::real_proposal_creation_and_reviewed_decline_publish_to_every_existing_model_watcher";
        if crate::host::agent_execution::tests::isolated(name).is_some() {
            return;
        }
        let (host, request, path) = crate::host::agent_execution::tests::fixture();
        let bootstrap = {
            let mut state = host.lock().unwrap();
            let queue = state.register_client(6100);
            state.watch_model(6100).unwrap();
            queue
        };
        let accepted = agent_execution::submit(&host, &Authority::default(), request).unwrap();
        let registry = agent_execution::registry(&host).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
            if run.state.is_terminal() {
                assert_eq!(run.state, State::Succeeded);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "existing private provider fixture did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let settled_sequence = registry.lock().unwrap().execution_sequence().unwrap();
        // Observe the original actor's final announcement before subscribing
        // the proposal-only witnesses. A late actor edge must not satisfy a
        // missing proposal publisher accidentally.
        let queue = Arc::clone(&bootstrap);
        let (sent, received) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            while let Some(outbound) = queue.pop() {
                if matches!(outbound.message, ServerMessage::WorkspaceModelEdited {
                    sequence,
                    edit: ModelEdit::AgentsChanged,
                } if sequence == settled_sequence)
                {
                    let _ = sent.send(());
                    return;
                }
            }
        });
        let announced = received.recv_timeout(Duration::from_secs(5));
        bootstrap.abort("private bootstrap watch retired");
        reader.join().unwrap();
        assert!(
            announced.is_ok(),
            "the existing actor's final watch edge must settle first"
        );
        host.lock().unwrap().forget_client_delivery(6100);

        let authority = {
            let registry = registry.lock().unwrap();
            let session = registry.agent(accepted.agent.id).unwrap().session.unwrap();
            registry.retained_agent_authority(session.id()).unwrap()
        };
        let definition = Definition {
            name: "A request visible in every window".into(),
            prompt: "Private source consent; never a new provider turn".into(),
            agent: accepted.agent.spec.clone(),
            schedule: Schedule {
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                timezone: "UTC".into(),
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        };
        let root = path.parent().unwrap();
        let prompts = std::fs::read(root.join("prompts")).unwrap();
        let spawns = std::fs::read(root.join("spawns")).unwrap();
        let watches = [6101, 6102].map(|id| {
            let mut state = host.lock().unwrap();
            let queue = state.register_client(id);
            state.watch_model(id).unwrap();
            queue
        });
        let Response::Proposal(proposal) = super::super::service::process_under_control(
            &host,
            authority.clone(),
            Request::Propose {
                request: uuid::Uuid::new_v4(),
                definition,
            },
        )
        .unwrap() else {
            panic!("actual hosted proposal receipt");
        };
        assert_eq!(proposal.state, ProposalState::Pending);
        let created_sequence = registry.lock().unwrap().execution_sequence().unwrap();
        assert!(created_sequence > settled_sequence);
        for queue in &watches {
            completed_watch(queue, created_sequence);
        }
        let delivery = registry
            .lock()
            .unwrap()
            .automation_proposal_delivery()
            .unwrap();
        assert_eq!(delivery.pending_automation_proposal_count, 1);
        assert_eq!(delivery.pending_automation_proposals[0].id, proposal.id);
        for id in [6101, 6102] {
            host.lock().unwrap().forget_client_delivery(id);
        }
        let watches = [6103, 6104].map(|id| {
            let mut state = host.lock().unwrap();
            let queue = state.register_client(id);
            state.watch_model(id).unwrap();
            queue
        });
        let Response::ProposalReview(resolved) = super::super::service::process_under_control(
            &host,
            Authority::default(),
            Request::ResolveReviewedProposal {
                identity: delivery.identity,
                proposal: proposal.clone(),
                approved: false,
            },
        )
        .unwrap() else {
            panic!("actual identity-scoped immutable decision receipt");
        };
        assert_eq!(resolved.identity, delivery.identity);
        assert_eq!(resolved.proposal.id, proposal.id);
        assert_eq!(resolved.proposal.state, ProposalState::Declined);
        let resolved_sequence = registry.lock().unwrap().execution_sequence().unwrap();
        assert!(resolved_sequence > created_sequence);
        for queue in &watches {
            completed_watch(queue, resolved_sequence);
        }
        let current = registry
            .lock()
            .unwrap()
            .automation_proposal_delivery()
            .unwrap();
        assert_eq!(current.pending_automation_proposal_count, 0);
        assert!(current.pending_automation_proposals.is_empty());
        assert_eq!(
            current.automation_proposal_sequence,
            delivery.automation_proposal_sequence + 1
        );
        let Response::Proposal(receipt) = super::super::service::process_under_control(
            &host,
            authority,
            Request::Proposal { id: proposal.id },
        )
        .unwrap() else {
            panic!("the original hosted waiter can read its winning receipt");
        };
        assert_eq!(receipt, resolved.proposal);
        assert_eq!(std::fs::read(root.join("prompts")).unwrap(), prompts);
        assert_eq!(std::fs::read(root.join("spawns")).unwrap(), spawns);
        assert!(matches!(
            agent_execution::process(
                &host,
                Authority::default(),
                Operation::Close(accepted.agent.id)
            )
            .unwrap(),
            Reply::Closed(_)
        ));
    }
}
