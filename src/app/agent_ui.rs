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

/// How often the daemon is asked what the agent in a live pane is doing.
const AGENT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// A hibernated agent - restored and never opened, or evicted from memory -
/// shows no pane, only its sidebar row, which can trail the agent by a few
/// seconds more than an open pane's can.
const HIBERNATED_AGENT_POLL_INTERVAL: Duration = Duration::from_secs(6);
/// Every agent while the window is unfocused: a window on another monitor, or
/// one glanced at from another application, still shows a turn ending, but
/// nobody is typing into it.
const UNFOCUSED_AGENT_POLL_INTERVAL: Duration = Duration::from_secs(10);

impl Cagents {
    /// Whether `card` runs an observed agent, and whether it is hibernated,
    /// which decides how often it is polled.
    fn polled_terminal_agent(&self, card: CardId) -> Option<bool> {
        let state = self.card_state(card)?;
        if !state.existence.is_open() {
            return None;
        }
        let CardKind::Terminal(session) = &state.kind else {
            return None;
        };
        observes_terminal_agent(session)
            .then(|| matches!(self.lifecycle.state(card), Some((_, Residency::Hibernated))))
    }

    /// Polling slows while the window is unfocused, and regaining focus polls
    /// every agent at once, so rows are current whenever they are looked at.
    pub(super) fn terminal_agent_activity_subscription(&self) -> Subscription<Message> {
        let (mut live, mut hibernated) = (false, false);
        for (card, runtime) in &self.cards {
            if let CardRuntime::Terminal(_) = runtime {
                match self.polled_terminal_agent(*card) {
                    Some(true) => hibernated = true,
                    Some(false) => live = true,
                    None => {}
                }
            }
        }
        if !self.refresh_ui.focused {
            return if live || hibernated {
                iced::time::every(UNFOCUSED_AGENT_POLL_INTERVAL)
                    .map(|_| Message::PollTerminalAgentActivity { hibernated: None })
            } else {
                Subscription::none()
            };
        }
        Subscription::batch([
            if live {
                iced::time::every(AGENT_POLL_INTERVAL).map(|_| Message::PollTerminalAgentActivity {
                    hibernated: Some(false),
                })
            } else {
                Subscription::none()
            },
            if hibernated {
                iced::time::every(HIBERNATED_AGENT_POLL_INTERVAL).map(|_| {
                    Message::PollTerminalAgentActivity {
                        hibernated: Some(true),
                    }
                })
            } else {
                Subscription::none()
            },
        ])
    }

    /// Asks the daemon about every agent polled at the cadence `hibernated`
    /// names; `None` polls them all.
    pub(super) fn poll_terminal_agent_activity(&self, hibernated: Option<bool>) {
        for (card, runtime) in &self.cards {
            if let CardRuntime::Terminal(surface) = runtime
                && let Some(polled) = self.polled_terminal_agent(*card)
                && hibernated.is_none_or(|hibernated| hibernated == polled)
            {
                let _ = surface.refresh_agent_activity();
            }
        }
    }
}
