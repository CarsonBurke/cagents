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
