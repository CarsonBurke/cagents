//! Bounded user-only discovery of immutable requests; no execution authority.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ProposalSummary;

pub const PROPOSAL_PAGE_SIZE: usize = 64;

/// One proposal-only SQLite snapshot, independent of the workspace snapshot.
/// The positive clock is a delivery capability; zero must not disable legacy
/// polling. This is all pending requests, not an unread-notification count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalDelivery {
    pub identity: Uuid,
    pub automation_proposal_sequence: u64,
    pub pending_automation_proposal_count: u64,
    /// First ID-ordered page only. Fetch an exact Proposal to review consent.
    pub pending_automation_proposals: Vec<ProposalSummary>,
}

/// A page is bound to one registry/clock. It is never an unstamped Vec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalPage {
    pub identity: Uuid,
    pub automation_proposal_sequence: u64,
    pub after: Option<Uuid>,
    pub proposals: Vec<ProposalSummary>,
}

/// The full immutable consent read from an explicitly expected registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalReview {
    pub identity: Uuid,
    pub proposal: Box<super::Proposal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::{Request, Response};

    #[test]
    fn delivery_and_global_pages_never_grant_hosted_management_authority() {
        for request in [
            Request::ProposalDelivery,
            Request::ReviewProposal {
                identity: Uuid::new_v4(),
                id: Uuid::new_v4(),
            },
            Request::AllPendingProposals {
                identity: None,
                sequence: None,
                after: None,
                limit: 64,
            },
            Request::ResolveProposal {
                id: Uuid::new_v4(),
                revision: 1,
                approved: true,
            },
            Request::ResolveProposal {
                id: Uuid::new_v4(),
                revision: 1,
                approved: false,
            },
        ] {
            assert!(!request.allowed_from_hosted_conversation());
        }
        assert!(Request::Proposal { id: Uuid::new_v4() }.allowed_from_hosted_conversation());
        let response = Response::ProposalDelivery(ProposalDelivery {
            identity: Uuid::new_v4(),
            automation_proposal_sequence: 1,
            pending_automation_proposal_count: 0,
            pending_automation_proposals: Vec::new(),
        });
        assert_eq!(
            postcard::from_bytes::<Response>(&postcard::to_allocvec(&response).unwrap()).unwrap(),
            response
        );
        assert_eq!(
            serde_json::from_value::<Response>(serde_json::to_value(&response).unwrap()).unwrap(),
            response
        );
    }
}
