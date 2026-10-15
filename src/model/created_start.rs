//! Durable obligation only. This contains no root descriptor, factory, or
//! permission to start; missing original host authority must always refuse.
use super::{CardId, OperationId, TerminalId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DirectoryStartKind {
    Terminal,
    NativeChat,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryStartRequirement {
    pub card: CardId,
    pub session: TerminalId,
    /// The actual intent operation: settled creation or a host-verified fallback
    /// transition. This field itself certifies neither origin.
    pub operation: OperationId,
    pub generation: u64,
    pub attempt: uuid::Uuid,
    pub kind: DirectoryStartKind,
}
impl DirectoryStartRequirement {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.card.as_uuid().is_nil()
            || self.session.as_uuid().is_nil()
            || self.operation.as_uuid().is_nil()
            || self.attempt.is_nil()
            || self.generation > i64::MAX as u64
        {
            return Err("Directory start requirement has invalid identity or generation");
        }
        Ok(())
    }
}

// Compatibility names for the first source-only creation consumer packet.
// The serialized six-field obligation is unchanged; kind describes target,
// while actual creation/fallback provenance stays in host-private authority.
pub type CreatedStartRequirement = DirectoryStartRequirement;
pub type CreatedStartKind = DirectoryStartKind;
