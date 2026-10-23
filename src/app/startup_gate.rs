//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    saving: bool,
    reported: bool,
}

impl State {
    pub(super) fn window_opened(&self) -> bool {
        self.opened.is_some()
    }
    #[cfg(test)]
    pub(super) fn fixture_opened() -> Self {
        Self {
            opened: Some(Instant::now()),
            ..Self::default()
        }
    }
}

impl Cagents {
    fn startup_conversation_count(&self) -> usize {
        self.durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && match &card.kind {
                        CardKind::Chat(session) => {
                            !session.is_unstarted()
                                && !self.retained_startup_draft(card.id, session)
                        }
                        CardKind::Terminal(session) => !terminal_ended(session),
                        _ => false,
                    }
            })
            .count()
    }

    /// An exact retained local copy is saved data, not a hosted pane admitted
    /// by this window. A pending marker on a shared pane alone cannot waive
    /// its original attachment/readiness requirement.
    fn retained_startup_draft(&self, card: CardId, session: &crate::chat::Session) -> bool {
        self.durable.local_only_cards.contains(&card)
            && self.durable.pending_chat_admissions.get(&card) == Some(session)
            && self.chat_awaiting_admission(card)
    }

    pub(super) fn startup_pane_check(&self) -> Result<bool, String> {
        // A native window opens independently of the observer/SeedView task.
        // Wait for that actual admission boundary before judging saved chats
        // or declaring a formerly shared workspace recovered.
        let mut pending = !self.shared_views_ready_for_startup();
        let mut failures = Vec::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            if let CardKind::Terminal(session) = &card.kind {
                if terminal_ended(session) {
                    continue;
                }
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface))
                        if surface.terminal_id() == session.id
                            && session.continuity.live_proof().is_some_and(|proof| {
                                proof.is_complete() && surface.host_proof().as_ref() == Some(&proof)
                            })
                            && !surface.snapshot().detached => {}
                    Some(CardRuntime::Loading) | None => pending = true,
                    _ => failures.push(card.id),
                }
                continue;
            }
            let CardKind::Chat(session) = &card.kind else {
                continue;
            };
            if session.is_unstarted() || self.retained_startup_draft(card.id, session) {
                continue;
            }
            if self.chat_awaiting_admission(card.id) && pending {
                continue;
            }
            if self.chat_start_pending(card.id) {
                pending = true;
                continue;
            }
            match self.cards.get(&card.id) {
                Some(CardRuntime::Chat(runtime)) => match runtime.restored_pane_ready() {
                    Ok(true) => {}
                    Ok(false) => pending = true,
                    Err(_) => failures.push(card.id),
                },
                Some(CardRuntime::Loading) | None => pending = true,
                _ => failures.push(card.id),
            }
        }
        if pending {
            return Ok(false);
        }
        if !failures.is_empty() {
            return Err(format!(
                "recovery-incomplete: {} saved panes could not reconnect (panes {}). Open the affected panes and use their recovery controls. Their drafts and recovery evidence are retained; this window remains available.",
                failures.len(),
                failures
                    .iter()
                    .take(12)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(true)
    }

    /// Qualification attaches retained owners without waking their offscreen
    /// rendering surface or replaying a terminal launch recipe.
    pub(super) fn qualify_startup_terminals(&mut self) -> Task<Message> {
        for card in &self.durable.cards {
            if card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session) if !terminal_ended(session))
                && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                && !self.restored_agent_attaches.contains(&card.id)
            {
                self.restored_agent_attaches.push_back(card.id);
            }
        }
        self.attach_next_restored_agent()
    }

    pub(super) fn begin_startup_gate(&mut self) -> Task<Message> {
        if !instance::ui_launch_handshake_requested() {
            return report(Ok(()));
        }
        self.startup_gate.opened = Some(Instant::now());
        let count = self.startup_conversation_count();
        let attach = self.qualify_startup_terminals();
        let progress = cosmic::task::future(async move {
            match tokio::task::spawn_blocking(move || instance::report_ui_recovery_progress(count))
                .await
            {
                Ok(Ok(())) => Message::VerifyStartupPanes,
                Ok(Err(error)) => Message::StartupReported(Err(error.to_string())),
                Err(error) => Message::StartupReported(Err(error.to_string())),
            }
        });
        Task::batch([progress, attach])
    }

    pub(super) fn verify_startup_gate(&mut self) -> Task<Message> {
        if self.startup_gate.reported
            || self.startup_gate.saving
            || self.startup_gate.opened.is_none()
        {
            return Task::none();
        }
        match self.startup_pane_check() {
            Err(reason) => {
                self.startup_gate.reported = true;
                self.status = Some(TransientNotice::warning(reason.clone()));
                report(Err(reason))
            }
            Ok(false) => {
                let count = self.startup_conversation_count();
                let timeout = Duration::from_secs(30 + 30 * count as u64);
                if self.startup_gate.opened.unwrap().elapsed() >= timeout {
                    self.startup_gate.reported = true;
                    let reason = "recovery-incomplete: saved conversations are still reconnecting. The window remains available; open the affected chats to inspect their recovery controls. The update has not been verified.".to_owned();
                    self.status = Some(TransientNotice::warning(reason.clone()));
                    return report(Err(reason));
                }
                cosmic::task::future(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Message::VerifyStartupPanes
                })
            }
            Ok(true) => {
                self.startup_gate.saving = true;
                self.persist();
                let save = self.capture_workspace_save();
                cosmic::task::future(async move {
                    let (settings, workspace) = flush_workspace(save).await;
                    Message::StartupPanesSaved(settings, workspace)
                })
            }
        }
    }

    pub(super) fn startup_panes_saved(
        &mut self,
        settings: Result<SettingsSaved, String>,
        saved: Result<(), String>,
    ) -> Task<Message> {
        self.startup_gate.saving = false;
        let settings = settings.map(|settings| self.acknowledge_settings_save(settings));
        let mut result = settings.and(saved).map_err(|error| format!("recovery-incomplete: conversations reconnected, but the recovered workspace could not be saved: {error}. This window remains available; retry saving before updating again."));
        if result.is_ok() {
            match self.startup_pane_check() {
                Ok(true) => {}
                Ok(false) => return self.verify_startup_gate(),
                Err(reason) => result = Err(reason),
            }
        }
        self.startup_gate.reported = true;
        if let Err(reason) = &result {
            self.status = Some(TransientNotice::warning(reason.clone()));
        }
        report(result)
    }
}

fn report(result: Result<(), String>) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || instance::report_ui_startup(&result))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
        Message::StartupReported(result)
    })
}
