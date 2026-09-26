use super::*;
use crate::agent::Provider;

/// Whether a hosted terminal runs an agent whose activity the daemon can
/// observe, which is what the sidebar's spinner for that terminal needs.
pub(super) fn observes_terminal_agent(session: &TerminalSession) -> bool {
    if session.continuity.live_proof().is_none() {
        return false;
    }
    let provider = session
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .as_ref()
        .and_then(Provider::recognize_launch)
        .or_else(|| Provider::recognize_launch(&session.launch));
    matches!(provider, Some(Provider::Claude | Provider::Codex))
}
