//! The channel-work wire surface carries intent and captured placement, never
//! an actor identity or a checkout-derived ownership assertion.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

use crate::{
    channel_work::{
        CheckoutContext, Home, HomeLifecycle, Mutation, Page, Receipt, Reply, Snapshot, Target,
        TargetSnapshot,
    },
    model::{CardId, ChannelId, RunId, TerminalId},
};

/// Public comparison values returned by the daemon. These are not authority:
/// the authenticated source and canonical pane are rechecked at commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    pub session: TerminalId,
    pub card: CardId,
    /// Durable causal revision of this exact pane's placement and source.
    /// Presentation and unrelated pane edits cannot change this epoch.
    pub placement_epoch: u64,
    pub run: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub home: Home,
    pub title: String,
    pub caller: Option<Caller>,
}

/// A daemon-issued, short-lived selection. It grants no filesystem execution
/// authority and can only be used by its original authenticated caller/home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSelection {
    pub selection: Uuid,
    pub home: Home,
    pub context: CheckoutContext,
    /// Exact target CAS read after the daemon selects the Local identity.
    pub expected_revision: u64,
}

impl LocalSelection {
    pub fn validate(&self) -> Result<(), crate::channel_work::Error> {
        self.home.validate()?;
        self.context.validate()?;
        if self.selection.is_nil()
            || self.context.registry != self.home.registry
            || self.context.generation == 0
        {
            return Err(crate::channel_work::Error::InvalidMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// A session defaults to its actual canonical channel. User callers must
    /// choose a channel explicitly; cwd and the active window are not owners.
    Context {
        channel: Option<ChannelId>,
    },
    Read {
        home: Home,
        page: Page,
    },
    Mutate {
        mutation: Mutation,
        caller: Option<Caller>,
    },
    Receipt {
        home: Home,
        operation: Uuid,
    },
    /// Refresh only already tracked PRs. Forge facts never create claims.
    Refresh {
        home: Home,
    },
    ReadTarget {
        home: Home,
        target: Target,
    },
    /// Reconcile an original durable home without loading any work projection.
    ReadHome {
        home: Home,
    },
    PrepareLocal {
        home: Home,
        directory: PathBuf,
        caller: Option<Caller>,
    },
    /// Claim the selected Local identity. Exact committed replay precedes
    /// transient ticket lookup; fresh writes always need a valid selection.
    ApplySelection {
        selection: Uuid,
        mutation: Mutation,
        caller: Option<Caller>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// Validation or a rolled-back transaction: this attempt made no change.
    Refused(String),
    /// The original operation may have committed. Reconcile its UUID; a
    /// missing receipt does not authorize a replacement operation.
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Context(Context),
    Snapshot(Snapshot),
    Applied(Reply),
    Receipt(Option<Receipt>),
    Failed(Failure),
    Target(TargetSnapshot),
    Home {
        home: Home,
        lifecycle: HomeLifecycle,
        revision: u64,
    },
    Selected(LocalSelection),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_execution::Authority,
        channel_work::{Change, Role},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse,
        },
    };

    fn selected() -> LocalSelection {
        LocalSelection {
            selection: Uuid::from_u128(1),
            home: Home {
                registry: Uuid::from_u128(2),
                channel: ChannelId::from_uuid(Uuid::from_u128(3)),
                creation_ordinal: 4,
            },
            context: CheckoutContext {
                id: Uuid::from_u128(5),
                registry: Uuid::from_u128(2),
                repository: PathBuf::from("/project"),
                path: PathBuf::from("/project/worktree"),
                branch: Some("feature".into()),
                head: None,
                generation: 6,
                dirty: None,
            },
            expected_revision: 7,
        }
    }

    #[test]
    fn local_selection_requests_and_response_roundtrip_in_actual_wire_envelopes() {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("local-codec-fixture"),
            boot_id: BootId::new("local-codec-fixture"),
            host_incarnation: HostIncarnation::new("local-codec-fixture"),
        };
        assert!(proof.is_complete());
        for dirty in [None, Some(false), Some(true)] {
            let mut selected = selected();
            selected.context.dirty = dirty;
            selected.validate().unwrap();
            let requests = [
                Request::PrepareLocal {
                    home: selected.home,
                    directory: selected.context.path.clone(),
                    caller: None,
                },
                Request::ApplySelection {
                    selection: selected.selection,
                    mutation: Mutation {
                        home: selected.home,
                        operation: Uuid::from_u128(8),
                        expected_revision: selected.expected_revision,
                        change: Change::Claim {
                            target: Target::Local(selected.context.id),
                            role: Role::Contribute,
                        },
                    },
                    caller: None,
                },
            ];
            for request in requests {
                let envelope = ClientEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: 9,
                    proof: Some(proof.clone()),
                    request: ClientRequest::ChannelWork {
                        authority: Authority::default(),
                        request,
                    },
                };
                assert_eq!(
                    postcard::from_bytes::<ClientEnvelope>(
                        &postcard::to_stdvec(&envelope).unwrap()
                    )
                    .unwrap(),
                    envelope,
                );
            }
            let envelope = ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(9),
                proof: proof.clone(),
                message: ServerMessage::Response {
                    response: ServerResponse::ChannelWork(Response::Selected(selected)),
                },
            };
            assert_eq!(
                postcard::from_bytes::<ServerEnvelope>(&postcard::to_stdvec(&envelope).unwrap())
                    .unwrap(),
                envelope,
            );
        }
    }

    #[test]
    fn local_selection_rejects_unissued_or_foreign_metadata_and_reads_legacy_dirty_bool() {
        let good = selected();
        good.validate().unwrap();
        let mut json = serde_json::to_value(&good).unwrap();
        json["context"]["dirty"] = serde_json::json!(true);
        let decoded: LocalSelection = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.context.dirty, Some(true));
        decoded.validate().unwrap();

        let mut bad = good.clone();
        bad.selection = Uuid::nil();
        assert!(bad.validate().is_err());
        let mut bad = good.clone();
        bad.context.registry = Uuid::from_u128(10);
        assert!(bad.validate().is_err());
        let mut bad = good;
        bad.context.generation = 0;
        assert!(bad.validate().is_err());
    }
}
