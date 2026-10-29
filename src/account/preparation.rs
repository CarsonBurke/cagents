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
