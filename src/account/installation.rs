//! Explicit identity-only initialization on an already selected connection.
//! The request UUID is correlation, never a durable request-to-mint receipt.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationRequest {
    pub request_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InitializationRefusal {
    InvalidRequestId,
    StoreUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InitializationOutcome {
    /// The current registry contains this identity after its namespace barrier.
    /// This does not claim that this request created it.
    Confirmed { installation_id: Uuid },
    /// Positive proof that the initializer writer was never entered.
    NotAttempted { reason: InitializationRefusal },
    /// Storage was entered; neither rollback nor durable success is established.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationReply {
    pub request_id: Uuid,
    pub outcome: InitializationOutcome,
}

impl InitializationReply {
    /// Callers also require the original wire proof/version/correlation and
    /// dedicated reply kind. A mismatch never settles the request owner.
    pub fn matches(&self, request: InitializationRequest) -> bool {
        self.request_id == request.request_id
            && !matches!(self.outcome, InitializationOutcome::Confirmed { installation_id } if request.request_id.is_nil() || installation_id.is_nil())
    }
}

/// Store selection is lazy so a nil request cannot even prepare a namespace.
/// Outer admission/transport errors are not typed pre-writer refusal proof.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn initialize(
    request: InitializationRequest,
    select: impl FnOnce() -> Option<super::registry::Store>,
) -> InitializationReply {
    let outcome = if request.request_id.is_nil() {
        InitializationOutcome::NotAttempted {
            reason: InitializationRefusal::InvalidRequestId,
        }
    } else if let Some(store) = select() {
        match store.try_initialize_installation_id() {
            Ok(installation_id) => InitializationOutcome::Confirmed { installation_id },
            // Even lock/validation failures may follow namespace preparation.
            // Never expose raw filesystem paths or TOML contents on the wire.
            Err(_) => InitializationOutcome::Unknown,
        }
    } else {
        InitializationOutcome::NotAttempted {
            reason: InitializationRefusal::StoreUnavailable,
        }
    };
    InitializationReply {
        request_id: request.request_id,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_reply_requires_the_complete_uuid_and_non_nil_confirmation() {
        let request = InitializationRequest {
            request_id: Uuid::from_u128(17),
        };
        for outcome in [
            InitializationOutcome::Unknown,
            InitializationOutcome::NotAttempted {
                reason: InitializationRefusal::StoreUnavailable,
            },
            InitializationOutcome::Confirmed {
                installation_id: Uuid::from_u128(18),
            },
        ] {
            let reply = InitializationReply {
                request_id: request.request_id,
                outcome,
            };
            assert!(reply.matches(request));
            let bytes = postcard::to_allocvec(&reply).unwrap();
            assert_eq!(
                postcard::from_bytes::<InitializationReply>(&bytes).unwrap(),
                reply
            );
            assert!(
                !InitializationReply {
                    request_id: Uuid::from_u128(19),
                    ..reply
                }
                .matches(request)
            );
        }
        assert!(
            !InitializationReply {
                request_id: request.request_id,
                outcome: InitializationOutcome::Confirmed {
                    installation_id: Uuid::nil()
                }
            }
            .matches(request)
        );
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn nil_initialization_never_selects_storage_and_storage_errors_are_unknown() {
        let reply = initialize(
            InitializationRequest {
                request_id: Uuid::nil(),
            },
            || panic!("nil request selected storage"),
        );
        assert_eq!(
            reply.outcome,
            InitializationOutcome::NotAttempted {
                reason: InitializationRefusal::InvalidRequestId
            }
        );
        let request = InitializationRequest {
            request_id: Uuid::from_u128(20),
        };
        assert_eq!(
            initialize(request, || None).outcome,
            InitializationOutcome::NotAttempted {
                reason: InitializationRefusal::StoreUnavailable
            }
        );
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"private sentinel").unwrap();
        let reply = initialize(request, || Some(super::super::registry::Store::at(file)));
        assert_eq!(reply.outcome, InitializationOutcome::Unknown);
        assert!(!serde_json::to_string(&reply).unwrap().contains("sentinel"));
    }
}
