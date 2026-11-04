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
#[serde(rename_all = "snake_case")]
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
#[serde(rename_all = "snake_case")]
pub enum ExistingPendingRequest {
    Resume {
        #[serde(with = "request_target_serde")]
        target: ExistingPendingTarget,
    },
    Cancel {
        operation_id: Uuid,
        #[serde(with = "request_target_serde")]
        target: ExistingPendingTarget,
    },
}

/// Shared metadata only; dispatch and admission belong to the owning caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPreparationRequest {
    Preparation(PreparationRequest),
    ExistingPending(ExistingPendingRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPreparationRecord {
    Preparation(Preparation),
    ExistingPending(ExistingPendingRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// The new requests need an externally tagged target for positional codecs.
/// Keep this field adapter separate from the existing stored target's format.
mod request_target_serde {
    use super::{ExistingPendingBinding, ExistingPendingTarget};
    use crate::agent::Provider;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use uuid::Uuid;

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Target {
        Observed {
            #[serde(with = "super::provider_name")]
            provider: Provider,
            key: Uuid,
            started_at: u64,
            expected_installation: Option<Uuid>,
        },
        Bound {
            binding: ExistingPendingBinding,
        },
    }

    impl From<&ExistingPendingTarget> for Target {
        fn from(target: &ExistingPendingTarget) -> Self {
            match target {
                ExistingPendingTarget::Observed {
                    provider,
                    key,
                    started_at,
                    expected_installation,
                } => Self::Observed {
                    provider: *provider,
                    key: *key,
                    started_at: *started_at,
                    expected_installation: *expected_installation,
                },
                ExistingPendingTarget::Bound { binding } => Self::Bound {
                    binding: binding.clone(),
                },
            }
        }
    }

    impl From<Target> for ExistingPendingTarget {
        fn from(target: Target) -> Self {
            match target {
                Target::Observed {
                    provider,
                    key,
                    started_at,
                    expected_installation,
                } => Self::Observed {
                    provider,
                    key,
                    started_at,
                    expected_installation,
                },
                Target::Bound { binding } => Self::Bound { binding },
            }
        }
    }

    pub fn serialize<S: Serializer>(
        target: &ExistingPendingTarget,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Target::from(target).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ExistingPendingTarget, D::Error> {
        Target::deserialize(deserializer).map(Into::into)
    }
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

    fn request_cases() -> Vec<AccountPreparationRequest> {
        let binding = binding();
        let mut requests = vec![
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
        ];
        let targets = [
            ExistingPendingTarget::Observed {
                provider: Provider::Claude,
                key: Uuid::from_u128(4),
                started_at: 1_234,
                expected_installation: None,
            },
            ExistingPendingTarget::Observed {
                provider: Provider::Claude,
                key: Uuid::from_u128(4),
                started_at: 1_234,
                expected_installation: Some(Uuid::from_u128(2)),
            },
            ExistingPendingTarget::Bound {
                binding: existing_binding(),
            },
        ];
        for (index, target) in targets.into_iter().enumerate() {
            requests.push(AccountPreparationRequest::ExistingPending(
                ExistingPendingRequest::Resume {
                    target: target.clone(),
                },
            ));
            requests.push(AccountPreparationRequest::ExistingPending(
                ExistingPendingRequest::Cancel {
                    operation_id: Uuid::from_u128(6 + index as u128),
                    target,
                },
            ));
        }
        requests
    }

    fn outcome_cases() -> Vec<AccountPreparationOutcome> {
        let records = [
            AccountPreparationRecord::Preparation(Preparation {
                binding: binding(),
                state: PreparationState::Registered,
            }),
            AccountPreparationRecord::ExistingPending(ExistingPendingRecord {
                binding: existing_binding(),
                state: ExistingPendingState::CancelledRetained,
            }),
        ];
        let mut outcomes = Vec::new();
        for record in records {
            outcomes.push(AccountPreparationOutcome::Confirmed {
                record: record.clone(),
            });
            outcomes.push(AccountPreparationOutcome::ReconcileRequired {
                message: "The directory barrier failed; reconcile the retained request".into(),
                observed: Some(record),
            });
        }
        outcomes.push(AccountPreparationOutcome::ReconcileRequired {
            message: "The storage result is unknown".into(),
            observed: None,
        });
        outcomes.push(AccountPreparationOutcome::NotAttempted {
            message: "The caller refused admission before storage".into(),
        });
        outcomes
    }

    #[test]
    fn replies_round_trip_the_complete_request_and_uncertain_observation() {
        let outcomes = outcome_cases();
        for request in request_cases() {
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
    fn requests_round_trip_through_postcard_with_every_target_form() {
        for request in request_cases() {
            let bytes = postcard::to_allocvec(&request).unwrap();
            let (decoded, rest) =
                postcard::take_from_bytes::<AccountPreparationRequest>(&bytes).unwrap();
            assert_eq!(decoded, request);
            assert!(
                rest.is_empty(),
                "the decoder must consume the whole request"
            );
        }
    }

    #[test]
    fn replies_round_trip_through_postcard_with_every_outcome() {
        let outcomes = outcome_cases();
        for request in request_cases() {
            for outcome in &outcomes {
                let reply = AccountPreparationReply {
                    request: request.clone(),
                    outcome: outcome.clone(),
                };
                let bytes = postcard::to_allocvec(&reply).unwrap();
                let (decoded, rest) =
                    postcard::take_from_bytes::<AccountPreparationReply>(&bytes).unwrap();
                assert_eq!(decoded, reply);
                assert!(rest.is_empty(), "the decoder must consume the whole reply");
            }
        }
    }

    #[test]
    fn request_target_adapter_preserves_the_existing_stored_json_shape() {
        let binding = existing_binding();
        for target in [
            ExistingPendingTarget::Observed {
                provider: binding.provider,
                key: binding.key,
                started_at: binding.started_at,
                expected_installation: Some(binding.installation_id),
            },
            ExistingPendingTarget::Bound {
                binding: binding.clone(),
            },
        ] {
            let (stored, adapted) = match &target {
                ExistingPendingTarget::Observed {
                    provider: _,
                    key,
                    started_at,
                    expected_installation,
                } => (
                    serde_json::json!({
                        "kind": "observed",
                        "provider": "claude",
                        "key": key,
                        "started_at": started_at,
                        "expected_installation": expected_installation,
                    }),
                    serde_json::json!({"observed": {
                        "provider": "claude",
                        "key": key,
                        "started_at": started_at,
                        "expected_installation": expected_installation,
                    }}),
                ),
                ExistingPendingTarget::Bound { binding } => (
                    serde_json::json!({"kind": "bound", "binding": binding}),
                    serde_json::json!({"bound": {"binding": binding}}),
                ),
            };
            assert_eq!(serde_json::to_value(&target).unwrap(), stored);
            assert_eq!(
                serde_json::from_value::<ExistingPendingTarget>(stored).unwrap(),
                target
            );
            let request =
                AccountPreparationRequest::ExistingPending(ExistingPendingRequest::Resume {
                    target,
                });
            let value = serde_json::to_value(&request).unwrap();
            assert_eq!(
                value,
                serde_json::json!({"existing_pending": {"resume": {"target": adapted}}})
            );
            assert_eq!(
                serde_json::from_value::<AccountPreparationRequest>(value).unwrap(),
                request
            );
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
        assert_eq!(
            serde_json::to_value(&record).unwrap()["cancellation_ids"],
            serde_json::json!(record.cancellation_ids)
        );
        let value = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            value["request"],
            serde_json::json!({"existing_pending": {"cancel": {
                "operation_id": operation_id,
                "target": {"bound": {"binding": record.binding}},
            }}})
        );
        assert_eq!(
            value["outcome"]["confirmed"]["record"]["existing_pending"],
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
