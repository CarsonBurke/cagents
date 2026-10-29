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
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingPending {
    pub binding: ExistingPendingBinding,
    pub state: ExistingPendingState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) cancellation_ids: Vec<Uuid>,
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
