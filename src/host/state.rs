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
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        ShutdownCapability, ShutdownOperation, ShutdownOutcome, ShutdownPreflight,
        TerminalDescriptor, TerminalSnapshot, TerminalSpawnOptions, TerminalVisibility,
        WireTerminalEvent,
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
    screen_lost_after_crash: bool,
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
    /// Stable provider metadata must survive eviction from the transition log.
    conversation_title: Option<(u64, String)>,
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
    recover_holders: bool,
    /// Shared, never mutated: a redraw broadcast hands the same proof to every
    /// subscriber's queue, and cloning three `String`s per subscriber per frame
    /// made per-frame allocation scale with the number of attached views.
    proof: Arc<HostSessionProof>,
    pub(super) terminals: BTreeMap<TerminalId, TerminalRecord>,
    spawning: BTreeSet<TerminalId>,
    /// Covers the interval before a login's live TerminalRecord exists.
    browser_logins_spawning: BTreeSet<TerminalId>,
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
    pub(super) execution_registry: Option<Arc<Mutex<super::model::Registry>>>,
    pub(super) execution_gates: BTreeMap<crate::model::AgentId, Arc<Mutex<()>>>,
    pub(super) execution_startups: BTreeMap<
        crate::model::AgentId,
        (
            crate::agent_execution::HostedSession,
            Arc<crate::chat::protocol::ExecutionContext>,
        ),
    >,
    /// Connections that asked for shared-model edges, kept separately from
    /// `watchers` because watching terminals and watching the model are
    /// independent subscriptions with independent clients - the CLI reads the
    /// model without subscribing at all.
    model_watchers: BTreeSet<ClientId>,
    pub(super) notifications: Option<super::notifications::Handle>,
    pub(super) notification_closed: BTreeSet<TerminalId>,
    notification_watchers: BTreeMap<ClientId, crate::notifications::Presence>,
    notification_activation: BTreeMap<String, TerminalId>,
    subscriptions: Subscriptions,
    pub(super) shutting_down: bool,
    shutdown_requested: bool,
    pub(super) inventory_generation: u64,
    shutdown_capabilities: VecDeque<(ShutdownCapability, Instant)>,
    pub(super) chats: BTreeMap<TerminalId, super::chat::Record>,
    pub(super) chat_handoffs: BTreeMap<TerminalId, super::chat::TerminalHandoff>,
    pub(super) chat_spawning: BTreeMap<TerminalId, crate::chat::Session>,
    pub(super) chat_bindings: BTreeMap<TerminalId, super::chat::PendingBinding>,
    pub(super) native_closing: BTreeMap<TerminalId, super::chat::NativeClose>,
    pub(super) native_adoption_failures: super::chat::AdoptionFailures,
    pub(crate) agent_observer: Arc<crate::agent::observe::Observer>,
    pub(super) agent_activity_wake: Arc<super::activity::Wake>,
    /// Exclusive upgrade barrier; ordinary client operations remain concurrent.
    pub(super) control_gate: Arc<RwLock<()>>,
    upgrade: Option<PreparedUpgrade>,
    pub(super) upgrade_chats: BTreeMap<TerminalId, Vec<crate::upgrade::handoff::ChatSource>>,
}

/// A checkpoint saved and awaiting this daemon's exit.
struct PreparedUpgrade {
    store: crate::upgrade::handoff::Store,
    checkpoint: crate::upgrade::handoff::Checkpoint,
    /// The relaunched terminals' processes, stopped only once the checkpoint
    /// is final.
    frozen: Vec<crate::terminal::UpgradeSnapshot>,
    relaunch_agents: bool,
    /// Where carried terminals' holders wait, prepared before the checkpoint.
    holders: Option<std::path::PathBuf>,
}

impl HostState {
    pub(super) fn enable_holder_recovery(&mut self) {
        self.recover_holders = true;
    }
    pub(crate) fn new(proof: HostSessionProof) -> Self {
        Self {
            recover_holders: false,
            proof: Arc::new(proof),
            terminals: BTreeMap::new(),
            spawning: BTreeSet::new(),
            browser_logins_spawning: BTreeSet::new(),
            clients: BTreeMap::new(),
            watchers: BTreeSet::new(),
            model: None,
            execution_registry: None,
            execution_gates: BTreeMap::new(),
            execution_startups: BTreeMap::new(),
            model_watchers: BTreeSet::new(),
            notifications: None,
            notification_closed: BTreeSet::new(),
            notification_watchers: BTreeMap::new(),
            notification_activation: BTreeMap::new(),
            subscriptions: Subscriptions::default(),
            shutting_down: false,
            shutdown_requested: false,
            inventory_generation: 0,
            shutdown_capabilities: VecDeque::new(),
            chats: BTreeMap::new(),
            chat_handoffs: BTreeMap::new(),
            chat_spawning: BTreeMap::new(),
            chat_bindings: BTreeMap::new(),
            native_closing: BTreeMap::new(),
            native_adoption_failures: Default::default(),
            agent_observer: Arc::new(crate::agent::observe::Observer::new()),
            agent_activity_wake: Arc::new(super::activity::Wake::default()),
            control_gate: Arc::new(RwLock::new(())),
            upgrade: None,
            upgrade_chats: BTreeMap::new(),
        }
    }

    pub(crate) fn proof(&self) -> &HostSessionProof {
        &self.proof
    }

    pub(super) fn has_committed_upgrade(&self) -> bool {
        self.upgrade.is_some()
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
        if self.notification_watchers.remove(&client_id).is_some()
            && let Some(notifications) = &self.notifications
        {
            notifications.presence(client_id, None);
        }
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
        self.execution_registry = match registry.execution_connection() {
            Ok(connection) => Some(Arc::new(Mutex::new(connection))),
            Err(error) => {
                tracing::error!(%error, "cannot open the agent execution registry");
                None
            }
        };
        self.model = Some(registry);
    }

    pub(super) fn notification_snapshot(&self) -> crate::notifications::Snapshot {
        self.notifications.as_ref().map_or_else(
            || crate::notifications::Snapshot {
                diagnostic: Some("The daemon notification service is unavailable".into()),
                ..Default::default()
            },
            super::notifications::Handle::snapshot,
        )
    }

    pub(super) fn watch_notifications(
        &mut self,
        client: ClientId,
        presence: crate::notifications::Presence,
    ) -> Option<TerminalId> {
        let activation = self.notification_activation.remove(&presence.window);
        if let Some(notifications) = &self.notifications {
            notifications.presence(client, Some(presence.clone()));
        }
        self.notification_watchers.insert(client, presence);
        activation
    }

    pub(super) fn notification_presence(&mut self, presence: crate::notifications::Presence) {
        for (&client, current) in &mut self.notification_watchers {
            if current.window == presence.window {
                *current = presence.clone();
                if let Some(notifications) = &self.notifications {
                    notifications.presence(client, Some(presence.clone()));
                }
            }
        }
    }

    pub(super) fn notifications_changed(&self) {
        for client in self.notification_watchers.keys() {
            if let Some(delivery) = self.clients.get(client) {
                let _ = delivery.push(ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof().clone(),
                    message: ServerMessage::NotificationsChanged,
                });
            }
        }
    }

    pub(super) fn activate_notification(
        &mut self,
        session: TerminalId,
        remembered_window: Option<&str>,
    ) -> String {
        let owner = self
            .notification_watchers
            .iter()
            .filter(|(_, presence)| presence.sessions.contains(&session))
            .max_by_key(|(_, presence)| {
                (
                    presence.focused == Some(session),
                    Some(presence.window.as_str()) == remembered_window,
                )
            });
        if let Some((&client, presence)) = owner {
            let window = presence.window.clone();
            if let Some(delivery) = self.clients.get(&client) {
                let _ = delivery.push(ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof().clone(),
                    message: ServerMessage::NotificationActivated { session },
                });
            }
            return window;
        }
        let window = remembered_window
            .unwrap_or(crate::instance::PRIMARY_WINDOW)
            .to_owned();
        if self.notification_activation.len() < 32
            || self.notification_activation.contains_key(&window)
        {
            self.notification_activation.insert(window.clone(), session);
        }
        window
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
    pub(super) fn announce_model_edit(&self, origin: ClientId, sequence: u64, edit: &ModelEdit) {
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
        if self.notification_watchers.remove(&client_id).is_some()
            && let Some(notifications) = &self.notifications
        {
            notifications.presence(client_id, None);
        }
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
            screen_lost_after_crash: record.screen_lost_after_crash,
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
        self.reserve_spawn_for_view(terminal_id, None)
    }

    pub(super) fn retire_terminal(&mut self, terminal_id: TerminalId) -> Option<TerminalRecord> {
        let record = self.terminals.remove(&terminal_id)?;
        self.notification_closed.remove(&terminal_id);
        if let Some(notifications) = &self.notifications {
            notifications.retire(terminal_id);
        }
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

    pub(super) fn close_terminal_attention(
        &mut self,
        terminal_id: TerminalId,
        expected: Option<&TerminalHandle>,
    ) {
        // Completed descriptors may remain available for history. Explicit
        // closure still withdraws their alerts and fences in-flight observers.
        // Shutdown waits outside this lock: an old receipt must not suppress a
        // replacement worker that acquired the same durable session meanwhile.
        match self.terminals.get(&terminal_id) {
            Some(record) if expected.is_some_and(|handle| record.handle.same_worker(handle)) => {
                self.notification_closed.insert(terminal_id);
            }
            Some(_) => return,
            None => {}
        }
        if let Some(notifications) = &self.notifications {
            notifications.retire(terminal_id);
        }
    }

    fn reserve_spawn_for_view(
        &mut self,
        terminal_id: TerminalId,
        paired: Option<&TerminalSession>,
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
                    && !paired.is_some_and(|terminal| {
                        record.accepts_execution_console(terminal)
                            || (record.returning_terminal
                                && record.session.launch.provider == crate::agent::Provider::Codex)
                    })
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
        self.browser_logins_spawning.remove(&terminal_id);
        if self.spawning.remove(&terminal_id) {
            self.inventory_generation += 1;
        }
    }

    fn reserve_browser_login(&mut self, terminal_id: TerminalId) -> Result<(), ProtocolError> {
        if !self.browser_logins_spawning.is_empty()
            || self.terminals.values().any(|record| {
                crate::account::login::is_codex_browser(&record.session.launch)
                    && !record.handle.is_completed()
            })
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                crate::account::login::BUSY,
                false,
            ));
        }
        self.browser_logins_spawning.insert(terminal_id);
        Ok(())
    }

    /// Records that a restored terminal continues `source`, so clients that
    /// still hold the previous daemon's session find it again.
    fn adopt_upgrade_lineage(
        &mut self,
        source: &TerminalSession,
        previous_sources: &[TerminalSession],
        relaunches: &[crate::terminal_ipc::UpgradeRelaunch],
    ) -> Result<(), ProtocolError> {
        let record = self
            .terminals
            .get_mut(&source.id)
            .ok_or_else(|| not_found(source.id))?;
        record.upgrade_sources = previous_sources.to_vec();
        record.upgrade_sources.push(source.clone());
        record.upgrade_relaunches = relaunches.to_vec();
        self.detach(0, source.id)
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
        self.notification_closed.remove(&terminal_id);
        self.terminals.insert(
            terminal_id,
            TerminalRecord {
                screen_lost_after_crash: false,
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
                conversation_title: None,
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
        let title = record
            .conversation_title
            .iter()
            .filter(|(sequence, _)| {
                *sequence > after_sequence
                    && record
                        .events
                        .front()
                        .is_none_or(|(first, _)| sequence < first)
            })
            .map(|(sequence, title)| {
                (
                    *sequence,
                    WireTerminalEvent::ConversationTitleChanged {
                        title: title.clone(),
                    },
                )
            });
        let events = record
            .events
            .iter()
            .filter(|(sequence, _)| *sequence > after_sequence)
            .cloned();
        Ok(title.chain(events).collect())
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

/// How an upgrade restarts an agent it cannot resume exactly. Only an agent
/// its provider says has no conversation yet starts a fresh one; any other
/// reopens the provider's resume picker, so a conversation the agent moved to
/// without cagents seeing it is still one choice away rather than lost.
fn upgrade_relaunch(
    agent: &crate::model::agent_recovery::AgentProcess,
) -> Option<(crate::model::LaunchRecipe, &'static str)> {
    use crate::model::agent_recovery;
    if agent_recovery::fresh_omp_session(agent).is_some() {
        return agent_recovery::fresh_recipe(&agent.recovery).map(|recipe| (recipe, "fresh"));
    }
    agent_recovery::picker_recipe(&agent.recovery).map(|recipe| (recipe, "resume picker"))
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
        " Identified agents can instead be relaunched into their resume picker: turn on Relaunch agents on upgrade in Settings, or pass --relaunch-agents. Unrelated shell jobs still block the upgrade:"
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
    let holders = holder_directory()
        .inspect_err(|error| {
            tracing::warn!(%error, "no directory for holders; upgrade restarts every terminal")
        })
        .ok();
    prepare_upgrade_with_policy(
        host,
        capability,
        relaunch_agents,
        holders.as_deref(),
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
    recipe.remove_unset_environment(&mut restored);
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
    prepare_upgrade_with_policy(host, capability, false, None, discover)
}

/// `holders` is where carried terminals' holders wait; without it, every
/// terminal is restarted as it was before carrying.
fn prepare_upgrade_with_policy(
    host: &SharedHost,
    capability: ShutdownCapability,
    relaunch_agents: bool,
    holders: Option<&std::path::Path>,
    discover: impl FnOnce() -> Result<crate::upgrade::handoff::Store, String>,
) -> Result<(u64, u64), ProtocolError> {
    use crate::upgrade::handoff::{Carried, Chat, Checkpoint, Phase, Terminal};
    // What can stop a carry is checked now, while the upgrade can still be
    // refused: past this point a terminal that fails to carry is lost if a
    // restart cannot stand in for its running job.
    let holders = holders.filter(|holders| {
        crate::secure_file::prepare_private_directory(holders)
            .inspect_err(|error| {
                tracing::warn!(holders = %holders.display(), %error,
                    "cannot prepare the holders' directory; upgrade restarts every terminal")
            })
            .is_ok()
    });
    let (proof, terminals, carried, chats, paired_codex) = {
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
        // A durable native holder also keeps its paired terminal alive. Its
        // exact environment remains only in the two holders' private memory.
        let carries = |id: &TerminalId, record: &TerminalRecord| {
            (state.chats.get(id).is_some_and(|chat| {
                matches!(
                    chat.handle.native_continuity(),
                    crate::chat::NativeContinuity::Durable { .. }
                )
            }) || (!state.chats.contains_key(id) && record.execution_environment.is_none()))
                && holders.is_some_and(|holders| holder_socket_fits(&holder_socket(holders, *id)))
        };
        let carried = state
            .terminals
            .iter()
            .filter(|(id, record)| !record.handle.is_completed() && carries(id, record))
            .map(|(&id, record)| {
                let mut options = record.options.clone();
                options.initially_visible = false;
                Ok(Carried {
                    source: state.descriptor(0, id)?.session,
                    previous_sources: record.upgrade_sources.clone(),
                    relaunches: record.upgrade_relaunches.clone(),
                    options,
                })
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        let terminals = state
            .terminals
            .iter()
            .filter(|(id, record)| !record.handle.is_completed() && !carries(id, record))
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
            let mut source = record.session.clone();
            source.native_continuity = record.handle.native_continuity();
            if matches!(
                source.native_continuity,
                crate::chat::NativeContinuity::Durable { .. }
            ) {
                source.launch.session_id = record.handle.confirmed_session_id();
                crate::chat::protocol::align_resume_identity(&mut source.launch)
                    .map_err(upgrade_error)?;
            }
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
                && !matches!(
                    source.native_continuity,
                    crate::chat::NativeContinuity::Durable { .. }
                )
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
        (
            (*state.proof).clone(),
            terminals,
            carried,
            chats,
            paired_codex,
        )
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
        let Some(recipe) = upgrade_recipe(
            &source,
            &handle,
            &snapshot,
            &mut relaunches,
            relaunch_agents,
        )
        .map_err(upgrade_error)?
        else {
            blockers.push(describe_upgrade_blocker(&source));
            continue;
        };
        if !relaunches.iter().any(|accepted| accepted.source == source) {
            tracing::info!(
                terminal = %source.id,
                program = %recipe.program,
                cwd = %recipe.working_directory.display(),
                active = snapshot.active,
                recovery = crate::upgrade::recovery_plan(&source)
                    .map_or("none", |(_, kind)| kind.label()),
                "upgrade checkpoints a terminal"
            );
        }
        if let Some(execution) = execution_environment {
            validate_terminal_upgrade_environment(&recipe, &execution)?;
        }
        apply_size(&mut options, snapshot.size, snapshot.theme);
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
    let checkpoint = Checkpoint::new(proof, saved, carried, chats);
    let counts = (
        (checkpoint.terminals.len() + checkpoint.carried.len()) as u64,
        checkpoint.chats.len() as u64,
    );
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state
        .validate_upgrade_capability(&capability, Instant::now())
        .map_err(upgrade_error)?;
    // Failure leaves control enabled and drops reversible pidfd freeze guards.
    store.save(&checkpoint).map_err(upgrade_error)?;
    state.upgrade = Some(PreparedUpgrade {
        store,
        checkpoint,
        frozen,
        relaunch_agents,
        holders: holders.map(std::path::Path::to_owned),
    });
    state.shutting_down = true;
    state.shutdown_capabilities.clear();
    Ok(counts)
}

/// The launch that restarts a frozen terminal in the next daemon, or `None`
/// when that would lose an active job: a shell recipe alone cannot authorize
/// losing an arbitrary running job, so only an agent's own relaunch, accepted
/// by policy, stands in for one.
fn upgrade_recipe(
    source: &TerminalSession,
    handle: &TerminalHandle,
    snapshot: &crate::terminal::UpgradeSnapshot,
    relaunches: &mut Vec<crate::terminal_ipc::UpgradeRelaunch>,
    relaunch_agents: bool,
) -> Result<Option<crate::model::LaunchRecipe>, String> {
    let exact = source
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::recipe_from_hint);
    if snapshot.active && exact.is_none() {
        let accepted = relaunch_agents
            .then(|| handle.observe_agent_process())
            .flatten()
            .filter(|agent| snapshot.only_agent_tree(agent.pid))
            .and_then(|agent| upgrade_relaunch(&agent));
        let Some((recipe, kind)) = accepted else {
            tracing::info!(
                terminal = %source.id,
                program = %source.launch.program,
                relaunch_agents,
                "upgrade blocked by an active job without agent identity"
            );
            return Ok(None);
        };
        tracing::info!(
            terminal = %source.id,
            program = %recipe.program,
            cwd = %recipe.working_directory.display(),
            relaunch = kind,
            "upgrade relaunches an agent without exact conversation identity"
        );
        relaunches.push(crate::terminal_ipc::UpgradeRelaunch {
            source: source.clone(),
            recipe,
        });
    }
    crate::upgrade::checkpoint_recipe(source, relaunches).map(Some)
}

/// Where carried terminals' holders wait for the next daemon: this daemon's
/// private runtime directory, which the next one shares.
fn holder_directory() -> Result<std::path::PathBuf, String> {
    crate::daemon::RuntimePaths::discover()
        .map(|paths| paths.directory.join("holders"))
        .map_err(|error| error.to_string())
}

fn holder_socket(holders: &std::path::Path, terminal: TerminalId) -> std::path::PathBuf {
    holders.join(terminal.to_string())
}

/// Whether a Unix socket can be bound at `socket`, whose path must fit in
/// `sockaddr_un` with its terminating NUL.
fn holder_socket_fits(socket: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain old data; only its field's size is read.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    socket.as_os_str().as_bytes().len() < capacity
}

/// Provider close must succeed before Ready can authorize another writer.
pub(super) fn finish_upgrade(host: &SharedHost) -> Result<(), String> {
    let Some(PreparedUpgrade {
        store,
        mut checkpoint,
        mut frozen,
        relaunch_agents,
        holders,
    }) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade
        .take()
    else {
        return Ok(());
    };
    // Completed workers have already cached their final screen in their holder.
    // They are not relaunched from the checkpoint, but remain discoverable even
    // if another checkpoint component prevents this replacement from starting.
    let restarting = checkpoint
        .terminals
        .iter()
        .map(|terminal| terminal.source.id)
        .collect::<BTreeSet<_>>();
    let completed = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .terminals
        .values()
        .filter(|record| record.handle.is_completed() && !restarting.contains(&record.session.id))
        .map(|record| record.handle.clone())
        .collect::<Vec<_>>();
    for handle in completed {
        handle.preserve_completed_for_restart();
    }
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
        let durable = matches!(
            handle.native_continuity(),
            crate::chat::NativeContinuity::Durable { .. }
        );
        if durable {
            handle.preserve_for_restart()?;
        } else {
            handle.close()?;
        }
        if handle.owns_work() {
            return Err("Native provider shutdown remains unconfirmed; checkpoint retained".into());
        }
        if durable {
            // The retained server and every issued alias refer to this exact
            // launch. A terminal resume recipe describes a new server, not
            // the live owner the successor must authenticate.
            if chat.launch.session_id != handle.confirmed_session_id() {
                return Err("Retained provider changed its confirmed conversation during upgrade; checkpoint retained".into());
            }
        } else {
            chat.launch.session_id = handle.confirmed_session_id();
            chat.launch.recipe = chat.launch.terminal_recipe().ok_or(
                "Closed provider did not retain its exact conversation identity; checkpoint retained",
            )?;
        }
    }
    // Carried last: a holder left waiting by an upgrade that then fails
    // outlives its daemon for nothing, and only the final save follows this.
    if let Some(holders) = &holders {
        let mut stopping = Vec::new();
        carry_terminals(
            host,
            &mut checkpoint,
            &mut stopping,
            relaunch_agents,
            holders,
        );
        for terminal in &mut stopping {
            terminal.stop()?;
        }
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

/// Leaves each carried terminal to its holder, to wait for the next daemon.
///
/// The upgrade is past refusing by now, so a terminal that cannot be carried
/// is checkpointed as prepare would have: frozen with the relaunched terminals
/// and restarted by the next daemon, unless that would lose an active job, in
/// which case it ends with this daemon like any terminal did before carrying.
fn carry_terminals(
    host: &SharedHost,
    checkpoint: &mut crate::upgrade::handoff::Checkpoint,
    frozen: &mut Vec<crate::terminal::UpgradeSnapshot>,
    relaunch_agents: bool,
    holders: &std::path::Path,
) {
    if checkpoint.carried.is_empty() {
        return;
    }
    let prepared = crate::secure_file::prepare_private_directory(holders)
        .map_err(|error| format!("prepare {}: {error}", holders.display()));
    let mut carried = Vec::with_capacity(checkpoint.carried.len());
    let prepared_hints = checkpoint
        .carried
        .iter()
        .map(|terminal| (terminal.source.id, terminal.source.resume_hint.clone()))
        .collect::<BTreeMap<_, _>>();
    for mut terminal in std::mem::take(&mut checkpoint.carried) {
        let id = terminal.source.id;
        let Some(handle) = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .get(&id)
            .map(|record| record.handle.clone())
        else {
            tracing::warn!(terminal = %id, "a terminal to carry ended before the upgrade");
            continue;
        };
        let error = match prepared
            .clone()
            .and_then(|()| handle.carry(holder_socket(holders, id)))
        {
            Ok(state) => {
                tracing::info!(terminal = %id, "upgrade carries a terminal to the next daemon");
                apply_size(&mut terminal.options, state.size, state.theme);
                terminal.source.resume_hint = state.hint;
                carried.push(terminal);
                continue;
            }
            Err(error) => error,
        };
        // Completion can race the carry, or VT serialization can fail while
        // its process still runs. A registered holder can keep either result
        // without converting ownership into permission to execute a recipe.
        if crate::terminal::holder::evidence(&holder_socket(holders, id))
            .ok()
            .flatten()
            .is_some_and(|record| record.holder.is_current())
            && handle.preserve_for_restart().is_ok()
        {
            tracing::warn!(terminal = %id, %error,
                "upgrade preserves the registered holder without a new parser snapshot");
            carried.push(terminal);
            continue;
        }
        let fallback = handle.prepare_upgrade().and_then(|snapshot| {
            let mut source = terminal.source.clone();
            source.resume_hint.clone_from(&snapshot.hint);
            let mut relaunches = terminal.relaunches.clone();
            Ok(upgrade_recipe(
                &source,
                &handle,
                &snapshot,
                &mut relaunches,
                relaunch_agents,
            )?
            .map(|recipe| (source, relaunches, recipe, snapshot)))
        });
        match fallback {
            Ok(Some((source, relaunches, recipe, snapshot))) => {
                tracing::warn!(terminal = %id, %error, program = %recipe.program,
                    "upgrade could not carry a terminal; the next daemon restarts it");
                let mut options = terminal.options;
                apply_size(&mut options, snapshot.size, snapshot.theme);
                checkpoint
                    .terminals
                    .push(crate::upgrade::handoff::Terminal {
                        source,
                        previous_sources: terminal.previous_sources,
                        relaunches,
                        recipe,
                        options,
                    });
                frozen.push(snapshot);
            }
            Ok(None) => tracing::warn!(terminal = %id, %error,
                "upgrade could not carry a terminal whose active job a restart would lose; it ends with this daemon"),
            Err(unrestartable) => tracing::warn!(terminal = %id, %error, %unrestartable,
                "upgrade could neither carry nor safely restart a terminal; it ends with this daemon"),
        }
    }
    checkpoint.carried = carried;
    // An agent can have moved to another conversation since prepare; one that
    // another writer claims would make the checkpoint unsaveable, and so
    // strand every carried terminal. Its restart then relies on what prepare saw.
    if let Err(error) = checkpoint.validate() {
        tracing::warn!(%error, "keeping the conversations prepare observed for carried terminals");
        for terminal in &mut checkpoint.carried {
            terminal.source.resume_hint = prepared_hints[&terminal.source.id].clone();
        }
    }
}

fn apply_size(
    options: &mut TerminalSpawnOptions,
    size: crate::terminal::TerminalSize,
    theme: crate::terminal::TerminalTheme,
) {
    options.columns = size.columns;
    options.rows = size.rows;
    options.cell_width_px = size.cell_width_px;
    options.cell_height_px = size.cell_height_px;
    options.theme = theme;
}

/// Runs while HostLease is held, before binding the public listener.
pub(super) fn restore_upgrade(host: &SharedHost) -> Result<(), String> {
    restore_upgrade_from(
        host,
        crate::upgrade::handoff::Store::discover()?,
        std::path::Path::new("/proc"),
        holder_directory().ok().as_deref(),
    )
}

/// A holder's private record is discovery only. The live holder must return
/// matching process evidence and the previous daemon's issued lineage before
/// its PTY may be attached; a matching numeric terminal ID alone is insufficient.
/// Called only after the terminal holder authenticated these descriptors and
/// the restored worker was installed. A console keeps Chat authority until its
/// native holder installs the exact paired successor.
fn restore_terminal_execution_authority(
    host: &SharedHost,
    source: &TerminalSession,
    previous: &[TerminalSession],
) -> Result<(), String> {
    use crate::agent_execution::HostedSession;
    let registry = super::agent_execution::registry(host)?;
    let authority = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_authority_source(source.id)?;
    if !matches!(authority, Some(HostedSession::Terminal(_))) {
        return Ok(());
    }
    let successor = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .descriptor(0, source.id)
        .map_err(|error| error.message)?
        .session;
    let mut retained = Vec::new();
    for source in previous.iter().chain(std::iter::once(source)) {
        retained.push(HostedSession::Terminal(source.clone()));
        if let Some(proof) = source.continuity.live_proof() {
            // Holder discovery records detached ownership. Capability issuance
            // used the same process proof while it was attached to the daemon.
            let mut attached = source.clone();
            attached.continuity = TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            };
            retained.push(HostedSession::Terminal(attached.clone()));
            attached.resume_hint =
                crate::model::agent_recovery::observed_hint_from_launch(&attached.launch);
            retained.push(HostedSession::Terminal(attached));
        }
    }
    registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .adopt_authenticated_agent_session(&retained, &HostedSession::Terminal(successor))?;
    Ok(())
}

pub(super) fn restore_orphaned_holders(host: &SharedHost) -> Result<(), String> {
    let directory = crate::terminal::holder::directory()?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let records = crate::terminal::holder::discover(&directory, &proof.boot_id)?;
    for record in records {
        if crate::daemon::termination_requested() {
            break;
        }
        if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .contains_key(&record.terminal)
        {
            continue;
        }
        let socket = crate::terminal::holder::socket(&directory, record.terminal);
        let restored = (|| {
            let (recovery, screen_lost) =
                crate::terminal::inspect_holder(&socket, &record, &proof)?;
            let source = recovery.source.clone();
            let previous_sources = recovery.previous_sources.clone();
            let relaunches = recovery.relaunches.clone();
            let mut session = source.clone();
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
            start_terminal_view(
                host,
                0,
                session,
                recovery.options.clone(),
                recovery.execution_environment.clone(),
                false,
                Some(socket.clone()),
                Some(recovery),
            )
            .map_err(|error| error.message)?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .adopt_upgrade_lineage(&source, &previous_sources, &relaunches)
                .map_err(|error| error.message)?;
            if let Some(terminal) = state.terminals.get_mut(&record.terminal) {
                terminal.screen_lost_after_crash = screen_lost;
                terminal.session.resume_hint = source.resume_hint.clone();
            }
            drop(state);
            restore_terminal_execution_authority(host, &source, &previous_sources)?;
            Ok::<_, String>(())
        })();
        if let Err(error) = restored {
            tracing::warn!(terminal = %record.terminal, %error, "could not adopt orphaned terminal; preserved its holder");
        }
    }
    Ok(())
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
    holders: Option<&std::path::Path>,
) -> Result<(), String> {
    use crate::upgrade::{ensure_no_surviving_process, handoff::Phase};
    let Some(mut checkpoint) = store.load()? else {
        return Ok(());
    };
    checkpoint.validate()?;
    if checkpoint.chats.iter().any(|chat| {
        !matches!(
            chat.source.native_continuity,
            crate::chat::NativeContinuity::Durable { .. }
        )
    }) {
        checkpoint.require_ready().map_err(|error| format!(
            "{error}. Evidence: {}. After inspecting the exact provider conversations, `cagents daemon abandon-upgrade --yes` archives this checkpoint and permits an empty daemon start; native sessions then require explicit manual recovery",
            store.path.display()
        ))?;
    }
    for terminal in &checkpoint.terminals {
        ensure_no_surviving_process(&terminal.source, std::process::id(), proc_root)?;
    }
    for chat in &checkpoint.chats {
        // A private holder authenticates the exact still-running server. Its
        // presence is the purpose of adoption, not permission to relaunch it.
        if matches!(
            chat.source.native_continuity,
            crate::chat::NativeContinuity::Durable { .. }
        ) {
            continue;
        }
        let terminal = TerminalSession {
            owned_codex_listener: None,
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
    let mut adopted = Vec::new();
    let restored = (|| {
        // A managed CLI connects to its thread's server. Recreate that exact
        // actor first; starting the TUI first races a socket that does not exist.
        for chat in &checkpoint.chats {
            if super::chat::owns_codex_listener(&chat.source)
                && !matches!(
                    chat.source.native_continuity,
                    crate::chat::NativeContinuity::Durable { .. }
                )
            {
                super::chat::restore_upgrade(host, chat)?;
            }
        }
        // Carried terminals are adopted before anything is relaunched: their
        // processes have been waiting unattended since the previous daemon left.
        for terminal in &checkpoint.carried {
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            if adopt_carried_terminal(host, terminal, holders, proc_root) {
                adopted.push(terminal.source.id);
            }
        }
        // Completed paired views are deliberately absent from the execution
        // checkpoint, but their authenticated holders retain the final screen.
        // Restore those receipts before requiring a native pair to match.
        if checkpoint.chats.iter().any(|chat| {
            chat.source.terminal.is_some()
                && matches!(
                    chat.source.native_continuity,
                    crate::chat::NativeContinuity::Durable { .. }
                )
        }) {
            restore_orphaned_holders(host)?;
        }
        for chat in &checkpoint.chats {
            if matches!(
                chat.source.native_continuity,
                crate::chat::NativeContinuity::Durable { .. }
            ) {
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
            state
                .adopt_upgrade_lineage(
                    &terminal.source,
                    &terminal.previous_sources,
                    &terminal.relaunches,
                )
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
        let closed = close_failed_restore(host, &adopted, holders).is_ok();
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

/// How many times a carried terminal's holder is asked for it before the
/// terminal is given up, and how long apart: a holder busy with another
/// connection answers the next one.
const ADOPTION_ATTEMPTS: u32 = 3;
const ADOPTION_RETRY: Duration = Duration::from_millis(500);
/// How long the processes of a carried terminal that could not be adopted may
/// take to end, once their holder hangs them up, before a restart is refused.
const ABANDONED_EXIT_GRACE: Duration = Duration::from_secs(2);

/// Adopts a carried terminal from the holder its predecessor left it with,
/// reporting whether it did.
///
/// Its process has already outlived one daemon, so no failure here aborts the
/// rest of the restore: a terminal whose holder is gone is restarted from its
/// safe recipe once nothing of it survives, and otherwise left for the user,
/// since starting a second copy beside a surviving one could corrupt its work.
fn adopt_carried_terminal(
    host: &SharedHost,
    terminal: &crate::upgrade::handoff::Carried,
    holders: Option<&std::path::Path>,
    proc_root: &std::path::Path,
) -> bool {
    let id = terminal.source.id;
    let mut error = "no directory for holders".to_owned();
    if let Some(holders) = holders {
        let socket = holder_socket(holders, id);
        for attempt in 1..=ADOPTION_ATTEMPTS {
            let previous = (|| {
                if let Some(record) = crate::terminal::holder::evidence(&socket)? {
                    let proof = host
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .proof()
                        .clone();
                    let (recovery, screen_lost) =
                        crate::terminal::inspect_holder(&socket, &record, &proof)?;
                    // A failed prior restore may already have rotated the
                    // holder's owner. Preserve that intermediate receipt too.
                    if !std::iter::once(&recovery.source)
                        .chain(&recovery.previous_sources)
                        .any(|source| {
                            source.id == terminal.source.id
                                && source.launch == terminal.source.launch
                                && source.continuity.live_proof()
                                    == terminal.source.continuity.live_proof()
                        })
                    {
                        return Err("holder does not retain the checkpoint's issued lineage".into());
                    }
                    return Ok((recovery, screen_lost));
                }
                Ok::<_, String>((
                    crate::terminal::holder::Recovery {
                        source: terminal.source.clone(),
                        previous_sources: terminal.previous_sources.clone(),
                        relaunches: terminal.relaunches.clone(),
                        options: terminal.options.clone(),
                        execution_environment: None,
                    },
                    false,
                ))
            })();
            let (previous, screen_lost) = match previous {
                Ok(previous) => previous,
                Err(failure) => {
                    error = failure;
                    break;
                }
            };
            let mut session = terminal.source.clone();
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
            match start_terminal_view(
                host,
                0,
                session,
                previous.options.clone(),
                previous.execution_environment.clone(),
                false,
                Some(socket.clone()),
                Some(previous.clone()),
            ) {
                Ok(_) => {
                    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(record) = state.terminals.get_mut(&id) {
                        record.screen_lost_after_crash = screen_lost;
                    }
                    // The worker learns a conversation's identity only from
                    // output it sees, which the previous daemon already saw.
                    if let Some(record) = state.terminals.get_mut(&id)
                        && terminal.source.resume_hint.is_some()
                    {
                        record
                            .session
                            .resume_hint
                            .clone_from(&terminal.source.resume_hint);
                    }
                    if let Err(error) = state.adopt_upgrade_lineage(
                        &previous.source,
                        &previous.previous_sources,
                        &previous.relaunches,
                    ) {
                        tracing::warn!(terminal = %id, error = %error.message,
                            "adopted a carried terminal but could not record its lineage");
                    } else {
                        tracing::info!(terminal = %id, "adopted a terminal carried across the upgrade");
                    }
                    drop(state);
                    if let Err(error) = restore_terminal_execution_authority(
                        host,
                        &previous.source,
                        &previous.previous_sources,
                    ) {
                        tracing::warn!(terminal = %id, %error, "terminal survived but execution authority could not be restored");
                    }
                    return true;
                }
                Err(failure) => error = failure.message,
            }
            // A holder that stopped listening does not start again.
            if attempt == ADOPTION_ATTEMPTS || !socket.exists() {
                break;
            }
            thread::sleep(ADOPTION_RETRY);
        }
        // A completed root is absent from the process scan, but its live
        // registered holder still owns the result. Adoption failure must not
        // reinterpret that result as permission to execute its recipe again.
        match crate::terminal::holder::evidence(&socket) {
            Ok(Some(record)) if record.holder.is_current() => {
                tracing::warn!(terminal = %id, %error,
                    "registered holder still owns this terminal; recipe fallback is refused");
                return false;
            }
            Err(evidence_error) => {
                tracing::warn!(terminal = %id, %error, %evidence_error,
                    "holder ownership could not be validated; recipe fallback is refused");
                return false;
            }
            _ => {}
        }
    }
    let recipe = match crate::upgrade::checkpoint_recipe(&terminal.source, &terminal.relaunches) {
        Ok(recipe) => recipe,
        Err(unsafe_recipe) => {
            tracing::warn!(terminal = %id, %error, %unsafe_recipe,
                "could not adopt a carried terminal, which cannot safely be restarted");
            return false;
        }
    };
    let deadline = Instant::now() + ABANDONED_EXIT_GRACE;
    while let Err(survivor) =
        crate::upgrade::ensure_no_surviving_process(&terminal.source, std::process::id(), proc_root)
    {
        if Instant::now() >= deadline {
            tracing::warn!(terminal = %id, %error, %survivor,
                "could not adopt a carried terminal whose processes survive; it is not restarted beside them");
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    tracing::warn!(terminal = %id, %error, program = %recipe.program,
        "could not adopt a carried terminal; restarting it");
    let mut session = terminal.source.clone();
    session.launch = recipe;
    session.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: session.launch.clone(),
        last_exit: None,
    };
    let restarted = spawn_terminal_view(host, 0, session, terminal.options.clone(), None, false)
        .and_then(|_| {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .adopt_upgrade_lineage(
                    &terminal.source,
                    &terminal.previous_sources,
                    &terminal.relaunches,
                )
        });
    if let Err(error) = restarted {
        tracing::warn!(terminal = %id, error = %error.message,
            "could not restart a carried terminal");
    }
    false
}

/// Positively ends everything a failed restore started, so the checkpoint
/// can be replayed. Terminals it adopted are carried back to their holders
/// instead, to be adopted again by that replay.
fn close_failed_restore(
    host: &SharedHost,
    adopted: &[TerminalId],
    holders: Option<&std::path::Path>,
) -> Result<(), String> {
    if let Some(holders) = holders {
        for &id in adopted {
            let handle = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .get(&id)
                .map(|record| record.handle.clone());
            if let Some(handle) = handle {
                if handle.is_completed() {
                    handle.preserve_completed_for_restart();
                } else if let Err(error) = handle.carry(holder_socket(holders, id)) {
                    tracing::warn!(terminal = %id, %error,
                        "could not return an adopted terminal to its holder after a failed restore");
                    // Completion can race the carry request. Synchronize with
                    // teardown before taking its final holder, or deliberately
                    // orphan the still-live registered PTY without a screen.
                    handle.preserve_for_restart()?;
                }
            }
        }
    }
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
        if matches!(
            chat.native_continuity(),
            crate::chat::NativeContinuity::Durable { .. }
        ) {
            chat.preserve_for_restart()?;
        } else {
            chat.close()?;
        }
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
    mut session: TerminalSession,
    options: TerminalSpawnOptions,
) -> Result<TerminalDescriptor, ProtocolError> {
    // Only a native actor's paired view or a trusted checkpoint can issue
    // allocation evidence. A generic client spawn is an ordinary terminal.
    session.owned_codex_listener = None;
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
    start_terminal_view(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        paired,
        None,
        None,
    )
}

/// Returns the exact worker published with the descriptor. A subsequent lookup
/// by ID cannot establish ownership: another caller may already have replaced it.
pub(super) fn spawn_terminal_view_owned(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    start_terminal_view_owned(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        paired,
        None,
        None,
    )
}

/// Starts a terminal's worker, spawning its process or, given `adoption`,
/// adopting the one a predecessor daemon's holder waits with there.
fn start_terminal_view(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
    adoption: Option<std::path::PathBuf>,
    previous: Option<crate::terminal::holder::Recovery>,
) -> Result<TerminalDescriptor, ProtocolError> {
    start_terminal_view_owned(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        paired,
        adoption,
        previous,
    )
    .map(|(descriptor, _)| descriptor)
}

/// Starts a terminal's worker, spawning its process or, given `adoption`,
/// adopting the one a predecessor daemon's holder waits with there.
fn start_terminal_view_owned(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    mut execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
    adoption: Option<std::path::PathBuf>,
    previous: Option<crate::terminal::holder::Recovery>,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    validate_launch(&session)?;
    validate_spawn_options(&options)?;
    if let Some(environment) = &mut execution_environment {
        session.launch.remove_unset_environment(&mut environment.0);
    }
    let terminal_id = session.id;
    let native_handoff = execution_environment.is_some();
    let retained_environment = execution_environment.clone();
    let holder_recovery = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reserve_spawn_for_view(terminal_id, paired.then_some(&session))?;
        if crate::account::login::is_codex_browser(&session.launch) {
            let reserved = state.reserve_browser_login(terminal_id).and_then(|()| {
                // An adopted process already owns its callback listener.
                if adoption.is_some() {
                    return Ok(());
                }
                crate::account::login::check_callback_port().map_err(|message| {
                    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, false)
                })
            });
            if let Err(error) = reserved {
                state.cancel_spawn(terminal_id);
                return Err(error);
            }
        }
        state.recover_holders.then(|| {
            let mut source = session.clone();
            source.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: state.proof.host_session_id.clone(),
                boot_id: state.proof.boot_id.clone(),
                host_incarnation: state.proof.host_incarnation.clone(),
            };
            let (previous_sources, relaunches) = previous
                .map(|previous| {
                    let mut sources = previous.previous_sources;
                    sources.push(previous.source);
                    (sources, previous.relaunches)
                })
                .unwrap_or_default();
            crate::terminal::holder::Recovery {
                source,
                previous_sources,
                relaunches,
                options: options.clone(),
                execution_environment: retained_environment.clone(),
            }
        })
    };

    let agent_authority = if paired {
        match super::agent_execution::registry(shared) {
            Ok(registry) => registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retained_agent_authority(terminal_id)
                .unwrap_or_default(),
            Err(_) => Default::default(),
        }
    } else if adoption.is_none() {
        match super::agent_execution::registry(shared) {
            Ok(registry) => {
                let mut source = session.clone();
                let proof = shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .proof()
                    .clone();
                source.continuity = TerminalContinuity::Live {
                    host_session_id: proof.host_session_id,
                    boot_id: proof.boot_id,
                    host_incarnation: proof.host_incarnation,
                };
                source.resume_hint =
                    crate::model::agent_recovery::observed_hint_from_launch(&source.launch);
                match registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .fresh_agent_authority(&crate::agent_execution::HostedSession::Terminal(source))
                {
                    Ok(authority) => authority,
                    Err(error) => {
                        shared
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .cancel_spawn(terminal_id);
                        return Err(ProtocolError::new(
                            ProtocolErrorCode::Internal,
                            error,
                            false,
                        ));
                    }
                }
            }
            Err(_) => Default::default(),
        }
    } else {
        Default::default()
    };
    let weak = Arc::downgrade(shared);
    let config = TerminalConfig {
        agent_authority,
        columns: options.columns,
        rows: options.rows,
        cell_width_px: options.cell_width_px,
        cell_height_px: options.cell_height_px,
        max_scrollback: options.max_scrollback,
        initially_visible: options.initially_visible,
        theme: options.theme,
        execution_environment,
        adoption,
        holder_recovery,
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
            state
                .terminals
                .get_mut(&terminal_id)
                .expect("finished spawn")
                .options = options.clone();
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
    // Initial publication can happen before the record is subscribable. Clear
    // that edge after insertion; the Spawn response carries the same snapshot,
    // and later generations can now produce a fresh wake.
    redraw_handle.acknowledge_redraw(redraw_handle.generation());
    let activity_wake = Arc::clone(
        &shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_activity_wake,
    );
    redraw_handle.set_agent_process_observer(move || activity_wake.notify());
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
        if state
            .terminals
            .get(&terminal_id)
            .is_some_and(|record| record.handle.same_worker(&redraw_handle))
        {
            state.retire_terminal(terminal_id);
        }
        return Err(ProtocolError::new(
            ProtocolErrorCode::Internal,
            format!("could not start terminal event monitor: {error}"),
            true,
        ));
    }
    Ok((descriptor, redraw_handle))
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
        if let WireTerminalEvent::ConversationTitleChanged { title } = &wire_event {
            record.conversation_title = Some((sequence, title.clone()));
        }
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
mod native_upgrade_tests;

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
    fn browser_login_claim_is_atomic_across_windows_and_released_after_failed_spawn() {
        let shared = Arc::new(Mutex::new(test_host()));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let attempts: Vec<_> = (0..2)
            .map(|_| {
                let shared = shared.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let id = TerminalId::new();
                    barrier.wait();
                    let mut state = shared.lock().unwrap();
                    state.reserve_spawn(id).unwrap();
                    match state.reserve_browser_login(id) {
                        Ok(()) => Some(id),
                        Err(error) => {
                            assert_eq!(error.message, crate::account::login::BUSY);
                            state.cancel_spawn(id);
                            None
                        }
                    }
                })
            })
            .collect();
        let winners: Vec<_> = attempts
            .into_iter()
            .filter_map(|attempt| attempt.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
        let mut state = shared.lock().unwrap();
        state.cancel_spawn(winners[0]);
        assert!(state.reserve_browser_login(TerminalId::new()).is_ok());
    }

    #[test]
    fn browser_login_claim_survives_ui_disconnect_until_actual_worker_teardown() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("codex");
        fs::write(&program, "#!/bin/sh\nexec /bin/cat\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
        recipe.program = program.to_str().unwrap().into();
        recipe.arguments = vec!["login".into()];
        let session = TerminalSession {
            owned_codex_listener: None,
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
        let mut state = test_host();
        state.register_client(1);
        state.reserve_spawn(session.id).unwrap();
        state.reserve_browser_login(session.id).unwrap();
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        state
            .finish_spawn(1, session, handle.clone(), true)
            .unwrap();
        state.unregister_client(1);
        state.register_client(2);
        let blocked = state.reserve_browser_login(TerminalId::new()).is_err();
        handle.shutdown().unwrap();
        assert!(blocked, "a UI restart must not release its running login");
        assert!(handle.is_completed());
        assert!(state.reserve_browser_login(TerminalId::new()).is_ok());
    }

    #[test]
    fn browser_callback_failure_survives_hidden_terminal_completion() {
        let mut recipe = crate::model::LaunchRecipe::shell("/".into()).unwrap();
        recipe.program = "/bin/sh".into();
        recipe.arguments = vec![
            "-c".into(),
            "printf '%s\\n' 'Error: Port 127.0.0.1:1455 is already in use' >&2; exit 1".into(),
        ];
        let session = TerminalSession {
            owned_codex_listener: None,
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
        let handle = TerminalHandle::spawn(
            session,
            TerminalConfig {
                initially_visible: false,
                ..TerminalConfig::default()
            },
            |_| {},
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !handle.is_completed() {
            let _ = handle.shutdown();
            panic!("fixture login did not finish");
        }
        let text = handle
            .snapshot()
            .lines
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            crate::account::login::callback_failed(&text),
            "a hibernated login must retain its callback failure"
        );
    }

    #[test]
    fn notification_activation_prefers_the_focused_owner_and_restores_closed_window_routes() {
        use crate::notifications::Presence;
        let session = TerminalId::new();
        let mut host = test_host();
        let focused = host.register_client(1);
        host.register_client(2);
        host.watch_notifications(
            1,
            Presence {
                window: "primary".into(),
                sessions: vec![session],
                focused: Some(session),
            },
        );
        host.watch_notifications(
            2,
            Presence {
                window: "second".into(),
                sessions: vec![session],
                focused: None,
            },
        );
        assert_eq!(
            host.activate_notification(session, Some("second")),
            "primary"
        );
        assert!(
            matches!(focused.pop().unwrap().message, ServerMessage::NotificationActivated { session: found } if found == session)
        );
        drop(host.unregister_client(1));
        drop(host.unregister_client(2));
        assert_eq!(
            host.activate_notification(session, Some("second")),
            "second"
        );
        host.register_client(3);
        let presence = Presence {
            window: "second".into(),
            sessions: vec![session],
            focused: None,
        };
        assert_eq!(host.watch_notifications(3, presence.clone()), Some(session));
        assert_eq!(host.watch_notifications(3, presence), None);
    }

    #[test]
    fn explicit_close_fences_attention_without_discarding_completed_history() {
        let recipe = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), "exit 0".into()],
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let id = TerminalId::new();
        let session = TerminalSession {
            id,
            launch: recipe.clone(),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        let handle =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        let mut host = test_host();
        host.register_client(1);
        host.reserve_spawn(id).unwrap();
        host.finish_spawn(1, session.clone(), handle.clone(), false)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() {
            assert!(Instant::now() < deadline, "fixture did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !host.notification_closed.contains(&id),
            "natural completion retains attention"
        );
        host.close_terminal_attention(id, Some(&handle));
        assert!(host.notification_closed.contains(&id));
        assert!(
            host.terminals.contains_key(&id),
            "completed history remains readable"
        );
        host.retire_terminal(id);
        assert!(
            !host.notification_closed.contains(&id),
            "fence storage follows record lifetime"
        );
        let replacement =
            TerminalHandle::spawn(session.clone(), TerminalConfig::default(), |_| {}).unwrap();
        host.reserve_spawn(id).unwrap();
        host.finish_spawn(1, session, replacement.clone(), false)
            .unwrap();
        host.close_terminal_attention(id, Some(&handle));
        host.close_terminal_attention(id, None);
        assert!(
            !host.notification_closed.contains(&id),
            "stale closure must not fence a replacement worker"
        );
        replacement.shutdown().unwrap();
        host.close_terminal_attention(id, Some(&replacement));
        assert!(host.notification_closed.contains(&id));
    }

    #[test]
    fn upgrade_agent_relaunch_requires_opt_in_and_survives_repeated_restore() {
        use crate::upgrade::handoff::Store;
        let directory = tempfile::tempdir().unwrap();
        // A real foreground process, recognized by its provider argv. Its
        // executable is deliberately different from the recovery command, and
        // it ignores the resume picker selector a relaunch adds.
        let executable = directory.path().join("claude");
        fs::write(&executable, "#!/bin/bash\nexec -a \"$0\" /bin/cat\n").unwrap();
        fs::set_permissions(
            &executable,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let recipe = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: executable.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::from([(
                "HOME".into(),
                directory.path().to_string_lossy().into_owned(),
            )]),
        };
        let source = TerminalSession {
            owned_codex_listener: None,
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
                prepare_upgrade_with_policy(&host, capability, false, None, || Store::open(
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
                prepare_upgrade_with_policy(&host, capability, true, None, || Store::open(
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
            // With no conversation identified, the agent reopens its picker.
            assert_eq!(saved.terminals[0].recipe.arguments, ["--resume"]);
            host = Arc::new(Mutex::new(test_host_with_incarnation(incarnation)));
            restore_upgrade_from(
                &host,
                Store::open(directory.path()).unwrap(),
                &empty_proc,
                None,
            )
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
            native_continuity: Default::default(),
            owned_codex_listener: None,
            id: TerminalId::new(),
            launch: Launch {
                provider: Provider::Claude,
                recipe: LaunchRecipe {
                    unset_environment: Default::default(),
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
        restore_upgrade_from(&replacement, store, &empty_proc, None).unwrap();
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
            owned_codex_listener: None,
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
            None,
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
        restore_upgrade_from(&replacement, store, &empty_proc, None).unwrap();
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
            None,
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
            unset_environment: Default::default(),
            program: "omp".into(),
            arguments: Vec::new(),
            working_directory: directory.path().to_owned(),
            environment: BTreeMap::new(),
        };
        let source = Session {
            native_continuity: Default::default(),
            owned_codex_listener: None,
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
            owned_codex_listener: None,
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
        let retain = |deadline| {
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
    fn a_carried_terminal_keeps_its_process_across_an_upgrade() {
        use crate::upgrade::handoff::Store;
        let directory = tempfile::tempdir().unwrap();
        let holders = directory.path().join("holders");
        let empty_proc = directory.path().join("proc");
        fs::create_dir(&empty_proc).unwrap();
        let pid_file = directory.path().join("pid");
        let recipe = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                format!("echo $$ > '{}'; exec sleep 1000", pid_file.display()),
            ],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let source = TerminalSession {
            owned_codex_listener: None,
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
        let host = Arc::new(Mutex::new(test_host()));
        spawn_terminal(&host, 0, source.clone(), TerminalSpawnOptions::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the shell never started");
            thread::sleep(Duration::from_millis(10));
        };
        let old = host.lock().unwrap().terminals[&source.id].handle.clone();
        let carried_source = host
            .lock()
            .unwrap()
            .descriptor(0, source.id)
            .unwrap()
            .session;
        let capability = host
            .lock()
            .unwrap()
            .shutdown_preflight(Instant::now())
            .unwrap()
            .capability;
        // Its job keeps running, so no relaunch has to be accepted for it.
        assert_eq!(
            prepare_upgrade_with_policy(&host, capability, false, Some(&holders), || Store::open(
                directory.path()
            ))
            .unwrap(),
            (1, 0)
        );
        finish_upgrade(&host).unwrap();
        assert!(old.is_completed());
        let saved = Store::open(directory.path())
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        assert!(saved.terminals.is_empty());
        assert_eq!(saved.carried[0].source, carried_source);

        // A restore that fails after adopting it hands it back to its holder,
        // so replaying the checkpoint adopts it again.
        let store = Store::open(directory.path()).unwrap();
        let mut failing = store.load().unwrap().unwrap();
        let recipe = crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap();
        failing.terminals.push(crate::upgrade::handoff::Terminal {
            source: TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                id: TerminalId::new(),
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::DetachedSameBoot {
                    host_session_id: failing.proof.host_session_id.clone(),
                    boot_id: failing.proof.boot_id.clone(),
                    host_incarnation: failing.proof.host_incarnation.clone(),
                },
            },
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            recipe,
            // No terminal can start at this size.
            options: TerminalSpawnOptions {
                columns: 0,
                ..TerminalSpawnOptions::default()
            },
        });
        store.save(&failing).unwrap();
        let failed = Arc::new(Mutex::new(test_host_with_incarnation("failed")));
        assert!(restore_upgrade_from(&failed, store, &empty_proc, Some(&holders)).is_err());
        assert!(
            failed.lock().unwrap().terminals[&source.id]
                .handle
                .is_completed()
        );
        assert!(holders.join(source.id.to_string()).exists());
        let store = Store::open(directory.path()).unwrap();
        let mut retained = store.load().unwrap().unwrap();
        retained.terminals.clear();
        store.save(&retained).unwrap();

        let replacement = Arc::new(Mutex::new(test_host_with_incarnation("second")));
        restore_upgrade_from(
            &replacement,
            Store::open(directory.path()).unwrap(),
            &empty_proc,
            Some(&holders),
        )
        .unwrap();
        assert!(
            Store::open(directory.path())
                .unwrap()
                .load()
                .unwrap()
                .is_none()
        );
        let descriptor = replacement
            .lock()
            .unwrap()
            .descriptor(0, source.id)
            .unwrap();
        assert_eq!(descriptor.upgrade_sources, vec![carried_source]);
        assert_eq!(descriptor.session.launch, source.launch);
        assert_eq!(
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap()
                .trim(),
            "sleep",
            "the successor adopted the same process rather than starting another"
        );
        let handle = replacement.lock().unwrap().terminals[&source.id]
            .handle
            .clone();
        assert!(!handle.is_completed());
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn a_carried_terminal_whose_holder_is_gone_restarts_from_its_recipe() {
        use crate::upgrade::handoff::{Carried, Checkpoint, Phase, Store};
        let directory = tempfile::tempdir().unwrap();
        let empty_proc = directory.path().join("proc");
        fs::create_dir(&empty_proc).unwrap();
        let old = test_host().proof().clone();
        let source = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            id: TerminalId::new(),
            launch: crate::model::LaunchRecipe::shell(directory.path().to_owned()).unwrap(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: old.host_session_id.clone(),
                boot_id: old.boot_id.clone(),
                host_incarnation: old.host_incarnation.clone(),
            },
        };
        let mut checkpoint = Checkpoint::new(
            old,
            Vec::new(),
            vec![Carried {
                source: source.clone(),
                previous_sources: Vec::new(),
                relaunches: Vec::new(),
                options: TerminalSpawnOptions::default(),
            }],
            Vec::new(),
        );
        checkpoint.phase = Phase::Ready;
        let store = Store::open(directory.path()).unwrap();
        store.save(&checkpoint).unwrap();
        let host = Arc::new(Mutex::new(test_host_with_incarnation("second")));
        restore_upgrade_from(
            &host,
            store,
            &empty_proc,
            Some(&directory.path().join("holders")),
        )
        .unwrap();
        let descriptor = host.lock().unwrap().descriptor(0, source.id).unwrap();
        assert_eq!(descriptor.upgrade_sources, vec![source]);
        let handle = host.lock().unwrap().terminals[&descriptor.session.id]
            .handle
            .clone();
        assert!(!handle.is_completed());
        assert_eq!(
            handle.close_checked(true),
            crate::terminal_ipc::TerminalCloseResult::Closed
        );
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
            owned_codex_listener: None,
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
            Vec::new(),
        );
        checkpoint.phase = Phase::Ready;
        let store = Store::open(directory.path()).unwrap();
        store.save(&checkpoint).unwrap();
        let replacement = test_host_with_incarnation("replacement-incarnation");
        let expected = (*replacement.proof).clone();
        let host = Arc::new(Mutex::new(replacement));
        restore_upgrade_from(&host, store, &empty_proc, None).unwrap();
        let descriptor = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .descriptor(0, source.id)
            .unwrap();
        assert_eq!(descriptor.upgrade_sources, vec![source.clone()]);
        assert_eq!(descriptor.session.continuity.live_proof(), Some(expected));
        assert_eq!(descriptor.session.transcript_path, source.transcript_path);
        restore_upgrade_from(
            &host,
            Store::open(directory.path()).unwrap(),
            &empty_proc,
            None,
        )
        .unwrap();
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
        restore_upgrade_from(&host, store, &empty_proc, None).unwrap();
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
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            owned_codex_listener: None,
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
            unset_environment: Default::default(),
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
            owned_codex_listener: None,
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
            owned_codex_listener: None,
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
            let id = TerminalId::new();
            state.chat_spawning.insert(
                id,
                crate::chat::Session {
                    id,
                    launch: crate::chat::Launch {
                        provider: crate::agent::Provider::Claude,
                        recipe: recipe.clone(),
                        session_id: None,
                    },
                    native_continuity: Default::default(),
                    owned_codex_listener: None,
                    proof: Some(state.proof().clone()),
                    terminal: None,
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
    fn stable_conversation_title_survives_replay_eviction_without_duplicate_edges() {
        let shared = Arc::new(Mutex::new(test_host()));
        let recipe = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            recovery_origin: None,
            owned_codex_listener: None,
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
        shared
            .lock()
            .unwrap()
            .finish_spawn(1, session, handle.clone(), false)
            .unwrap();
        let weak = Arc::downgrade(&shared);
        record_event(
            &weak,
            id,
            TerminalEvent::ConversationTitleChanged("Stable title".into()),
        );
        assert_eq!(
            shared.lock().unwrap().replay_events(id, 0).unwrap().len(),
            1
        );
        for _ in 0..EVENT_REPLAY_CAPACITY {
            record_event(&weak, id, TerminalEvent::Bell);
        }
        let replay = shared.lock().unwrap().replay_events(id, 0).unwrap();
        assert_eq!(replay.len(), EVENT_REPLAY_CAPACITY + 1);
        assert!(
            matches!(&replay[0], (1, WireTerminalEvent::ConversationTitleChanged { title }) if title == "Stable title")
        );
        assert_eq!(
            shared.lock().unwrap().replay_events(id, 1).unwrap().len(),
            EVENT_REPLAY_CAPACITY
        );
        record_event(
            &weak,
            id,
            TerminalEvent::ConversationTitleChanged("Renamed".into()),
        );
        let replay = shared.lock().unwrap().replay_events(id, 0).unwrap();
        assert_eq!(
            replay
                .iter()
                .filter(|(_, event)| matches!(
                    event,
                    WireTerminalEvent::ConversationTitleChanged { .. }
                ))
                .count(),
            1
        );
        let _ = handle.shutdown();
    }

    #[test]
    fn leader_exit_preserves_consumption_credit_but_not_process_authority() {
        let shared = Arc::new(Mutex::new(test_host()));
        let recipe = crate::model::LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let session = TerminalSession {
            owned_codex_listener: None,
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
            unset_environment: Default::default(),
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::path::PathBuf::from("/tmp"),
            environment: BTreeMap::new(),
        };
        let mut handles = Vec::new();
        for count in 1..=3 {
            let session = TerminalSession {
                owned_codex_listener: None,
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
            owned_codex_listener: None,
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
