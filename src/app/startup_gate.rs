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

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    saving: bool,
    reported: bool,
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
        // A native window opens independently of the observer/SeedView task.
        // Wait for that actual admission boundary before judging saved chats
        // or declaring a formerly shared workspace recovered.
        let admission = !self.shared_views_ready_for_startup();
        let mut panes = Vec::new();
        let mut failures = Vec::new();
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
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface))
                        if surface.terminal_id() == session.id
                            && session.continuity.live_proof().is_some_and(|proof| {
                                proof.is_complete() && surface.host_proof().as_ref() == Some(&proof)
                            })
                            && !surface.snapshot().detached => {}
                    Some(CardRuntime::Loading) | None => panes.push(card.id),
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
            if self.chat_awaiting_admission(card.id) && (admission || !panes.is_empty()) {
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
                    Err(_) => failures.push(card.id),
                },
                Some(CardRuntime::Loading) | None => panes.push(card.id),
                _ => failures.push(card.id),
            }
        }
        if admission || !panes.is_empty() {
            return Ok(Some(Pending { admission, panes }));
        }
        if !failures.is_empty() {
            let them = match failures.len() {
                1 => "it",
                _ => "them",
            };
            return Err(format!(
                "recovery-incomplete: {} could not reconnect: {}. Open {them} to see why and retry. Nothing was closed, and this window remains available.",
                saved_panes(failures.len()),
                self.startup_pane_names(&failures),
            ));
        }
        Ok(None)
    }

    /// Qualification attaches retained owners without waking their offscreen
    /// rendering surface or replaying a terminal launch recipe.
    pub(super) fn qualify_startup_terminals(&mut self) -> Task<Message> {
        for card in &self.durable.cards {
            if card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session) if !terminal_ended(session))
                && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                && self.saved_terminal_current(card.id).is_none()
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
        self.startup_gate.retire_inspections();
        self.startup_gate.warned = false;
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
                Task::batch([poll(), self.inspect_daemon_startup()])
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
        let count = self.startup_conversation_count();
        let timeout = Duration::from_secs(30 + 30 * count as u64);
        now.duration_since(self.startup_gate.opened.unwrap()) >= timeout
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

    fn reconnecting_reason(&self, pending: &Pending) -> String {
        if pending.panes.is_empty() {
            return "recovery-incomplete: this window's shared workspace did not finish loading in time. The window and its saved panes remain available. The update has not been verified.".to_owned();
        }
        format!(
            "recovery-incomplete: {} did not reconnect in time: {}. They stay in this window, which remains available. The update has not been verified.",
            saved_panes(pending.panes.len()),
            self.startup_pane_names(&pending.panes),
        )
    }

    fn stalled_reason(&self, pending: &Pending) -> String {
        if pending.panes.is_empty() {
            return format!(
                "recovery-incomplete: this window's shared workspace made no progress loading for {} seconds. The window and its saved panes remain available. The update has not been verified.",
                NO_PROGRESS.as_secs()
            );
        }
        format!(
            "recovery-incomplete: {} made no progress reconnecting for {} seconds: {}. They stay in this window, which remains available. The update has not been verified.",
            saved_panes(pending.panes.len()),
            NO_PROGRESS.as_secs(),
            self.startup_pane_names(&pending.panes),
        )
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

    fn startup_connected_owners(&self) -> Vec<(CardId, crate::model::HostSessionProof)> {
        self.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(|card| {
                let proof = match (&card.kind, self.cards.get(&card.id)) {
                    (CardKind::Terminal(session), Some(CardRuntime::Terminal(surface)))
                        if !terminal_ended(session)
                            && surface.terminal_id() == session.id
                            && !surface.snapshot().detached =>
                    {
                        surface.host_proof().filter(|proof| proof.is_complete())
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
        let notice = TransientNotice::warning(notice_text(&reason));
        self.report_startup_failure(reason, notice)
    }

    /// `reason` answers the launcher; `notice` is what this window shows.
    fn report_startup_failure(&mut self, reason: String, notice: TransientNotice) -> Task<Message> {
        self.startup_gate.retire_inspections();
        self.startup_gate.reported = true;
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
        if let Err(reason) = &result {
            self.status = Some(TransientNotice::warning(notice_text(reason)));
        }
        report(result)
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
const RETAINED_NOTICE: &str = "Saved sessions could not be fully restored after the update. Their original processes are still running and nothing was closed or deleted. Until the terminal service restarts, it cannot start new terminals or chats or accept input in existing ones. The update has not been verified.";

/// The handshake's prefix tells the launcher to keep the window open; the
/// person reading the notice needs only the sentence after it.
fn notice_text(reason: &str) -> String {
    let text = reason
        .strip_prefix(instance::UI_LAUNCH_RECOVERY_INCOMPLETE_PREFIX)
        .unwrap_or(reason);
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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
mod tests;
