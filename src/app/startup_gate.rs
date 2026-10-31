//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;
use crate::terminal_ipc::TerminalVisibility;

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    saving: bool,
    reported: bool,
    /// Terminals whose attach-only startup attempts ended without a surface.
    /// Nothing else retries them until the card is opened, so qualification
    /// tries again a few times and the gate then reports them.
    detached: HashMap<CardId, Detached>,
}

/// The attach-only attempts made for one exact session.
struct Detached {
    terminal: TerminalId,
    reason: String,
    attempts: u32,
    at: Instant,
}

/// A terminal the daemon has just restored can still be settling when the
/// first attempt reaches it; more than this is a refusal to report.
const ATTACH_ATTEMPTS: u32 = 3;
const ATTACH_RETRY_DELAY: Duration = Duration::from_secs(2);

impl State {
    pub(super) fn window_opened(&self) -> bool {
        self.opened.is_some()
    }

    pub(super) fn terminal_detached(&mut self, card: CardId, terminal: TerminalId, reason: String) {
        let attempts = self
            .detached
            .get(&card)
            .filter(|detached| detached.terminal == terminal)
            .map_or(1, |detached| detached.attempts + 1);
        self.detached.insert(
            card,
            Detached {
                terminal,
                reason,
                attempts,
                at: Instant::now(),
            },
        );
    }

    /// `session`'s attempts so far. A different session on the same card is
    /// a new owner to qualify, not these outcomes.
    fn detached(&self, card: CardId, session: &TerminalSession) -> Option<&Detached> {
        self.detached
            .get(&card)
            .filter(|detached| detached.terminal == session.id)
    }

    /// Why `session` stays detached once its attempts are spent.
    fn detached_reason(&self, card: CardId, session: &TerminalSession) -> Option<&str> {
        self.detached(card, session)
            .filter(|detached| detached.attempts >= ATTACH_ATTEMPTS)
            .map(|detached| detached.reason.as_str())
    }

    /// Whether qualification may attempt `session` now.
    fn may_attach(&self, card: CardId, session: &TerminalSession) -> bool {
        self.detached(card, session).is_none_or(|detached| {
            detached.attempts < ATTACH_ATTEMPTS && detached.at.elapsed() >= ATTACH_RETRY_DELAY
        })
    }
    /// Lets every recorded attempt be retried at once.
    #[cfg(test)]
    pub(super) fn expire_attach_retries(&mut self) {
        for detached in self.detached.values_mut() {
            detached.at -= ATTACH_RETRY_DELAY;
        }
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
        // A native window opens independently of the observer/SeedView task.
        // Wait for that actual admission boundary before judging saved chats
        // or declaring a formerly shared workspace recovered.
        let mut pending = !self.shared_views_ready_for_startup();
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
                if let Some(reason) = self.saved_terminal_unavailable(card.id) {
                    failures.push(card.id);
                    reasons.push(reason);
                    continue;
                }
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface)) => {
                        match terminal_surface_ready(session, surface) {
                            Ok(true) => {}
                            Ok(false) => pending = true,
                            Err(reason) => {
                                failures.push(card.id);
                                reasons.push(reason);
                            }
                        }
                    }
                    Some(CardRuntime::Loading)
                        if !self.terminal_start_in_flight(card.id)
                            && let Some(reason) =
                                self.startup_gate.detached_reason(card.id, session) =>
                    {
                        failures.push(card.id);
                        reasons.push(reason.to_owned());
                    }
                    Some(CardRuntime::Loading) | None => pending = true,
                    Some(CardRuntime::Failed(failure)) => {
                        failures.push(card.id);
                        reasons.push(failure.message().to_owned());
                    }
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
        Ok(true)
    }

    /// Qualification attaches retained owners without waking their offscreen
    /// rendering surface or replaying a terminal launch recipe.
    ///
    /// It runs on every check until the gate settles: a shared view projects
    /// its panes only once the registry admits it, and can replace a pane's
    /// session afterwards. An attempt that ends detached is tried again a
    /// few times, then reported by the check.
    pub(super) fn qualify_startup_terminals(&mut self) -> Task<Message> {
        // Until then a shared pane's descriptor is a cache that opening the
        // card would not attach either.
        if !self.shared_views_ready_for_startup() {
            return Task::none();
        }
        for card in &self.durable.cards {
            if card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session)
                    if !terminal_ended(session)
                        && self.startup_gate.may_attach(card.id, session))
                && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                && self.saved_terminal_current(card.id).is_none()
                && !self.terminal_start_in_flight(card.id)
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

/// A hibernated screen marker is independent of its actual attachment. Only
/// the current connection's accepted exact terminal can qualify this row.
fn terminal_surface_ready(
    session: &TerminalSession,
    surface: &TerminalSurface,
) -> Result<bool, String> {
    if surface.terminal_id() != session.id {
        return Err("The terminal surface no longer matches this saved pane".to_owned());
    }
    let Some(proof) = surface.current_attachment()? else {
        return Ok(false);
    };
    if !proof.is_complete() || session.continuity.live_proof().as_ref() != Some(&proof) {
        return Err("The terminal attachment no longer matches this saved pane's owner".to_owned());
    }
    Ok(surface.visibility() == TerminalVisibility::Hibernated || !surface.snapshot().detached)
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
