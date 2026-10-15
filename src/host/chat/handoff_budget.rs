//! Preflight both the client ACK lane and the independent persisted JSON ledger.
use super::*;
use crate::terminal_ipc::{
    ChatSourceBinding, ClientEnvelope, FrameCodec, MAX_CLIENT_FRAME_BYTES, TerminalHandoffReceipt,
};

// Full receipt repeats its original/canonical sources and, for a paired view,
// embeds one additional terminal recipe in the post-source. Bound each input
// component before effects, leaving over a MiB for the generated descriptor's
// fixed scalars, <=16 KiB recovery hint and filesystem-bounded paths. Fresh
// spawned views have no upgrade aliases. Existing views are encoded exactly
// before their metadata-only receipt is issued.
const COMPONENT_BYTES: usize = 64 * 1024;
// Independently count escaping rather than deriving JSON bytes from postcard.
// The receipt embeds fewer than seven such complete input components plus its
// bounded hint/path/scalars; the shared 8 MiB promise retains ample headroom.
const COMPONENT_JSON_BYTES: usize = 512 * 1024;
fn component(value: &impl serde::Serialize) -> Result<(), ProtocolError> {
    FrameCodec::new(COMPONENT_BYTES).write(&mut std::io::sink(), value)
        .map_err(|_| unavailable("This conversation's complete handoff metadata exceeds the bounded acknowledgement budget; exact ownership and retry evidence were retained"))?;
    crate::terminal_ipc::bounded_json_bytes(value, COMPONENT_JSON_BYTES)
        .map(|_| ())
        .map_err(|_| unavailable("This conversation's complete handoff metadata exceeds its persisted JSON reservation; exact ownership and retry evidence were retained"))
}
pub(super) fn bindings(
    expected: &ChatSourceBinding,
    admitted: &ChatSourceBinding,
) -> Result<(), ProtocolError> {
    component(expected)?;
    component(admitted)
}
pub(super) fn recipe(recipe: &crate::model::LaunchRecipe) -> Result<(), ProtocolError> {
    component(recipe)
}
pub(super) fn receipt(receipt: &TerminalHandoffReceipt) -> Result<(), ProtocolError> {
    let envelope = ClientEnvelope {
        version: u16::MAX,
        request_id: u64::MAX,
        proof: receipt.source.proof.clone(),
        request: ClientRequest::ChatCommitTerminalReceipt {
            receipt: receipt.clone(),
        },
    };
    FrameCodec::new(MAX_CLIENT_FRAME_BYTES).write(&mut std::io::sink(), &envelope)
        .map_err(|_| unavailable("The complete handoff cannot be acknowledged within the client frame limit; exact ownership was retained and no receipt was issued"))?;
    crate::terminal_ipc::terminal_handoff_json_bytes(receipt).map(|_| ())
        .map_err(|_| unavailable("The complete handoff exceeds its persisted JSON reservation; exact ownership was retained and no receipt was issued"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_source_and_paired_recipe_fit_the_independent_json_reservation() {
        let proof = HostSessionProof { host_session_id: "escaped-json".into(), boot_id: "boot".into(), host_incarnation: "incarnation".into() };
        let recipe = crate::model::LaunchRecipe { program: "/bin/cat".into(), arguments: Vec::new(), working_directory: "/fixture".into(),
            environment: BTreeMap::from([("ESCAPED".into(), "\u{1}".repeat(60 * 1024))]), unset_environment: Default::default() };
        let source = Session { id: TerminalId::new(), proof: Some(proof), launch: chat::Launch { provider: Provider::Omp, recipe: recipe.clone(), session_id: Some("original".into()) },
            terminal: None, native_continuity: Default::default(), owned_codex_listener: None };
        let original = ChatSourceBinding { source: source.clone(), execution_instance: uuid::Uuid::new_v4(), backend: uuid::Uuid::new_v4() };
        let mut terminal = crate::terminal_ipc::TerminalDescriptor::relaunchable(source.id, recipe.clone(), None);
        terminal.execution_instance = uuid::Uuid::new_v4();
        let mut post = original.clone(); post.backend = uuid::Uuid::new_v4(); post.source.terminal = Some(terminal.session.clone());
        // Every admitted variable component has both wire AND JSON bounds
        // BEFORE any provider/terminal operation. The fixture invokes no IO.
        bindings(&original, &original).unwrap(); super::recipe(&recipe).unwrap();
        let receipt = TerminalHandoffReceipt { operation: uuid::Uuid::new_v4(), source,
            source_execution_instance: original.execution_instance, source_binding: original, retained_binding: Some(post),
            terminal_native_epoch: uuid::Uuid::new_v4(), terminal };
        super::receipt(&receipt).unwrap();
        let json = crate::terminal_ipc::terminal_handoff_json_bytes(&receipt).unwrap();
        assert_eq!(json, serde_json::to_vec(&receipt).unwrap().len());
        let mut wire = Vec::new(); FrameCodec::new(MAX_CLIENT_FRAME_BYTES).write(&mut wire, &receipt).unwrap();
        assert!(json > wire.len() * 4, "actual control escaping must make the old wire-only inference invalid");
        assert!(json <= crate::terminal_ipc::MAX_HANDOFF_RECEIPT_JSON_BYTES);
        let mut oversized = receipt;
        oversized.source.launch.recipe.environment.insert("ESCAPED".into(), "\u{1}".repeat(2 * 1024 * 1024));
        assert!(crate::terminal_ipc::terminal_handoff_json_bytes(&oversized).is_err(), "exact issuer never exceeds the shared persisted promise");
        assert!(component(&oversized.source).is_err(), "oversize input is refused at preflight before effects");
    }
}
