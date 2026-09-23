use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    sync::{Arc, Mutex, RwLock, Weak},
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
        ProtocolError, ProtocolErrorCode, ServerMessage, ShutdownCapability, ShutdownOperation,
        ShutdownOutcome, ShutdownPreflight, TerminalDescriptor, TerminalSnapshot,
        TerminalSpawnOptions, TerminalVisibility, WireTerminalEvent,
    },
    workspace_model::{ModelEdit, WorkspaceModel},
};

use super::delivery::{DEFAULT_CLIENT_QUEUE_CAPACITY, DeliveryQueue, Outbound};

const EVENT_REPLAY_CAPACITY: usize = 256;
pub(super) const MAX_TERMINALS: usize = 1024;
const SHUTDOWN_CONFIRMATION_TTL: Duration = Duration::from_secs(60);
const MAX_SHUTDOWN_CAPABILITIES: usize = 32;

/// Why the shared model cannot be served.
///
/// Reported rather than answered with an empty model: an empty model is a
/// meaningful answer - no window has seeded the registry yet - and a window
/// that believed it would offer to donate its state over and over.
fn missing_registry() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::Internal,
        "the shared workspace model store is unavailable",
        false,
    )
}

fn model_unavailable(error: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, error, false)
}

pub(crate) type ClientId = u64;
pub(crate) type SharedHost = Arc<Mutex<HostState>>;

pub(crate) struct TerminalRecord {
    pub(super) session: TerminalSession,
    pub(super) handle: TerminalHandle,
    pub(super) execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    options: TerminalSpawnOptions,
    upgrade_sources: Vec<TerminalSession>,
    upgrade_relaunches: Vec<crate::terminal_ipc::UpgradeRelaunch>,
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
    /// Shared, never mutated: a redraw broadcast hands the same proof to every
    /// subscriber's queue, and cloning three `String`s per subscriber per frame
    /// made per-frame allocation scale with the number of attached views.
    proof: Arc<HostSessionProof>,
    pub(super) terminals: BTreeMap<TerminalId, TerminalRecord>,
    spawning: BTreeSet<TerminalId>,
    pub(super) clients: BTreeMap<ClientId, Arc<DeliveryQueue>>,
    /// Connections that asked for terminal-inventory edges. A client that never
    /// asked receives none, which is what keeps every strict request/response
    /// client free of unsolicited frames.
    watchers: BTreeSet<ClientId>,
    /// The durable shared sidebar model, installed by the daemon's entry point.
    ///
    /// `None` when its store could not be opened, and deliberately not an empty
    /// model: a window reads an empty model as "no window has seeded this yet"
    /// and answers by donating its own state, so serving one from a broken store
    /// would invite every window to re-seed on every connection. A failure is
    /// reported instead, and terminals keep working meanwhile.
    model: Option<super::model::Registry>,
    /// Connections that asked for shared-model edges, kept separately from
    /// `watchers` because watching terminals and watching the model are
    /// independent subscriptions with independent clients - the CLI reads the
    /// model without subscribing at all.
    model_watchers: BTreeSet<ClientId>,
    subscriptions: Subscriptions,
    pub(super) shutting_down: bool,
    shutdown_requested: bool,
    pub(super) inventory_generation: u64,
    shutdown_capabilities: VecDeque<(ShutdownCapability, Instant)>,
    pub(super) chats: BTreeMap<TerminalId, super::chat::Record>,
    pub(super) chat_handoffs: BTreeMap<TerminalId, super::chat::TerminalHandoff>,
    pub(super) chat_spawning: BTreeMap<TerminalId, crate::chat::Launch>,
    pub(super) chat_bindings: BTreeMap<TerminalId, super::chat::PendingBinding>,
    pub(crate) agent_observer: Arc<crate::agent::observe::Observer>,
    /// Exclusive upgrade barrier; ordinary client operations remain concurrent.
    pub(super) control_gate: Arc<RwLock<()>>,
    upgrade: Option<(
        crate::upgrade::handoff::Store,
        crate::upgrade::handoff::Checkpoint,
        Vec<crate::terminal::UpgradeSnapshot>,
    )>,
    pub(super) upgrade_chats: BTreeMap<TerminalId, Vec<crate::upgrade::handoff::ChatSource>>,
}

impl HostState {
    pub(crate) fn new(proof: HostSessionProof) -> Self {
        Self {
            proof: Arc::new(proof),
            terminals: BTreeMap::new(),
            spawning: BTreeSet::new(),
            clients: BTreeMap::new(),
            watchers: BTreeSet::new(),
            model: None,
            model_watchers: BTreeSet::new(),
            subscriptions: Subscriptions::default(),
            shutting_down: false,
            shutdown_requested: false,
            inventory_generation: 0,
            shutdown_capabilities: VecDeque::new(),
            chats: BTreeMap::new(),
            chat_handoffs: BTreeMap::new(),
            chat_spawning: BTreeMap::new(),
            chat_bindings: BTreeMap::new(),
            agent_observer: Arc::new(crate::agent::observe::Observer::new()),
            control_gate: Arc::new(RwLock::new(())),
            upgrade: None,
            upgrade_chats: BTreeMap::new(),
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    /// The shared proof, for a broadcast that must not clone it per subscriber.
    pub(crate) fn proof_handle(&self) -> Arc<HostSessionProof> {
        Arc::clone(&self.proof)
    }

    pub(crate) fn register_client(&mut self, client_id: ClientId) -> Arc<DeliveryQueue> {
        let delivery = Arc::new(DeliveryQueue::new(client_id, DEFAULT_CLIENT_QUEUE_CAPACITY));
        self.clients.insert(client_id, Arc::clone(&delivery));
        delivery
    }

    /// Subscribes `client_id` to terminal-inventory edges. Repeating it is
    /// harmless, so a reconnecting observer never has to track whether its
    /// previous subscription survived.
    pub(crate) fn watch_terminals(&mut self, client_id: ClientId) {
        self.watchers.insert(client_id);
    }

    /// Drops a departing client's delivery identity without unregistering its
    /// subscriptions, which is all a committed shutdown may still do.
    pub(crate) fn forget_client_delivery(&mut self, client_id: ClientId) {
        self.clients.remove(&client_id);
        self.watchers.remove(&client_id);
        self.model_watchers.remove(&client_id);
    }

    /// Installs the durable shared sidebar model.
    ///
    /// The daemon's entry point opens the store, not [`HostState::new`]: a host
    /// constructed for a test must not reach into the user's data directory, and
    /// a daemon that cannot open the store still has terminals to host.
    pub(crate) fn install_model(&mut self, registry: super::model::Registry) {
        self.model = Some(registry);
    }

    /// Subscribes `client_id` to shared-model edges and returns the model.
    ///
    /// Both happen under one lock, so an edit applied between subscribing and
    /// reading cannot fall into a gap: a client either sees it in the model it
    /// is handed or receives it as an edge afterwards, never neither.
    pub(crate) fn watch_model(
        &mut self,
        client_id: ClientId,
    ) -> Result<WorkspaceModel, ProtocolError> {
        let model = self.registry()?.model().map_err(model_unavailable)?;
        self.model_watchers.insert(client_id);
        Ok(model)
    }

    pub(crate) fn read_model(&self) -> Result<WorkspaceModel, ProtocolError> {
        self.model
            .as_ref()
            .ok_or_else(missing_registry)?
            .model()
            .map_err(model_unavailable)
    }

    /// Applies one edit and tells every other watcher about it.
    pub(crate) fn edit_model(
        &mut self,
        origin: ClientId,
        edit: ModelEdit,
    ) -> Result<WorkspaceModel, ProtocolError> {
        let edit = edit
            .canonicalized()
            .map_err(|error| ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false))?;
        let applied = self.registry()?.apply(&edit).map_err(model_unavailable)?;
        if applied.changed {
            self.announce_model_edit(origin, applied.model.sequence, &edit);
        }
        Ok(applied.model)
    }

    fn registry(&mut self) -> Result<&mut super::model::Registry, ProtocolError> {
        self.model.as_mut().ok_or_else(missing_registry)
    }

    /// Tells every model watcher except `origin` about an applied edit.
    ///
    /// The edges go through the per-client delivery queue rather than a redraw
    /// broadcast, because a redraw broadcast returns early for a subscriber that
    /// is not visible and a sidebar edge is not a repaint - a hidden window that
    /// missed one would show a channel that no longer exists until it restarted.
    fn announce_model_edit(&self, origin: ClientId, sequence: u64, edit: &ModelEdit) {
        for client_id in &self.model_watchers {
            if *client_id == origin {
                continue;
            }
            if let Some(delivery) = self.clients.get(client_id) {
                let _ = delivery.push(Outbound::broadcast(
                    &self.proof,
                    ServerMessage::WorkspaceModelEdited {
                        sequence,
                        edit: edit.clone(),
                    },
                ));
            }
        }
    }

    /// Tells every observing client except `creator` that a terminal now
    /// exists.
    ///
    /// The creator is excluded because it already holds the descriptor its own
    /// `Spawn` returned; announcing it back would ask every spawning client to
    /// recognise and discard its own work. One descriptor serves the whole
    /// fan-out: a recipient cannot be attached to a terminal that was created
    /// by someone else a moment ago.
    pub(crate) fn announce_created(&self, creator: ClientId, terminal_id: TerminalId) {
        let Ok(terminal) = self.descriptor_as(false, terminal_id) else {
            return;
        };
        for client_id in &self.watchers {
            if *client_id == creator {
                continue;
            }
            if let Some(delivery) = self.clients.get(client_id) {
                let _ = delivery.push(Outbound::broadcast(
                    &self.proof,
                    ServerMessage::TerminalCreated {
                        terminal: terminal.clone(),
                    },
                ));
            }
        }
    }

    pub(crate) fn unregister_client(&mut self, client_id: ClientId) -> Vec<TerminalHandle> {
        for receipt in self.chat_handoffs.values_mut() {
            receipt.detach(client_id);
        }
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
        self.watchers.remove(&client_id);
        self.model_watchers.remove(&client_id);
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
            let _ = delivery.push(Outbound::broadcast(
                &self.proof,
                ServerMessage::Redraw {
                    terminal_id,
                    generation: record.handle.generation(),
                },
            ));
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
        self.attached_terminal(client_id, terminal_id)?;
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
        self.descriptor_as(
            self.subscriptions.attached(client_id, terminal_id),
            terminal_id,
        )
    }

    /// The descriptor an observer with attachment state `attached` is owed.
    ///
    /// Attachment is the only per-client input, so a broadcast to observers
    /// that are all unattached by construction can build one descriptor
    /// instead of one per recipient.
    fn descriptor_as(
        &self,
        attached: bool,
        terminal_id: TerminalId,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        let record = self
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| not_found(terminal_id))?;
        let mut session = record.session.clone();
        session.continuity = if !record.handle.is_completed() && attached {
            TerminalContinuity::Live {
                host_session_id: self.proof.host_session_id.clone(),
                boot_id: self.proof.boot_id.clone(),
                host_incarnation: self.proof.host_incarnation.clone(),
            }
        } else if !record.handle.is_completed() {
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
            upgrade_sources: record.upgrade_sources.clone(),
            upgrade_relaunches: record.upgrade_relaunches.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve_spawn(&mut self, terminal_id: TerminalId) -> Result<(), ProtocolError> {
        self.reserve_spawn_for_view(terminal_id, false)
    }

    pub(super) fn retire_terminal(&mut self, terminal_id: TerminalId) -> Option<TerminalRecord> {
        let record = self.terminals.remove(&terminal_id)?;
        let clients: Vec<_> = self
            .subscriptions
            .by_terminal
            .get(&terminal_id)
            .map(|clients| clients.keys().copied().collect())
            .unwrap_or_default();
        for client in clients {
            self.subscriptions.detach(client, terminal_id);
        }
        self.inventory_generation += 1;
        Some(record)
    }

    fn reserve_spawn_for_view(
        &mut self,
        terminal_id: TerminalId,
        paired: bool,
    ) -> Result<(), ProtocolError> {
        if self.shutting_down {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "the cagents daemon is shutting down",
                true,
            ));
        }
        if self.chat_spawning.contains_key(&terminal_id)
            || self.chats.get(&terminal_id).is_some_and(|record| {
                record.handle.owns_work()
                    && !(paired
                        && record.returning_terminal
                        && record.session.launch.provider == crate::agent::Provider::Codex)
            })
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
                options: TerminalSpawnOptions::default(),
                execution_environment: None,
                upgrade_sources: Vec::new(),
                upgrade_relaunches: Vec::new(),
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
    /// Presentation and input still belong to a PTY held by descendants, even
    /// after its original process no longer grants process-control authority.
    pub(crate) fn surface_handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalHandle, ProtocolError> {
        let record = self.attached_terminal(client_id, terminal_id)?;
        if record.handle.is_completed() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "terminal output has completed",
                false,
            ));
        }
        Ok(record.handle.clone())
    }

    pub(crate) fn focus_handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<Option<TerminalHandle>, ProtocolError> {
        let record = self.attached_terminal(client_id, terminal_id)?;
        Ok((!record.handle.is_completed()).then(|| record.handle.clone()))
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
            if !record.completion_observed
                && !record.handle.owns_work()
                && !record.returning_terminal
            {
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
            proof: (*self.proof).clone(),
            token: uuid::Uuid::new_v4().to_string(),
            inventory_generation: self.inventory_generation,
            live_terminals,
            expires_at_unix_ms: unix_time_ms() + SHUTDOWN_CONFIRMATION_TTL.as_millis() as i64,
        };
        self.shutdown_capabilities
            .push_back((capability.clone(), now + SHUTDOWN_CONFIRMATION_TTL));
        Ok(ShutdownPreflight { capability })
    }

    fn validate_upgrade_capability(
        &mut self,
        capability: &ShutdownCapability,
        now: Instant,
    ) -> Result<(), String> {
        let count = self.inventory_count();
        if self.shutting_down {
            return Err("Daemon is already shutting down".into());
        }
        if capability.proof != *self.proof {
            return Err("Daemon changed; obtain a new upgrade preflight".into());
        }
        let Some((stored, deadline)) = self
            .shutdown_capabilities
            .iter()
            .find(|(stored, _)| stored.token == capability.token)
        else {
            return Err("Invalid upgrade capability; obtain a new preflight".into());
        };
        if stored != capability || now >= *deadline {
            return Err("Invalid or expired upgrade capability; obtain a new preflight".into());
        }
        if capability.inventory_generation != self.inventory_generation
            || capability.live_terminals != count
        {
            return Err("Daemon inventory changed; obtain a new upgrade preflight".into());
        }
        if !self.spawning.is_empty() || !self.chat_spawning.is_empty() {
            return Err(
                "Sessions are still starting; wait for reservations to settle and retry".into(),
            );
        }
        Ok(())
    }

    /// Call under the host lock: the predicate and spawn fence are indivisible.
    pub(crate) fn shutdown(
        &mut self,
        operation: ShutdownOperation,
        now: Instant,
    ) -> ShutdownOutcome {
        let count = self.inventory_count();
        if let ShutdownOperation::Stop { require_empty, .. } = operation
            && (!self.chat_spawning.is_empty()
                || self
                    .chats
                    .values()
                    .any(|record| record.handle.owns_work() || record.returning_terminal))
        {
            // Native chats own work, so this stop is refused for confirmation,
            // not because the caller demanded an empty daemon. Reporting
            // `EmptyRequired` for a plain `cagents daemon stop` told the user
            // they had asked for something they never asked for.
            return if require_empty {
                ShutdownOutcome::EmptyRequired {
                    current_count: count,
                }
            } else {
                ShutdownOutcome::ConfirmationRequired {
                    current_count: count,
                }
            };
        }
        let (require_empty, expected_generation) = match operation {
            ShutdownOperation::Stop {
                require_empty,
                expected_generation,
            } => (require_empty, expected_generation),
            ShutdownOperation::Confirm { capability } => {
                if capability.proof != *self.proof {
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

fn upgrade_error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}

/// Names a terminal whose running job would be lost by the upgrade: what is
/// running, where, and which terminal it is, so the user can find it in the
/// sidebar without opening each one.
fn describe_upgrade_blocker(source: &TerminalSession) -> String {
    let launch = &source.launch;
    let mut command = launch.program.clone();
    for argument in &launch.arguments {
        command.push(' ');
        command.push_str(argument);
    }
    format!(
        "{command} in {} (terminal {})",
        launch.working_directory.display(),
        source.id
    )
}

fn upgrade_blockers_message(blockers: &[String], relaunch_agents: bool) -> String {
    // "Active work" is narrower than it sounds, and the message says so: the
    // session's process cannot be resumed after the daemon restarts, only
    // started again, and the upgrade will not make that choice for the user.
    let mut message = String::from(
        "These active terminals cannot be resumed automatically. Finish or close their jobs before upgrading.",
    );
    // With relaunching accepted, whatever still blocks is no identified agent.
    message.push_str(if relaunch_agents {
        " Each runs a job that is not an agent cagents can relaunch:"
    } else {
        " Identified agents can instead start fresh conversations: turn on Relaunch agents on upgrade in Settings, or pass --relaunch-agents. Unrelated shell jobs still block the upgrade:"
    });
    for blocker in blockers {
        message.push_str("\n  ");
        message.push_str(blocker);
    }
    message
}

/// Caller holds control_gate throughout reversible snapshots and durable commit.
pub(super) fn prepare_upgrade(
    host: &SharedHost,
    capability: ShutdownCapability,
    relaunch_agents: bool,
) -> Result<(u64, u64), ProtocolError> {
    prepare_upgrade_with_policy(
        host,
        capability,
        relaunch_agents,
        crate::upgrade::handoff::Store::discover,
    )
}

fn validate_terminal_upgrade_environment(
    recipe: &crate::model::LaunchRecipe,
    execution: &crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<(), ProtocolError> {
    let provider = crate::agent::Provider::from_program(&recipe.program).ok_or_else(|| {
        upgrade_error("Cannot verify the returned terminal's execution environment for upgrade")
    })?;
    let mut restored = crate::model::agent_recovery::fresh_native_environment(provider, false);
    restored.extend(recipe.environment.clone());
    if execution.0 != restored {
        return Err(upgrade_error(
            "Terminal execution environment cannot survive daemon upgrade without losing selected credentials or account settings. Align the daemon configuration or finish and close this terminal before upgrading; no source was stopped",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn prepare_upgrade_with(
    host: &SharedHost,
    capability: ShutdownCapability,
    discover: impl FnOnce() -> Result<crate::upgrade::handoff::Store, String>,
) -> Result<(u64, u64), ProtocolError> {
    prepare_upgrade_with_policy(host, capability, false, discover)
}

fn prepare_upgrade_with_policy(
    host: &SharedHost,
    capability: ShutdownCapability,
    relaunch_agents: bool,
    discover: impl FnOnce() -> Result<crate::upgrade::handoff::Store, String>,
) -> Result<(u64, u64), ProtocolError> {
    use crate::upgrade::handoff::{Chat, Checkpoint, Phase, Terminal};
    let (proof, terminals, chats, paired_codex) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .validate_upgrade_capability(&capability, Instant::now())
            .map_err(upgrade_error)?;
        // A receipt is retained so a window that has not saved its replacement
        // can still replay it, which is worth refusing an upgrade for while the
        // window can still act. Nothing ever released a receipt whose window
        // stopped acting, so one discarded card left this daemon unable to
        // checkpoint for the rest of its life, and terminating every hosted
        // session became the only way to install a new build. The receipt is
        // left in place rather than deleted here: the checkpoint can still fail
        // below, and a daemon that keeps running keeps its evidence replayable.
        let now = Instant::now();
        if state
            .chat_handoffs
            .values()
            .any(|receipt| !receipt.unacknowledgeable(now))
        {
            return Err(upgrade_error(
                "A native-to-terminal handoff is not durably acknowledged; save its replacement before upgrading",
            ));
        }
        for (chat, receipt) in &state.chat_handoffs {
            tracing::warn!(
                chat = %chat,
                terminal = %receipt.replacement().id,
                "checkpointing past a native-to-terminal handoff no client acknowledged"
            );
        }
        if state.chats.values().any(|record| record.returning_terminal) {
            return Err(upgrade_error(
                "A native-to-terminal replacement is still pending; retry or explicitly close it before upgrading",
            ));
        }
        let terminals = state
            .terminals
            .iter()
            .filter(|(_, record)| !record.handle.is_completed())
            .map(|(&id, record)| {
                if let Some(execution) = &record.execution_environment {
                    validate_terminal_upgrade_environment(&record.session.launch, execution)?;
                }
                Ok((
                    state.descriptor(0, id)?.session,
                    record.handle.clone(),
                    record.options.clone(),
                    record.upgrade_sources.clone(),
                    record.upgrade_relaunches.clone(),
                    record.execution_environment.clone(),
                ))
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        let mut chats = Vec::new();
        let mut paired_codex = BTreeMap::new();
        for record in state
            .chats
            .values()
            .filter(|record| record.handle.owns_work())
        {
            if record.handle.is_completed() {
                return Err(upgrade_error(
                    "A native provider has unconfirmed work after a failed close; resolve that provider before upgrading",
                ));
            }
            let source = record.session.clone();
            let mut launch = source.launch.clone();
            launch.session_id = record.handle.confirmed_session_id();
            // An interrupt-driven reset rotates the live conversation past
            // the originally launched resume ID. The confirmed identity is
            // the provider's own report, so record it in the recipe too —
            // otherwise the checkpoint neither validates nor restarts the
            // final conversation.
            super::super::chat::protocol::align_resume_identity(&mut launch)
                .map_err(upgrade_error)?;
            if launch.terminal_recipe().is_none() {
                return Err(upgrade_error(format!(
                    "Native chat {} has no exact resumable conversation; wait for provider initialization or close it explicitly",
                    source.id
                )));
            }
            // The successor restarts this conversation from its terminal only
            // if that terminal is checkpointed, which a completed one is not,
            // and is restored first, which it is not for a conversation that
            // owns its Codex listener.
            let hosted_source = !super::chat::owns_codex_listener(&source)
                && state
                    .terminals
                    .get(&source.id)
                    .is_some_and(|terminal| !terminal.handle.is_completed());
            super::chat::validate_upgrade_environment(&launch, hosted_source, &record.handle)
                .map_err(upgrade_error)?;
            if launch.provider == crate::agent::Provider::Codex
                && state.terminals.contains_key(&source.id)
            {
                if source.proof.as_ref() != Some(state.proof())
                    || source
                        .terminal
                        .as_ref()
                        .map(|terminal| (terminal.id, terminal.continuity.live_proof()))
                        != Some((source.id, Some(state.proof().clone())))
                {
                    return Err(upgrade_error("The paired Codex terminal ownership changed"));
                }
                paired_codex.insert(
                    source.id,
                    (
                        record
                            .source_process
                            .clone()
                            .or_else(|| {
                                state
                                    .terminals
                                    .get(&source.id)
                                    .and_then(|terminal| terminal.handle.observe_agent_process())
                            })
                            .ok_or_else(|| {
                                upgrade_error(
                                    "The paired Codex chat has no confirmed source process",
                                )
                            })?,
                        record.handle.clone(),
                    ),
                );
            }
            chats.push(Chat {
                previous_sources: state
                    .upgrade_chats
                    .get(&source.id)
                    .cloned()
                    .unwrap_or_default(),
                source,
                launch,
            });
        }
        ((*state.proof).clone(), terminals, chats, paired_codex)
    };
    let store = discover().map_err(upgrade_error)?;
    if let Some(previous) = store.load().map_err(upgrade_error)?
        && (previous.proof != proof || previous.phase != Phase::Prepared)
    {
        return Err(upgrade_error(format!(
            "An unresolved upgrade checkpoint exists at {}; it will not be overwritten",
            store.path.display()
        )));
    }
    let mut frozen = Vec::with_capacity(terminals.len());
    let mut saved = Vec::with_capacity(terminals.len());
    // Every terminal that would refuse the upgrade is reported at once, so the
    // user resolves them in one pass instead of discovering them one retry at
    // a time. Snapshots taken before a blocker is found stay reversible under
    // the control gate, exactly as they do for any other refusal below.
    let mut blockers = Vec::new();
    for (
        mut source,
        handle,
        mut options,
        previous_sources,
        mut relaunches,
        execution_environment,
    ) in terminals
    {
        let snapshot = handle.prepare_upgrade().map_err(upgrade_error)?;
        source.resume_hint = if let Some((selected, native)) = paired_codex.get(&source.id) {
            // The worker's argv hint can be fresh or name an older TUI thread.
            // Only the still-confirmed attachment may supersede it, and only
            // after freezing and revalidating its exact process/account.
            let current = handle.observe_agent_process().ok_or_else(|| {
                upgrade_error("The paired Codex source disappeared before upgrade")
            })?;
            if !snapshot.only_agent_tree(current.pid)
                || native.is_completed()
                || !native.owns_work()
            {
                return Err(upgrade_error(
                    "The paired Codex source has unrelated jobs or lost its native attachment",
                ));
            }
            let chat = chats
                .iter()
                .find(|chat| chat.source.id == source.id)
                .expect("paired chat collected with source");
            let recipe = super::chat::paired_upgrade_recipe(
                selected,
                &current,
                &chat.launch,
                native.confirmed_session_id().as_deref(),
            )
            .map_err(upgrade_error)?;
            Some(
                crate::model::agent_recovery::hint_from_launch(&recipe).ok_or_else(|| {
                    upgrade_error("The paired Codex exact recipe cannot be saved")
                })?,
            )
        } else {
            snapshot.hint.clone()
        };
        let exact = source
            .resume_hint
            .as_deref()
            .and_then(crate::model::agent_recovery::recipe_from_hint);
        // A shell recipe alone cannot authorize losing its arbitrary running job.
        if snapshot.active && exact.is_none() {
            let accepted = relaunch_agents
                .then(|| handle.observe_agent_process())
                .flatten()
                .filter(|agent| snapshot.only_agent_tree(agent.pid))
                .and_then(|agent| crate::model::agent_recovery::fresh_recipe(&agent.recovery));
            match accepted {
                Some(recipe) => relaunches.push(crate::terminal_ipc::UpgradeRelaunch {
                    source: source.clone(),
                    recipe,
                }),
                None => {
                    blockers.push(describe_upgrade_blocker(&source));
                    continue;
                }
            }
        }
        let recipe =
            crate::upgrade::checkpoint_recipe(&source, &relaunches).map_err(upgrade_error)?;
        if let Some(execution) = execution_environment {
            validate_terminal_upgrade_environment(&recipe, &execution)?;
        }
        options.columns = snapshot.size.columns;
        options.rows = snapshot.size.rows;
        options.cell_width_px = snapshot.size.cell_width_px;
        options.cell_height_px = snapshot.size.cell_height_px;
        options.theme = snapshot.theme;
        options.initially_visible = false;
        saved.push(Terminal {
            source,
            previous_sources,
            relaunches,
            recipe,
            options,
        });
        frozen.push(snapshot);
    }
    if !blockers.is_empty() {
        return Err(upgrade_error(upgrade_blockers_message(
            &blockers,
            relaunch_agents,
        )));
    }
    let checkpoint = Checkpoint::new(proof, saved, chats);
    let counts = (
        checkpoint.terminals.len() as u64,
        checkpoint.chats.len() as u64,
    );
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state
        .validate_upgrade_capability(&capability, Instant::now())
        .map_err(upgrade_error)?;
    // Failure leaves control enabled and drops reversible pidfd freeze guards.
    store.save(&checkpoint).map_err(upgrade_error)?;
    state.upgrade = Some((store, checkpoint, frozen));
    state.shutting_down = true;
    state.shutdown_capabilities.clear();
    Ok(counts)
}

/// Provider close must succeed before Ready can authorize another writer.
pub(super) fn finish_upgrade(host: &SharedHost) -> Result<(), String> {
    let Some((store, mut checkpoint, mut frozen)) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade
        .take()
    else {
        return Ok(());
    };
    for terminal in &mut frozen {
        terminal.stop()?;
    }
    let chats = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        checkpoint
            .chats
            .iter()
            .map(|chat| {
                state
                    .chats
                    .get(&chat.source.id)
                    .map(|record| record.handle.clone())
                    .ok_or_else(|| {
                        "Checkpoint native provider disappeared during shutdown".to_string()
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    for (chat, handle) in checkpoint.chats.iter_mut().zip(chats) {
        handle.close()?;
        if handle.owns_work() {
            return Err("Native provider shutdown remains unconfirmed; checkpoint retained".into());
        }
        chat.launch.session_id = handle.confirmed_session_id();
        chat.launch.recipe = chat.launch.terminal_recipe().ok_or(
            "Closed provider did not retain its exact conversation identity; checkpoint retained",
        )?;
    }
    let handles = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .terminals
        .values()
        .map(|record| record.handle.clone())
        .collect::<Vec<_>>();
    for handle in handles {
        if let Err(error) = handle.shutdown()
            && !handle.is_completed()
        {
            return Err(error.to_string());
        }
    }
    checkpoint.phase = crate::upgrade::handoff::Phase::Ready;
    store.save(&checkpoint)
}

/// Runs while HostLease is held, before binding the public listener.
pub(super) fn restore_upgrade(host: &SharedHost) -> Result<(), String> {
    restore_upgrade_from(
        host,
        crate::upgrade::handoff::Store::discover()?,
        std::path::Path::new("/proc"),
    )
}

/// Sentinel restore failure for an operator-requested stop; see
/// [`restore_upgrade_from`].
const RESTORE_TERMINATED: &str = "daemon termination was requested during upgrade restore";

/// Replays a checkpoint, abandoning the replay as soon as termination is
/// requested.
///
/// The accept loop is the only other place that observes
/// `daemon::termination_requested`, and it is not reached until this returns, so
/// without these checks `cagents daemon stop-for-rebuild` reported "daemon
/// stopping" while the daemon kept launching checkpointed processes, and a
/// logout `SIGKILL` mid-restore left the checkpoint in `Phase::Restoring`, which
/// is never automatically replayed for native sessions. Aborting through
/// `close_failed_restore` positively closes whatever was already started and
/// leaves the checkpoint in a replayable `Ready` phase.
fn restore_upgrade_from(
    host: &SharedHost,
    store: crate::upgrade::handoff::Store,
    proc_root: &std::path::Path,
) -> Result<(), String> {
    use crate::upgrade::{ensure_no_surviving_process, handoff::Phase};
    let Some(mut checkpoint) = store.load()? else {
        return Ok(());
    };
    checkpoint.validate()?;
    if !checkpoint.chats.is_empty() {
        checkpoint.require_ready().map_err(|error| format!(
            "{error}. Evidence: {}. After inspecting the exact provider conversations, `cagents daemon abandon-upgrade --yes` archives this checkpoint and permits an empty daemon start; native sessions then require explicit manual recovery",
            store.path.display()
        ))?;
    }
    for terminal in &checkpoint.terminals {
        ensure_no_surviving_process(&terminal.source, std::process::id(), proc_root)?;
    }
    for chat in &checkpoint.chats {
        let terminal = TerminalSession {
            recovery_origin: None,
            id: chat.source.id,
            launch: chat
                .launch
                .terminal_recipe()
                .ok_or("Native exact recipe unavailable")?,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: checkpoint.proof.host_session_id.clone(),
                boot_id: checkpoint.proof.boot_id.clone(),
                host_incarnation: checkpoint.proof.host_incarnation.clone(),
            },
        };
        ensure_no_surviving_process(&terminal, std::process::id(), proc_root)?;
    }
    if crate::daemon::termination_requested() {
        tracing::warn!(
            checkpoint = %store.path.display(),
            "skipped upgrade restore because the daemon was asked to stop; the checkpoint remains replayable"
        );
        return Ok(());
    }
    checkpoint.phase = Phase::Restoring;
    store.save(&checkpoint)?;
    let restored = (|| {
        // A managed CLI connects to its thread's server. Recreate that exact
        // actor first; starting the TUI first races a socket that does not exist.
        for chat in &checkpoint.chats {
            if super::chat::owns_codex_listener(&chat.source) {
                super::chat::restore_upgrade(host, chat)?;
            }
        }
        for terminal in &checkpoint.terminals {
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            let mut session = terminal.source.clone();
            session.launch = terminal.recipe.clone();
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
            let paired = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.chats.get_mut(&session.id).is_some_and(|record| {
                    if !super::chat::owns_codex_listener(&record.session) {
                        return false;
                    }
                    record.returning_terminal = true;
                    true
                })
            };
            let descriptor =
                spawn_terminal_view(host, 0, session, terminal.options.clone(), None, paired)
                    .map_err(|error| error.message)?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if paired && let Some(chat) = state.chats.get_mut(&terminal.source.id) {
                chat.returning_terminal = false;
                chat.session.terminal = Some(descriptor.session);
            }
            let sources = &mut state
                .terminals
                .get_mut(&terminal.source.id)
                .expect("restored terminal")
                .upgrade_sources;
            sources.clone_from(&terminal.previous_sources);
            sources.push(terminal.source.clone());
            state
                .terminals
                .get_mut(&terminal.source.id)
                .expect("restored terminal")
                .upgrade_relaunches
                .clone_from(&terminal.relaunches);
            state
                .detach(0, terminal.source.id)
                .map_err(|error| error.message)?;
        }
        for chat in &checkpoint.chats {
            if super::chat::owns_codex_listener(&chat.source) {
                continue;
            }
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            super::chat::restore_upgrade(host, chat)?;
        }
        Ok::<_, String>(())
    })();
    if let Err(error) = restored {
        // A synchronous failure can be retried only after positively closing
        // every newly started writer. A crash has no such proof and retains
        // Restoring, which is never automatically replayed for native sessions.
        let closed = close_failed_restore(host).is_ok();
        if closed {
            checkpoint.phase = Phase::Ready;
            store.save(&checkpoint)?;
        }
        // Any failure observed while stopping is an abandoned replay, not a
        // broken one: report the stop and let the caller shut down promptly.
        if closed && crate::daemon::termination_requested() {
            tracing::warn!(
                checkpoint = %store.path.display(),
                %error,
                "abandoned upgrade restore because the daemon was asked to stop; the checkpoint remains replayable"
            );
            return Ok(());
        }
        return Err(format!(
            "Upgrade restore failed: {error}; checkpoint retained at {}",
            store.path.display()
        ));
    }
    store.remove()
}

fn close_failed_restore(host: &SharedHost) -> Result<(), String> {
    let (chats, terminals) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state
                .chats
                .values()
                .map(|record| record.handle.clone())
                .collect::<Vec<_>>(),
            state
                .terminals
                .values()
                .map(|record| record.handle.clone())
                .collect::<Vec<_>>(),
        )
    };
    for chat in chats {
        chat.close()?;
        if chat.owns_work() {
            return Err("Failed restore still has unconfirmed native work".into());
        }
    }
    for terminal in terminals {
        if !terminal.is_completed()
            && terminal.close_checked(true) != crate::terminal_ipc::TerminalCloseResult::Closed
        {
            return Err("Failed restore still has unconfirmed terminal processes".into());
        }
    }
    Ok(())
}

pub(crate) fn spawn_terminal(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
) -> Result<TerminalDescriptor, ProtocolError> {
    spawn_terminal_with_environment(shared, client_id, session, options, None)
}

pub(super) fn spawn_terminal_with_environment(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
) -> Result<TerminalDescriptor, ProtocolError> {
    spawn_terminal_view(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        false,
    )
}

pub(super) fn spawn_terminal_view(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
) -> Result<TerminalDescriptor, ProtocolError> {
    validate_launch(&session)?;
    validate_spawn_options(&options)?;
    let terminal_id = session.id;
    let native_handoff = execution_environment.is_some();
    let retained_environment = execution_environment.clone();
    {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reserve_spawn_for_view(terminal_id, paired)?;
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
        execution_environment,
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
    let descriptor = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let descriptor = state.finish_spawn(client_id, session, handle, options.initially_visible);
        if descriptor.is_ok() {
            state
                .terminals
                .get_mut(&terminal_id)
                .expect("finished spawn")
                .execution_environment = retained_environment;
        }
        if native_handoff {
            // A chat connection cannot consume PTY events. The GUI attaches a
            // terminal connection after checkpointing the returned live proof.
            state.subscriptions.detach(client_id, terminal_id);
        }
        descriptor
    };
    let descriptor = match descriptor {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = redraw_handle.shutdown();
            return Err(error);
        }
    };
    shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .terminals
        .get_mut(&terminal_id)
        .expect("finished spawn")
        .options = options;
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
                record_event_from(&event_host, terminal_id, event, Some(&completion_handle));
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
        TerminalSendError::PasteTooLarge(bytes) => ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            format!(
                "terminal paste of {bytes} bytes exceeds the {}-byte limit",
                crate::terminal_ipc::MAX_INPUT_BYTES
            ),
            false,
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
                .push(Outbound::broadcast(
                    proof,
                    ServerMessage::TerminalObserved {
                        terminal_id,
                        epoch: observation.epoch,
                    },
                ))
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

/// Publishes a repaint to every visible subscriber of `terminal_id`.
///
/// A subscriber for which the daemon already recorded the exact capture it last
/// delivered receives the repaint itself, diff-encoded against that capture,
/// rather than a `Redraw` notification the client would have to answer with a
/// `ClientRequest::Snapshot` naming the very base the daemon already knew. That
/// request carried no information and cost a socket round trip on every frame,
/// so steady-state frame rate was set by IPC latency instead of by work.
/// `Redraw` remains the message for a subscriber with no recorded base — attach,
/// resume and reveal still fetch a full grid through the request path.
///
/// The acknowledge/credit protocol is untouched: the worker still withholds the
/// next capture until the client acknowledges this generation, so the daemon
/// cannot run ahead of the UI. `last_sent` advances at push time, which keeps
/// every delta chained to the previous delivery; an unapplicable delta is
/// detected by `base_generation` and answered with a full repaint, so a client
/// that misses a push still recovers.
fn broadcast_redraw(host: &Weak<Mutex<HostState>>, terminal_id: TerminalId, generation: u64) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let mut pending = Vec::new();
    {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let visible = state
            .subscriptions
            .by_terminal
            .get(&terminal_id)
            .into_iter()
            .flat_map(|clients| clients.iter())
            .filter(|(_, visibility)| **visibility == TerminalVisibility::Visible)
            .map(|(client_id, _)| *client_id)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            // Serialize visibility commands with attachment/reveal changes.
            // Otherwise this Hide could overtake a newly attached UI's Reveal.
            if state.subscriptions.visibility(terminal_id) == TerminalVisibility::Hidden
                && let Some(record) = state.terminals.get(&terminal_id)
            {
                let _ = record.handle.hide();
            }
            return;
        }
        let proof = state.proof_handle();
        // One capture serves every subscriber; only its encoding differs, and a
        // row the worker did not rebuild is the same `Arc` in every delta.
        let capture = state
            .terminals
            .get(&terminal_id)
            .map(|record| record.handle.snapshot());
        for client_id in visible {
            let Some(delivery) = state.clients.get(&client_id).cloned() else {
                continue;
            };
            let snapshot = match (
                &capture,
                state.subscriptions.last_sent(client_id, terminal_id),
            ) {
                (Some(capture), Some(base))
                    if !capture.detached && capture.generation > base.generation =>
                {
                    Some(TerminalSnapshot::diff(capture, base))
                }
                _ => None,
            };
            let message = match snapshot {
                Some(snapshot) => {
                    let capture = capture.as_ref().expect("diffed capture");
                    state
                        .subscriptions
                        .record_sent(client_id, terminal_id, Arc::clone(capture));
                    ServerMessage::Snapshot {
                        terminal_id,
                        snapshot,
                    }
                }
                None => ServerMessage::Redraw {
                    terminal_id,
                    generation,
                },
            };
            pending.push((delivery, Outbound::broadcast(&proof, message)));
        }
    }
    for (delivery, message) in pending {
        let _ = delivery.push(message);
    }
}

#[cfg(test)]
fn record_event(host: &Weak<Mutex<HostState>>, terminal_id: TerminalId, event: TerminalEvent) {
    record_event_from(host, terminal_id, event, None);
}
fn record_event_from(
    host: &Weak<Mutex<HostState>>,
    terminal_id: TerminalId,
    event: TerminalEvent,
    worker: Option<&TerminalHandle>,
) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let wire_event = WireTerminalEvent::from(event);
    let (proof, sequence, deliveries) = {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let client_ids = state.subscriptions.clients(terminal_id);
        let proof = state.proof_handle();
        let Some(record) = state.terminals.get_mut(&terminal_id) else {
            return;
        };
        if worker.is_some_and(|worker| !record.handle.same_worker(worker)) {
            return;
        }
        if let WireTerminalEvent::ResumeHintChanged { hint } = &wire_event {
            record.session.resume_hint = hint.clone();
        }
        record.event_sequence = record.event_sequence.saturating_add(1);
        let sequence = record.event_sequence;
        if let WireTerminalEvent::Exited {
            exit_code, signal, ..
        } = &wire_event
        {
            record.running = false;
            record.last_exit = Some(TerminalExit {
                code: signal
                    .is_none()
                    .then(|| i32::try_from(*exit_code).ok())
                    .flatten(),
                signal: signal.clone(),
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
    let message = ServerMessage::Event {
        terminal_id,
        sequence,
        event: wire_event,
    };
    for delivery in deliveries {
        let _ = delivery.push(Outbound::broadcast(&proof, message.clone()));
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

    fn test_host_with_incarnation(incarnation: &str) -> HostState {
        HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("test-session"),
            boot_id: BootId::new("test-boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        })
    }

    fn test_host() -> HostState {
        test_host_with_incarnation("test-incarnation")
    }

    #[test]
    fn upgrade_agent_relaunch_requires_opt_in_and_survives_repeated_restore() {
        use crate::upgrade::handoff::Store;
        let directory = tempfile::tempdir().unwrap();
        // A real foreground process, recognized by its provider argv. Its
        // executable is deliberately different from the recovery command.
        let executable = directory.path().join("claude");
        std::os::unix::fs::symlink("/bin/cat", &executable).unwrap();
        let recipe = crate::model::LaunchRecipe {
            program: executable.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([(
                "HOME".into(),
                directory.path().to_string_lossy().into_owned(),
            )]),
        };
        let source = TerminalSession {
            recovery_origin: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let mut host = Arc::new(Mutex::new(test_host()));
        spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
        let empty_proc = directory.path().join("proc");
        fs::create_dir(&empty_proc).unwrap();
        for incarnation in ["second", "third"] {
            let handle = host.lock().unwrap().terminals[&source.id].handle.clone();
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.observe_agent_process().is_none() {
                assert!(Instant::now() < deadline, "agent never became foreground");
                thread::sleep(Duration::from_millis(10));
            }
            let capability = host
                .lock()
                .unwrap()
                .shutdown_preflight(Instant::now())
                .unwrap()
                .capability;
            assert!(
                prepare_upgrade_with_policy(&host, capability, false, || Store::open(
                    directory.path()
                ))
                .is_err()
            );
            assert!(!handle.is_completed(), "refusal must preserve the agent");
            let capability = host
                .lock()
                .unwrap()
                .shutdown_preflight(Instant::now())
                .unwrap()
                .capability;
            assert_eq!(
                prepare_upgrade_with_policy(&host, capability, true, || Store::open(
                    directory.path()
                ))
                .unwrap(),
                (1, 0)
            );
            finish_upgrade(&host).unwrap();
            let saved = Store::open(directory.path())
                .unwrap()
                .load()
                .unwrap()
                .unwrap();
            assert_eq!(
                saved.terminals[0].recipe.program,
                executable.to_string_lossy()
            );
            assert!(!saved.terminals[0].relaunches.is_empty());
            host = Arc::new(Mutex::new(test_host_with_incarnation(incarnation)));
            restore_upgrade_from(&host, Store::open(directory.path()).unwrap(), &empty_proc)
                .unwrap();
            let descriptor = host.lock().unwrap().descriptor(0, source.id).unwrap();
            assert!(!descriptor.upgrade_relaunches.is_empty());
        }
        let handle = host.lock().unwrap().terminals[&source.id].handle.clone();
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
    }

    #[test]
    fn native_upgrade_retains_hidden_lineage_and_restarts_final_claude_conversation_once() {
        use crate::{
            agent::Provider,
            chat::{Command, Launch, Session},
            model::LaunchRecipe,
            terminal_ipc::ClientRequest,
            upgrade::handoff::Store,
        };
        use std::os::unix::fs::PermissionsExt;
        const A: &str = "12345678-1234-4234-8234-123456789abc";
        const B: &str = "98765432-1234-4234-8234-123456789abc";
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().canonicalize().unwrap();
        let executable = cwd.join("claude");
        let log = cwd.join("launches");
        fs::write(&executable, r#"#!/usr/bin/env python3
import json, os, sys
identity = next(arg.split("=", 1)[1] for arg in sys.argv if arg.startswith("--resume="))
with open(os.environ["LAUNCH_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
print(json.dumps({"type": "system", "subtype": "init", "session_id": identity}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    if request["request"]["subtype"] == "interrupt":
        print(json.dumps({"type": "conversation_reset", "session_id": identity}), flush=True)
        identity = os.environ["NEXT_SESSION"]
        print(json.dumps({"type": "system", "subtype": "init", "session_id": identity}), flush=True)
    print(json.dumps({"type": "control_response", "response": {"subtype": "success", "request_id": request["request_id"], "response": {}}}), flush=True)
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let config = cwd.join("config");
        let project: String = cwd
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let history = config.join("projects").join(project);
        fs::create_dir_all(&history).unwrap();
        for id in [A, B] {
            fs::write(history.join(format!("{id}.jsonl")), "").unwrap();
        }
        let host = Arc::new(Mutex::new(test_host()));
        let source = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    program: executable.to_string_lossy().into_owned(),
                    arguments: vec!["--resume".into(), A.into()],
                    working_directory: cwd.clone(),
                    environment: BTreeMap::from([
                        (
                            "CLAUDE_CONFIG_DIR".into(),
                            config.to_string_lossy().into_owned(),
                        ),
                        ("LAUNCH_LOG".into(), log.to_string_lossy().into_owned()),
                        ("NEXT_SESSION".into(), B.into()),
                    ]),
                },
                session_id: Some(A.into()),
            },
            proof: Some(
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .proof()
                    .clone(),
            ),
            terminal: None,
        };
        super::super::chat::process(
            &host,
            1,
            ClientRequest::ChatStart {
                session: source.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
        .unwrap();
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&source.id]
            .handle
            .clone();
        let wait_for_identity = |expected: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.confirmed_session_id().as_deref() != Some(expected) {
                assert!(
                    Instant::now() < deadline,
                    "provider failed to confirm {expected}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for_identity(A);
        // The hidden actor changes conversations without a presentation read
        // refreshing its persisted card's original launch or cached identity.
        handle.command(Command::Interrupt).unwrap();
        wait_for_identity(B);
        let store = Store::open(&cwd.join("checkpoint")).unwrap();
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        prepare_upgrade_with(&host, capability, || Ok(store)).unwrap();
        finish_upgrade(&host).unwrap();
        let store = Store::open(&cwd.join("checkpoint")).unwrap();
        let saved = store.load().unwrap().unwrap();
        saved.require_ready().unwrap();
        assert_eq!(saved.chats[0].source, source);
        assert_eq!(saved.chats[0].launch.session_id.as_deref(), Some(B));
        let empty_proc = cwd.join("proc");
        fs::create_dir(&empty_proc).unwrap();
        let replacement = test_host_with_incarnation("replacement-incarnation");
        let replacement = Arc::new(Mutex::new(replacement));
        restore_upgrade_from(&replacement, store, &empty_proc).unwrap();
        let restored = super::super::chat::resolve_upgrade(&replacement, &source).unwrap();
        assert_eq!(restored.launch.session_id.as_deref(), Some(B));
        assert_ne!(restored.proof, source.proof);
        let mut hidden = source.clone();
        hidden.launch.session_id = None;
        assert_eq!(
            super::super::chat::resolve_upgrade(&replacement, &hidden).unwrap(),
            restored
        );
        let mut changed = source.clone();
        changed.launch.recipe.arguments = vec!["--resume".into(), B.into()];
        assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
        changed = source.clone();
        changed.proof = restored.proof.clone();
        assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
        changed = source.clone();
        changed.id = TerminalId::new();
        assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
        changed = source.clone();
        changed.terminal = Some(TerminalSession {
            recovery_origin: None,
            id: source.id,
            launch: source.launch.recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: source.launch.recipe.clone(),
                last_exit: None,
            },
        });
        assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
        restore_upgrade_from(
            &replacement,
            Store::open(&cwd.join("checkpoint")).unwrap(),
            &empty_proc,
        )
        .unwrap();
        // The durable P1 card stays closed at P2. A second full handoff must
        // carry its issued launch/proof, not just the most recent P2 source.
        let capability = replacement
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        prepare_upgrade_with(&replacement, capability, || {
            Store::open(&cwd.join("checkpoint"))
        })
        .unwrap();
        finish_upgrade(&replacement).unwrap();
        let store = Store::open(&cwd.join("checkpoint")).unwrap();
        let second = store.load().unwrap().unwrap();
        assert_eq!(second.chats[0].previous_sources[0].source, source);
        let mut forged = second.clone();
        forged.chats[0].previous_sources[0].source.id = TerminalId::new();
        assert!(forged.validate().is_err());
        let mut forged = second.clone();
        forged.chats[0].previous_sources[0]
            .source
            .launch
            .recipe
            .environment
            .insert("UNRELATED".into(), "1".into());
        assert!(forged.validate().is_err());
        let mut forged = second.clone();
        forged.chats[0].previous_sources[0].source.proof = second.chats[0].source.proof.clone();
        assert!(forged.validate().is_err());
        let third = test_host_with_incarnation("third-incarnation");
        let replacement = Arc::new(Mutex::new(third));
        restore_upgrade_from(&replacement, store, &empty_proc).unwrap();
        let third = super::super::chat::resolve_upgrade(&replacement, &source).unwrap();
        assert_eq!(third.launch.session_id.as_deref(), Some(B));
        assert_ne!(third.proof, restored.proof);
        assert_eq!(
            super::super::chat::resolve_upgrade(&replacement, &hidden).unwrap(),
            third
        );
        assert_eq!(
            super::super::chat::resolve_upgrade(&replacement, &restored).unwrap(),
            third
        );
        assert!(super::super::chat::resolve_upgrade(&replacement, &changed).is_err());
        let mut unrelated = source.clone();
        unrelated.proof = third.proof.clone();
        assert!(super::super::chat::resolve_upgrade(&replacement, &unrelated).is_err());
        unrelated = source.clone();
        unrelated.launch.recipe.arguments = vec!["--resume".into(), B.into()];
        assert!(super::super::chat::resolve_upgrade(&replacement, &unrelated).is_err());
        restore_upgrade_from(
            &replacement,
            Store::open(&cwd.join("checkpoint")).unwrap(),
            &empty_proc,
        )
        .unwrap();
        let launches: Vec<Vec<String>> = fs::read_to_string(log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            launches.len(),
            3,
            "consumed checkpoint spawned another writer"
        );
        assert!(launches[0].contains(&format!("--resume={A}")));
        assert!(launches[1].contains(&format!("--resume={B}")));
        assert!(!launches[1].iter().any(|arg| arg.contains(A)));
        assert!(launches[2].contains(&format!("--resume={B}")));
        assert!(!launches[2].iter().any(|arg| arg.contains(A)));
        let restored_handle = replacement.lock().unwrap_or_else(|p| p.into_inner()).chats
            [&source.id]
            .handle
            .clone();
        restored_handle.close().unwrap();
    }

    #[test]
    fn upgrade_commit_failure_preserves_live_control_authority() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
        // An open directory descriptor is retained but cannot create files after
        // unlink. This fails the real persistence operation, not a mock echo.
        fs::remove_dir(directory.path()).unwrap();
        let host = Arc::new(Mutex::new(test_host()));
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        assert!(prepare_upgrade_with(&host, capability, || Ok(store)).is_err());
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        assert!(!state.is_shutting_down());
        assert!(state.reserve_spawn(TerminalId::new()).is_ok());
    }

    #[test]
    fn upgrade_rejects_stale_inventory_and_pending_reservations_without_shutdown() {
        let mut state = test_host();
        let now = Instant::now();
        let stale = state.shutdown_preflight(now).unwrap().capability;
        let id = TerminalId::new();
        state.reserve_spawn(id).unwrap();
        assert!(state.validate_upgrade_capability(&stale, now).is_err());
        let pending = state.shutdown_preflight(now).unwrap().capability;
        assert!(state.validate_upgrade_capability(&pending, now).is_err());
        state.cancel_spawn(id);
        let current = state.shutdown_preflight(now).unwrap().capability;
        assert!(state.validate_upgrade_capability(&current, now).is_ok());
        assert!(!state.shutting_down);
    }

    #[test]
    fn committed_upgrade_fences_control_and_retains_checkpoint_until_close() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
        let host = Arc::new(Mutex::new(test_host()));
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        assert_eq!(
            prepare_upgrade_with(&host, capability, || Ok(store)).unwrap(),
            (0, 0)
        );
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .reserve_spawn(TerminalId::new())
                .is_err()
        );
        let checkpoint = crate::upgrade::handoff::Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(checkpoint.require_ready().is_err());
        finish_upgrade(&host).unwrap();
        crate::upgrade::handoff::Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
            .require_ready()
            .unwrap();
    }

    #[test]
    fn a_handoff_no_window_can_still_save_stops_refusing_the_upgrade() {
        use crate::{
            agent::Provider,
            chat::{Launch, Session},
            model::LaunchRecipe,
            upgrade::handoff::Store,
        };
        let directory = tempfile::tempdir().unwrap();
        let host = Arc::new(Mutex::new(test_host()));
        let recipe = LaunchRecipe {
            program: "omp".into(),
            arguments: Vec::new(),
            working_directory: directory.path().to_owned(),
            environment: BTreeMap::new(),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Omp,
                recipe: recipe.clone(),
                session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
            },
            proof: Some(
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .proof()
                    .clone(),
            ),
            terminal: None,
        };
        let replacement = TerminalSession {
            recovery_origin: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let chat = source.id;
        let mut retain = |deadline| {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.chat_handoffs.insert(
                chat,
                crate::host::chat::TerminalHandoff::new(
                    source.clone(),
                    replacement.clone(),
                    1,
                    BTreeMap::new(),
                    deadline,
                ),
            );
            state.shutdown_preflight(Instant::now()).unwrap().capability
        };
        let live = retain(Instant::now() + Duration::from_secs(3600));
        let failure =
            prepare_upgrade_with(&host, live, || Store::open(directory.path())).unwrap_err();
        assert!(failure.message.contains("not durably acknowledged"));
        // Nothing is destroyed by a refused upgrade: the window this receipt
        // belongs to can still replay its replacement afterwards.
        assert!(
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .chat_handoffs
                .contains_key(&chat)
        );
        let expired = retain(Instant::now());
        assert_eq!(
            prepare_upgrade_with(&host, expired, || Store::open(directory.path())).unwrap(),
            (0, 0)
        );
        Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn restored_shell_links_exact_old_source_and_does_not_replay_consumed_checkpoint() {
        use crate::upgrade::handoff::{Checkpoint, Phase, Store, Terminal};
        let directory = tempfile::tempdir().unwrap();
        let empty_proc = directory.path().join("proc");
        fs::create_dir(&empty_proc).unwrap();
        let old = test_host().proof().clone();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        let source = TerminalSession {
            recovery_origin: None,
            id: TerminalId::new(),
            launch: recipe.clone(),
            transcript_path: Some(directory.path().join("shell.log")),
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: old.host_session_id.clone(),
                boot_id: old.boot_id.clone(),
                host_incarnation: old.host_incarnation.clone(),
            },
        };
        let mut checkpoint = Checkpoint::new(
            old,
            vec![Terminal {
                source: source.clone(),
                previous_sources: Vec::new(),
                relaunches: Vec::new(),
                recipe,
                options: TerminalSpawnOptions::default(),
            }],
            Vec::new(),
        );
        checkpoint.phase = Phase::Ready;
        let store = Store::open(directory.path()).unwrap();
        store.save(&checkpoint).unwrap();
        let replacement = test_host_with_incarnation("replacement-incarnation");
        let expected = (*replacement.proof).clone();
        let host = Arc::new(Mutex::new(replacement));
        restore_upgrade_from(&host, store, &empty_proc).unwrap();
        let descriptor = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .descriptor(0, source.id)
            .unwrap();
        assert_eq!(descriptor.upgrade_sources, vec![source.clone()]);
        assert_eq!(descriptor.session.continuity.live_proof(), Some(expected));
        assert_eq!(descriptor.session.transcript_path, source.transcript_path);
        restore_upgrade_from(&host, Store::open(directory.path()).unwrap(), &empty_proc).unwrap();
        let capability = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        prepare_upgrade_with(&host, capability, || Store::open(directory.path())).unwrap();
        finish_upgrade(&host).unwrap();
        let store = Store::open(directory.path()).unwrap();
        let second = store.load().unwrap().unwrap();
        let mut forged = second.clone();
        forged.terminals[0].previous_sources[0].id = TerminalId::new();
        assert!(forged.validate().is_err());
        let mut forged = second.clone();
        forged.terminals[0].previous_sources[0]
            .launch
            .environment
            .insert("UNRELATED".into(), "1".into());
        assert!(forged.validate().is_err());
        let mut forged = second.clone();
        forged.terminals[0].previous_sources[0].continuity =
            second.terminals[0].source.continuity.clone();
        assert!(forged.validate().is_err());
        let third = test_host_with_incarnation("third-incarnation");
        let host = Arc::new(Mutex::new(third));
        restore_upgrade_from(&host, store, &empty_proc).unwrap();
        let third = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .descriptor(0, source.id)
            .unwrap();
        assert_eq!(
            third.upgrade_sources,
            vec![source.clone(), second.terminals[0].source.clone()]
        );
        assert_ne!(
            third.session.continuity.live_proof(),
            descriptor.session.continuity.live_proof()
        );
        let handle = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.terminals.len(), 1);
            state.terminals[&source.id].handle.clone()
        };
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
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
            recovery_origin: None,
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
            recovery_origin: None,
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

    /// A visible subscriber whose delta base the daemon already recorded must
    /// receive the repaint itself. Sending `Redraw` made the client request the
    /// very base the daemon had just diffed against, which cost a socket round
    /// trip on every frame.
    #[test]
    fn a_recorded_base_turns_a_redraw_edge_into_a_pushed_repaint() {
        let directory = tempfile::tempdir().unwrap();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        let session = TerminalSession {
            recovery_origin: None,
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
        let shared = Arc::new(Mutex::new(test_host()));
        let (delivery, handle, base) = {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            let delivery = state.register_client(1);
            state.reserve_spawn(id).unwrap();
            let handle = TerminalHandle::spawn(
                session.clone(),
                TerminalConfig {
                    initially_visible: true,
                    ..TerminalConfig::default()
                },
                |_| {},
            )
            .unwrap();
            state
                .finish_spawn(1, session, handle.clone(), true)
                .unwrap();
            // Stand in for the request path that delivered this capture.
            let base = handle.snapshot();
            state.subscriptions.record_sent(1, id, Arc::clone(&base));
            (delivery, handle, base)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.generation() <= base.generation {
            assert!(Instant::now() < deadline, "the terminal never repainted");
            // The worker withholds the next capture until the presented
            // generation is acknowledged; the UI does this after drawing.
            handle.acknowledge_redraw(handle.generation());
            thread::sleep(Duration::from_millis(10));
        }

        broadcast_redraw(&Arc::downgrade(&shared), id, handle.generation());
        delivery.close_after_drain();
        let mut pushed = None;
        while let Some(message) = delivery.pop() {
            if let ServerMessage::Snapshot {
                terminal_id,
                snapshot,
            } = message.message
                && terminal_id == id
            {
                pushed = Some(snapshot);
            }
        }
        let pushed = pushed.expect("the redraw edge did not carry the repaint");
        assert!(pushed.generation > base.generation);
        // Whatever encoding was chosen must resolve against the recorded base,
        // or the saved round trip would cost a full repaint instead.
        pushed.apply(Some(&base)).unwrap();
        handle.shutdown().unwrap();
    }

    fn empty_stop() -> ShutdownOperation {
        ShutdownOperation::Stop {
            require_empty: true,
            expected_generation: None,
        }
    }

    /// A plain `cagents daemon stop` that native work refuses must say what it
    /// actually wants — a confirmed stop — instead of reporting the
    /// `require_empty` demand the caller never made.
    #[test]
    fn a_stop_refused_for_native_work_names_confirmation_unless_emptiness_was_demanded() {
        let directory = tempfile::tempdir().unwrap();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        for (require_empty, expected) in [
            (
                false,
                ShutdownOutcome::ConfirmationRequired { current_count: 1 },
            ),
            (true, ShutdownOutcome::EmptyRequired { current_count: 1 }),
        ] {
            let mut state = test_host();
            state.chat_spawning.insert(
                TerminalId::new(),
                crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: recipe.clone(),
                    session_id: None,
                },
            );
            assert_eq!(
                state.shutdown(
                    ShutdownOperation::Stop {
                        require_empty,
                        expected_generation: None,
                    },
                    Instant::now()
                ),
                expected
            );
            assert!(!state.shutting_down);
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
            recovery_origin: None,
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
                intentional: false,
            },
        );
        let capability = {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state
                .set_visibility(1, id, TerminalVisibility::Visible)
                .unwrap();
            let generation = handle.generation();
            assert!(state.redraw_ack_handle(1, id, generation).is_ok());
            assert!(state.focus_handle(1, id).unwrap().is_some());
            assert!(
                state.surface_handle(1, id).is_ok(),
                "a descendant's live PTY retains input and scrolling"
            );
            assert!(
                state
                    .descriptor(1, id)
                    .unwrap()
                    .session
                    .continuity
                    .live_proof()
                    .is_some()
            );
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
                recovery_origin: None,
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
            recovery_origin: None,
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
        let mut restarted = test_host_with_incarnation("new-incarnation");
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
