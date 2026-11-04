//! Session layout before protocol129, used only by qualified source inspectors.
//! Keep the launch fields frozen: postcard does not delimit nested structs.
use crate::{
    agent::Provider,
    chat::{Launch, NativeContinuity, OwnedCodexListener, Session},
    model::{HostSessionProof, LaunchRecipe, TerminalId, TerminalSession},
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(super) struct LegacySession {
    id: TerminalId,
    launch: LegacyLaunch,
    proof: Option<HostSessionProof>,
    terminal: Option<TerminalSession>,
    owned_codex_listener: Option<OwnedCodexListener>,
    native_continuity: NativeContinuity,
}

#[derive(Serialize, Deserialize)]
struct LegacyLaunch {
    provider: Provider,
    recipe: LaunchRecipe,
    session_id: Option<String>,
}

impl From<Session> for LegacySession {
    fn from(value: Session) -> Self {
        Self {
            id: value.id,
            launch: LegacyLaunch {
                provider: value.launch.provider,
                recipe: value.launch.recipe,
                session_id: value.launch.session_id,
            },
            proof: value.proof,
            terminal: value.terminal,
            owned_codex_listener: value.owned_codex_listener,
            native_continuity: value.native_continuity,
        }
    }
}

impl From<LegacySession> for Session {
    fn from(value: LegacySession) -> Self {
        Self {
            id: value.id,
            launch: Launch {
                provider: value.launch.provider,
                recipe: value.launch.recipe,
                session_id: value.launch.session_id,
                draft_model: None,
            },
            proof: value.proof,
            terminal: value.terminal,
            owned_codex_listener: value.owned_codex_listener,
            native_continuity: value.native_continuity,
        }
    }
}
