//! Durable obligation only. This contains no root descriptor, factory, or
//! permission to start; missing original host authority must always refuse.
use super::{CardId, OperationId, TerminalId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CreatedStartKind { Terminal, NativeChat }

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatedStartRequirement {
    pub card: CardId,
    pub session: TerminalId,
    pub operation: OperationId,
    pub generation: u64,
    pub attempt: uuid::Uuid,
    pub kind: CreatedStartKind,
}
impl CreatedStartRequirement {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.card.as_uuid().is_nil() || self.session.as_uuid().is_nil()
            || self.operation.as_uuid().is_nil() || self.attempt.is_nil()
            || self.generation > i64::MAX as u64 {
            return Err("Created start requirement has invalid identity or generation");
        }
        Ok(())
    }
}
