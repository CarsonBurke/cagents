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

impl Cagents {
    fn polls_terminal_agent(&self, card: CardId) -> bool {
        let Some(state) = self.card_state(card) else {
            return false;
        };
        if !state.existence.is_open() {
            return false;
        }
        let CardKind::Terminal(session) = &state.kind else {
            return false;
        };
        observes_terminal_agent(session)
    }

    pub(super) fn terminal_agent_activity_subscription(&self) -> Subscription<Message> {
        if self.cards.iter().any(|(card, runtime)| {
            matches!(runtime, CardRuntime::Terminal(_)) && self.polls_terminal_agent(*card)
        }) {
            iced::time::every(std::time::Duration::from_secs(2))
                .map(|_| Message::PollTerminalAgentActivity)
        } else {
            Subscription::none()
        }
    }

    pub(super) fn poll_terminal_agent_activity(&mut self) -> Task<Message> {
        for (card, runtime) in &self.cards {
            if self.polls_terminal_agent(*card)
                && let CardRuntime::Terminal(surface) = runtime
            {
                let _ = surface.refresh_agent_activity();
            }
        }
        Task::none()
    }
}
