use super::*;
use crate::agent::Provider;

/// Whether a hosted terminal needs an attachment for background agent metadata.
/// Claude/Codex use daemon observers; Omp publishes foreground-verified title
/// activity. Both reach the sidebar through the same hidden attachment.
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
    matches!(
        provider,
        Some(Provider::Claude | Provider::Codex | Provider::Omp)
    )
}

#[cfg(test)]
mod tests;
