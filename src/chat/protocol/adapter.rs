//! Provider steps and the in-process decoder contract, independent of transports.
use crate::chat::{Command, Event};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct Step {
    /// The decoder has not qualified this frame as the current turn. Its
    /// transcript and exact managed execution evidence remain valid, but it
    /// cannot replace the current conversation's notification context/error.
    /// This transport-local flag never enters the shared wire codec.
    pub suppress_current_notification: bool,
    /// Bounded pre-acknowledgement context selected by the exact accepted
    /// provider turn. Restores only notification metadata, never transcript
    /// deltas that would otherwise be appended a second time.
    pub notification_preview: Option<crate::chat::notification::Preview>,
    /// Written in order before the next incoming frame is processed.
    pub writes: Vec<Value>,
    pub events: Vec<Event>,
}
/// Account identity verified during initialize on the actual server connection.
/// A holder can retain it when the account directory is subsequently removed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerIdentity {
    pub user_agent: String,
    pub canonical_home: PathBuf,
    pub reported_home: String,
}
/// What became of a stop an adapter held, unsent, until the provider named
/// the turn it was for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeldStop {
    /// No stop is held: none was asked for, or it went out at once.
    #[default]
    None,
    /// Held until the provider names the turn.
    Waiting,
    /// Sent once the provider named the turn.
    Sent,
    /// Never sent: the provider refused the turn or had already finished it.
    Dropped,
}

/// An adapter is confined to one daemon worker. No UI state, subprocesses,
/// unbounded transcript storage, credentials, or global mutable state live here.
pub trait Adapter: Send {
    /// Private launch selection, distinct from a provider-confirmed session.
    /// Only a provider with a retained bootstrap contract supplies this value.
    #[cfg(feature = "host")]
    fn prepared_launch_identity(&self) -> Option<super::claude::PreparedIdentity> {
        None
    }
    fn initialize(&mut self) -> Result<Step, String>;
    /// Preserve resource failures during history preparation without treating
    /// malformed initialization data as an invitation to repeat it.
    #[cfg(feature = "host")]
    fn initialize_for_start(&mut self) -> Result<Step, super::SetupError> {
        self.initialize().map_err(super::SetupError::Invalid)
    }
    /// Whether the initial handshake still needs its matching provider reply.
    /// Advisory frames do not complete it; later readiness changes are separate.
    fn initialization_pending(&self) -> bool {
        false
    }
    /// What became of a stop this adapter held until the provider named
    /// the turn it is for.
    fn held_stop(&self) -> HeldStop {
        HeldStop::None
    }
    /// Whether a settled turn's outcome still waits on a provider reply, such
    /// as the acknowledgement of an interrupt sent before its result arrived.
    /// Closing keeps reading the provider until that reply decides it.
    fn outcome_pending(&self) -> bool {
        false
    }
    fn command(&mut self, command: Command) -> Result<Step, String>;
    /// Whether this exact pending answer refuses work. Providers with opaque
    /// action IDs must inspect their retained decision, never its UI label.
    fn answer_declines(&self, _request_id: &str, action: &str) -> bool {
        crate::chat::declines(action)
    }
    fn command_for_run(
        &mut self,
        _run: crate::model::RunId,
        _command: Command,
    ) -> Result<Step, String> {
        Err("This provider transport does not support correlated agent runs".into())
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String>;
    /// Identity reported by the initialized server on this exact connection.
    /// Only a holder with a verified provider contract can use it for durable
    /// ownership; an executable's separate `--version` output is insufficient.
    fn server_identity(&self) -> Option<&ServerIdentity> {
        None
    }
}
