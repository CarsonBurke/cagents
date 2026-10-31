//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;

mod retries;

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    saving: bool,
    reported: bool,
    /// Terminals whose attach-only startup attempts ended without settling.
    pub(super) retries: retries::Retries,
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
                        CardKind::Terminal(session) => {
                            !terminal_ended(session) && !self.saved_terminal_ready(card.id)
                        }
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
        if let Some(failure) = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .find_map(|card| self.shared_chat_admission_failure(card.id))
        {
            return Err(format!(
                "Saved workspace recovery failed: {}",
                failure.message()
            ));
        }
        // A native window opens independently of the observer/SeedView task.
        // Wait for that actual admission boundary before judging saved chats
        // or declaring a formerly shared workspace recovered.
        // A daemon whose registry cannot open never admits them; the panes
        // are still qualified from their own records, and the shared
        // workspace is then reported as not restored.
        let registry_unavailable =
            self.shared_views.registry_unavailable && !self.shared_views_ready_for_startup();
        let mut pending = !self.shared_views_ready_for_startup() && !registry_unavailable;
        let mut failures = Vec::new();
        let mut reasons = Vec::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            if let CardKind::Terminal(session) = &card.kind {
                if terminal_ended(session) || self.saved_terminal_ready(card.id) {
                    continue;
                }
                if self.saved_terminal_unavailable(card.id) {
                    match self.startup_attach_exhausted(card.id) {
                        Some(reason) => {
                            failures.push(card.id);
                            reasons.push(reason.to_owned());
                        }
                        None => pending = true,
                    }
                    continue;
                }
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface)) => {
                        let settling = self.shared_views.has_pending_update(card.id);
                        match terminal_surface_ready(session, surface, settling) {
                            Ok(true) => {}
                            Ok(false) => pending = true,
                            Err(reason) => {
                                failures.push(card.id);
                                reasons.push(reason);
                            }
                        }
                    }
                    Some(CardRuntime::Loading) if !self.terminal_start_in_flight(card.id) => {
                        if let Some(reason) = self.startup_attach_exhausted(card.id) {
                            failures.push(card.id);
                            reasons.push(reason.to_owned());
                        } else {
                            pending = true;
                        }
                    }
                    Some(CardRuntime::Loading) | None => pending = true,
                    Some(CardRuntime::Failed(failure)) => {
                        failures.push(card.id);
                        reasons.push(failure.message().to_owned());
                    }
                    Some(CardRuntime::Completed) => {
                        failures.push(card.id);
                        reasons.push("it ended while it was reconnecting".into());
                    }
                    Some(_) => {
                        failures.push(card.id);
                        reasons.push("its pane no longer holds a terminal".into());
                    }
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
            let reason = reasons
                .first()
                .map(|reason| format!(" A terminal reported: {}.", startup_detail(reason)))
                .unwrap_or_default();
            return Err(format!(
                "recovery-incomplete: {} saved panes could not reconnect (panes {}).{reason} Open the affected panes and use their recovery controls. Their drafts and recovery evidence are retained; this window remains available.",
                failures.len(),
                failures
                    .iter()
                    .take(12)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if registry_unavailable {
            return Err(format!(
                "recovery-incomplete: saved panes reconnected, but the shared workspace could not be restored because {}. This window remains available with its own saved layout; the update has not been verified.",
                crate::workspace_model::REGISTRY_UNAVAILABLE
            ));
        }
        Ok(true)
    }

    /// Qualification attaches retained owners without waking their offscreen
    /// rendering surface or replaying a terminal launch recipe.
    ///
    /// It runs on every check until the gate settles: a shared view projects
    /// its panes only once the registry admits it, and can replace a pane's
    /// session afterwards. An attempt that ends detached, or a saved pane
    /// whose inspection could not verify its owner, is tried again until its
    /// refusal stops changing; the check then reports it.
    pub(super) fn qualify_startup_terminals(&mut self) -> Task<Message> {
        // Until then a shared pane's descriptor is a cache that opening the
        // card would not attach either.
        if !self.shared_views_ready_for_startup() && !self.shared_views.registry_unavailable {
            return Task::none();
        }
        let mut inspections = Vec::new();
        for card in &self.durable.cards {
            if !card.existence.is_open()
                || !matches!(&card.kind, CardKind::Terminal(session) if !terminal_ended(session))
                || self.terminal_start_in_flight(card.id)
                || !self.startup_attach_may_retry(card.id)
            {
                continue;
            }
            // A saved pane is checked again the way Reconnect checks it.
            if self.saved_terminal_unavailable(card.id) {
                inspections.push(card.id);
            } else if matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                && self.saved_terminal_current(card.id).is_none()
                && !self.restored_agent_attaches.contains(&card.id)
            {
                self.restored_agent_attaches.push_back(card.id);
            }
        }
        let mut tasks = inspections
            .into_iter()
            .map(|card| self.inspect_saved_terminal(card))
            .collect::<Vec<_>>();
        tasks.push(self.attach_next_restored_agent());
        Task::batch(tasks)
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
                let attach = self.qualify_startup_terminals();
                let next = cosmic::task::future(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Message::VerifyStartupPanes
                });
                Task::batch([attach, next])
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

/// Whether `surface` is attached to the exact terminal `session` names, on
/// its current connection and under the terminal service that hosts it now:
/// `Ok(false)` while that is still settling, or why it cannot.
///
/// A detached snapshot is not evidence either way: a hibernated view holds
/// one, and so does a view just woken until its first frame. While `settling`
/// the saved record is about to follow the registry, which catches up once
/// this window's edit carrying the attachment is applied, so a record that
/// names a different service is not yet a refusal.
fn terminal_surface_ready(
    session: &TerminalSession,
    surface: &TerminalSurface,
    settling: bool,
) -> Result<bool, String> {
    if surface.terminal_id() != session.id {
        return Err("The terminal surface no longer matches this saved pane".to_owned());
    }
    // The view's own connection is still attaching or reconnecting, and
    // reports a refusal if it gives up.
    let Some(proof) = surface.current_attachment()? else {
        return Ok(false);
    };
    if proof.is_complete() && session.continuity.live_proof().as_ref() == Some(&proof) {
        return Ok(true);
    }
    if settling {
        return Ok(false);
    }
    Err("The terminal attachment no longer matches this saved pane's owner".to_owned())
}

/// Leave room for the launcher's fixed prefix and pane IDs in its 4 KiB line.
/// Stop preparing text once the bounded summary is full; retain raw diagnostics
/// in their existing recovery presentation.
fn startup_detail(detail: &str) -> String {
    const LIMIT: usize = 1024;
    let mut text = String::new();
    let mut space = false;
    for ch in detail.chars() {
        if ch.is_whitespace() {
            space = !text.is_empty();
            continue;
        }
        let width = ch.len_utf8() + usize::from(space);
        if text.len() + width > LIMIT {
            text.push('…');
            break;
        }
        if space {
            text.push(' ');
            space = false;
        }
        text.push(ch);
    }
    text
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

#[cfg(test)]
mod attachment_tests;
