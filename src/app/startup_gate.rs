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
    pending: Pending,
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
                let current =
                    self.startup_gate.daemon.as_ref().is_none_or(|observed| {
                        self.startup_observation_current(observed, &pending)
                    });
                let gate = &mut self.startup_gate;
                if !current {
                    gate.daemon = None;
                }
                if gate.pending.as_ref() != Some(&pending) {
                    gate.progressed = Some(now);
                    gate.pending = Some(pending);
                }
                // The inventory budget remains an absolute cap, even while the
                // daemon reports that it is still restoring.
                let count = self.startup_conversation_count();
                let timeout = Duration::from_secs(30 + 30 * count as u64);
                if now.duration_since(self.startup_gate.opened.unwrap()) >= timeout {
                    return self.fail_startup_gate("recovery-incomplete: saved conversations are still reconnecting. The window remains available; open the affected chats to inspect their recovery controls. The update has not been verified.".to_owned());
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
                    let reason = stalled_reason(gate.pending.as_ref().unwrap());
                    return self.fail_startup_gate(reason);
                }
                let poll = cosmic::task::future(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Message::VerifyStartupPanes
                });
                Task::batch([poll, self.inspect_daemon_startup()])
            }
            Ok(None) => {
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
        let (Some(opened), Some(pending), Some(ticket)) = (
            gate.opened,
            gate.pending.clone(),
            gate.next_inspection.checked_add(1),
        ) else {
            return None;
        };
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
            Ok(Some(pending)) if pending == inspection.context.pending => {}
            Ok(_) => return self.verify_startup_gate(),
        }
        let now = Instant::now();
        self.startup_gate.inspected = Some(now);
        let observed = match inspection.outcome {
            Ok(daemon) => {
                let observed = Observed {
                    context: inspection.context,
                    daemon,
                };
                if !self.startup_observation_current(&observed, &observed.context.pending) {
                    return Task::none();
                }
                Some(observed)
            }
            Err(error) => {
                tracing::debug!(%error, "daemon startup state is unavailable");
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
        self.fail_startup_gate(reason)
    }

    fn startup_observation_current(&self, observed: &Observed, pending: &Pending) -> bool {
        if self.startup_gate.opened != Some(observed.context.opened)
            || &observed.context.pending != pending
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
        self.startup_gate.retire_inspections();
        self.startup_gate.reported = true;
        self.status = Some(TransientNotice::warning(reason.clone()));
        report(Err(reason))
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

/// The daemon kept its original owners after a failed restore. That does not
/// resolve by waiting, so the window says so at once and stays available.
fn retained_reason(detail: &str) -> String {
    let mut detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.len() > MAX_DAEMON_DETAIL_BYTES {
        let end = (0..=MAX_DAEMON_DETAIL_BYTES)
            .rev()
            .find(|&end| detail.is_char_boundary(end))
            .unwrap_or(0);
        detail.truncate(end);
        detail.push('…');
    }
    let detail = detail.trim_end_matches('.');
    format!(
        "recovery-incomplete: the terminal service could not finish restoring saved sessions and kept their original processes: {detail}. Saved conversations cannot reconnect while it does; open the affected panes to use their recovery controls. Their drafts and recovery evidence are retained; this window remains available. The update has not been verified."
    )
}

fn stalled_reason(pending: &Pending) -> String {
    if pending.panes.is_empty() {
        return format!(
            "recovery-incomplete: this window's shared workspace made no progress loading for {} seconds. The window remains available; its saved panes are retained. The update has not been verified.",
            NO_PROGRESS.as_secs()
        );
    }
    format!(
        "recovery-incomplete: {} saved panes made no progress reconnecting for {} seconds (panes {}). The window remains available; open the affected panes to inspect their recovery controls. The update has not been verified.",
        pending.panes.len(),
        NO_PROGRESS.as_secs(),
        pending
            .panes
            .iter()
            .take(12)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
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
