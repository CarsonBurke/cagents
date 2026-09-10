use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    sync::{Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    model::{
        HostSessionProof, TerminalContinuity, TerminalExit, TerminalId, TerminalSession,
        TranscriptMetadata,
    },
    terminal::{RenderSnapshot, TerminalConfig, TerminalEvent, TerminalHandle, TerminalSendError},
    terminal_ipc::{
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        ShutdownCapability, ShutdownOperation, ShutdownOutcome, ShutdownPreflight,
        TerminalDescriptor, TerminalSnapshot, TerminalSpawnOptions, TerminalVisibility,
        WireTerminalEvent,
    },
};

use super::delivery::{DEFAULT_CLIENT_QUEUE_CAPACITY, DeliveryQueue};

const EVENT_REPLAY_CAPACITY: usize = 256;
const MAX_TERMINALS: usize = 1024;
const SHUTDOWN_CONFIRMATION_TTL: Duration = Duration::from_secs(60);
const MAX_SHUTDOWN_CAPABILITIES: usize = 32;

pub(crate) type ClientId = u64;
pub(crate) type SharedHost = Arc<Mutex<HostState>>;

pub(crate) struct TerminalRecord {
    pub(super) session: TerminalSession,
    pub(super) handle: TerminalHandle,
    running: bool,
    completion_observed: bool,
    last_exit: Option<TerminalExit>,
    event_sequence: u64,
    events: VecDeque<(u64, WireTerminalEvent)>,
}

#[derive(Default)]
struct OutputObservation {
    epoch: u64,
    after_sequence: u64,
    notified: bool,
}

#[derive(Default)]
struct Subscriptions {
    by_client: BTreeMap<ClientId, BTreeMap<TerminalId, TerminalVisibility>>,
    by_terminal: BTreeMap<TerminalId, BTreeMap<ClientId, TerminalVisibility>>,
    /// The exact capture last handed to each subscription.
    ///
    /// `VisualResources::capture` returns the same `Arc<RowSnapshot>` for every
    /// row it did not rebuild, so keeping the previous capture per subscriber
    /// lets the next repaint be diffed by pointer identity and sent as a row
    /// delta. Dropped on detach and disconnect so a stale base can never be
    /// used as a delta base, and so a departing client frees its rows.
    sent: BTreeMap<(ClientId, TerminalId), Arc<RenderSnapshot>>,
    output: BTreeMap<(ClientId, TerminalId), OutputObservation>,
}

impl Subscriptions {
    fn attach(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        visibility: TerminalVisibility,
    ) {
        self.by_client
            .entry(client_id)
            .or_default()
            .insert(terminal_id, visibility);
        self.by_terminal
            .entry(terminal_id)
            .or_default()
            .insert(client_id, visibility);
        self.output.entry((client_id, terminal_id)).or_default();
    }

    fn detach(&mut self, client_id: ClientId, terminal_id: TerminalId) {
        if let Some(terminals) = self.by_client.get_mut(&client_id) {
            terminals.remove(&terminal_id);
            if terminals.is_empty() {
                self.by_client.remove(&client_id);
            }
        }
        if let Some(clients) = self.by_terminal.get_mut(&terminal_id) {
            clients.remove(&client_id);
            if clients.is_empty() {
                self.by_terminal.remove(&terminal_id);
            }
        }
        self.sent.remove(&(client_id, terminal_id));
        self.output.remove(&(client_id, terminal_id));
    }

    /// Remembers the capture just sent to `client_id`, so the next repaint for
    /// that subscription can be a delta against it.
    fn record_sent(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        snapshot: Arc<RenderSnapshot>,
    ) {
        self.sent.insert((client_id, terminal_id), snapshot);
    }

    fn last_sent(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Option<&Arc<RenderSnapshot>> {
        self.sent.get(&(client_id, terminal_id))
    }

    fn disconnect(&mut self, client_id: ClientId) {
        let Some(terminals) = self.by_client.remove(&client_id) else {
            return;
        };
        self.sent
            .retain(|(sent_client, _), _| *sent_client != client_id);
        self.output.retain(|(client, _), _| *client != client_id);
        for terminal_id in terminals.keys() {
            if let Some(clients) = self.by_terminal.get_mut(terminal_id) {
                clients.remove(&client_id);
                if clients.is_empty() {
                    self.by_terminal.remove(terminal_id);
                }
            }
        }
    }

    fn attached(&self, client_id: ClientId, terminal_id: TerminalId) -> bool {
        self.by_client
            .get(&client_id)
            .is_some_and(|terminals| terminals.contains_key(&terminal_id))
    }

    fn clients(&self, terminal_id: TerminalId) -> Vec<ClientId> {
        self.by_terminal
            .get(&terminal_id)
            .map(|clients| clients.keys().copied().collect())
            .unwrap_or_default()
    }

    fn visibility(&self, terminal_id: TerminalId) -> TerminalVisibility {
        let mut effective = TerminalVisibility::Hibernated;
        let Some(clients) = self.by_terminal.get(&terminal_id) else {
            return TerminalVisibility::Hidden;
        };
        for visibility in clients.values() {
            match visibility {
                TerminalVisibility::Visible => return TerminalVisibility::Visible,
                TerminalVisibility::Hidden => effective = TerminalVisibility::Hidden,
                TerminalVisibility::Hibernated => {}
            }
        }
        effective
    }

    fn set_visibility(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        visibility: TerminalVisibility,
    ) {
        if let Some(terminals) = self.by_client.get_mut(&client_id) {
            terminals.insert(terminal_id, visibility);
        }
        if let Some(clients) = self.by_terminal.get_mut(&terminal_id) {
            clients.insert(client_id, visibility);
        }
    }
}

pub(crate) struct HostState {
    proof: HostSessionProof,
    pub(super) terminals: BTreeMap<TerminalId, TerminalRecord>,
    spawning: BTreeSet<TerminalId>,
    pub(super) clients: BTreeMap<ClientId, Arc<DeliveryQueue>>,
    subscriptions: Subscriptions,
    pub(super) shutting_down: bool,
    shutdown_requested: bool,
    pub(super) inventory_generation: u64,
    shutdown_capabilities: VecDeque<(ShutdownCapability, Instant)>,
    pub(super) chats: BTreeMap<TerminalId, super::chat::Record>,
    pub(super) chat_spawning: BTreeMap<TerminalId, crate::chat::Launch>,
    pub(crate) agent_observer: Arc<crate::agent::observe::Observer>,
}

impl HostState {
    pub(crate) fn new(proof: HostSessionProof) -> Self {
        Self {
            proof,
            terminals: BTreeMap::new(),
            spawning: BTreeSet::new(),
            clients: BTreeMap::new(),
            subscriptions: Subscriptions::default(),
            shutting_down: false,
            shutdown_requested: false,
            inventory_generation: 0,
            shutdown_capabilities: VecDeque::new(),
            chats: BTreeMap::new(),
            chat_spawning: BTreeMap::new(),
            agent_observer: Arc::new(crate::agent::observe::Observer::new()),
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    pub(crate) fn register_client(&mut self, client_id: ClientId) -> Arc<DeliveryQueue> {
        let delivery = Arc::new(DeliveryQueue::new(DEFAULT_CLIENT_QUEUE_CAPACITY));
        self.clients.insert(client_id, Arc::clone(&delivery));
        delivery
    }

    pub(crate) fn unregister_client(&mut self, client_id: ClientId) -> Vec<TerminalHandle> {
        for record in self.chats.values_mut() {
            record.subscribers.remove(&client_id);
            record.observations.remove(&client_id);
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
        }
        let terminal_ids = self
            .subscriptions
            .by_client
            .get(&client_id)
            .map(|terminals| terminals.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let handles = terminal_ids
            .iter()
            .filter_map(|terminal_id| {
                self.terminals
                    .get(terminal_id)
                    .map(|record| record.handle.clone())
            })
            .collect::<Vec<_>>();
        self.subscriptions.disconnect(client_id);
        for terminal_id in terminal_ids {
            let visibility = self.subscriptions.visibility(terminal_id);
            if let Some(record) = self.terminals.get(&terminal_id) {
                let _ = apply_visibility(&record.handle, visibility);
            }
            self.disarm_output_if_observed(terminal_id);
        }
        if let Some(delivery) = self.clients.remove(&client_id) {
            delivery.close_after_drain();
        }
        handles
    }

    pub(crate) fn attach(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        if self.subscriptions.attached(client_id, terminal_id) {
            return self.descriptor(client_id, terminal_id);
        }
        let old_visibility = self.subscriptions.visibility(terminal_id);
        self.subscriptions
            .attach(client_id, terminal_id, TerminalVisibility::Hidden);
        let new_visibility = self.subscriptions.visibility(terminal_id);
        if old_visibility != new_visibility
            && let Err(error) = apply_visibility(&record.handle, new_visibility)
        {
            self.subscriptions.detach(client_id, terminal_id);
            return Err(send_error(error));
        }
        self.rearm_output(client_id, terminal_id);
        self.descriptor(client_id, terminal_id)
    }

    pub(crate) fn detach(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<(), ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        let Some(old_requested) = self
            .subscriptions
            .by_client
            .get(&client_id)
            .and_then(|terminals| terminals.get(&terminal_id))
            .copied()
        else {
            return Ok(());
        };
        let old_visibility = self.subscriptions.visibility(terminal_id);
        self.subscriptions.detach(client_id, terminal_id);
        let new_visibility = self.subscriptions.visibility(terminal_id);
        if old_visibility != new_visibility
            && let Err(error) = apply_visibility(&record.handle, new_visibility)
        {
            self.subscriptions
                .attach(client_id, terminal_id, old_requested);
            return Err(send_error(error));
        }
        self.disarm_output_if_observed(terminal_id);
        Ok(())
    }

    pub(crate) fn set_visibility(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        requested: TerminalVisibility,
    ) -> Result<(), ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        let Some(old_requested) = self
            .subscriptions
            .by_client
            .get(&client_id)
            .and_then(|terminals| terminals.get(&terminal_id))
            .copied()
        else {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                format!("client is not attached to terminal {terminal_id}"),
                false,
            ));
        };
        let old_visibility = self.subscriptions.visibility(terminal_id);
        self.subscriptions
            .set_visibility(client_id, terminal_id, requested);
        let new_visibility = self.subscriptions.visibility(terminal_id);
        if old_visibility != new_visibility
            && let Err(error) = apply_visibility(&record.handle, new_visibility)
        {
            self.subscriptions
                .set_visibility(client_id, terminal_id, old_requested);
            return Err(send_error(error));
        }
        // Another subscriber may already be visible, or the worker may have
        // completed. Neither case creates a new capture when this view returns.
        // Offer the retained generation so the returning view can catch up.
        if requested == TerminalVisibility::Visible
            && let Some(delivery) = self.clients.get(&client_id)
        {
            let _ = delivery.push(ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: self.proof.clone(),
                message: ServerMessage::Redraw {
                    terminal_id,
                    generation: record.handle.generation(),
                },
            });
        }
        if (old_requested == TerminalVisibility::Visible)
            != (requested == TerminalVisibility::Visible)
        {
            self.rearm_output(client_id, terminal_id);
        }
        Ok(())
    }

    fn rearm_output(&mut self, client_id: ClientId, terminal_id: TerminalId) {
        let Some(observation) = self.subscriptions.output.get_mut(&(client_id, terminal_id)) else {
            return;
        };
        observation.notified = false;
        if self
            .subscriptions
            .by_client
            .get(&client_id)
            .and_then(|terminals| terminals.get(&terminal_id))
            .is_some_and(|visibility| *visibility != TerminalVisibility::Visible)
            && let Some(record) = self.terminals.get(&terminal_id)
        {
            observation.after_sequence = record.handle.observe_next_output();
        }
        self.disarm_output_if_observed(terminal_id);
    }

    fn disarm_output_if_observed(&self, terminal_id: TerminalId) {
        let needs_output = self
            .subscriptions
            .by_terminal
            .get(&terminal_id)
            .is_some_and(|clients| {
                clients.iter().any(|(client, visibility)| {
                    *visibility != TerminalVisibility::Visible
                        && self
                            .subscriptions
                            .output
                            .get(&(*client, terminal_id))
                            .is_some_and(|observation| !observation.notified)
                })
            });
        if !needs_output && let Some(record) = self.terminals.get(&terminal_id) {
            record.handle.stop_observing_output();
        }
    }

    pub(crate) fn acknowledge_output(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        epoch: u64,
    ) -> Result<(), ProtocolError> {
        self.handle(client_id, terminal_id)?;
        let observation = self
            .subscriptions
            .output
            .get_mut(&(client_id, terminal_id))
            .expect("attached terminal observation");
        if epoch > observation.epoch {
            observation.epoch = epoch;
            self.rearm_output(client_id, terminal_id);
        }
        Ok(())
    }

    pub(crate) fn list(&self, client_id: ClientId) -> Vec<TerminalDescriptor> {
        self.terminals
            .keys()
            .filter_map(|terminal_id| self.descriptor(client_id, *terminal_id).ok())
            .collect()
    }

    pub(crate) fn descriptor(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        let mut session = record.session.clone();
        let attached = self.subscriptions.attached(client_id, terminal_id);
        session.continuity = if record.running && attached {
            TerminalContinuity::Live {
                host_session_id: self.proof.host_session_id.clone(),
                boot_id: self.proof.boot_id.clone(),
                host_incarnation: self.proof.host_incarnation.clone(),
            }
        } else if record.running {
            TerminalContinuity::DetachedSameBoot {
                host_session_id: self.proof.host_session_id.clone(),
                boot_id: self.proof.boot_id.clone(),
                host_incarnation: self.proof.host_incarnation.clone(),
            }
        } else {
            TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: record.last_exit.clone(),
            }
        };
        Ok(TerminalDescriptor {
            session,
            activity: record.handle.activity(),
            agent_activity: record.handle.agent_activity(),
            generation: record.handle.generation(),
            event_sequence: record.event_sequence,
            oldest_event_sequence: record
                .events
                .front()
                .map(|(sequence, _)| *sequence)
                .unwrap_or_else(|| record.event_sequence.saturating_add(1)),
            attached,
            transcript: transcript_metadata(record.session.transcript_path.as_deref()),
        })
    }

    pub(crate) fn reserve_spawn(&mut self, terminal_id: TerminalId) -> Result<(), ProtocolError> {
        if self.shutting_down {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "the cagents daemon is shutting down",
                true,
            ));
        }
        if self.chat_spawning.contains_key(&terminal_id)
            || self
                .chats
                .get(&terminal_id)
                .is_some_and(|record| record.handle.owns_work())
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalAlreadyExists,
                "Native chat still owns this session; explicitly close it before CLI handoff",
                false,
            ));
        }
        if self.terminals.len() + self.spawning.len() >= MAX_TERMINALS {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "the cagents daemon reached its terminal limit",
                true,
            ));
        }
        if self.terminals.contains_key(&terminal_id) || !self.spawning.insert(terminal_id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalAlreadyExists,
                format!("terminal {terminal_id} already exists"),
                false,
            ));
        }
        self.inventory_generation += 1;
        Ok(())
    }

    pub(crate) fn cancel_spawn(&mut self, terminal_id: TerminalId) {
        if self.spawning.remove(&terminal_id) {
            self.inventory_generation += 1;
        }
    }

    pub(crate) fn finish_spawn(
        &mut self,
        client_id: ClientId,
        mut session: TerminalSession,
        handle: TerminalHandle,
        initially_visible: bool,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        let terminal_id = session.id;
        self.cancel_spawn(terminal_id);
        if self.shutting_down {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "the cagents daemon accepted shutdown while this terminal was spawning",
                false,
            ));
        }
        session.resume_hint =
            crate::model::agent_recovery::observed_hint_from_launch(&session.launch);
        self.inventory_generation += 1;
        self.terminals.insert(
            terminal_id,
            TerminalRecord {
                session,
                handle,
                running: true,
                completion_observed: false,
                last_exit: None,
                event_sequence: 0,
                events: VecDeque::new(),
            },
        );
        let visibility = if initially_visible {
            TerminalVisibility::Visible
        } else {
            TerminalVisibility::Hidden
        };
        self.subscriptions
            .attach(client_id, terminal_id, visibility);
        self.rearm_output(client_id, terminal_id);
        self.descriptor(client_id, terminal_id)
    }

    fn attached_terminal(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<&TerminalRecord, ProtocolError> {
        if !self.subscriptions.attached(client_id, terminal_id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                format!("client is not attached to terminal {terminal_id}"),
                false,
            ));
        }
        self.terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))
    }

    pub(crate) fn handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalHandle, ProtocolError> {
        let record = self.attached_terminal(client_id, terminal_id)?;
        if !record.running {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                format!("terminal {terminal_id} has exited and can only be relaunched"),
                true,
            ));
        }
        Ok(record.handle.clone())
    }

    /// A retained final grid can gain or lose UI focus after its process exits.
    pub(crate) fn focus_handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<Option<TerminalHandle>, ProtocolError> {
        let record = self.attached_terminal(client_id, terminal_id)?;
        Ok(record.running.then(|| record.handle.clone()))
    }

    /// Releasing presentation credit is not a process mutation. A shell's exit
    /// does not imply PTY EOF: descendants may still be producing output.
    /// Return the handle; acknowledgment may synchronously wake the host and
    /// must be performed by the caller after releasing the host lock.
    pub(crate) fn redraw_ack_handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
        generation: u64,
    ) -> Result<TerminalHandle, ProtocolError> {
        let record = self.attached_terminal(client_id, terminal_id)?;
        if generation > record.handle.generation() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "cannot acknowledge a terminal generation that was not published",
                false,
            ));
        }
        Ok(record.handle.clone())
    }

    /// Produces the repaint a client needs to reach the terminal's current
    /// generation, or `None` when it already holds it.
    ///
    /// A delta is only offered when the recorded base for this subscription is
    /// exactly the generation the client says it holds; any disagreement, a
    /// resize, a detached capture, or a first request falls back to a full
    /// repaint. The base is recorded before returning, so the diff for the next
    /// repaint is a pointer scan over the row list.
    pub(crate) fn snapshot(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        after_generation: u64,
    ) -> Result<Option<TerminalSnapshot>, ProtocolError> {
        if !self.subscriptions.attached(client_id, terminal_id) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                format!("client is not attached to terminal {terminal_id}"),
                false,
            ));
        }
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        let snapshot = record.handle.snapshot();
        if snapshot.generation <= after_generation {
            return Ok(None);
        }
        let wire = match self.subscriptions.last_sent(client_id, terminal_id) {
            Some(base) if base.generation == after_generation => {
                TerminalSnapshot::diff(&snapshot, base)
            }
            _ => TerminalSnapshot::full(&snapshot),
        };
        self.subscriptions
            .record_sent(client_id, terminal_id, Arc::clone(&snapshot));
        Ok(Some(wire))
    }

    pub(crate) fn replay_events(
        &self,
        terminal_id: TerminalId,
        after_sequence: u64,
    ) -> Result<Vec<(u64, WireTerminalEvent)>, ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        Ok(record
            .events
            .iter()
            .filter(|(sequence, _)| *sequence > after_sequence)
            .cloned()
            .collect())
    }

    fn inventory_count(&mut self) -> u64 {
        let mut count = (self.spawning.len() + self.chat_spawning.len()) as u64;
        for record in self.chats.values_mut() {
            if !record.completion_observed && !record.handle.owns_work() {
                record.completion_observed = true;
                self.inventory_generation += 1;
            }
            if !record.completion_observed {
                count += 1;
            }
        }
        for record in self.terminals.values_mut() {
            if !record.completion_observed && record.handle.is_completed() {
                record.completion_observed = true;
                self.inventory_generation += 1;
            }
            if !record.completion_observed {
                count += 1;
            }
        }
        count
    }

    pub(crate) fn shutdown_preflight(
        &mut self,
        now: Instant,
    ) -> Result<ShutdownPreflight, ShutdownOutcome> {
        if self.shutting_down {
            return Err(ShutdownOutcome::AlreadyShuttingDown);
        }
        if self.shutdown_capabilities.len() == MAX_SHUTDOWN_CAPABILITIES {
            self.shutdown_capabilities.pop_front();
        }
        let live_terminals = self.inventory_count();
        let capability = ShutdownCapability {
            proof: self.proof.clone(),
            token: uuid::Uuid::new_v4().to_string(),
            inventory_generation: self.inventory_generation,
            live_terminals,
            expires_at_unix_ms: unix_time_ms() + SHUTDOWN_CONFIRMATION_TTL.as_millis() as i64,
        };
        self.shutdown_capabilities
            .push_back((capability.clone(), now + SHUTDOWN_CONFIRMATION_TTL));
        Ok(ShutdownPreflight { capability })
    }

    /// Call under the host lock: the predicate and spawn fence are indivisible.
    pub(crate) fn shutdown(
        &mut self,
        operation: ShutdownOperation,
        now: Instant,
    ) -> ShutdownOutcome {
        let count = self.inventory_count();
        if matches!(operation, ShutdownOperation::Stop { .. })
            && (!self.chat_spawning.is_empty()
                || self.chats.values().any(|record| record.handle.owns_work()))
        {
            return ShutdownOutcome::EmptyRequired {
                current_count: count,
            };
        }
        let (require_empty, expected_generation) = match operation {
            ShutdownOperation::Stop {
                require_empty,
                expected_generation,
            } => (require_empty, expected_generation),
            ShutdownOperation::Confirm { capability } => {
                if capability.proof != self.proof {
                    return ShutdownOutcome::DaemonRestarted;
                }
                let Some((stored, deadline)) = self
                    .shutdown_capabilities
                    .iter()
                    .find(|(stored, _)| stored.token == capability.token)
                else {
                    return ShutdownOutcome::InvalidCapability;
                };
                if stored != &capability {
                    return ShutdownOutcome::InvalidCapability;
                }
                if now >= *deadline {
                    return ShutdownOutcome::Expired;
                }
                (false, Some(capability.inventory_generation))
            }
            ShutdownOperation::Preflight => return ShutdownOutcome::InvalidCapability,
        };
        // A duplicate of an accepted request cannot request termination twice.
        if self.shutting_down {
            return ShutdownOutcome::AlreadyShuttingDown;
        }
        if expected_generation.is_some_and(|generation| generation != self.inventory_generation) {
            return ShutdownOutcome::InventoryChanged {
                current_count: count,
            };
        }
        if require_empty && count != 0 {
            return ShutdownOutcome::EmptyRequired {
                current_count: count,
            };
        }
        self.shutting_down = true;
        ShutdownOutcome::Accepted {
            live_terminals_before: count,
            termination_requested: true,
        }
    }

    pub(crate) fn request_shutdown(&mut self) {
        self.shutting_down = true;
        self.shutdown_requested = true;
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutdown_requested
    }

    pub(crate) fn graceful_shutdown(&mut self) -> (Vec<TerminalHandle>, Vec<Arc<DeliveryQueue>>) {
        self.shutting_down = true;
        let handles = self
            .terminals
            .values()
            .map(|record| record.handle.clone())
            .collect();
        let deliveries = self.clients.values().cloned().collect();
        self.subscriptions = Subscriptions::default();
        (handles, deliveries)
    }
}

pub(crate) fn spawn_terminal(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
) -> Result<TerminalDescriptor, ProtocolError> {
    validate_launch(&session)?;
    validate_spawn_options(&options)?;
    let terminal_id = session.id;
    {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reserve_spawn(terminal_id)?;
    }

    let weak = Arc::downgrade(shared);
    let config = TerminalConfig {
        columns: options.columns,
        rows: options.rows,
        cell_width_px: options.cell_width_px,
        cell_height_px: options.cell_height_px,
        max_scrollback: options.max_scrollback,
        initially_visible: options.initially_visible,
        theme: options.theme,
        ..TerminalConfig::default()
    };
    let handle = match TerminalHandle::spawn(session.clone(), config, move |wake| {
        broadcast_redraw(&weak, terminal_id, wake.generation);
    }) {
        Ok(handle) => handle,
        Err(error) => {
            shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cancel_spawn(terminal_id);
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                error.to_string(),
                true,
            ));
        }
    };
    let output_host = Arc::downgrade(shared);
    handle
        .set_output_observer(move |sequence| broadcast_output(&output_host, terminal_id, sequence));

    let event_rx = handle.events();
    let redraw_handle = handle.clone();
    let descriptor = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .finish_spawn(client_id, session, handle, options.initially_visible);
    let descriptor = match descriptor {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = redraw_handle.shutdown();
            return Err(error);
        }
    };
    // Initial publication can happen before the record is subscribable. Clear
    // that edge after insertion; the Spawn response carries the same snapshot,
    // and later generations can now produce a fresh wake.
    redraw_handle.acknowledge_redraw(redraw_handle.generation());
    let event_host = Arc::downgrade(shared);
    let completion_handle = redraw_handle.clone();
    let event_thread = thread::Builder::new()
        .name(format!("cagentsd-events-{terminal_id}"))
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                record_event(&event_host, terminal_id, event);
            }
            // The final grid is captured after the exit event and may race a
            // reveal queued just as the worker stops accepting commands.
            // Notify only visible subscribers once teardown has preserved it.
            broadcast_redraw(&event_host, terminal_id, completion_handle.generation());
        });
    if let Err(error) = event_thread {
        let _ = redraw_handle.shutdown();
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.terminals.remove(&terminal_id);
        state.inventory_generation += 1;
        state.subscriptions.detach(client_id, terminal_id);
        return Err(ProtocolError::new(
            ProtocolErrorCode::Internal,
            format!("could not start terminal event monitor: {error}"),
            true,
        ));
    }
    Ok(descriptor)
}

fn apply_visibility(
    handle: &TerminalHandle,
    visibility: TerminalVisibility,
) -> Result<(), TerminalSendError> {
    match visibility {
        TerminalVisibility::Visible => handle.reveal(),
        TerminalVisibility::Hidden => handle.hide(),
        TerminalVisibility::Hibernated => handle.hibernate(),
    }
    .or_else(|error| match error {
        // Visibility is presentation-only. A completed worker retains its
        // final snapshot and must not require restarting its process to view it.
        TerminalSendError::Closed => Ok(()),
        error => Err(error),
    })
}

pub(crate) fn send_error(error: TerminalSendError) -> ProtocolError {
    match error {
        TerminalSendError::Full => ProtocolError::new(
            ProtocolErrorCode::QueueFull,
            "the terminal command queue is full",
            true,
        ),
        TerminalSendError::Closed => ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "the terminal worker has closed",
            true,
        ),
    }
}

fn validate_launch(session: &TerminalSession) -> Result<(), ProtocolError> {
    if session.launch.program.is_empty() {
        return Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "the launch program must not be empty",
            false,
        ));
    }
    if !session.launch.working_directory.is_absolute() {
        return Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "the launch working directory must be absolute",
            false,
        ));
    }
    Ok(())
}

fn validate_spawn_options(options: &TerminalSpawnOptions) -> Result<(), ProtocolError> {
    if !(1..=1000).contains(&options.columns)
        || !(1..=500).contains(&options.rows)
        || !(1..=4096).contains(&options.cell_width_px)
        || !(1..=4096).contains(&options.cell_height_px)
        || options.max_scrollback > 1_000_000
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "terminal dimensions or scrollback are outside supported bounds",
            false,
        ));
    }
    Ok(())
}

fn broadcast_output(host: &Weak<Mutex<HostState>>, terminal_id: TerminalId, sequence: u64) {
    let Some(host) = host.upgrade() else { return };
    let mut state = host.lock().unwrap_or_else(|error| error.into_inner());
    let HostState {
        proof,
        subscriptions,
        clients,
        terminals,
        ..
    } = &mut *state;
    let Some(subscribers) = subscriptions.by_terminal.get(&terminal_id) else {
        return;
    };
    let mut needs_output = false;
    for (&client, &visibility) in subscribers {
        if visibility == TerminalVisibility::Visible {
            continue;
        }
        let Some(observation) = subscriptions.output.get_mut(&(client, terminal_id)) else {
            continue;
        };
        if observation.notified {
            continue;
        }
        if sequence > observation.after_sequence
            && let Some(delivery) = clients.get(&client)
        {
            observation.notified = delivery
                .push(ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: proof.clone(),
                    message: ServerMessage::TerminalObserved {
                        terminal_id,
                        epoch: observation.epoch,
                    },
                })
                .is_ok();
        }
        needs_output |= !observation.notified;
    }
    if let Some(record) = terminals.get(&terminal_id) {
        if needs_output {
            record.handle.observe_next_output();
        } else {
            record.handle.stop_observing_output();
        }
    }
}

fn broadcast_redraw(host: &Weak<Mutex<HostState>>, terminal_id: TerminalId, generation: u64) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let (proof, deliveries) = {
        let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let deliveries = state
            .subscriptions
            .by_terminal
            .get(&terminal_id)
            .into_iter()
            .flat_map(|clients| clients.iter())
            .filter(|(_, visibility)| **visibility == TerminalVisibility::Visible)
            .filter_map(|(client_id, _)| state.clients.get(client_id).cloned())
            .collect::<Vec<_>>();
        if deliveries.is_empty() {
            // Serialize visibility commands with attachment/reveal changes.
            // Otherwise this Hide could overtake a newly attached UI's Reveal.
            if state.subscriptions.visibility(terminal_id) == TerminalVisibility::Hidden
                && let Some(record) = state.terminals.get(&terminal_id)
            {
                let _ = record.handle.hide();
            }
            return;
        }
        (state.proof.clone(), deliveries)
    };
    let envelope = ServerEnvelope {
        version: PROTOCOL_VERSION,
        request_id: None,
        proof,
        message: ServerMessage::Redraw {
            terminal_id,
            generation,
        },
    };
    for delivery in deliveries {
        let _ = delivery.push(envelope.clone());
    }
}

fn record_event(host: &Weak<Mutex<HostState>>, terminal_id: TerminalId, event: TerminalEvent) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let wire_event = WireTerminalEvent::from(event);
    let (proof, sequence, deliveries) = {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let client_ids = state.subscriptions.clients(terminal_id);
        let proof = state.proof.clone();
        let Some(record) = state.terminals.get_mut(&terminal_id) else {
            return;
        };
        if let WireTerminalEvent::ResumeHintChanged { hint } = &wire_event {
            record.session.resume_hint = hint.clone();
        }
        record.event_sequence = record.event_sequence.saturating_add(1);
        let sequence = record.event_sequence;
        if let WireTerminalEvent::Exited { exit_code, .. } = &wire_event {
            record.running = false;
            record.last_exit = Some(TerminalExit {
                code: i32::try_from(*exit_code).ok(),
                signal: None,
                occurred_at_unix_ms: unix_time_ms(),
            });
        }
        record.events.push_back((sequence, wire_event.clone()));
        while record.events.len() > EVENT_REPLAY_CAPACITY {
            record.events.pop_front();
        }
        let deliveries = client_ids
            .into_iter()
            .filter_map(|client_id| state.clients.get(&client_id).cloned())
            .collect::<Vec<_>>();
        (proof, sequence, deliveries)
    };
    let envelope = ServerEnvelope {
        version: PROTOCOL_VERSION,
        request_id: None,
        proof,
        message: ServerMessage::Event {
            terminal_id,
            sequence,
            event: wire_event,
        },
    };
    for delivery in deliveries {
        let _ = delivery.push(envelope.clone());
    }
}

fn transcript_metadata(path: Option<&std::path::Path>) -> Option<TranscriptMetadata> {
    let path = path?;
    let metadata = fs::metadata(path).ok()?;
    let updated_at_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    Some(TranscriptMetadata {
        path: path.to_owned(),
        byte_len: metadata.len(),
        // The existing worker flushes on orderly close but does not fsync each
        // output batch. Zero is the only crash-durability claim the host can
        // make without changing that worker's persistence contract.
        durable_offset: 0,
        updated_at_unix_ms,
    })
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn not_found(terminal_id: TerminalId) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::TerminalNotFound,
        format!("terminal {terminal_id} was not found"),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    fn test_host() -> HostState {
        HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("test-session"),
            boot_id: BootId::new("test-boot"),
            host_incarnation: HostIncarnation::new("test-incarnation"),
        })
    }

    #[test]
    fn hidden_output_is_per_subscriber_and_rearms_without_revealing() {
        let shared = Arc::new(Mutex::new(test_host()));
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let id = session.id;
        let handle = TerminalHandle::spawn(
            session.clone(),
            TerminalConfig {
                initially_visible: false,
                ..TerminalConfig::default()
            },
            |_| {},
        )
        .unwrap();
        let (first, second) = {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            let deliveries = (state.register_client(1), state.register_client(2));
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(1, session, handle.clone(), false)
                .unwrap();
            state.attach(2, id).unwrap();
            state
                .set_visibility(2, id, TerminalVisibility::Visible)
                .unwrap();
            deliveries
        };
        let weak = Arc::downgrade(&shared);
        broadcast_output(&weak, id, 1);
        broadcast_output(&weak, id, 2);
        {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state
                .set_visibility(1, id, TerminalVisibility::Hibernated)
                .unwrap();
        }
        broadcast_output(&weak, id, 3); // Hibernation is not a new hidden epoch.
        {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state.acknowledge_output(1, id, 1).unwrap();
            assert_eq!(
                state.subscriptions.by_client[&1][&id],
                TerminalVisibility::Hibernated
            );
            state
                .set_visibility(2, id, TerminalVisibility::Hidden)
                .unwrap();
        }
        broadcast_output(&weak, TerminalId::new(), 4); // A stale identity has no subscribers.
        broadcast_output(&weak, id, 4);
        broadcast_output(&weak, id, 5);
        {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            // A late acknowledgement from an earlier scope cannot erase a newer one.
            state.acknowledge_output(1, id, 0).unwrap();
            state.acknowledge_output(1, id, 1).unwrap();
        }
        broadcast_output(&weak, id, 6);
        for (delivery, expected) in [(first, vec![0, 1]), (second, vec![0])] {
            delivery.close_after_drain();
            let mut epochs = Vec::new();
            while let Some(envelope) = delivery.pop() {
                if let ServerMessage::TerminalObserved { terminal_id, epoch } = envelope.message {
                    assert_eq!(terminal_id, id);
                    epochs.push(epoch);
                }
            }
            assert_eq!(epochs, expected);
        }
        handle.shutdown().unwrap();
    }

    #[test]
    fn disconnected_ui_reattaches_the_same_shell_without_relaunching() {
        let directory = tempfile::tempdir().unwrap();
        let starts = directory.path().join("starts");
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                "printf '%s\\n' \"$$\" >> \"$1\"; while IFS= read -r line; do printf '\\033]2;reply:%s:%s\\007' \"$$\" \"$line\"; done".into(),
                "continuity-test".into(),
                starts.to_str().unwrap().into(),
            ],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let id = session.id;
        let mut host = test_host();
        host.register_client(1);
        host.reserve_spawn(id).unwrap();
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        let events = handle.events();
        host.finish_spawn(1, session, handle.clone(), false)
            .unwrap();
        let reply = |value: &str| {
            handle.input(format!("{value}\n").into_bytes()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let event = events
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap();
                if let TerminalEvent::TitleChanged(title) = event
                    && title.ends_with(&format!(":{value}"))
                {
                    break title;
                }
            }
        };
        let before = reply("before");
        let proof = host
            .descriptor(1, id)
            .unwrap()
            .session
            .continuity
            .live_proof()
            .unwrap();
        drop(host.unregister_client(1));
        host.register_client(2);
        let inventory = host.list(2);
        assert_eq!(
            inventory
                .iter()
                .map(|terminal| terminal.session.id)
                .collect::<Vec<_>>(),
            [id]
        );
        let reattached = host.attach(2, id).unwrap();
        assert!(reattached.session.continuity.can_resume_live(&proof));
        assert!(
            host.reserve_spawn(id).is_err(),
            "reopening cannot duplicate a live terminal"
        );
        let after = reply("after");
        assert_eq!(
            before.trim_end_matches(":before"),
            after.trim_end_matches(":after")
        );
        let pids = fs::read_to_string(starts).unwrap();
        assert_eq!(
            pids.lines().count(),
            1,
            "the launch recipe ran exactly once"
        );
        assert_eq!(before, format!("reply:{}:before", pids.trim()));
        handle.shutdown().unwrap();
    }

    #[test]
    fn redraw_delivery_excludes_hidden_and_hibernated_subscribers() {
        let shared = Arc::new(Mutex::new(test_host()));
        let terminal_id = TerminalId::new();
        let deliveries = {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            [
                TerminalVisibility::Visible,
                TerminalVisibility::Hidden,
                TerminalVisibility::Hibernated,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, visibility)| {
                let client_id = index as u64 + 1;
                let delivery = state.register_client(client_id);
                state
                    .subscriptions
                    .attach(client_id, terminal_id, visibility);
                delivery
            })
            .collect::<Vec<_>>()
        };

        broadcast_redraw(&Arc::downgrade(&shared), terminal_id, 7);
        for delivery in &deliveries {
            delivery.close_after_drain();
        }
        assert!(matches!(
            deliveries[0].pop().unwrap().message,
            ServerMessage::Redraw { generation: 7, .. }
        ));
        assert!(deliveries[1].pop().is_none());
        assert!(deliveries[2].pop().is_none());
    }

    fn empty_stop() -> ShutdownOperation {
        ShutdownOperation::Stop {
            require_empty: true,
            expected_generation: None,
        }
    }

    #[test]
    fn leader_exit_preserves_consumption_credit_but_not_process_authority() {
        let shared = Arc::new(Mutex::new(test_host()));
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let id = session.id;
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(1, session, handle.clone(), false)
                .unwrap();
        }
        // The shell-exit event can precede PTY EOF while a descendant owns it.
        record_event(
            &Arc::downgrade(&shared),
            id,
            TerminalEvent::Exited {
                exit_code: 0,
                signal: None,
            },
        );
        let capability = {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state
                .set_visibility(1, id, TerminalVisibility::Visible)
                .unwrap();
            let generation = handle.generation();
            assert!(state.redraw_ack_handle(1, id, generation).is_ok());
            assert!(state.focus_handle(1, id).unwrap().is_none());
            assert_eq!(
                state.focus_handle(2, id).err().unwrap().code,
                ProtocolErrorCode::PermissionDenied
            );
            assert_eq!(
                state.handle(1, id).err().unwrap().code,
                ProtocolErrorCode::TerminalUnavailable
            );
            assert_eq!(
                state
                    .redraw_ack_handle(2, id, generation)
                    .err()
                    .unwrap()
                    .code,
                ProtocolErrorCode::PermissionDenied
            );
            assert_eq!(
                state.redraw_ack_handle(1, id, u64::MAX).err().unwrap().code,
                ProtocolErrorCode::InvalidRequest
            );
            assert_eq!(
                state.shutdown(empty_stop(), Instant::now()),
                ShutdownOutcome::EmptyRequired { current_count: 1 }
            );
            state.shutdown_preflight(Instant::now()).unwrap().capability
        };
        handle.shutdown().unwrap();
        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state
            .set_visibility(1, id, TerminalVisibility::Hidden)
            .unwrap();
        state
            .set_visibility(1, id, TerminalVisibility::Visible)
            .unwrap();
        assert_eq!(
            state.shutdown(ShutdownOperation::Confirm { capability }, Instant::now()),
            ShutdownOutcome::InventoryChanged { current_count: 0 }
        );
        assert_eq!(
            state.terminals.len(),
            1,
            "completed descriptors remain available as history"
        );
        assert_eq!(
            state.shutdown(empty_stop(), Instant::now()),
            ShutdownOutcome::Accepted {
                live_terminals_before: 0,
                termination_requested: true
            }
        );
    }

    #[test]
    fn established_terminals_require_confirmation_and_pending_finish_is_rejected() {
        let now = Instant::now();
        let mut state = test_host();
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let mut handles = Vec::new();
        for count in 1..=3 {
            let session = TerminalSession {
                id: TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: recipe.clone(),
                    last_exit: None,
                },
            };
            state.reserve_spawn(session.id).unwrap();
            let handle =
                TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
            handles.push(handle.clone());
            state.finish_spawn(1, session, handle, false).unwrap();
            assert_eq!(
                state.shutdown(empty_stop(), now),
                ShutdownOutcome::EmptyRequired {
                    current_count: count
                }
            );
        }
        let pending = TerminalSession {
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        state.reserve_spawn(pending.id).unwrap();
        let capability = state.shutdown_preflight(now).unwrap().capability;
        assert_eq!(capability.live_terminals, 4);
        assert_eq!(
            state.shutdown(ShutdownOperation::Confirm { capability }, now),
            ShutdownOutcome::Accepted {
                live_terminals_before: 4,
                termination_requested: true
            }
        );
        let pending_handle =
            TerminalHandle::spawn(pending.clone(), TerminalConfig::default(), |_| {}).unwrap();
        assert!(
            state
                .finish_spawn(1, pending, pending_handle.clone(), false)
                .is_err()
        );
        assert_eq!(state.terminals.len(), 3);
        pending_handle.shutdown().unwrap();
        for handle in handles {
            handle.shutdown().unwrap();
        }
    }

    #[test]
    fn shutdown_reservations_and_spawn_fence_are_atomic() {
        let now = Instant::now();
        let mut empty = test_host();
        assert_eq!(
            empty.shutdown(empty_stop(), now),
            ShutdownOutcome::Accepted {
                live_terminals_before: 0,
                termination_requested: true
            }
        );
        assert!(empty.reserve_spawn(TerminalId::new()).is_err());
        assert_eq!(
            empty.shutdown(empty_stop(), now),
            ShutdownOutcome::AlreadyShuttingDown
        );
        for count in [1, 7] {
            let mut state = test_host();
            for _ in 0..count {
                state.reserve_spawn(TerminalId::new()).unwrap();
            }
            assert_eq!(
                state.shutdown(empty_stop(), now),
                ShutdownOutcome::EmptyRequired {
                    current_count: count
                }
            );
            assert!(!state.shutting_down);
        }
    }

    #[test]
    fn shutdown_capabilities_detect_inventory_aba_expiry_tampering_and_restart() {
        let now = Instant::now();
        let mut state = test_host();
        let capability = state.shutdown_preflight(now).unwrap().capability;
        let terminal_id = TerminalId::new();
        state.reserve_spawn(terminal_id).unwrap();
        state.cancel_spawn(terminal_id);
        assert_eq!(
            state.shutdown(ShutdownOperation::Confirm { capability }, now),
            ShutdownOutcome::InventoryChanged { current_count: 0 }
        );
        let capability = state.shutdown_preflight(now).unwrap().capability;
        assert_eq!(
            state.shutdown(
                ShutdownOperation::Confirm {
                    capability: capability.clone()
                },
                now + SHUTDOWN_CONFIRMATION_TTL
            ),
            ShutdownOutcome::Expired
        );
        let mut tampered = capability.clone();
        tampered.live_terminals = 500;
        assert_eq!(
            state.shutdown(
                ShutdownOperation::Confirm {
                    capability: tampered
                },
                now
            ),
            ShutdownOutcome::InvalidCapability
        );
        let mut restarted = test_host();
        restarted.proof.host_incarnation = HostIncarnation::new("new-incarnation");
        assert_eq!(
            restarted.shutdown(
                ShutdownOperation::Confirm {
                    capability: capability.clone()
                },
                now
            ),
            ShutdownOutcome::DaemonRestarted
        );
        assert!(matches!(
            state.shutdown(
                ShutdownOperation::Confirm {
                    capability: capability.clone()
                },
                now
            ),
            ShutdownOutcome::Accepted { .. }
        ));
        assert_eq!(
            state.shutdown(ShutdownOperation::Confirm { capability }, now),
            ShutdownOutcome::AlreadyShuttingDown
        );
    }

    #[test]
    fn concurrent_reservation_and_empty_stop_have_only_safe_orderings() {
        for spawn_first in [false, true] {
            let host = Arc::new(Mutex::new(test_host()));
            let (release, wait) = std::sync::mpsc::channel();
            let other = Arc::clone(&host);
            let worker = thread::spawn(move || {
                wait.recv().unwrap();
                other.lock().unwrap().reserve_spawn(TerminalId::new())
            });
            if spawn_first {
                release.send(()).unwrap();
                worker.join().unwrap().unwrap();
                assert_eq!(
                    host.lock().unwrap().shutdown(empty_stop(), Instant::now()),
                    ShutdownOutcome::EmptyRequired { current_count: 1 }
                );
            } else {
                let outcome = host.lock().unwrap().shutdown(empty_stop(), Instant::now());
                release.send(()).unwrap();
                assert!(matches!(outcome, ShutdownOutcome::Accepted { .. }));
                assert!(worker.join().unwrap().is_err());
            }
        }
    }

    #[test]
    fn disconnect_drops_only_attachments_and_allows_reconnect() {
        let terminal_id = TerminalId::new();
        let mut subscriptions = Subscriptions::default();
        subscriptions.attach(1, terminal_id, TerminalVisibility::Visible);
        assert!(subscriptions.attached(1, terminal_id));

        subscriptions.disconnect(1);
        assert!(!subscriptions.attached(1, terminal_id));
        assert!(subscriptions.clients(terminal_id).is_empty());

        subscriptions.attach(2, terminal_id, TerminalVisibility::Hidden);
        assert!(subscriptions.attached(2, terminal_id));
        assert_eq!(subscriptions.clients(terminal_id), vec![2]);
    }

    #[test]
    fn visibility_is_aggregated_and_empty_terminals_stay_hidden() {
        let terminal_id = TerminalId::new();
        let mut subscriptions = Subscriptions::default();
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hidden
        );

        subscriptions.attach(1, terminal_id, TerminalVisibility::Hibernated);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hibernated
        );
        subscriptions.attach(2, terminal_id, TerminalVisibility::Hidden);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hidden
        );
        subscriptions.attach(3, terminal_id, TerminalVisibility::Visible);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Visible
        );

        subscriptions.detach(3, terminal_id);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hidden
        );
        subscriptions.disconnect(2);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hibernated
        );
        subscriptions.detach(1, terminal_id);
        assert_eq!(
            subscriptions.visibility(terminal_id),
            TerminalVisibility::Hidden
        );
    }
}
