//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;

mod retries;

/// The launcher's deadline starts when it spawns this process, and the gate's
/// budget when the window opens. This covers the daemon handshake and state
/// loading in between, so the launcher's generic timeout does not replace the
/// report naming what is stuck.
const LAUNCHER_MARGIN: Duration = Duration::from_secs(15);
const DETAILS_LABEL: &str = " Details: ";

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    /// Fixed from the inventory announced to the launcher, which fixes its own
    /// deadline from that announcement once.
    budget: Option<Duration>,
    saving: bool,
    reported: bool,
    /// Terminals the gate is waiting on without anything settling them.
    pub(super) retries: retries::Retries,
    /// Since when a view this window shows has not been admitted.
    views_unready: Option<Instant>,
}

impl State {
    pub(super) fn window_opened(&self) -> bool {
        self.opened.is_some()
    }

    /// Ends the window for a view that has not been admitted.
    #[cfg(test)]
    pub(super) fn expire_views_unready(&mut self) {
        if let Some(since) = &mut self.views_unready {
            *since -= retries::RETRY_WINDOW;
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn fixture_reported() -> Self {
        Self {
            reported: true,
            ..Self::fixture_opened()
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
        // A daemon whose registry cannot open never admits them; the panes
        // are still qualified from their own records, and the shared
        // workspace is then reported as not restored.
        let views_hold = self.shared_views_startup_hold();
        let registry_unavailable = self.shared_views.registry_unavailable && views_hold.is_some();
        let views_hold = views_hold.filter(|_| !registry_unavailable);
        // A view that is never admitted is reported, with each pane it holds.
        let views_stalled = views_hold.filter(|_| {
            self.startup_gate
                .views_unready
                .is_some_and(|since| since.elapsed() >= retries::RETRY_WINDOW)
        });
        let views_pending = views_hold.is_some();
        let mut pending = views_pending && views_stalled.is_none();
        let mut failures = Vec::new();
        let mut out_of_step = Vec::new();
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
                        match terminal_surface_ready(session, surface) {
                            Ok(true) => {}
                            Ok(false) => pending = true,
                            // Its record can still follow the registry to
                            // the attachment, so it is given the window.
                            Err(Unready::Mismatch { .. }) => {
                                match self.startup_attach_exhausted(card.id) {
                                    Some(reason) => {
                                        out_of_step.push(card.id);
                                        reasons.push(reason.to_owned());
                                    }
                                    None => pending = true,
                                }
                            }
                            Err(Unready::Failed(reason)) => {
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
            if self.chat_awaiting_admission(card.id) && (pending || views_pending) {
                if let Some(hold) = views_stalled {
                    failures.push(card.id);
                    reasons.push(hold.to_owned());
                }
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
                    Err(reason) => {
                        failures.push(card.id);
                        reasons.push(reason);
                    }
                },
                Some(CardRuntime::Loading) | None => pending = true,
                Some(CardRuntime::Failed(failure)) => {
                    failures.push(card.id);
                    reasons.push(failure.message().to_owned());
                }
                Some(_) => {
                    failures.push(card.id);
                    reasons.push("its pane no longer holds its conversation".into());
                }
            }
        }
        if pending {
            return Ok(false);
        }
        if !out_of_step.is_empty() {
            return Err(self.startup_out_of_step(&failures, &out_of_step, reasons.first()));
        }
        if !failures.is_empty() {
            return Err(self.startup_incomplete(&failures, reasons.first()));
        }
        if let Some(hold) = views_stalled {
            return Err(views_incomplete(hold));
        }
        if registry_unavailable {
            return Err(format!(
                "recovery-incomplete: saved panes reconnected, but the shared workspace could not be restored because {}. This window remains available with its own saved layout.",
                crate::workspace_model::REGISTRY_UNAVAILABLE
            ));
        }
        Ok(true)
    }

    /// The report for `failures`, named as their tabs are, with the first
    /// reason any of them gave.
    fn startup_incomplete(&self, failures: &[CardId], reason: Option<&String>) -> String {
        let names = self.startup_names(failures);
        let (count, verb) = match failures.len() {
            1 => ("1 pane".to_owned(), "it"),
            count => (format!("{count} panes"), "them"),
        };
        let detail = startup_details(reason);
        if failures.iter().all(|card| self.startup_pane_held(*card)) {
            return format!(
                "recovery-incomplete: {count} did not reconnect: {names}. Nothing was closed or restarted, and this window stays available.{detail}"
            );
        }
        format!(
            "recovery-incomplete: {count} did not reconnect: {names}. Open {verb} to reconnect or recover {verb}; drafts and recovery evidence are kept, and this window stays available.{detail}"
        )
    }

    /// Whether `card` has no surface because something holds it back, so
    /// opening it would only show why rather than reconnect it.
    fn startup_pane_held(&self, card: CardId) -> bool {
        if !matches!(self.cards.get(&card), Some(CardRuntime::Loading) | None) {
            return false;
        }
        self.shared_views_startup_hold()
            .filter(|_| !self.shared_views.registry_unavailable)
            .is_some()
            || matches!(
                self.card_state(card).map(|state| &state.kind),
                Some(CardKind::Terminal(session))
                    if self.shared_terminal_attach_hold(card, session).is_some()
            )
    }

    /// The report when `out_of_step` panes are attached and working, so
    /// opening them would change nothing, but the shared workspace's record
    /// of them names something else.
    fn startup_out_of_step(
        &self,
        failures: &[CardId],
        out_of_step: &[CardId],
        reason: Option<&String>,
    ) -> String {
        let names = self.startup_names(out_of_step);
        let (count, verb) = match out_of_step.len() {
            1 => ("1 pane".to_owned(), "it"),
            count => (format!("{count} panes"), "them"),
        };
        let sentence = format!(
            "{count} reconnected and can be used in this window, but the shared workspace lists {verb} under a different terminal service: {names}."
        );
        let detail = startup_details(reason);
        if failures.is_empty() {
            format!("recovery-incomplete: {sentence} This window stays available.{detail}")
        } else {
            let failed = self.startup_incomplete(failures, None);
            format!("{failed} {sentence}{detail}")
        }
    }

    /// `cards` as their tabs name them, short enough for the launcher's line.
    fn startup_names(&self, cards: &[CardId]) -> String {
        const NAMED: usize = 8;
        let mut names = cards
            .iter()
            .take(NAMED)
            .map(|card| {
                let title = self
                    .card_state(*card)
                    .map_or("Untitled", |state| self.card_display_title(state));
                format!("\u{201c}{}\u{201d}", startup_name(title))
            })
            .collect::<Vec<_>>()
            .join(", ");
        if cards.len() > NAMED {
            names.push_str(&format!(" and {} more", cards.len() - NAMED));
        }
        names
    }

    /// What the gate reports once its budget runs out: each pane it was
    /// still waiting on, named as its tab is, with the first reason known.
    pub(super) fn startup_timeout_report(&self) -> String {
        let mut waiting = Vec::new();
        let mut reasons = Vec::new();
        let views_hold = self
            .shared_views_startup_hold()
            .filter(|_| !self.shared_views.registry_unavailable);
        reasons.extend(views_hold.map(str::to_owned));
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            let reason = match &card.kind {
                CardKind::Terminal(session) => {
                    if terminal_ended(session) || self.saved_terminal_ready(card.id) {
                        continue;
                    }
                    match self.cards.get(&card.id) {
                        Some(CardRuntime::Terminal(surface)) => {
                            match terminal_surface_ready(session, surface) {
                                Ok(true) => continue,
                                Ok(false) => "its connection was still attaching".to_owned(),
                                Err(Unready::Failed(reason)) => reason,
                                Err(Unready::Mismatch { .. }) => self
                                    .startup_terminal_latest(card.id)
                                    .unwrap_or("its record names a different terminal service")
                                    .to_owned(),
                            }
                        }
                        _ if self.terminal_start_in_flight(card.id) => {
                            "it was still reconnecting".to_owned()
                        }
                        _ => self
                            .startup_terminal_latest(card.id)
                            .unwrap_or(if self.saved_terminal_current(card.id).is_some() {
                                "its saved terminal was still being checked"
                            } else {
                                "it was still waiting to reconnect"
                            })
                            .to_owned(),
                    }
                }
                CardKind::Chat(session) => {
                    if session.is_unstarted()
                        || self.retained_startup_draft(card.id, session)
                        || matches!(self.cards.get(&card.id), Some(CardRuntime::Chat(runtime))
                            if runtime.restored_pane_ready() == Ok(true))
                    {
                        continue;
                    }
                    "it was still reconnecting".to_owned()
                }
                _ => continue,
            };
            waiting.push(card.id);
            reasons.push(reason);
        }
        match (waiting.is_empty(), views_hold) {
            (false, _) => self.startup_incomplete(&waiting, reasons.first()),
            (true, Some(hold)) => views_incomplete(hold),
            (true, None) => "recovery-incomplete: saved conversations are still reconnecting. The window remains available; open the affected chats to inspect their recovery controls.".to_owned(),
        }
    }

    /// Records why each saved terminal the gate is waiting on, with nothing
    /// in flight to settle it, is still waiting: it is held back from
    /// attaching, or attached under a different terminal service than its
    /// record names. Each is reported once that has lasted its window.
    fn observe_unsettled_terminals(&mut self) {
        let views_hold = self
            .shared_views_startup_hold()
            .filter(|_| !self.shared_views.registry_unavailable);
        if views_hold.is_none() {
            self.startup_gate.views_unready = None;
        } else {
            self.startup_gate
                .views_unready
                .get_or_insert_with(Instant::now);
        }
        let mut unsettled = Vec::new();
        let mut settled = Vec::new();
        let mut attached_now = Vec::new();
        for card in &self.durable.cards {
            let CardKind::Terminal(session) = &card.kind else {
                continue;
            };
            settled.push(card.id);
            if !card.existence.is_open()
                || terminal_ended(session)
                || self.saved_terminal_current(card.id).is_some()
                || self.terminal_start_in_flight(card.id)
            {
                continue;
            }
            match self.cards.get(&card.id) {
                Some(CardRuntime::Terminal(surface)) => {
                    attached_now.push(card.id);
                    if let Err(Unready::Mismatch { attached }) =
                        terminal_surface_ready(session, surface)
                    {
                        let edit_pending = self.shared_views.has_pending_update(card.id);
                        let registry = self
                            .workspace_model
                            .views
                            .iter()
                            .flat_map(|view| &view.panes)
                            .find(|pane| pane.id == card.id)
                            .and_then(|pane| match &pane.kind {
                                CardKind::Terminal(registered) => {
                                    registered.continuity.live_proof()
                                }
                                _ => None,
                            });
                        let detail = format!(
                            "attached {attached:?}, saved {:?}, registry {registry:?}, edit pending {edit_pending}",
                            session.continuity.live_proof()
                        );
                        settled.pop();
                        unsettled.push((
                            card.id,
                            session.id,
                            "this window's record of it names a different terminal service than the one it is attached to",
                            detail,
                        ));
                    }
                }
                Some(CardRuntime::Loading) => {
                    if let Some(hold) =
                        views_hold.or_else(|| self.restored_attach_hold(card.id, session))
                    {
                        settled.pop();
                        unsettled.push((card.id, session.id, hold, String::new()));
                    }
                }
                _ => {}
            }
        }
        for card in settled {
            self.startup_terminal_settled(card);
        }
        // A refusal before this attach says nothing about the pane now.
        for card in attached_now {
            self.startup_terminal_attached(card);
        }
        for (card, terminal, reason, detail) in unsettled {
            self.startup_terminal_unsettled(card, terminal, reason, &detail);
        }
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
        self.startup_gate.budget = Some(launcher_budget(count));
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
        self.observe_unsettled_terminals();
        match self.startup_pane_check() {
            Err(reason) => {
                self.startup_gate.reported = true;
                self.status = Some(incomplete_notice(&reason));
                report(Err(reason))
            }
            Ok(false) => {
                let elapsed = self.startup_gate.opened.unwrap().elapsed();
                if self
                    .startup_gate
                    .budget
                    .is_some_and(|budget| elapsed >= budget)
                {
                    self.startup_gate.reported = true;
                    let reason = self.startup_timeout_report();
                    self.status = Some(incomplete_notice(&reason));
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
            self.status = Some(incomplete_notice(reason));
        }
        report(result)
    }
}

/// Why a terminal surface does not qualify its saved pane yet.
#[derive(Debug, PartialEq)]
enum Unready {
    /// Attached to the exact terminal on its current connection, under
    /// `attached`, which is not the complete proof the pane's record names.
    Mismatch {
        attached: HostSessionProof,
    },
    Failed(String),
}

/// Whether `surface` is attached to the exact terminal `session` names, on
/// its current connection and under the terminal service that hosts it now:
/// `Ok(false)` while it is still attaching or reconnecting, or why not.
///
/// A detached snapshot is not evidence either way: a hibernated view holds
/// one, and so does a view just woken until its first frame.
fn terminal_surface_ready(
    session: &TerminalSession,
    surface: &TerminalSurface,
) -> Result<bool, Unready> {
    if surface.terminal_id() != session.id {
        return Err(Unready::Failed(
            "The terminal surface no longer matches this saved pane".to_owned(),
        ));
    }
    // The view's own connection reports a refusal if it gives up.
    let Some(attached) = surface.current_attachment().map_err(Unready::Failed)? else {
        return Ok(false);
    };
    if attached.is_complete() && session.continuity.live_proof().as_ref() == Some(&attached) {
        return Ok(true);
    }
    Err(Unready::Mismatch { attached })
}

/// The report for a view this window shows that was never admitted.
fn views_incomplete(hold: &str) -> String {
    format!(
        "recovery-incomplete: this window's shared workspace did not finish restoring: {hold}. This window stays available."
    )
}

/// The first reason known, as it ends a report.
fn startup_details(reason: Option<&String>) -> String {
    reason
        .map(|reason| format!("{DETAILS_LABEL}{}.", startup_detail(reason)))
        .unwrap_or_default()
}

/// A tab title short enough to name in the launcher's 4 KiB line.
fn startup_name(title: &str) -> String {
    const LIMIT: usize = 40;
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    match title.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}\u{2026}", &title[..end]),
        None => title,
    }
}

/// Leave room for the launcher's fixed prefix and pane names in its 4 KiB line.
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

fn launcher_budget(count: usize) -> Duration {
    Duration::from_secs(30 + 30 * count as u64).saturating_sub(LAUNCHER_MARGIN)
}

/// What a pane the gate gave up on says in place of "Starting terminal…".
/// Waiting would not settle it, and starting a replacement is not this
/// window's decision, so it says so instead of offering either.
pub(super) fn unavailable_terminal_caption(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => {
            let reason = reason.trim_end_matches('.');
            let mut letters = reason.chars();
            let reason = letters.next().map_or(String::new(), |first| {
                first.to_uppercase().chain(letters).collect()
            });
            format!("This terminal could not reconnect. {reason}. Nothing was closed or restarted.")
        }
        None => "This terminal could not reconnect. Nothing was closed or restarted. The notice in this window has the details.".to_owned(),
    }
}

/// What this window shows for `reason`. The handshake prefix tells the
/// launcher how to treat the report and means nothing to the person reading;
/// the component's own wording goes beneath the message, as details.
fn incomplete_notice(reason: &str) -> TransientNotice {
    let text = reason
        .strip_prefix(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX)
        .unwrap_or(reason);
    let (message, details) = match text.split_once(DETAILS_LABEL) {
        Some((message, details)) => (message, Some(details.trim_end_matches('.'))),
        None => (text, None),
    };
    let mut chars = message.chars();
    let notice = TransientNotice::warning(
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect::<String>())
            .unwrap_or_default(),
    );
    match details {
        Some(details) => notice.with_details(details.to_owned()),
        None => notice,
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

#[cfg(test)]
mod attachment_tests;
