//! Shared metadata for durable, retained account setup. No credential or path authority.
use crate::agent::Provider;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparationPayload {
    #[serde(with = "provider_name")]
    pub provider: Provider,
    pub copy_codex_config: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparationBinding {
    pub request_id: Uuid,
    pub installation_id: Uuid,
    pub key: Uuid,
    pub payload: PreparationPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationState {
    Planned,
    /// Persisted before entering mkdir. A crash or partial seed can never
    /// turn this into permission to create or initialize another home.
    Uncertain,
    Created,
    CancelledRetained,
    /// Historic registration of this preparation. It does not establish a
    /// current account row, an available home, or the provider's current login.
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preparation {
    pub binding: PreparationBinding,
    pub state: PreparationState,
}

/// A persisted observation of an older pending home. This never grants path
/// or deletion authority and is distinct from a newly planned preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExistingPendingTarget {
    Observed {
        #[serde(with = "provider_name")]
        provider: Provider,
        key: Uuid,
        started_at: u64,
        expected_installation: Option<Uuid>,
    },
    Bound {
        binding: ExistingPendingBinding,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingPendingBinding {
    pub installation_id: Uuid,
    #[serde(with = "provider_name")]
    pub provider: Provider,
    pub key: Uuid,
    pub started_at: u64,
    pub generation_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistingPendingState {
    Pending,
    CancelledRetained,
    /// Historic registration, not proof of a current account row or login.
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingPending {
    pub binding: ExistingPendingBinding,
    pub state: ExistingPendingState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) cancellation_ids: Vec<Uuid>,
}

/// Public metadata for an existing pending sign-in. Cancellation replay history
/// remains in the owning registry and is not part of a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingPendingRecord {
    pub binding: ExistingPendingBinding,
    pub state: ExistingPendingState,
}

impl From<&ExistingPending> for ExistingPendingRecord {
    fn from(record: &ExistingPending) -> Self {
        Self {
            binding: record.binding.clone(),
            state: record.state,
        }
    }
}

/// A request's complete retained identity and options. These identifiers do not
/// confer authority over a home, provider process, or account registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreparationRequest {
    Plan {
        request_id: Uuid,
        payload: PreparationPayload,
    },
    Create {
        binding: PreparationBinding,
    },
    CancelRetained {
        binding: PreparationBinding,
    },
    Reconcile {
        binding: PreparationBinding,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExistingPendingRequest {
    Resume {
        target: ExistingPendingTarget,
    },
    Cancel {
        operation_id: Uuid,
        target: ExistingPendingTarget,
    },
}

/// Shared metadata only; dispatch and admission belong to the owning caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum AccountPreparationRequest {
    Preparation(PreparationRequest),
    ExistingPending(ExistingPendingRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum AccountPreparationRecord {
    Preparation(Preparation),
    ExistingPending(ExistingPendingRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountPreparationOutcome {
    /// The owning Store completed its durability barrier for this metadata.
    /// This does not confirm home creation, login, or current registration.
    Confirmed { record: AccountPreparationRecord },
    /// Effects may have been attempted or remain unknown. A visible observation
    /// does not establish that the owning Store completed its durability barrier.
    ReconcileRequired {
        message: String,
        observed: Option<AccountPreparationRecord>,
    },
    /// Admission was refused before entering storage. This must not describe an
    /// arbitrary storage error or imply that attempted effects were rolled back.
    NotAttempted { message: String },
}

/// Echoes the entire submitted request, including cancellation operation IDs and
/// exact target options, so a delayed reply cannot match a different attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPreparationReply {
    pub request: AccountPreparationRequest,
    pub outcome: AccountPreparationOutcome,
}

mod provider_name {
    use crate::agent::Provider;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(provider: &Provider, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(provider.executable())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Provider, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "claude" => Ok(Provider::Claude),
            "codex" => Ok(Provider::Codex),
            _ => Err(D::Error::custom("Unsupported account provider")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> PreparationBinding {
        PreparationBinding {
            request_id: Uuid::from_u128(1),
            installation_id: Uuid::from_u128(2),
            key: Uuid::from_u128(3),
            payload: PreparationPayload {
                provider: Provider::Codex,
                copy_codex_config: true,
            },
        }
    }

    fn existing_binding() -> ExistingPendingBinding {
        ExistingPendingBinding {
            installation_id: Uuid::from_u128(2),
            provider: Provider::Claude,
            key: Uuid::from_u128(4),
            started_at: 1_234,
            generation_id: Uuid::from_u128(5),
        }
    }

    #[test]
    fn replies_round_trip_the_complete_request_and_uncertain_observation() {
        let binding = binding();
        let observed = ExistingPendingTarget::Observed {
            provider: Provider::Claude,
            key: Uuid::from_u128(4),
            started_at: 1_234,
            expected_installation: Some(Uuid::from_u128(2)),
        };
        let bound = ExistingPendingTarget::Bound {
            binding: existing_binding(),
        };
        let requests = [
            AccountPreparationRequest::Preparation(PreparationRequest::Plan {
                request_id: binding.request_id,
                payload: binding.payload.clone(),
            }),
            AccountPreparationRequest::Preparation(PreparationRequest::Create {
                binding: binding.clone(),
            }),
            AccountPreparationRequest::Preparation(PreparationRequest::CancelRetained {
                binding: binding.clone(),
            }),
            AccountPreparationRequest::Preparation(PreparationRequest::Reconcile {
                binding: binding.clone(),
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
                target: observed.clone(),
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
                target: bound.clone(),
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                operation_id: Uuid::from_u128(6),
                target: observed,
            }),
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                operation_id: Uuid::from_u128(7),
                target: bound,
            }),
        ];
        let record = AccountPreparationRecord::Preparation(Preparation {
            binding,
            state: PreparationState::Registered,
        });
        let outcomes = [
            AccountPreparationOutcome::Confirmed {
                record: record.clone(),
            },
            AccountPreparationOutcome::ReconcileRequired {
                message: "The directory barrier failed; reconcile the retained request".into(),
                observed: Some(record),
            },
            AccountPreparationOutcome::ReconcileRequired {
                message: "The storage result is unknown".into(),
                observed: None,
            },
            AccountPreparationOutcome::NotAttempted {
                message: "The caller refused admission before storage".into(),
            },
        ];
        for request in requests {
            for outcome in &outcomes {
                let reply = AccountPreparationReply {
                    request: request.clone(),
                    outcome: outcome.clone(),
                };
                let bytes = serde_json::to_vec(&reply).unwrap();
                assert_eq!(
                    serde_json::from_slice::<AccountPreparationReply>(&bytes).unwrap(),
                    reply
                );
            }
        }
    }

    #[test]
    fn cancellation_reply_echoes_its_operation_without_exposing_internal_history() {
        let record = ExistingPending {
            binding: existing_binding(),
            state: ExistingPendingState::CancelledRetained,
            cancellation_ids: vec![Uuid::from_u128(8), Uuid::from_u128(9)],
        };
        let operation_id = Uuid::from_u128(10);
        let reply = AccountPreparationReply {
            request: AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                operation_id,
                target: ExistingPendingTarget::Bound {
                    binding: record.binding.clone(),
                },
            }),
            outcome: AccountPreparationOutcome::Confirmed {
                record: AccountPreparationRecord::ExistingPending((&record).into()),
            },
        };
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            value["request"]["request"]["operation_id"],
            operation_id.to_string()
        );
        assert_eq!(
            value["outcome"]["record"]["record"],
            serde_json::json!({
                "binding": {
                    "installation_id": record.binding.installation_id,
                    "provider": "claude",
                    "key": record.binding.key,
                    "started_at": record.binding.started_at,
                    "generation_id": record.binding.generation_id,
                },
                "state": "cancelled_retained",
            })
        );
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(!serialized.contains("cancellation_ids"));
        for internal_id in &record.cancellation_ids {
            assert!(!serialized.contains(&internal_id.to_string()));
        }
        assert_eq!(
            serde_json::from_value::<AccountPreparationReply>(value).unwrap(),
            reply
        );
        let mut different_attempt = reply.clone();
        different_attempt.request =
            AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Cancel {
                operation_id: Uuid::from_u128(11),
                target: ExistingPendingTarget::Bound {
                    binding: record.binding,
                },
            });
        assert_ne!(reply, different_attempt);
    }
}
