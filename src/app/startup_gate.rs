//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;
use crate::terminal::ipc::StartupState;

/// Every saved pane's own attach is bounded: a native chat's attach gives up
/// on a reply after 70 s (two 30 s holder startup exchanges plus slack), and a
/// terminal's control requests far sooner. While the daemon is not restoring,
/// no pane settling for longer than that means nothing is still working.
const NO_PROGRESS: Duration = Duration::from_secs(90);
/// How often the gate asks the daemon about its startup recovery.
const INSPECT_EVERY: Duration = Duration::from_secs(1);
/// How long a window whose panes have all settled waits for the answer to its
/// first settled-phase inspection before saving without one. A local worker
/// gives up within two 5 s control IO bounds, the handshake and the request;
/// the rest allows for a blocking pool that is busy just after an update.
const SETTLED_ANSWER_BOUND: Duration = Duration::from_secs(15);
/// Most panes a notice names; the rest are counted.
const NAMED_PANES: usize = 8;
/// Longest pane title a notice repeats, in characters.
const MAX_PANE_TITLE_CHARS: usize = 60;
/// The launcher reads one handshake line of at most 4 KiB; the daemon's
/// reason must leave room for the rest of the sentence.
const MAX_DAEMON_DETAIL_BYTES: usize = 1024;

mod retries;

/// The launcher's deadline starts when it spawns this process, and the gate's
/// budget when the window opens. This covers the daemon handshake and state
/// loading in between, so the launcher's generic timeout does not replace the
/// report naming what is stuck. Everything spent here is taken from a daemon's
/// restore, including for a window with no panes of its own, and running past
/// it only makes the launcher's report less specific.
const LAUNCHER_MARGIN: Duration = Duration::from_secs(5);
/// The longest report sent to the launcher. Its limit on one line is 4 KiB
/// (`MAX_UI_LAUNCH_HANDSHAKE_BYTES`), past which it closes the window; this
/// leaves room for the line's prefix.
const MAX_REPORT_BYTES: usize = crate::control::MAX_UI_LAUNCH_HANDSHAKE_BYTES - 128;
const DETAILS_LABEL: &str = " Details: ";

#[derive(Default)]
pub(super) struct State {
    pub(super) initial_panes: startup_restore::InitialPanes,
    opened: Option<Instant>,
    /// Fixed from the inventory announced to the launcher, which fixes its own
    /// deadline from that announcement once.
    budget: Option<Duration>,
    saving: bool,
    reported: bool,
    /// The daemon kept its original owners, so no pane reconnects until the
    /// terminal service restarts.
    retained: bool,
    /// What this window's report said, so a failed update's notice that only
    /// repeats it is not shown twice.
    reported_notice: Option<String>,
    /// Terminals the gate is waiting on without anything settling them.
    pub(super) retries: retries::Retries,
    /// Since when a view this window shows has not been admitted.
    views_unready: Option<Instant>,
    /// What the gate last waited for, and when that or the daemon last moved.
    pending: Option<Pending>,
    progressed: Option<Instant>,
    /// Since when every pane has settled while the gate waits for the daemon,
    /// and when it first asked the daemon in that phase.
    settled: Option<Instant>,
    asked: Option<Instant>,
    /// The latest inspection in this phase could not reach the daemon.
    unavailable: bool,
    /// This phase accepted an answer, even one since dropped because its
    /// owners changed; the daemon has not been silent.
    answered: bool,
    /// An answer in this phase came from a reachable daemon, but not for
    /// exactly this window's connected owners; only a current answer clears it.
    mismatched: bool,
    /// An unreachable daemon is logged once per opening.
    warned: bool,
    /// A successful answer remains bound to its waiting phase and exact owner.
    daemon: Option<Observed>,
    inspecting: Option<InspectionContext>,
    /// An in-flight worker retains occupancy even after its result is retired.
    outstanding: Option<u64>,
    next_inspection: u64,
    inspected: Option<Instant>,
    #[cfg(test)]
    fixture_endpoint: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InspectionContext {
    ticket: u64,
    opened: Instant,
    /// `None` once every pane has settled.
    pending: Option<Pending>,
    owners: Vec<(CardId, crate::model::HostSessionProof)>,
}

#[derive(Clone, Debug)]
struct Observation {
    socket: PathBuf,
    proof: crate::model::HostSessionProof,
    state: StartupState,
}

#[derive(Clone, Debug)]
struct Observed {
    context: InspectionContext,
    daemon: Observation,
}

#[derive(Clone, Debug)]
pub(super) struct Inspection {
    context: InspectionContext,
    outcome: Result<Observation, String>,
}

/// What one check of the saved panes found.
struct Scan {
    admission: bool,
    panes: Vec<CardId>,
    failures: Vec<CardId>,
    out_of_step: Vec<CardId>,
    reasons: Vec<String>,
    views_stalled: Option<&'static str>,
    registry_unavailable: bool,
}

/// What a waiting gate is still waiting for. Any change is progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pending {
    admission: bool,
    panes: Vec<CardId>,
}

impl State {
    fn retire_inspections(&mut self) {
        self.inspecting = None;
        self.daemon = None;
        self.pending = None;
        self.settled = None;
        self.asked = None;
        self.unavailable = false;
        self.answered = false;
        self.mismatched = false;
    }

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

    pub(super) fn reported_notice(&self) -> Option<&str> {
        self.reported_notice.as_deref()
    }

    /// Whether the gate accepted a daemon answer, is saving, and has reported.
    #[cfg(test)]
    pub(in crate::app) fn fixture_verdict(&self) -> (bool, bool, bool) {
        (self.daemon.is_some(), self.saving, self.reported)
    }

    /// Lets the next check ask the daemon again, as one whose request has been
    /// answered and whose interval has passed.
    #[cfg(test)]
    pub(in crate::app) fn fixture_ask_again(&mut self) {
        self.inspected = None;
        self.outstanding = None;
    }

    #[cfg(test)]
    pub(in crate::app) fn fixture_reported_saying(message: &str) -> Self {
        Self {
            reported_notice: Some(message.to_owned()),
            ..Self::fixture_reported()
        }
    }

    #[cfg(test)]
    pub(super) fn fixture_opened() -> Self {
        Self {
            opened: Some(Instant::now()),
            fixture_endpoint: Some(PathBuf::from("/fixture/startup-inspection.sock")),
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
        self.startup_pending().map(|pending| pending.is_none())
    }

    /// What still keeps the gate waiting, or `None` once every saved pane has
    /// recovered.
    pub(super) fn startup_pending(&self) -> Result<Option<Pending>, String> {
        let scan = self.startup_scan();
        if scan.admission || !scan.panes.is_empty() {
            return Ok(Some(Pending {
                admission: scan.admission,
                panes: scan.panes,
            }));
        }
        if !scan.out_of_step.is_empty() || !scan.failures.is_empty() {
            return Err(self.startup_incomplete(
                &scan.failures,
                &scan.out_of_step,
                scan.reasons.first(),
            ));
        }
        if let Some(hold) = scan.views_stalled {
            return Err(views_incomplete(hold));
        }
        if scan.registry_unavailable {
            return Err(format!(
                "recovery-incomplete: saved panes reconnected, but the shared workspace could not be restored because {}. This window remains available with its own saved layout.",
                crate::workspace_model::REGISTRY_UNAVAILABLE
            ));
        }
        Ok(None)
    }

    fn startup_scan(&self) -> Scan {
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
        let admission = views_pending && views_stalled.is_none();
        let mut panes = Vec::new();
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
                        None => panes.push(card.id),
                    }
                    continue;
                }
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface)) => {
                        match terminal_surface_ready(session, surface) {
                            Ok(true) => {}
                            Ok(false) => panes.push(card.id),
                            // Its record can still follow the registry to
                            // the attachment, so it is given the window.
                            Err(Unready::Mismatch { .. }) => {
                                match self.startup_attach_exhausted(card.id) {
                                    Some(reason) => {
                                        out_of_step.push(card.id);
                                        reasons.push(reason.to_owned());
                                    }
                                    None => panes.push(card.id),
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
                            panes.push(card.id);
                        }
                    }
                    Some(CardRuntime::Loading) | None => panes.push(card.id),
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
            if self.chat_awaiting_admission(card.id) && (views_pending || !panes.is_empty()) {
                if let Some(hold) = views_stalled {
                    failures.push(card.id);
                    reasons.push(hold.to_owned());
                }
                continue;
            }
            if self.chat_start_pending(card.id) {
                panes.push(card.id);
                continue;
            }
            match self.cards.get(&card.id) {
                Some(CardRuntime::Chat(runtime)) => match runtime.restored_pane_ready() {
                    Ok(true) => {}
                    Ok(false) => panes.push(card.id),
                    Err(reason) => {
                        failures.push(card.id);
                        reasons.push(reason);
                    }
                },
                Some(CardRuntime::Loading) | None => panes.push(card.id),
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
        Scan {
            admission,
            panes,
            failures,
            out_of_step,
            reasons,
            views_stalled,
            registry_unavailable,
        }
    }

    /// The report for panes that will not settle by waiting, named as their
    /// tabs are, with the first reason any of them gave.
    fn startup_incomplete(
        &self,
        failures: &[CardId],
        out_of_step: &[CardId],
        reason: Option<&String>,
    ) -> String {
        let sentences = self.startup_unsettled_sentences(failures, out_of_step);
        let detail = startup_details(reason);
        format!(
            "recovery-incomplete: {sentences} Nothing was closed or restarted, and this window remains available.{detail}"
        )
    }

    /// What is true of each kind of unsettled pane, since opening one does
    /// different things: a failed pane offers a retry, a held one says what it
    /// waits for, and an out-of-step one is already working.
    fn startup_unsettled_sentences(&self, failures: &[CardId], out_of_step: &[CardId]) -> String {
        let (needs_registry, failed): (Vec<_>, Vec<_>) = failures
            .iter()
            .copied()
            .partition(|card| self.startup_pane_needs_registry(*card));
        let mut sentences = Vec::new();
        if !failed.is_empty() {
            let them = pronoun(failed.len());
            sentences.push(format!(
                "{} could not reconnect: {}. Open {them} to see why and retry.",
                saved_panes(failed.len()),
                self.startup_pane_names(&failed),
            ));
        }
        if self.shared_views.registry_unavailable {
            if !needs_registry.is_empty() {
                sentences.push(format!(
                    "{} cannot reopen while the shared workspace is unavailable: {}.",
                    saved_panes(needs_registry.len()),
                    self.startup_pane_names(&needs_registry),
                ));
            }
        } else {
            sentences.extend(self.startup_held_sentences(&needs_registry));
        }
        if !out_of_step.is_empty() {
            let them = pronoun(out_of_step.len());
            sentences.push(format!(
                "{} reconnected and can be used in this window, but the shared workspace lists {them} under a different terminal service: {}.",
                saved_panes(out_of_step.len()),
                self.startup_pane_names(out_of_step),
            ));
        }
        if self.shared_views.registry_unavailable {
            sentences.push(format!(
                "The shared workspace could not be restored because {}.",
                crate::workspace_model::REGISTRY_UNAVAILABLE
            ));
        }
        sentences.join(" ")
    }

    /// What held panes wait for and what happens when it comes. Nothing is
    /// asked of the reader: projecting a view admits its chats whether or not
    /// they are open, and releases its terminals to attach once opened.
    fn startup_held_sentences(&self, held: &[CardId]) -> Vec<String> {
        let (chats, terminals): (Vec<_>, Vec<_>) = held
            .iter()
            .copied()
            .partition(|card| self.chat_awaiting_admission(*card));
        let mut sentences = Vec::new();
        if !terminals.is_empty() {
            let reconnects = match terminals.len() {
                1 => "it reconnects when you open it",
                _ => "each reconnects when you open it",
            };
            sentences.push(format!(
                "{} {} waiting for the shared workspace: {}. Nothing is needed from you now; once the workspace is ready, {reconnects}.",
                saved_panes(terminals.len()),
                is_or_are(terminals.len()),
                self.startup_pane_names(&terminals),
            ));
        }
        if !chats.is_empty() {
            let (reopens, them) = match chats.len() {
                1 => ("It reopens on its own", "it"),
                _ => ("They reopen on their own", "them"),
            };
            sentences.push(format!(
                "{} {} waiting for the shared workspace: {}. {reopens} once the workspace is ready; until then, opening {them} shows any unsent text.",
                saved_panes(chats.len()),
                is_or_are(chats.len()),
                self.startup_pane_names(&chats),
            ));
        }
        sentences
    }

    /// Whether `card` can do nothing until the registry answers: a shared chat
    /// it has not admitted, or a terminal whose start is held for it with no
    /// attempt under way.
    fn startup_pane_needs_registry(&self, card: CardId) -> bool {
        self.chat_awaiting_admission(card)
            || (matches!(self.cards.get(&card), Some(CardRuntime::Loading) | None)
                && !self.terminal_start_in_flight(card)
                && !self.terminal_retry_pending(card)
                && self.terminal_start_held(card, false))
    }

    /// Whether `card` is waiting for a registry that may yet answer, rather
    /// than one the daemon has said it cannot give.
    pub(in crate::app) fn startup_pane_held(&self, card: CardId) -> bool {
        !self.shared_views.registry_unavailable && self.startup_pane_needs_registry(card)
    }

    /// The panes still waiting when the time ran out: `lapsed` for those with
    /// something under way, and what a held pane waits for otherwise.
    fn startup_waiting_sentences(&self, panes: &[CardId], lapsed: String) -> String {
        let (held, attempting): (Vec<_>, Vec<_>) = panes
            .iter()
            .copied()
            .partition(|card| self.startup_pane_held(*card));
        let mut sentences = Vec::new();
        if !attempting.is_empty() {
            sentences.push(format!(
                "{} {lapsed}: {}. {}.",
                saved_panes(attempting.len()),
                self.startup_pane_names(&attempting),
                they_stay(attempting.len()),
            ));
        }
        sentences.extend(self.startup_held_sentences(&held));
        if attempting.is_empty() {
            sentences.push("This window remains available.".to_owned());
        }
        sentences.join(" ")
    }

    /// Why the gate was still waiting when its time ran out, as a report's
    /// last sentence: what holds the view, or else the first pane's reason.
    fn startup_waiting_details(&self, pending: &Pending) -> String {
        let hold = self
            .shared_views_startup_hold()
            .filter(|_| !self.shared_views.registry_unavailable)
            .map(str::to_owned);
        let reason = hold.or_else(|| {
            pending
                .panes
                .first()
                .map(|card| self.startup_waiting_reason(*card))
        });
        startup_details(reason.as_ref())
    }

    /// Panes that had already stopped waiting when the time ran out, as a
    /// sentence following a report about the ones still waiting.
    fn startup_settled_alongside(&self) -> String {
        let scan = self.startup_scan();
        if scan.failures.is_empty() && scan.out_of_step.is_empty() {
            return String::new();
        }
        format!(
            " {}",
            self.startup_unsettled_sentences(&scan.failures, &scan.out_of_step)
        )
    }

    fn startup_waiting_reason(&self, card: CardId) -> String {
        let Some(CardKind::Terminal(session)) = self.card_state(card).map(|state| &state.kind)
        else {
            return "it was still reconnecting".to_owned();
        };
        match self.cards.get(&card) {
            Some(CardRuntime::Terminal(surface)) => {
                match terminal_surface_ready(session, surface) {
                    Ok(true) | Ok(false) => "its connection was still attaching".to_owned(),
                    Err(Unready::Failed(reason)) => reason,
                    Err(Unready::Mismatch { .. }) => self
                        .startup_terminal_latest(card)
                        .unwrap_or("its record names a different terminal service")
                        .to_owned(),
                }
            }
            _ if self.terminal_start_in_flight(card) => "it was still reconnecting".to_owned(),
            _ => self
                .startup_terminal_latest(card)
                .unwrap_or(if self.saved_terminal_current(card).is_some() {
                    "its saved terminal was still being checked"
                } else {
                    "it was still waiting to reconnect"
                })
                .to_owned(),
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
        let mut views_ready = None;
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
                    if let Some(hold) = views_hold
                        .or_else(|| self.restored_attach_hold(card.id, session, &mut views_ready))
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
            instance::finish_window_restore();
            return report(Ok(()));
        }
        self.startup_gate.retire_inspections();
        self.startup_gate.warned = false;
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
        match self.startup_pending() {
            Err(reason) => self.fail_startup_gate(reason),
            Ok(Some(pending)) => {
                let now = Instant::now();
                self.retire_stale_observation(Some(&pending));
                let gate = &mut self.startup_gate;
                let resumed = gate.settled.take().is_some();
                if resumed || gate.pending.as_ref() != Some(&pending) {
                    gate.progressed = Some(now);
                    // An answer to another set, even one this set returns to,
                    // describes owners and refusals that have since changed.
                    gate.inspecting = None;
                    gate.pending = Some(pending);
                }
                // The inventory budget remains an absolute cap, even while the
                // daemon reports that it is still restoring.
                if self.startup_budget_spent(now) {
                    let reason =
                        self.reconnecting_reason(self.startup_gate.pending.as_ref().unwrap());
                    return self.fail_startup_gate(reason);
                }
                let gate = &self.startup_gate;
                if gate
                    .daemon
                    .as_ref()
                    .is_some_and(|observed| observed.daemon.state == StartupState::Ready)
                    && gate
                        .progressed
                        .is_some_and(|progressed| now.duration_since(progressed) >= NO_PROGRESS)
                {
                    let reason = self.stalled_reason(gate.pending.as_ref().unwrap());
                    return self.fail_startup_gate(reason);
                }
                let attach = self.qualify_startup_terminals();
                Task::batch([attach, poll(), self.inspect_daemon_startup()])
            }
            Ok(None) => {
                let now = Instant::now();
                match self.settled_verdict(now) {
                    Settled::Save => {}
                    Settled::Wait => return Task::batch([poll(), self.inspect_daemon_startup()]),
                    Settled::Report(reason) => return self.fail_startup_gate(reason),
                }
                self.startup_gate.retire_inspections();
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

    /// A terminal can attach while the daemon keeps its original owners, so
    /// panes that all settled do not prove the update recovered them: the
    /// gate saves once the daemon reports Ready for exactly these owners, or
    /// cannot be reached before it has answered at all. A Retained answer was
    /// reported when it arrived.
    fn settled_verdict(&mut self, now: Instant) -> Settled {
        self.retire_stale_observation(None);
        let gate = &mut self.startup_gate;
        if gate.settled.is_none() {
            // Answers, failed ones included, belong to the phase that asked.
            gate.settled = Some(now);
            gate.asked = None;
            gate.unavailable = false;
            gate.answered = false;
            gate.mismatched = false;
            gate.inspecting = None;
            gate.pending = None;
            gate.progressed = Some(now);
        }
        let gate = &self.startup_gate;
        match gate.daemon.as_ref().map(|observed| &observed.daemon.state) {
            Some(StartupState::Ready) => Settled::Save,
            // Its restore can still end Retained; the inventory cap bounds it.
            Some(StartupState::Restoring { awaiting }) => {
                if self.startup_budget_spent(now) {
                    Settled::Report(restoring_reason(*awaiting))
                } else {
                    Settled::Wait
                }
            }
            // Retained is reported, and the gate retired, when it arrives.
            Some(StartupState::Retained { .. }) => Settled::Wait,
            // An answer this phase gave was dropped when its owners changed,
            // or the daemon that answered is not these owners'. Neither is
            // silence, and a later failed inspection does not undo it, so
            // the gate waits for a current answer.
            None if gate.answered || gate.mismatched => {
                if self.startup_budget_spent(now) {
                    Settled::Report(unconfirmed_reason())
                } else {
                    Settled::Wait
                }
            }
            None if gate.unavailable => Settled::Save,
            None => {
                let answer_overdue = gate
                    .asked
                    .is_some_and(|asked| now.duration_since(asked) >= SETTLED_ANSWER_BOUND);
                // An earlier phase's worker can hold the one slot for its own
                // bound before this phase may ask.
                let past_backstop = gate
                    .settled
                    .is_some_and(|settled| now.duration_since(settled) >= SETTLED_ANSWER_BOUND * 2);
                if answer_overdue || past_backstop {
                    Settled::Save
                } else {
                    Settled::Wait
                }
            }
        }
    }

    fn startup_budget_spent(&self, now: Instant) -> bool {
        let budget = self
            .startup_gate
            .budget
            .unwrap_or_else(|| launcher_budget(self.startup_conversation_count()));
        now.duration_since(self.startup_gate.opened.unwrap()) >= budget
    }

    /// The panes named as their tabs are, on one bounded line.
    fn startup_pane_names(&self, panes: &[CardId]) -> String {
        let mut names = panes
            .iter()
            .take(NAMED_PANES)
            .map(|card| {
                let title = self
                    .card_state(*card)
                    .map_or("Untitled", |state| self.card_display_title(state));
                format!("\u{201c}{}\u{201d}", pane_title(title))
            })
            .collect::<Vec<_>>()
            .join(", ");
        if panes.len() > NAMED_PANES {
            names.push_str(&format!(" and {} more", panes.len() - NAMED_PANES));
        }
        names
    }

    pub(super) fn reconnecting_reason(&self, pending: &Pending) -> String {
        let detail = self.startup_waiting_details(pending);
        let settled = self.startup_settled_alongside();
        if pending.panes.is_empty() {
            return format!(
                "recovery-incomplete: this window's shared workspace did not finish loading in time. The window and its saved panes remain available.{settled} The update has not been verified.{detail}"
            );
        }
        let waiting =
            self.startup_waiting_sentences(&pending.panes, "did not reconnect in time".into());
        format!("recovery-incomplete: {waiting}{settled} The update has not been verified.{detail}")
    }

    fn stalled_reason(&self, pending: &Pending) -> String {
        let detail = self.startup_waiting_details(pending);
        let settled = self.startup_settled_alongside();
        if pending.panes.is_empty() {
            return format!(
                "recovery-incomplete: this window's shared workspace made no progress loading for {} seconds. The window and its saved panes remain available.{settled} The update has not been verified.{detail}",
                NO_PROGRESS.as_secs()
            );
        }
        let waiting = self.startup_waiting_sentences(
            &pending.panes,
            format!(
                "made no progress reconnecting for {} seconds",
                NO_PROGRESS.as_secs()
            ),
        );
        format!("recovery-incomplete: {waiting}{settled} The update has not been verified.{detail}")
    }

    fn retire_stale_observation(&mut self, pending: Option<&Pending>) {
        let current = self
            .startup_gate
            .daemon
            .as_ref()
            .is_none_or(|observed| self.startup_observation_current(observed, pending));
        if !current {
            self.startup_gate.daemon = None;
        }
    }

    /// Asks the daemon off the UI thread, at most one request at a time. A
    /// refused pane looks the same whether its daemon is still restoring or
    /// has already given up; only the daemon can tell them apart.
    fn inspect_daemon_startup(&mut self) -> Task<Message> {
        let Some(context) = self.reserve_startup_inspection() else {
            return Task::none();
        };
        inspection_task(context, crate::device::is_remote(), || {
            // Never start a daemon merely to ask it; the panes' own
            // attaches decide whether this window needs one.
            let socket =
                crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
            let mut client = crate::control::ControlClient::connect(&socket)
                .map_err(|error| error.to_string())?;
            let proof = client.proof().clone();
            let state = client.startup_state().map_err(|error| error.to_string())?;
            Ok(Observation {
                socket,
                proof,
                state,
            })
        })
    }

    fn reserve_startup_inspection(&mut self) -> Option<InspectionContext> {
        let owners = self.startup_connected_owners();
        let gate = &mut self.startup_gate;
        if gate.outstanding.is_some()
            || gate
                .inspected
                .is_some_and(|inspected| inspected.elapsed() < INSPECT_EVERY)
        {
            return None;
        }
        let (Some(opened), Some(ticket)) = (gate.opened, gate.next_inspection.checked_add(1))
        else {
            return None;
        };
        // A waiting gate records what it waits for; a settled one has nothing.
        if gate.pending.is_none() && gate.settled.is_none() {
            return None;
        }
        if gate.settled.is_some() && gate.asked.is_none() {
            gate.asked = Some(Instant::now());
        }
        let pending = gate.pending.clone();
        let context = InspectionContext {
            ticket,
            opened,
            pending,
            owners,
        };
        gate.next_inspection = ticket;
        gate.outstanding = Some(ticket);
        gate.inspecting = Some(context.clone());
        gate.inspected = Some(Instant::now());
        Some(context)
    }

    pub(super) fn startup_state_inspected(&mut self, inspection: Inspection) -> Task<Message> {
        if self.startup_gate.outstanding != Some(inspection.context.ticket) {
            return Task::none();
        }
        self.startup_gate.outstanding = None;
        if self.startup_gate.inspecting.as_ref() != Some(&inspection.context) {
            return Task::none();
        }
        self.startup_gate.inspecting = None;
        if self.startup_gate.reported
            || self.startup_gate.saving
            || self.startup_gate.opened != Some(inspection.context.opened)
        {
            return Task::none();
        }
        // Its restoring is why panes are refused, so that time does not count
        // against them, however late this answer found them already reported.
        if matches!(&inspection.outcome, Ok(observation)
            if matches!(observation.state, StartupState::Restoring { .. }))
        {
            self.startup_gate.retries.restart_windows();
            self.startup_gate.views_unready = None;
        }
        match self.startup_pending() {
            Err(reason) => return self.fail_startup_gate(reason),
            Ok(pending) if pending == inspection.context.pending => {}
            // Every inspection is issued beside the gate's own rescheduled
            // check, which records this move, settled panes included; starting
            // another here would add a poll loop for each such inspection.
            Ok(_) => return Task::none(),
        }
        let now = Instant::now();
        self.startup_gate.inspected = Some(now);
        let observed = match inspection.outcome {
            Ok(daemon) => {
                let observed = Observed {
                    context: inspection.context,
                    daemon,
                };
                // Whatever it answered, the daemon was reachable.
                self.startup_gate.unavailable = false;
                if !self.startup_observation_current(&observed, observed.context.pending.as_ref()) {
                    self.startup_gate.mismatched = true;
                    return Task::none();
                }
                self.startup_gate.mismatched = false;
                self.startup_gate.answered = true;
                Some(observed)
            }
            Err(error) => {
                if !self.startup_gate.warned {
                    tracing::warn!(%error, "daemon startup state is unavailable");
                    self.startup_gate.warned = true;
                }
                self.startup_gate.unavailable = true;
                None
            }
        };
        // Only time the daemon reports as ready counts against the panes:
        // while it restores, or cannot answer, they are not expected to move,
        // and its finishing a restore is progress in its own right.
        let gate = &mut self.startup_gate;
        let same_ready_owner = matches!((&gate.daemon, &observed), (Some(old), Some(new))
            if new.daemon.state == StartupState::Ready
                && old.daemon.state == new.daemon.state
                && old.daemon.proof == new.daemon.proof
                && old.daemon.socket == new.daemon.socket);
        if !same_ready_owner {
            gate.progressed = Some(now);
        }
        gate.daemon = observed;
        let Some(Observed {
            daemon:
                Observation {
                    state: StartupState::Retained { detail },
                    ..
                },
            ..
        }) = &gate.daemon
        else {
            return Task::none();
        };
        let reason = retained_reason(detail);
        let notice = TransientNotice::warning(RETAINED_NOTICE).with_details(bounded_detail(detail));
        self.startup_gate.retained = true;
        self.report_startup_failure(reason, notice)
    }

    fn startup_observation_current(&self, observed: &Observed, pending: Option<&Pending>) -> bool {
        if self.startup_gate.opened != Some(observed.context.opened)
            || observed.context.pending.as_ref() != pending
            || !observed.daemon.proof.is_complete()
            || self.startup_inspection_endpoint().as_ref() != Some(&observed.daemon.socket)
        {
            return false;
        }
        let owners = self.startup_connected_owners();
        observed.context.owners == owners
            && owners
                .iter()
                .all(|(_, proof)| proof == &observed.daemon.proof)
    }

    pub(super) fn startup_connected_owners(&self) -> Vec<(CardId, crate::model::HostSessionProof)> {
        self.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(|card| {
                let proof = match (&card.kind, self.cards.get(&card.id)) {
                    // A hibernated surface shows a detached snapshot but keeps
                    // its connection, so that says nothing about its owner.
                    (CardKind::Terminal(session), Some(CardRuntime::Terminal(surface)))
                        if !terminal_ended(session) && surface.terminal_id() == session.id =>
                    {
                        surface
                            .current_attachment()
                            .ok()
                            .flatten()
                            .filter(|proof| proof.is_complete())
                    }
                    (CardKind::Chat(_), Some(CardRuntime::Chat(runtime))) => {
                        runtime.startup_host_proof()
                    }
                    _ => None,
                };
                proof.map(|proof| (card.id, proof))
            })
            .collect()
    }

    fn startup_inspection_endpoint(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(endpoint) = &self.startup_gate.fixture_endpoint {
            // Pure gate fixtures have no ambient daemon/runtime dependency.
            // The actual inspection producer still uses the real connector.
            return Some(endpoint.clone());
        }
        if crate::device::is_remote() {
            return None;
        }
        crate::terminal::ipc::daemon_socket_path().ok()
    }

    fn fail_startup_gate(&mut self, reason: String) -> Task<Message> {
        let notice = incomplete_notice(&reason);
        self.report_startup_failure(reason, notice)
    }

    /// `reason` answers the launcher; `notice` is what this window shows.
    fn report_startup_failure(&mut self, reason: String, notice: TransientNotice) -> Task<Message> {
        self.startup_gate.retire_inspections();
        self.startup_gate.reported = true;
        instance::finish_window_restore();
        self.startup_gate.reported_notice = Some(notice.message.clone());
        self.status = Some(notice);
        report(Err(reason))
    }

    pub(super) fn startup_panes_saved(
        &mut self,
        settings: Result<SettingsSaved, String>,
        saved: Result<(), String>,
    ) -> Task<Message> {
        self.startup_gate.saving = false;
        let settings = settings.map(|settings| self.acknowledge_settings_save(settings));
        let mut result = settings.and(saved).map_err(|error| format!("recovery-incomplete: saved panes reconnected, but the recovered workspace could not be saved: {error}. This window remains available; retry saving before updating again."));
        if result.is_ok() {
            match self.startup_pane_check() {
                Ok(true) => {}
                Ok(false) => return self.verify_startup_gate(),
                Err(reason) => result = Err(reason),
            }
        }
        self.startup_gate.reported = true;
        instance::finish_window_restore();
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
        "recovery-incomplete: this window's shared workspace did not finish restoring: {hold}. Nothing was closed or restarted, and this window stays available. The update has not been verified."
    )
}

/// The first reason known, as it ends a report.
fn startup_details(reason: Option<&String>) -> String {
    reason
        .map(|reason| {
            format!(
                "{DETAILS_LABEL}{}.",
                bounded_detail(reason).trim_end_matches('.')
            )
        })
        .unwrap_or_default()
}

fn launcher_budget(count: usize) -> Duration {
    Duration::from_secs(30 + 30 * count as u64).saturating_sub(LAUNCHER_MARGIN)
}

/// What a pane the gate gave up on says in place of "Starting terminal…".
/// Waiting would not settle it, and starting a replacement is not this
/// window's decision, so it says so instead of offering either.
pub(super) fn unavailable_terminal_caption(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!(
            "This terminal could not reconnect. {}. Nothing was closed or restarted.",
            reason_sentence(reason)
        ),
        None => "This terminal could not reconnect. Nothing was closed or restarted. The notice in this window has the details.".to_owned(),
    }
}

/// What an opened terminal shows while its start is held for the registry.
/// Opening it is all that was needed, so releasing it attaches it here.
pub(super) fn held_terminal_caption(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!(
            "This terminal reconnects here once the shared workspace is ready. {}. Nothing was closed or restarted.",
            reason_sentence(reason)
        ),
        None => "This terminal reconnects here once the shared workspace is ready. Nothing was closed or restarted.".to_owned(),
    }
}

/// What an opened terminal shows once the daemon has kept its original owners:
/// waiting would not bring it back, so it points at the notice's steps.
const RETAINED_TERMINAL_CAPTION: &str = "This terminal cannot reconnect until the terminal service restarts. Nothing was closed or restarted. The notice in this window has the steps.";

fn reason_sentence(reason: &str) -> String {
    let reason = reason.trim_end_matches('.');
    let mut letters = reason.chars();
    letters.next().map_or(String::new(), |first| {
        first.to_uppercase().chain(letters).collect()
    })
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

/// The production producer and private fixtures share this exact worker
/// boundary. Eligibility and physical occupancy are retired independently.
fn inspection_task(
    context: InspectionContext,
    remote: bool,
    inspect: impl FnOnce() -> Result<Observation, String> + Send + 'static,
) -> Task<Message> {
    cosmic::task::future(async move {
        let outcome = tokio::task::spawn_blocking(move || inspect_existing_daemon(remote, inspect))
            .await
            .map_err(|error| error.to_string())
            .and_then(|state| state);
        Message::StartupStateInspected(Inspection { context, outcome })
    })
}

/// Ordinary remote bridges may start their selected daemon on connection.
/// Until an inspection-only route exists, keep that caller unavailable.
fn inspect_existing_daemon(
    remote: bool,
    inspect: impl FnOnce() -> Result<Observation, String>,
) -> Result<Observation, String> {
    if remote {
        return Err("Startup inspection is unavailable for this remote connection".into());
    }
    inspect()
}

/// What the window says when the daemon kept its original owners after a
/// failed restore; the daemon's own reason goes beneath it as details.
const RETAINED_NOTICE: &str = "Saved sessions could not be fully restored after the update. Their original processes are still running and nothing was closed or deleted. Until the terminal service restarts, it cannot start new terminals or chats or accept input in existing ones. To start new work, let these sessions finish or close them, then run `cagents daemon stop` and reopen cagents. The update has not been verified.";

/// How the retained report begins, in every build that has written one.
const RETAINED_LEAD: &str =
    "the terminal service could not reconnect saved sessions after the update";

/// A failed update's job as someone reads it. The updater that wrote the
/// message is the build updated from, which may list each window's handshake
/// report in turn, so this is decided here rather than where it is written.
pub(super) struct UpdateFailure {
    /// Fits a control's label.
    pub(super) summary: String,
    pub(super) notice: TransientNotice,
    /// Says what this window's own retained report already says.
    pub(super) retained: bool,
}

pub(super) fn update_failure(message: &str) -> UpdateFailure {
    let grouped = group_window_reports(message);
    if grouped.to_lowercase().contains(RETAINED_LEAD) {
        return UpdateFailure {
            summary: "saved sessions could not be fully restored".to_owned(),
            notice: TransientNotice::error(RETAINED_NOTICE).with_details(grouped),
            retained: true,
        };
    }
    let (text, details) = match grouped.split_once(DETAILS_LABEL) {
        Some((text, details)) => (text, Some(details.trim_end_matches('.'))),
        None => (grouped.as_str(), None),
    };
    let notice = TransientNotice::error(text);
    let notice = match details {
        Some(details) => notice.with_details(details),
        None => notice,
    };
    UpdateFailure {
        summary: text.trim_end_matches('.').to_owned(),
        notice,
        retained: false,
    }
}

/// Parses `primary: report; window-2: report` and reads it the way the current
/// updater writes it. Anything else, a report's own semicolons included, is
/// left alone apart from a leading handshake prefix.
fn group_window_reports(message: &str) -> String {
    let is_slot = |slot: &str| {
        slot == "primary"
            || slot
                .strip_prefix("window-")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut reports: Vec<(&str, String)> = Vec::new();
    for piece in message.split("; ") {
        match piece.split_once(": ") {
            Some((slot, report)) if is_slot(slot) => reports.push((slot, report.to_owned())),
            _ => match reports.last_mut() {
                Some((_, report)) => {
                    report.push_str("; ");
                    report.push_str(piece);
                }
                None => {
                    return message
                        .strip_prefix(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX)
                        .unwrap_or(message)
                        .to_owned();
                }
            },
        }
    }
    instance::relaunch_failure(reports.len(), &reports)
}

/// The daemon's reason on one line, bounded to fit the launcher's handshake.
fn bounded_detail(detail: &str) -> String {
    let mut detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.len() > MAX_DAEMON_DETAIL_BYTES {
        let end = (0..=MAX_DAEMON_DETAIL_BYTES)
            .rev()
            .find(|&end| detail.is_char_boundary(end))
            .unwrap_or(0);
        detail.truncate(end);
        detail.push('…');
    }
    detail
}

/// The daemon kept its original owners after a failed restore. That does not
/// resolve by waiting, so the launcher learns at once, with the daemon's
/// reason, and the window stays available.
fn retained_reason(detail: &str) -> String {
    let detail = bounded_detail(detail);
    let detail = detail.trim_end_matches('.');
    format!(
        "recovery-incomplete: the terminal service could not reconnect saved sessions after the update: {detail}. Their original processes were kept and nothing was closed or deleted; this window remains available. The update has not been verified."
    )
}

/// What a settled gate does next.
enum Settled {
    Save,
    Wait,
    Report(String),
}

/// Every pane this window shows reconnected, but the daemon's restore can
/// still keep their original owners; the update is not verified.
fn restoring_reason(awaiting: u64) -> String {
    format!(
        "recovery-incomplete: this window's saved panes reconnected, but the terminal service was still restoring {} after the update. The window remains available. The update has not been verified.",
        match awaiting {
            1 => "1 saved session".to_owned(),
            count => format!("{count} saved sessions"),
        }
    )
}

/// Every pane this window shows reconnected, but the daemon did not confirm
/// for exactly these owners that its restore had finished.
fn unconfirmed_reason() -> String {
    "recovery-incomplete: this window's saved panes reconnected, but the terminal service did not confirm that it had finished restoring saved sessions. The window remains available. The update has not been verified.".to_owned()
}

/// Where waiting panes are, and how to check on them.
fn they_stay(count: usize) -> &'static str {
    match count {
        1 => "It stays in this window, which remains available; open it to see its status",
        _ => "They stay in this window, which remains available; open them to see their status",
    }
}

fn is_or_are(count: usize) -> &'static str {
    match count {
        1 => "is",
        _ => "are",
    }
}

fn pronoun(count: usize) -> &'static str {
    match count {
        1 => "it",
        _ => "them",
    }
}

fn saved_panes(count: usize) -> String {
    match count {
        1 => "1 saved pane".to_owned(),
        count => format!("{count} saved panes"),
    }
}

/// A pane title on one line, bounded so a notice naming several still fits
/// the launcher's handshake line.
fn pane_title(title: &str) -> String {
    let mut title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((end, _)) = title.char_indices().nth(MAX_PANE_TITLE_CHARS) {
        title.truncate(end);
        title.push('\u{2026}');
    }
    title
}

/// The gate's own next check; it is the only poll loop.
fn poll() -> Task<Message> {
    cosmic::task::future(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Message::VerifyStartupPanes
    })
}

/// `reason` cut to what the launcher accepts, keeping its recovery prefix: past
/// its limit the launcher treats the window as broken and closes it.
fn bounded_report(mut reason: String) -> String {
    if reason.len() > MAX_REPORT_BYTES {
        let end = (0..=MAX_REPORT_BYTES - '\u{2026}'.len_utf8())
            .rev()
            .find(|&end| reason.is_char_boundary(end))
            .unwrap_or(0);
        reason.truncate(end);
        reason.push('\u{2026}');
    }
    reason
}

fn report(result: Result<(), String>) -> Task<Message> {
    let result = result.map_err(bounded_report);
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
#[cfg(test)]
impl Cagents {
    /// Completes the inspection the gate reserved, with the daemon answering
    /// `state` under `proof`.
    pub(in crate::app) fn fixture_answer_startup_under(
        &mut self,
        proof: crate::model::HostSessionProof,
        state: StartupState,
    ) -> Task<Message> {
        let context = self
            .startup_gate
            .inspecting
            .clone()
            .expect("the gate reserved no inspection");
        let socket = self.startup_gate.fixture_endpoint.clone().unwrap();
        self.startup_state_inspected(Inspection {
            context,
            outcome: Ok(Observation {
                socket,
                proof,
                state,
            }),
        })
    }
}

#[cfg(test)]
mod tests;
