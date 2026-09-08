use super::*;
use crate::model::{
    TerminalActivity, TerminalActivityLifecycle, TerminalActivityState, TerminalActivityTimer,
};

pub(super) struct ActivityPresentation {
    terminal: TerminalId,
    proof: HostSessionProof,
    revision: u64,
    reducer: TerminalActivity,
}
#[derive(Clone)]
struct ActivityTimerSource(CardId, TerminalActivityTimer, Instant);
impl std::hash::Hash for ActivityTimerSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.terminal_id.hash(state);
        self.1.sequence.hash(state);
        self.1.deadline.hash(state);
        self.1.proof.host_session_id.hash(state);
        self.1.proof.boot_id.hash(state);
        self.1.proof.host_incarnation.hash(state);
    }
}
fn timer_stream(
    source: &ActivityTimerSource,
) -> Pin<Box<dyn cosmic::iced::futures::Stream<Item = (CardId, TerminalActivityTimer)> + Send>> {
    let (card, timer, origin) = (source.0, source.1.clone(), source.2);
    Box::pin(cosmic::iced::futures::stream::once(async move {
        tokio::time::sleep(timer.deadline.saturating_sub(origin.elapsed())).await;
        (card, timer)
    }))
}
impl Cagents {
    pub(super) fn refresh_terminal_activity(&mut self) {
        let workspace = &self.workspace;
        self.terminal_activity
            .retain(|card, _| workspace.visibility(*card) == Some(VisibilityClass::Visible));
        for card in workspace
            .order()
            .filter(|&card| workspace.visibility(card) == Some(VisibilityClass::Visible))
        {
            let Some(CardState {
                kind: CardKind::Terminal(session),
                ..
            }) = self.durable.cards.iter().find(|state| state.id == card)
            else {
                continue;
            };
            let terminal = session.id;
            let (proof, revision, output) = match self.cards.get(&card) {
                Some(CardRuntime::Terminal(handle)) => {
                    (handle.host_proof(), handle.generation(), handle.activity())
                }
                _ => (
                    session.continuity.live_proof(),
                    self.terminal_activity
                        .get(&card)
                        .filter(|entry| entry.terminal == terminal)
                        .map_or(0, |entry| entry.revision),
                    None,
                ),
            };
            let Some(proof) = proof else {
                self.terminal_activity.remove(&card);
                continue;
            };
            let lifecycle = match &session.continuity {
                TerminalContinuity::Exited { .. }
                | TerminalContinuity::RelaunchAvailable {
                    last_exit: Some(_), ..
                } => TerminalActivityLifecycle::Exited,
                TerminalContinuity::Failed { .. } => TerminalActivityLifecycle::Failed,
                TerminalContinuity::Live { .. } | TerminalContinuity::DetachedSameBoot { .. } => {
                    match self.cards.get(&card) {
                        Some(CardRuntime::Terminal(_)) => TerminalActivityLifecycle::Live,
                        Some(CardRuntime::Loading) => TerminalActivityLifecycle::Starting,
                        Some(CardRuntime::Failed(_)) => TerminalActivityLifecycle::Failed,
                        _ => TerminalActivityLifecycle::Unknown,
                    }
                }
                _ => TerminalActivityLifecycle::Unknown,
            };
            let entry =
                self.terminal_activity
                    .entry(card)
                    .or_insert_with(|| ActivityPresentation {
                        terminal,
                        proof: proof.clone(),
                        revision: 0,
                        reducer: TerminalActivity::new(terminal, proof.clone()),
                    });
            if entry.terminal != terminal || entry.proof != proof {
                *entry = ActivityPresentation {
                    terminal,
                    proof: proof.clone(),
                    revision: 0,
                    reducer: TerminalActivity::new(terminal, proof.clone()),
                };
            }
            entry.reducer.observe(
                &proof,
                revision,
                lifecycle,
                output,
                self.activity_origin.elapsed(),
                unix_millis(),
            );
            entry.revision = entry.revision.max(revision);
        }
    }
    pub(super) fn activity_subscriptions(&self) -> Vec<Subscription<Message>> {
        self.terminal_activity
            .iter()
            .filter_map(|(&card, state)| {
                state.reducer.timer().map(|timer| {
                    Subscription::run_with(
                        ActivityTimerSource(card, timer, self.activity_origin),
                        timer_stream,
                    )
                    .map(|(card, timer)| Message::TerminalActivityExpired { card, timer })
                })
            })
            .collect()
    }
    pub(super) fn expire_terminal_activity(
        &mut self,
        card: CardId,
        timer: TerminalActivityTimer,
    ) -> Task<Message> {
        if let Some(state) = self.terminal_activity.get_mut(&card) {
            state.reducer.expire(&timer, self.activity_origin.elapsed());
        }
        Task::none()
    }
    pub(super) fn terminal_activity_label(&self, card: CardId) -> &'static str {
        match self
            .terminal_activity
            .get(&card)
            .map(|state| state.reducer.state())
        {
            Some(TerminalActivityState::Starting) => "Starting",
            Some(TerminalActivityState::OutputActive) => "Output active — PTY output observed",
            Some(TerminalActivityState::Quiet) => "Quiet — no PTY output observed within 5 seconds",
            Some(TerminalActivityState::Exited) => "Exited",
            Some(TerminalActivityState::Failed) => "Failed",
            _ => match terminal_continuity(self.card_state(card)) {
                Some(
                    TerminalContinuity::Exited { .. }
                    | TerminalContinuity::RelaunchAvailable {
                        last_exit: Some(_), ..
                    },
                ) => "Exited",
                Some(TerminalContinuity::Failed { .. }) => "Failed",
                _ => "Activity unknown",
            },
        }
    }
}
