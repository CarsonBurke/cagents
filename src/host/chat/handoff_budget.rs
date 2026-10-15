//! Ensure an issued full receipt always fits its smaller client ACK lane.
use super::*;
use crate::terminal_ipc::{ChatSourceBinding, ClientEnvelope, FrameCodec, TerminalHandoffReceipt, MAX_CLIENT_FRAME_BYTES};

// Full receipt repeats its original/canonical sources and, for a paired view,
// embeds one additional terminal recipe in the post-source. Bound each input
// component before effects, leaving over a MiB for the generated descriptor's
// fixed scalars, <=16 KiB recovery hint and filesystem-bounded paths. Fresh
// spawned views have no upgrade aliases. Existing views are encoded exactly
// before their metadata-only receipt is issued.
const COMPONENT_BYTES: usize = 64 * 1024;
fn component(value: &impl serde::Serialize) -> Result<(), ProtocolError> {
    FrameCodec::new(COMPONENT_BYTES).write(&mut std::io::sink(), value)
        .map_err(|_| unavailable("This conversation's complete handoff metadata exceeds the bounded acknowledgement budget; exact ownership and retry evidence were retained"))
}
pub(super) fn bindings(expected: &ChatSourceBinding, admitted: &ChatSourceBinding) -> Result<(), ProtocolError> {
    component(expected)?;
    component(admitted)
}
pub(super) fn recipe(recipe: &crate::model::LaunchRecipe) -> Result<(), ProtocolError> { component(recipe) }
pub(super) fn receipt(receipt: &TerminalHandoffReceipt) -> Result<(), ProtocolError> {
    let envelope = ClientEnvelope { version: u16::MAX, request_id: u64::MAX,
        proof: receipt.source.proof.clone(), request: ClientRequest::ChatCommitTerminalReceipt { receipt: receipt.clone() } };
    FrameCodec::new(MAX_CLIENT_FRAME_BYTES).write(&mut std::io::sink(), &envelope)
        .map_err(|_| unavailable("The complete handoff cannot be acknowledged within the client frame limit; exact ownership was retained and no receipt was issued"))
}
