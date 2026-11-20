#[cfg(all(test, target_os = "linux"))]
mod paired_upgrade_tests;
mod presentation;
mod removal;
mod retention;
pub(super) use retention::Retirement;
mod setup_restore;
pub(super) use presentation::snapshot as client_snapshot;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak, atomic::AtomicBool},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use crate::workspace_model::WorkspaceModel;
use crate::{
    model::{
        CardId, HostSessionProof, TerminalContinuity, TerminalExit, TerminalId, TerminalSession,
        TranscriptMetadata,
    },
    terminal::{RenderSnapshot, TerminalConfig, TerminalEvent, TerminalHandle, TerminalSendError},
    terminal_ipc::{
        PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        ShutdownCapability, ShutdownOperation, ShutdownOutcome, ShutdownPreflight, StartupState,
        TerminalDescriptor, TerminalSnapshot, TerminalSpawnOptions, TerminalVisibility,
        WireTerminalEvent,
    },
    workspace_model::ModelEdit,
};

use super::delivery::{DEFAULT_CLIENT_QUEUE_CAPACITY, DeliveryQueue, Outbound};
pub(super) mod update;

const EVENT_REPLAY_CAPACITY: usize = 256;
pub(super) const MAX_TERMINALS: usize = 1024;
const SHUTDOWN_CONFIRMATION_TTL: Duration = Duration::from_secs(60);
const MAX_SHUTDOWN_CAPABILITIES: usize = 32;

pub(super) const RETAINED_CHILD_CLEANUP_MESSAGE: &str = "Original helper cleanup is still retained or quarantined outside the session inventory; termination was not requested. Wait for exact cleanup, or resolve retained ownership diagnostics before retrying.";

/// Why the shared model cannot be served.
///
/// Reported rather than answered with an empty model: an empty model is a
/// meaningful answer - no window has seeded the registry yet - and a window
/// that believed it would offer to donate its state over and over.
fn missing_registry() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::Internal,
        crate::workspace_model::REGISTRY_UNAVAILABLE,
        false,
    )
}

fn model_unavailable(error: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, error, false)
}

pub(crate) type ClientId = u64;
/// The lock every host user serializes on; see [`super::lock`].
pub(crate) type HostLock = super::lock::HostMutex<HostState>;
pub(crate) type SharedHost = Arc<HostLock>;

pub(crate) struct TerminalRecord {
    screen_lost_after_crash: bool,
    /// Finite presentation revisions outlive disconnected viewports. Hidden
    /// reconnects suppress replay instead of sending their retained revision.
    presentation_watermark: u64,
    pub(super) session: TerminalSession,
    pub(super) handle: TerminalHandle,
    pub(super) execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    /// The actual validated worker command, independent of its issued pane.
    execution_recipe: Option<crate::model::LaunchRecipe>,
    options: TerminalSpawnOptions,
    upgrade_sources: Vec<TerminalSession>,
    upgrade_relaunches: Vec<crate::terminal_ipc::UpgradeRelaunch>,
    running: bool,
    completion_observed: bool,
    completed_at: Option<Instant>,
    last_exit: Option<TerminalExit>,
    event_sequence: u64,
    events: VecDeque<(u64, WireTerminalEvent)>,
    /// Stable provider metadata must survive eviction from the transition log.
    conversation_title: Option<(u64, String)>,
    /// Actual OSC/display title, separate from provider conversation metadata.
    display_title: Option<String>,
    working_directory: Option<(u64, PathBuf)>,
}

impl TerminalRecord {
    pub(super) fn execution_launch(&self) -> &crate::model::LaunchRecipe {
        self.execution_recipe
            .as_ref()
            .unwrap_or(&self.session.launch)
    }

    /// The title the provider last set, for the session directory.
    pub(super) fn conversation_title(&self) -> Option<&str> {
        self.conversation_title
            .as_ref()
            .map(|(_, title)| title.as_str())
    }

    /// The shell-reported working directory, falling back to the launch one.
    pub(super) fn current_directory(&self) -> &std::path::Path {
        self.working_directory
            .as_ref()
            .map_or(&self.session.launch.working_directory, |(_, path)| path)
    }
}

#[derive(Default)]
struct OutputObservation {
    epoch: u64,
    after_sequence: u64,
    notified: bool,
}

/// The frame last recorded as sent to one subscription.
struct SentFrame {
    snapshot: Arc<RenderSnapshot>,
    /// Whether the frame was queued in the same hold that recorded it, as a
    /// redraw broadcast's are. A `ClientRequest::Snapshot` reply is recorded
    /// under the lock but queued by its connection after the lock is
    /// released, so a broadcast can reach the queue first: it must not be a
    /// delta against a frame the subscriber may not hold yet.
    queued: bool,
}

#[derive(Default)]
struct Subscriptions {
    viewports: BTreeMap<(ClientId, TerminalId), presentation::Viewport>,
    size_owners: BTreeMap<TerminalId, ClientId>,
    by_client: BTreeMap<ClientId, BTreeMap<TerminalId, TerminalVisibility>>,
    by_terminal: BTreeMap<TerminalId, BTreeMap<ClientId, TerminalVisibility>>,
    /// The exact capture last handed to each subscription.
    ///
    /// `VisualResources::capture` returns the same `Arc<RowSnapshot>` for every
    /// row it did not rebuild, so keeping the previous capture per subscriber
    /// lets the next repaint be diffed by pointer identity and sent as a row
    /// delta. Dropped on detach and disconnect so a stale base can never be
    /// used as a delta base, and so a departing client frees its rows.
    sent: BTreeMap<(ClientId, TerminalId), SentFrame>,
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
        self.viewports.remove(&(client_id, terminal_id));
        self.sent.remove(&(client_id, terminal_id));
        self.output.remove(&(client_id, terminal_id));
    }

    /// Remembers the capture just sent to `client_id`, so the next repaint for
    /// that subscription can be a delta against it. `queued` says whether it
    /// was queued under this same hold; see [`SentFrame::queued`].
    fn record_sent(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
        snapshot: Arc<RenderSnapshot>,
        queued: bool,
    ) {
        self.sent
            .insert((client_id, terminal_id), SentFrame { snapshot, queued });
    }

    fn last_sent(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Option<&Arc<RenderSnapshot>> {
        self.sent
            .get(&(client_id, terminal_id))
            .map(|sent| &sent.snapshot)
    }

    fn disconnect(&mut self, client_id: ClientId) {
        let Some(terminals) = self.by_client.remove(&client_id) else {
            return;
        };
        self.sent
            .retain(|(sent_client, _), _| *sent_client != client_id);
        self.output.retain(|(client, _), _| *client != client_id);
        self.viewports.retain(|(client, _), _| *client != client_id);
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

struct PendingPublication {
    handle: TerminalHandle,
    source: TerminalSession,
    pane: Option<crate::model::CardId>,
    original: Option<(crate::model::CardId, u64)>,
    reservation: Option<super::workspace::PendingTerminal>,
}

pub(crate) struct HostState {
    recover_holders: bool,
    /// Shared, never mutated: a redraw broadcast hands the same proof to every
    /// subscriber's queue, and cloning three `String`s per subscriber per frame
    /// made per-frame allocation scale with the number of attached views.
    proof: Arc<HostSessionProof>,
    /// Where this daemon allocates each Codex conversation's own listener.
    pub(super) codex_listeners: super::chat::CodexListenerRoots,
    pub(super) terminals: BTreeMap<TerminalId, TerminalRecord>,
    pub(super) terminal_retirements: BTreeMap<TerminalId, Retirement>,
    spawning: BTreeSet<TerminalId>,
    /// Actual parent-owned records installed before an adoption commit. They
    /// remain listed and explicitly closeable if confirmation is unavailable.
    startup_custody: BTreeSet<TerminalId>,
    publication_pending: BTreeMap<TerminalId, PendingPublication>,
    /// Missing original owners after an acknowledged incomplete update. Their
    /// saved pane recipes are evidence, never automatic execution authority.
    paused_update_terminals: BTreeSet<TerminalId>,
    startup_recovery: Option<String>,
    /// Immutable accepted inventory survives a failed final unlink/fsync as
    /// well as its Store handle. This is live custody, not a durable retry or
    /// permission to replay any recipe.
    retained_startup_checkpoint: Option<crate::upgrade::handoff::Checkpoint>,
    /// What this daemon started while startup recovery held original
    /// ownership, which that hold therefore does not cover.
    pub(super) recovery_independence: super::recovery_admission::Independence,
    #[cfg(test)]
    pub(super) test_fail_startup_commit: Option<TerminalId>,
    #[cfg(test)]
    test_checkpoint_failure_at: Option<(TerminalId, PathBuf)>,
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
    /// Layout persistence and hydration never borrow the global terminal lock.
    pub(super) workspace_registry: Option<Arc<Mutex<super::model::Registry>>>,
    pub(super) execution_registry: Option<Arc<Mutex<super::model::Registry>>>,
    /// Connections that authenticated as an agent session, and its lineage.
    pub(super) agent_clients: BTreeMap<ClientId, crate::agent_execution::Lineage>,
    pub(super) automations: Option<super::automations::Handle>,
    /// Answers Work board reads; absent before startup and after shutdown.
    pub(super) work: Option<super::work::Handle>,
    /// Wakes the courier that hands messages to idle conversations.
    pub(super) mailbox: Option<super::agent_execution::mailbox::Handle>,
    /// Seals the messages the courier delivers and recognizes them in
    /// transcripts. Absent without the registry that keeps its key.
    pub(super) mailbox_seal: Option<Arc<crate::agent_execution::mailbox::Seal>>,
    /// Operations own their gates, including while waiting to acquire them.
    /// Weak entries let later lookups reclaim gates after every caller leaves.
    /// Each gate holds the run whose dispatcher released it to wait for the
    /// provider to become ready, before the prompt was submitted.
    pub(super) execution_gates:
        BTreeMap<crate::model::AgentId, Weak<Mutex<Option<super::agent_execution::Undelivered>>>>,
    /// Serializes starting an agent's queued prompts, which may first reopen
    /// its conversation. Kept apart from `execution_gates`: reopening adopts
    /// the successor under that gate.
    pub(super) execution_starts: BTreeMap<crate::model::AgentId, Weak<Mutex<()>>>,
    /// The latest runs that went out once their agent's conversation was
    /// reopened for them, so a retry or a racing delivery still reports the
    /// reopening. Bounded, and lost with the daemon.
    pub(super) reopened_runs: std::collections::VecDeque<crate::model::RunId>,
    /// Resume successors that took over their agent when they started and
    /// have been given no work since. Ending one gives the agent back stopped,
    /// for the next resume to take over, rather than retiring it: until then
    /// the successor is only a reopening its card may still give up.
    pub(super) idle_adoptions: BTreeSet<TerminalId>,
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
    client_pids: BTreeMap<ClientId, u32>,
    pub(super) notifications: Option<super::notifications::Handle>,
    pub(super) notification_closed: BTreeSet<TerminalId>,
    notification_watchers: BTreeMap<ClientId, crate::notifications::Presence>,
    notification_activation:
        BTreeMap<crate::notifications::Destination, crate::notifications::Target>,
    subscriptions: Subscriptions,
    pub(super) shutting_down: bool,
    shutdown_requested: bool,
    /// Ordinary exit has closed new child reservations but still owns exact
    /// preexisting tickets. This is independent of terminal/model inventory.
    ordinary_exit_cleanup_pending: bool,
    /// Registration and the final delivery snapshot share this HostState lock.
    /// A client thread accepted before the seal may register only as closed.
    ordinary_exit_registration_closed: bool,
    /// An admitted checked Close owns its correlated reply through queueing.
    /// Its terminal may be the final startup owner blocking ordinary exit.
    checked_close_replies: BTreeSet<ClientId>,
    pub(super) inventory_generation: u64,
    published_terminal_workers: removal::PublishedWorkers,
    shutdown_capabilities: VecDeque<(ShutdownCapability, Instant)>,
    pub(super) native_chat_capacity: usize,
    /// How long each provider account's prompt caches last, as observed.
    pub(super) prompt_cache: Arc<super::agent_execution::prompt_cache::Calibration>,
    pub(super) chats: BTreeMap<TerminalId, super::chat::Record>,
    pub(super) chat_handoffs: BTreeMap<TerminalId, super::chat::TerminalHandoff>,
    pub(super) chat_spawning: BTreeMap<TerminalId, crate::chat::Session>,
    /// Subset of transition reservations that retire an exact original source.
    pub(super) chat_close_reservations: BTreeMap<TerminalId, super::chat::capacity::CloseRemoval>,
    pub(super) pane_chat_starts: BTreeMap<CardId, super::chat::pane_start::Claim>,
    /// Private inventory exclusions for exact historical cleanup reservations.
    pub(super) historical_chat_retirements: BTreeMap<TerminalId, crate::chat::Session>,
    pub(super) chat_bindings: BTreeMap<TerminalId, super::chat::PendingBinding>,
    pub(super) native_closing: BTreeMap<TerminalId, super::chat::NativeClose>,
    pub(super) native_actor_outcomes: Option<super::chat::retained_actor::Broker>,
    pub(super) native_actor_startups: BTreeMap<TerminalId, super::chat::retained_actor::Startup>,
    pub(super) native_adoption_failures: super::chat::AdoptionFailures,
    pub(crate) agent_observer: Arc<crate::agent::observe::Observer>,
    pub(super) agent_activity_wake: Arc<super::activity::Wake>,
    /// Provider listings running outside the control gate.
    pub(super) agent_probes: Arc<super::activity::Probes>,
    /// Exclusive upgrade barrier; ordinary client operations remain concurrent.
    pub(super) control_gate: Arc<super::control_gate::ControlGate>,
    pub(super) account_lifecycle: super::account_lifecycle::Reservations,
    pub(super) worktree_removals: super::worktree_removal::Reservations,
    upgrade: Option<PreparedUpgrade>,
    pub(super) upgrade_chats: BTreeMap<TerminalId, Vec<crate::upgrade::handoff::ChatSource>>,
    pub(super) chat_resolutions: BTreeMap<ClientId, super::chat::upgrade_resolution::Attempt>,
    /// Where this daemon's upgrade handoff keeps durable evidence, including
    /// retained conversations a restore could not reattach.
    pub(super) upgrade_evidence: Option<Arc<crate::upgrade::handoff::Store>>,
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
    /// Exact registered owners promised uninterrupted process continuity by a
    /// confirmed update. Errors must orphan these owners, never release them.
    continuing: Vec<TerminalHandle>,
    /// Stable lossless reader/input cuts remain reversible until every carry
    /// or emergency preservation outcome has settled, including final-save failure.
    prepared_input: Vec<PreparedTerminalInput>,
}

/// A unique reversible PTY cut, separate from the existing pidfd process
/// freezes. Cancellation never certifies ownership transfer or reader restart.
struct PreparedTerminalInput {
    handle: TerminalHandle,
    token: Arc<AtomicBool>,
    resumed: crossbeam_channel::Receiver<Result<(), String>>,
}

impl PreparedTerminalInput {
    fn prepare(handle: TerminalHandle) -> Result<Self, String> {
        let token = Arc::new(AtomicBool::new(true));
        let (reply, resumed) = crossbeam_channel::bounded(1);
        // Own rollback before sending: timeout or any later save/refusal must
        // cancel this exact token, never a subsequent preparation.
        let cut = Self {
            handle,
            token,
            resumed,
        };
        cut.handle.prepare_input_handoff(cut.token.clone(), reply)?;
        Ok(cut)
    }
}

impl Drop for PreparedTerminalInput {
    fn drop(&mut self) {
        self.handle.cancel_input_handoff(&self.token);
        match self.resumed.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(%error, "terminal input rollback remains unconfirmed; original owner stays fenced")
            }
            Err(error) => {
                tracing::error!(%error, "terminal input cut outcome remains unconfirmed; original owner stays fenced")
            }
        }
    }
}

impl HostState {
    pub(super) fn enable_holder_recovery(&mut self) {
        self.recover_holders = true;
    }
    pub(crate) fn new(proof: HostSessionProof) -> Self {
        Self {
            recover_holders: false,
            account_lifecycle: Default::default(),
            worktree_removals: Default::default(),
            proof: Arc::new(proof),
            codex_listeners: super::chat::CodexListenerRoots::at(std::env::temp_dir()),
            terminals: BTreeMap::new(),
            terminal_retirements: BTreeMap::new(),
            spawning: BTreeSet::new(),
            startup_custody: BTreeSet::new(),
            publication_pending: BTreeMap::new(),
            paused_update_terminals: BTreeSet::new(),
            startup_recovery: None,
            retained_startup_checkpoint: None,
            recovery_independence: Default::default(),
            #[cfg(test)]
            test_fail_startup_commit: None,
            #[cfg(test)]
            test_checkpoint_failure_at: None,
            browser_logins_spawning: BTreeSet::new(),
            clients: BTreeMap::new(),
            watchers: BTreeSet::new(),
            model: None,
            workspace_registry: None,
            execution_registry: None,
            agent_clients: BTreeMap::new(),
            automations: None,
            work: None,
            mailbox: None,
            mailbox_seal: None,
            execution_gates: BTreeMap::new(),
            execution_starts: BTreeMap::new(),
            reopened_runs: std::collections::VecDeque::new(),
            idle_adoptions: BTreeSet::new(),
            execution_startups: BTreeMap::new(),
            model_watchers: BTreeSet::new(),
            client_pids: BTreeMap::new(),
            notifications: None,
            notification_closed: BTreeSet::new(),
            notification_watchers: BTreeMap::new(),
            notification_activation: BTreeMap::new(),
            subscriptions: Subscriptions::default(),
            shutting_down: false,
            shutdown_requested: false,
            ordinary_exit_cleanup_pending: false,
            ordinary_exit_registration_closed: false,
            checked_close_replies: BTreeSet::new(),
            inventory_generation: 0,
            published_terminal_workers: Default::default(),
            shutdown_capabilities: VecDeque::new(),
            native_chat_capacity: crate::native_chat_capacity::DEFAULT_LIMIT as usize,
            prompt_cache: Arc::new(super::agent_execution::prompt_cache::Calibration::in_memory()),
            chats: BTreeMap::new(),
            chat_handoffs: BTreeMap::new(),
            chat_spawning: BTreeMap::new(),
            chat_close_reservations: BTreeMap::new(),
            pane_chat_starts: BTreeMap::new(),
            historical_chat_retirements: BTreeMap::new(),
            chat_bindings: BTreeMap::new(),
            native_closing: BTreeMap::new(),
            native_actor_outcomes: None,
            native_actor_startups: BTreeMap::new(),
            native_adoption_failures: Default::default(),
            agent_observer: Arc::new(crate::agent::observe::Observer::new()),
            agent_activity_wake: Arc::new(super::activity::Wake::default()),
            agent_probes: Arc::default(),
            control_gate: Arc::default(),
            upgrade: None,
            upgrade_chats: BTreeMap::new(),
            chat_resolutions: BTreeMap::new(),
            upgrade_evidence: None,
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
        if self.ordinary_exit_registration_closed {
            // The existing writer closes both stream directions when this
            // already-closed queue drains; its reader cannot hang final join.
            delivery.close_after_drain();
            return delivery;
        }
        self.clients.insert(client_id, Arc::clone(&delivery));
        delivery
    }

    pub(super) fn record_client_pid(&mut self, client: ClientId, pid: u32) {
        if self.clients.contains_key(&client) {
            self.client_pids.insert(client, pid);
        }
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
        self.chat_resolutions.remove(&client_id);
        if self.notification_watchers.remove(&client_id).is_some()
            && let Some(notifications) = &self.notifications
        {
            notifications.presence(client_id, None);
        }
        self.clients.remove(&client_id);
        self.client_pids.remove(&client_id);
        self.watchers.remove(&client_id);
        self.model_watchers.remove(&client_id);
        self.agent_clients.remove(&client_id);
    }

    /// Installs the durable shared sidebar model.
    ///
    /// The daemon's entry point opens the store, not [`HostState::new`]: a host
    /// constructed for a test must not reach into the user's data directory, and
    /// a daemon that cannot open the store still has terminals to host.
    pub(crate) fn install_model(&mut self, registry: super::model::Registry) {
        self.mailbox_seal = match registry.mailbox_key() {
            Ok(key) => Some(Arc::new(crate::agent_execution::mailbox::Seal::new(key))),
            Err(error) => {
                tracing::error!(%error, "cannot read the mailbox key; messages are unavailable");
                None
            }
        };
        self.execution_registry = match registry.execution_connection() {
            Ok(connection) => Some(Arc::new(Mutex::new(connection))),
            Err(error) => {
                tracing::error!(%error, "cannot open the agent execution registry");
                None
            }
        };
        self.workspace_registry = match registry.execution_connection() {
            Ok(connection) => Some(Arc::new(Mutex::new(connection))),
            Err(error) => {
                tracing::error!(%error, "cannot open the workspace model writer");
                None
            }
        };
        self.model = Some(registry);
    }

    /// Another connection to the shared-model store, for work that must not
    /// hold the host lock while it writes.
    pub(super) fn independent_model(&self) -> Option<Result<super::model::Registry, String>> {
        self.model
            .as_ref()
            .map(super::model::Registry::execution_connection)
    }

    /// How many hosted agents are working, and how many are waiting on the
    /// user, across running terminals and native conversations.
    pub(super) fn agent_tally(&self) -> crate::tray::AgentTally {
        use crate::agent::ActivityState;
        let terminals = self
            .terminals
            .values()
            .filter(|record| record.running)
            .filter_map(|record| record.handle.agent_activity())
            .map(|activity| activity.state);
        let chats = self.chats.values().map(|record| record.handle.activity());
        terminals
            .chain(chats)
            .fold(crate::tray::AgentTally::default(), |mut tally, state| {
                match state {
                    ActivityState::Working => tally.working += 1,
                    ActivityState::Waiting => tally.waiting += 1,
                    ActivityState::Idle | ActivityState::Unknown => {}
                }
                tally
            })
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
    ) -> Option<crate::notifications::Target> {
        let activation = self.notification_activation.remove(&presence.destination());
        if let Some(notifications) = &self.notifications {
            notifications.presence(client, Some(presence.clone()));
        }
        self.notification_watchers.insert(client, presence);
        activation
    }

    /// The sessions this publication adds to its window's live claim.
    pub(super) fn notification_sessions_unclaimed(
        &self,
        presence: &crate::notifications::Presence,
    ) -> Vec<TerminalId> {
        presence
            .sessions
            .iter()
            .filter(|session| {
                !self.notification_watchers.values().any(|current| {
                    current.destination() == presence.destination()
                        && current.sessions.contains(session)
                })
            })
            .copied()
            .collect()
    }

    pub(super) fn notification_presence(&mut self, presence: crate::notifications::Presence) {
        for (&client, current) in &mut self.notification_watchers {
            if current.destination() == presence.destination() {
                *current = presence.clone();
                if let Some(notifications) = &self.notifications {
                    notifications.presence(client, Some(presence.clone()));
                }
            }
        }
    }

    /// Tells every window watching notifications that `repository`'s pending
    /// automation requests changed, so one offering them reads them again.
    pub(in crate::host) fn automation_proposals_changed(&self, repository: &std::path::Path) {
        for client in self.notification_watchers.keys() {
            if let Some(delivery) = self.clients.get(client) {
                let _ = delivery.push(ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof().clone(),
                    message: ServerMessage::AutomationProposalsChanged {
                        repository: repository.to_path_buf(),
                    },
                });
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

    #[cfg(test)]
    pub(super) fn activate_notification(
        &mut self,
        target: crate::notifications::Target,
        source_session: Option<TerminalId>,
        remembered_window: Option<&str>,
    ) -> String {
        self.activate_notification_destination(target, source_session, remembered_window, None)
            .window
    }

    pub(super) fn activate_notification_destination(
        &mut self,
        target: crate::notifications::Target,
        source_session: Option<TerminalId>,
        remembered_window: Option<&str>,
        preferred: Option<&crate::notifications::Destination>,
    ) -> crate::notifications::Destination {
        use crate::notifications::{Application, Destination};
        let owner = self
            .notification_watchers
            .iter()
            .filter(|(_, presence)| {
                source_session.is_some_and(|session| presence.sessions.contains(&session))
                    || (matches!(
                        target,
                        crate::notifications::Target::AgentRun(_)
                            | crate::notifications::Target::Automation { .. }
                    ) && presence.application == Application::Cagents)
                    || preferred.is_some_and(|destination| *destination == presence.destination())
            })
            .max_by_key(|(_, presence)| {
                (
                    source_session.is_some_and(|session| presence.focused == Some(session)),
                    Some(presence.window.as_str()) == remembered_window,
                    source_session.is_some_and(|session| presence.sessions.contains(&session)),
                    presence.window == crate::instance::PRIMARY_WINDOW,
                )
            });
        if let Some((&client, presence)) = owner {
            let destination = presence.destination();
            if let Some(delivery) = self.clients.get(&client) {
                let _ = delivery.push(ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: self.proof().clone(),
                    message: ServerMessage::NotificationActivated { target },
                });
            }
            return destination;
        }
        let destination = preferred.cloned().unwrap_or_else(|| Destination {
            application: Application::Cagents,
            window: remembered_window
                .unwrap_or(crate::instance::PRIMARY_WINDOW)
                .to_owned(),
        });
        if self.notification_activation.len() < 32
            || self.notification_activation.contains_key(&destination)
        {
            self.notification_activation
                .insert(destination.clone(), target);
        }
        destination
    }

    pub(super) fn subscribe_model(&mut self, client_id: ClientId) {
        self.model_watchers.insert(client_id);
    }

    /// Subscribes `client_id` to shared-model edges and returns the model.
    ///
    /// Both happen under one lock, so an edit applied between subscribing and
    /// reading cannot fall into a gap: a client either sees it in the model it
    /// is handed or receives it as an edge afterwards, never neither.
    #[cfg(test)]
    pub(crate) fn watch_model(
        &mut self,
        client_id: ClientId,
    ) -> Result<WorkspaceModel, ProtocolError> {
        let model = self.registry()?.model().map_err(model_unavailable)?;
        self.model_watchers.insert(client_id);
        Ok(model)
    }

    #[cfg(test)]
    pub(crate) fn read_model(&self) -> Result<WorkspaceModel, ProtocolError> {
        self.model
            .as_ref()
            .ok_or_else(missing_registry)?
            .model()
            .map_err(model_unavailable)
    }

    /// Applies one edit and tells every other watcher about it.
    #[cfg(test)]
    pub(crate) fn edit_model(
        &mut self,
        origin: ClientId,
        edit: ModelEdit,
    ) -> Result<WorkspaceModel, ProtocolError> {
        self.edit_model_delta(origin, edit)?;
        self.registry()?.model().map_err(model_unavailable)
    }

    pub(crate) fn edit_model_delta(
        &mut self,
        origin: ClientId,
        edit: ModelEdit,
    ) -> Result<crate::workspace_model::delta::EditReceipt, ProtocolError> {
        self.ensure_view_retention_available(&edit)?;
        let edit = edit
            .canonicalized()
            .map_err(|error| ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false))?;
        if let ModelEdit::SuggestConversationTitle {
            id,
            card,
            expected_revision,
            title,
            source,
        } = &edit
        {
            let expected = self
                .registry()?
                .title_conversation(*id, *card, *expected_revision)
                .map_err(model_unavailable)?;
            let invalid = || {
                ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    "The conversation's current name or source changed; refresh before suggesting a title",
                    false,
                )
            };
            let actual = self.chats.get(&expected.id).ok_or_else(invalid)?;
            if expected.proof.as_ref() != Some(self.proof())
                || actual.session.launch.provider != expected.launch.provider
                || actual.session.launch.recipe != expected.launch.recipe
            {
                return Err(invalid());
            }
            // The request holds workspace writer admission as well as this
            // resident identity guard. Read only current metadata, not Store,
            // history or files; later edits cannot overtake this admission.
            let (session, current_title) =
                actual.handle.conversation_title().map_err(|reason| {
                    let busy = reason == crate::chat::runtime::TitleUnavailable::Busy;
                    ProtocolError::new(
                        if busy {
                            ProtocolErrorCode::QueueFull
                        } else {
                            ProtocolErrorCode::Internal
                        },
                        if busy {
                            "Conversation metadata is busy; retry the original title suggestion"
                        } else {
                            "Conversation metadata is unavailable"
                        },
                        busy,
                    )
                })?;
            // A pane that has not recorded its conversation is identified by
            // its resident chat, as the view writer identifies it, whether or
            // not the provider has reported the conversation yet: Claude
            // reports it only after it reads the first prompt, and a chat
            // shown in no window reads no snapshot, so its pane learns the
            // conversation only when someone opens it. A pane that recorded
            // one names only that conversation.
            if let Some(recorded) = &expected.launch.session_id
                && session.as_ref() != Some(recorded)
            {
                return Err(invalid());
            }
            let current_title = current_title
                .as_deref()
                .and_then(crate::workspace_model::channel_title);
            match source {
                crate::model::ChannelTitleSource::Provider
                    if current_title.as_ref() == Some(title) => {}
                crate::model::ChannelTitleSource::Prompt if current_title.is_none() => {}
                _ => return Err(invalid()),
            }
        }
        if matches!(
            &edit,
            ModelEdit::ChangeView {
                change: crate::workspace_model::views::Change::RemoveSource { .. },
                ..
            }
        ) {
            let ModelEdit::ChangeView { operation, .. } = &edit else {
                unreachable!()
            };
            if !self
                .registry()?
                .view_operation_seen(*operation)
                .map_err(model_unavailable)?
            {
                self.validate_view_removal(&edit).map_err(|reason| {
                    ProtocolError::new(ProtocolErrorCode::InvalidRequest, reason, false)
                })?;
            }
        }
        let applied = super::workspace::apply_delta(self.registry()?, &edit)?;
        if applied.changed {
            self.announce_model_edit(
                origin,
                applied.receipt.sequence,
                &edit,
                &applied.receipt.effects,
            );
        }
        Ok(applied.receipt)
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
    pub(super) fn announce_model_edit(
        &self,
        origin: ClientId,
        sequence: u64,
        edit: &ModelEdit,
        effects: &crate::workspace_model::delta::EditEffects,
    ) {
        let edit = edit.committed();
        for client_id in &self.model_watchers {
            if *client_id == origin {
                continue;
            }
            if let Some(delivery) = self.clients.get(client_id) {
                let _ = delivery.push(Outbound::broadcast(
                    &self.proof,
                    ServerMessage::WorkspaceModelEdited {
                        sequence,
                        edit: edit.as_ref().clone(),
                        effects: effects.clone(),
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
        self.chat_resolutions.remove(&client_id);
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
        // With each terminal's visibility before the disconnect: only a change
        // is sent, as every other writer does. Re-sending Visible while
        // another client still shows the terminal would force a full capture.
        let terminals = self
            .subscriptions
            .by_client
            .get(&client_id)
            .map(|terminals| {
                terminals
                    .keys()
                    .map(|terminal_id| (*terminal_id, self.subscriptions.visibility(*terminal_id)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let handles = terminals
            .iter()
            .filter_map(|(terminal_id, _)| {
                self.terminals
                    .get(terminal_id)
                    .map(|record| record.handle.clone())
            })
            .collect::<Vec<_>>();
        for (terminal_id, _) in &terminals {
            self.retain_presentation_watermark(client_id, *terminal_id);
        }
        self.subscriptions.disconnect(client_id);
        for (terminal_id, old_visibility) in terminals {
            self.refresh_presentation_owner(terminal_id);
            let new_visibility = self.subscriptions.visibility(terminal_id);
            if old_visibility != new_visibility
                && let Some(record) = self.terminals.get(&terminal_id)
            {
                record.handle.set_host_visibility(new_visibility);
            }
            self.disarm_output_if_observed(terminal_id);
        }
        if let Some(delivery) = self.clients.remove(&client_id) {
            delivery.close_after_drain();
        }
        self.client_pids.remove(&client_id);
        self.watchers.remove(&client_id);
        self.model_watchers.remove(&client_id);
        self.agent_clients.remove(&client_id);
        handles
    }

    pub(crate) fn attach(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        self.ensure_retention_available(&[terminal_id])?;
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
        if old_visibility != new_visibility {
            record.handle.set_host_visibility(new_visibility);
        }
        self.rearm_output(client_id, terminal_id);
        self.descriptor(client_id, terminal_id)
    }

    pub(crate) fn detach(
        &mut self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<(), ProtocolError> {
        if !self.terminals.contains_key(&terminal_id) {
            return Err(not_found(terminal_id));
        }
        if !self.subscriptions.attached(client_id, terminal_id) {
            return Ok(());
        }
        self.retain_presentation_watermark(client_id, terminal_id);
        let old_visibility = self.subscriptions.visibility(terminal_id);
        self.subscriptions.detach(client_id, terminal_id);
        let new_visibility = self.subscriptions.visibility(terminal_id);
        if old_visibility != new_visibility {
            self.terminals[&terminal_id]
                .handle
                .set_host_visibility(new_visibility);
        }
        self.refresh_presentation_owner(terminal_id);
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
        if old_visibility != new_visibility {
            record.handle.set_host_visibility(new_visibility);
        }
        self.refresh_presentation_owner(terminal_id);
        if requested == TerminalVisibility::Visible {
            let generation = self.presentation_revision(client_id, terminal_id);
            if let Some(delivery) = self.clients.get(&client_id) {
                let _ = delivery.push(Outbound::broadcast(
                    &self.proof,
                    ServerMessage::Redraw {
                        terminal_id,
                        generation,
                    },
                ));
            }
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

    /// The descriptors after `after`, in identity order, until their encoding
    /// would pass `budget` bytes, and whether the page ends the inventory. A
    /// page always holds at least one descriptor when any follows `after`.
    pub(crate) fn list_page(
        &self,
        client_id: ClientId,
        after: Option<TerminalId>,
        budget: usize,
    ) -> (Vec<TerminalDescriptor>, bool) {
        let following = match after {
            Some(after) => self
                .terminals
                .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded)),
            None => self.terminals.range(..),
        };
        let mut page = Vec::new();
        let mut used = 0;
        for terminal_id in following.map(|(terminal_id, _)| *terminal_id) {
            let Ok(descriptor) = self.descriptor(client_id, terminal_id) else {
                continue;
            };
            // A descriptor that cannot be measured cannot be encoded either;
            // charging it the whole budget gives it a page of its own.
            let size = postcard::experimental::serialized_size(&descriptor).unwrap_or(budget);
            if !page.is_empty() && used + size > budget {
                return (page, false);
            }
            used += size;
            page.push(descriptor);
        }
        (page, true)
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

    /// Typed recovery projects this already authenticated paired worker before
    /// installing the native-console association. Subscription state does not
    /// grant or remove the worker's current daemon ownership.
    pub(super) fn native_terminal_projection(
        &self,
        id: TerminalId,
    ) -> Result<TerminalSession, String> {
        let terminal = self
            .terminals
            .get(&id)
            .ok_or("The native console worker is unavailable")?;
        if terminal.handle.is_completed() || !self.proof().is_complete() {
            return Err(
                "The native console worker completed or its current ownership is unavailable"
                    .into(),
            );
        }
        let proof = self.proof();
        let mut current = terminal.session.clone();
        current.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        Ok(current)
    }

    pub(super) fn terminal_options(&self, id: TerminalId) -> Option<TerminalSpawnOptions> {
        self.terminals.get(&id).map(|record| record.options.clone())
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
            transcript: transcript_metadata(
                record.session.transcript_path.as_deref(),
                record.handle.transcript_location(),
            ),
            upgrade_sources: record.upgrade_sources.clone(),
            upgrade_relaunches: record.upgrade_relaunches.clone(),
            title: record.display_title.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn reserve_spawn(&mut self, terminal_id: TerminalId) -> Result<(), ProtocolError> {
        self.reserve_spawn_for_view(terminal_id, None)
    }

    pub(super) fn retire_terminal(&mut self, terminal_id: TerminalId) -> Option<TerminalRecord> {
        let record = self.terminals.remove(&terminal_id)?;
        // A long-lived resident can outlast the bounded publication history.
        // Retain this actual departing worker before absence becomes visible.
        self.published_terminal_workers
            .observe(terminal_id, record.handle.execution_instance());
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

    #[cfg(test)]
    fn reserve_spawn_for_view(
        &mut self,
        terminal_id: TerminalId,
        paired: Option<&TerminalSession>,
    ) -> Result<(), ProtocolError> {
        self.reserve_spawn_for_view_reserved(terminal_id, paired, None)
    }

    fn reserve_spawn_for_view_reserved(
        &mut self,
        terminal_id: TerminalId,
        paired: Option<&TerminalSession>,
        transition: Option<&super::chat::TerminalViewReservation<'_>>,
    ) -> Result<(), ProtocolError> {
        let owns_transition = transition.is_some_and(|transition| {
            paired.is_some_and(|source| source.id == terminal_id)
                && transition.authorizes(self, terminal_id)
        });
        if transition.is_some() && !owns_transition {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "The reserved native attachment changed before opening its CLI view",
                true,
            ));
        }
        if self.shutting_down {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "the cagents daemon is shutting down",
                true,
            ));
        }
        if (self.chat_spawning.contains_key(&terminal_id) && !owns_transition)
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

    /// Whether any of `sessions` runs, or is starting or handing off, here.
    /// A successor joins a lineage only when none of its members does. A
    /// terminal counts until its teardown completes, as long as its authority
    /// can still act.
    pub(super) fn any_live(&self, sessions: &[TerminalId]) -> bool {
        sessions.iter().any(|id| self.is_live(id))
    }

    /// Whether any of `sessions` other than `predecessor` is live, for a
    /// successor whose start has itself proved `predecessor` stopped.
    pub(super) fn any_other_live(&self, sessions: &[TerminalId], predecessor: TerminalId) -> bool {
        sessions
            .iter()
            .any(|id| *id != predecessor && self.is_live(id))
    }

    pub(super) fn is_live(&self, id: &TerminalId) -> bool {
        self.terminals
            .get(id)
            .is_some_and(|record| record.running || !record.handle.is_completed())
            || self.spawning.contains(id)
            || self
                .chats
                .get(id)
                .is_some_and(|record| record.handle.owns_work() || record.returning_terminal)
            || self.chat_spawning.contains_key(id)
            || self.chat_handoffs.contains_key(id)
            || self.native_actor_startups.contains_key(id)
            || self.native_closing.contains_key(id)
            || self.historical_chat_retirements.contains_key(id)
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

    /// Records what an installed worker inherits from the holder it adopted:
    /// its lineage, whether the screen was lost, and the conversation's
    /// resume hint. Runs whether or not its source publication succeeded, so
    /// an original kept for recovery still resolves by every earlier source.
    fn record_adopted_original(
        &mut self,
        recovery: &crate::terminal::holder::Recovery,
        screen_lost: bool,
        resume_hint: Option<&str>,
    ) -> Result<(), ProtocolError> {
        let record = self
            .terminals
            .get_mut(&recovery.source.id)
            .ok_or_else(|| not_found(recovery.source.id))?;
        record.screen_lost_after_crash = screen_lost;
        // The worker learns a conversation's identity only from output it
        // sees, which the previous daemon already saw.
        if let Some(hint) = resume_hint {
            record.session.resume_hint = Some(hint.to_owned());
        }
        self.adopt_upgrade_lineage(
            &recovery.source,
            &recovery.previous_sources,
            &recovery.relaunches,
        )
    }

    #[cfg(test)]
    pub(crate) fn finish_spawn(
        &mut self,
        client_id: ClientId,
        session: TerminalSession,
        handle: TerminalHandle,
        initially_visible: bool,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        self.finish_spawn_with_execution(
            super::model::terminal_sources::PublishedTerminal::fixture(),
            client_id,
            session,
            handle,
            initially_visible,
            None,
            None,
        )
    }

    #[cfg(test)]
    fn finish_spawn_with_execution(
        &mut self,
        _published: super::model::terminal_sources::PublishedTerminal,
        client_id: ClientId,
        session: TerminalSession,
        handle: TerminalHandle,
        initially_visible: bool,
        execution_recipe: Option<crate::model::LaunchRecipe>,
        execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    ) -> Result<TerminalDescriptor, ProtocolError> {
        let id = session.id;
        let instance = handle.execution_instance();
        let descriptor = self.install_startup_parent(
            Some(client_id),
            session,
            handle,
            initially_visible,
            execution_recipe,
            execution_environment,
        )?;
        self.published_terminal_workers.observe(id, instance);
        self.rearm_output(client_id, id);
        self.publish_worker(client_id, id);
        Ok(descriptor)
    }

    /// Installs a starting terminal's worker, attaching `subscriber` hidden
    /// when the request came from a client that will present it.
    fn install_startup_parent(
        &mut self,
        subscriber: Option<ClientId>,
        mut session: TerminalSession,
        handle: TerminalHandle,
        initially_visible: bool,
        execution_recipe: Option<crate::model::LaunchRecipe>,
        execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
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
                presentation_watermark: 0,
                session,
                handle,
                options: TerminalSpawnOptions::default(),
                execution_environment,
                execution_recipe,
                upgrade_sources: Vec::new(),
                upgrade_relaunches: Vec::new(),
                running: true,
                completion_observed: false,
                completed_at: None,
                last_exit: None,
                event_sequence: 0,
                events: VecDeque::new(),
                conversation_title: None,
                display_title: None,
                working_directory: None,
            },
        );
        let Some(client_id) = subscriber else {
            return self.descriptor_as(false, terminal_id);
        };
        let visibility = if initially_visible {
            TerminalVisibility::Visible
        } else {
            TerminalVisibility::Hidden
        };
        self.subscriptions
            .attach(client_id, terminal_id, visibility);
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

    #[cfg(test)]
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

    /// Read-only history and presentation remain available after PTY teardown.
    pub(crate) fn history_handle(
        &self,
        client_id: ClientId,
        terminal_id: TerminalId,
    ) -> Result<TerminalHandle, ProtocolError> {
        Ok(self
            .attached_terminal(client_id, terminal_id)?
            .handle
            .clone())
    }

    #[cfg(test)]
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
        if generation
            > self
                .subscriptions
                .viewports
                .get(&(client_id, terminal_id))
                .map_or(record.handle.generation(), |view| view.revision)
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "cannot acknowledge a terminal generation that was not published",
                false,
            ));
        }
        Ok(record.handle.clone())
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
        let cwd = record
            .working_directory
            .iter()
            .filter(|(sequence, _)| {
                *sequence > after_sequence
                    && record
                        .events
                        .front()
                        .is_none_or(|(first, _)| sequence < first)
            })
            .map(|(sequence, path)| {
                (
                    *sequence,
                    WireTerminalEvent::WorkingDirectoryChanged { path: path.clone() },
                )
            });
        let mut replay: Vec<_> = title.chain(cwd).chain(events).collect();
        replay.sort_by_key(|(sequence, _)| *sequence);
        Ok(replay)
    }

    /// Called with exclusive control admission and window-launch fences held.
    /// Existing processes can change cwd without asking the daemon, so automatic
    /// cleanup waits for global process quiescence, not a cached cwd match.
    pub(super) fn allow_automatic_worktree_cleanup(
        &mut self,
        client: ClientId,
        windows: &[crate::instance::VerifiedWindow],
    ) -> Result<(), String> {
        // Account probes/creation now retain a physical reservation instead
        // of the global writer. They can still own operands inside this tree.
        if !self
            .account_lifecycle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
        {
            return Err("An account operation is still using its credential directories".into());
        }
        let verified = |id: &ClientId| {
            self.client_pids
                .get(id)
                .is_some_and(|pid| windows.iter().any(|window| window.pid() == *pid))
        };
        if !verified(&client) || self.model_watchers.iter().any(|id| !verified(id)) {
            return Err("A window has not established directory protection".into());
        }
        if self.shutting_down
            || self.upgrade.is_some()
            || self.inventory_count() != 0
            // An original Child can remain ours after its hosted record is
            // detached or removed, including before holder discovery exists.
            || crate::daemon::child_reaper::has_outstanding_ownership()
            || !self.browser_logins_spawning.is_empty()
            || !self.execution_startups.is_empty()
            || !self.chat_bindings.is_empty()
            || !self.chat_handoffs.is_empty()
            || !self.native_closing.is_empty()
            || !self.native_adoption_failures.is_empty()
        {
            return Err("Hosted process ownership has not settled".into());
        }
        let registry = self
            .model
            .as_ref()
            .ok_or("The session registry is unavailable")?;
        if !registry.cleanup_execution_is_quiescent()?
            || !registry.active_automation_firings()?.is_empty()
        {
            return Err("Accepted or uncertain background work has not settled".into());
        }
        if let Some(store) = &self.upgrade_evidence
            && store.load()?.is_some_and(|checkpoint| {
                !checkpoint.terminals.is_empty()
                    || !checkpoint.carried.is_empty()
                    || !checkpoint.chats.is_empty()
            })
        {
            return Err("Retained upgrade ownership has not settled".into());
        }
        // Recovery discovery is intentionally tolerant of unrelated records;
        // cleanup instead preserves every remaining record it cannot disprove.
        let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
        for kind in [
            crate::daemon::holder_registry::Kind::Terminal,
            crate::daemon::holder_registry::Kind::Native,
        ] {
            let directory = crate::daemon::holder_registry::directory(kind)?;
            crate::daemon::holder_registry::discover(&directory, &boot, kind)?;
            for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    return Err("Detached or uncertain holder ownership remains".into());
                }
            }
        }
        Ok(())
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
        let (background_subagents, background_identity) = self.background_impact();
        let capability = ShutdownCapability {
            proof: (*self.proof).clone(),
            token: uuid::Uuid::new_v4().to_string(),
            inventory_generation: self.inventory_generation,
            live_terminals,
            background_subagents,
            background_identity,
            expires_at_unix_ms: unix_time_ms() + SHUTDOWN_CONFIRMATION_TTL.as_millis() as i64,
        };
        self.shutdown_capabilities
            .push_back((capability.clone(), now + SHUTDOWN_CONFIRMATION_TTL));
        Ok(ShutdownPreflight { capability })
    }

    fn background_impact(&self) -> (u64, String) {
        use sha2::{Digest, Sha256};
        let mut tasks: Vec<_> = self
            .chats
            .iter()
            .filter(|(_, record)| record.handle.owns_work())
            .flat_map(|(&session, record)| {
                record
                    .handle
                    .background_subagents()
                    .into_iter()
                    .map(move |id| (session, id))
            })
            .collect();
        tasks.sort_unstable();
        let mut digest = Sha256::new();
        for (session, task) in &tasks {
            // Session IDs have a fixed UUID spelling. Length-prefix task IDs
            // so provider-controlled separators cannot alias another task.
            digest.update(session.to_string().as_bytes());
            digest.update((task.len() as u64).to_le_bytes());
            digest.update(task.as_bytes());
        }
        let identity = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (tasks.len() as u64, identity)
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
        if !self.terminal_retirements.is_empty() {
            return Err("Terminal retirement is not confirmed; retry after cleanup settles or explicitly close the retained result".into());
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
            || (
                capability.background_subagents,
                capability.background_identity.clone(),
            ) != self.background_impact()
        {
            return Err("Daemon inventory changed; obtain a new upgrade preflight".into());
        }
        if !self.spawning.is_empty() || !self.chat_spawning.is_empty() {
            return Err(
                "Sessions are still starting; wait for reservations to settle and retry".into(),
            );
        }
        // This observes only already-published external custody. A worker or
        // join lane can publish later without taking control_gate; this is not
        // an exit-wide absence fence. Both prepare and commit validate here.
        if crate::daemon::child_reaper::has_retained_ownership() {
            return Err(RETAINED_CHILD_CLEANUP_MESSAGE.into());
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
                if (
                    capability.background_subagents,
                    capability.background_identity.clone(),
                ) != self.background_impact()
                {
                    return ShutdownOutcome::InventoryChanged {
                        current_count: count,
                    };
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

    pub(super) fn begin_ordinary_exit_cleanup(&mut self) {
        self.ordinary_exit_cleanup_pending = true;
        self.shutting_down = true;
        self.shutdown_requested = true;
    }

    pub(super) fn ordinary_exit_cleanup_pending(&self) -> bool {
        self.ordinary_exit_cleanup_pending
    }

    pub(super) fn terminal_close_admission_open(&self) -> bool {
        !self.ordinary_exit_registration_closed
            && (!self.shutting_down || self.ordinary_exit_cleanup_pending)
    }

    /// Called only after the exit caller holds the core's sealed absence
    /// proof. Includes clients registered during the pending cleanup interval.
    pub(super) fn close_ordinary_exit_registration(&mut self) -> Vec<Arc<DeliveryQueue>> {
        self.ordinary_exit_registration_closed = true;
        self.clients.values().cloned().collect()
    }

    pub(super) fn begin_checked_close_reply(
        &mut self,
        client: ClientId,
    ) -> Result<(), ProtocolError> {
        if self.ordinary_exit_registration_closed {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "Ordinary daemon exit has sealed terminal cleanup and client delivery",
                false,
            ));
        }
        if !self.checked_close_replies.insert(client) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::Internal,
                "This connection already owns a checked Close reply",
                false,
            ));
        }
        Ok(())
    }

    pub(super) fn finish_checked_close_reply(&mut self, client: ClientId) {
        if self.checked_close_replies.remove(&client) && self.ordinary_exit_cleanup_pending {
            crate::daemon::wake();
        }
    }

    pub(super) fn ordinary_exit_cleanup_blocked(&self) -> bool {
        self.startup_blocks_shutdown()
            || !self.checked_close_replies.is_empty()
            || !self.terminal_retirements.is_empty()
    }

    pub(super) fn startup_recovery(&self) -> Option<&str> {
        #[cfg(debug_assertions)]
        if let Some(held) = super::fixture_startup_recovery::held() {
            return Some(held.detail());
        }
        self.startup_recovery.as_deref().or_else(|| {
            (!self.startup_custody.is_empty())
                .then_some("Original terminal ownership is awaiting its startup outcome")
        })
    }

    pub(super) fn startup_custody_contains(&self, id: TerminalId) -> bool {
        self.startup_custody.contains(&id)
    }

    /// The update checkpoint this daemon's restore accepted, which names every
    /// original its recovery may still hold.
    pub(super) fn retained_startup_checkpoint(
        &self,
    ) -> Option<&crate::upgrade::handoff::Checkpoint> {
        self.retained_startup_checkpoint.as_ref()
    }

    pub(in crate::host) fn pause_update_terminals(&mut self, ids: &[TerminalId]) {
        self.paused_update_terminals.extend(ids.iter().copied());
    }

    /// A retained failure outranks custody still awaiting its outcome: it is
    /// never cleared, so waiting on the remaining custody cannot finish startup.
    pub(super) fn startup_state(&self) -> StartupState {
        #[cfg(debug_assertions)]
        if let Some(held) = super::fixture_startup_recovery::held() {
            return held.state();
        }
        match &self.startup_recovery {
            Some(detail) => StartupState::Retained {
                detail: detail.clone(),
            },
            None if self.startup_custody.is_empty() => StartupState::Ready,
            None => StartupState::Restoring {
                awaiting: self.startup_custody.len() as u64,
            },
        }
    }

    #[cfg(test)]
    pub(in crate::host) fn retain_startup_checkpoint_for_test(
        &mut self,
        checkpoint: crate::upgrade::handoff::Checkpoint,
    ) {
        self.retained_startup_checkpoint = Some(checkpoint);
    }

    #[cfg(test)]
    pub(in crate::host) fn await_startup_for_test(&mut self, id: TerminalId) {
        self.startup_custody.insert(id);
    }

    pub(super) fn retain_startup_recovery(&mut self, message: String) {
        if self.startup_recovery.is_none() {
            self.startup_recovery = Some(message);
        }
        // A late startup result cannot revoke already-owned ordinary exit.
        if !self.ordinary_exit_cleanup_pending {
            self.shutting_down = false;
            self.shutdown_requested = false;
        }
    }

    pub(super) fn startup_closed(&mut self, id: TerminalId, handle: &TerminalHandle) {
        if handle.is_completed()
            && self
                .terminals
                .get(&id)
                .is_some_and(|record| record.handle.same_worker(handle))
        {
            self.startup_custody.remove(&id);
            if self
                .publication_pending
                .get(&id)
                .is_some_and(|p| p.handle.same_worker(handle))
            {
                self.publication_pending.remove(&id);
            }
            if self.ordinary_exit_cleanup_pending {
                // An adopted holder may have no local reaper credit. Exact
                // checked Close must wake the exit caller even if its client
                // remains connected and no reaper epoch changes.
                crate::daemon::wake();
            }
        }
    }

    /// Whether an original recovery holds still runs. Work started during
    /// the hold is independent of it and stops as on any shutdown.
    pub(super) fn recovery_owns_work(&self) -> bool {
        let held = |id: &TerminalId| !self.recovery_independence.contains(*id);
        self.startup_recovery().is_some()
            && (self
                .terminals
                .iter()
                .any(|(id, record)| held(id) && !record.handle.is_completed())
                || self
                    .chats
                    .iter()
                    .any(|(id, record)| held(id) && record.handle.owns_work()))
    }

    pub(super) fn startup_blocks_shutdown(&self) -> bool {
        !self.startup_custody.is_empty() || self.recovery_owns_work()
    }

    pub(crate) fn request_shutdown(&mut self) {
        if self.startup_blocks_shutdown() {
            // A signal or ordinary stop is not actor-specific permission to
            // discard a committed replay prefix. Keep the recovery listener.
            return;
        }
        self.shutting_down = true;
        self.shutdown_requested = true;
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutdown_requested
    }

    pub(crate) fn graceful_shutdown(&mut self) -> (Vec<TerminalHandle>, Vec<Arc<DeliveryQueue>>) {
        if self.startup_blocks_shutdown() {
            return (Vec::new(), Vec::new());
        }
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
        "These active terminals cannot be checkpointed automatically. The updater can stop the terminal service after listing the affected work for confirmation.",
    );
    message.push_str(if relaunch_agents {
        " These jobs require the stopped-pane fallback:"
    } else {
        " These sessions require the stopped-pane fallback:"
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

/// Typed update consent promises that carried processes continue. Refuse an
/// unavailable preservation boundary rather than silently choosing relaunch.
pub(super) fn prepare_preserving_upgrade(
    host: &SharedHost,
    capability: ShutdownCapability,
    relaunch_agents: bool,
) -> Result<(u64, u64), ProtocolError> {
    let holders = holder_directory().map_err(upgrade_error)?;
    prepare_upgrade_with_carry_policy(
        host,
        capability,
        relaunch_agents,
        Some(&holders),
        true,
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
    recipe.withhold_environment(&mut restored);
    if execution.0 != restored {
        return Err(upgrade_error(
            "Terminal execution environment cannot survive daemon upgrade without losing selected credentials or account settings. The updater can restore this terminal stopped after confirmation; no source was stopped",
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
    prepare_upgrade_with_carry_policy(host, capability, relaunch_agents, holders, false, discover)
}

fn prepare_upgrade_with_carry_policy(
    host: &SharedHost,
    capability: ShutdownCapability,
    relaunch_agents: bool,
    holders: Option<&std::path::Path>,
    require_continuity: bool,
    discover: impl FnOnce() -> Result<crate::upgrade::handoff::Store, String>,
) -> Result<(u64, u64), ProtocolError> {
    use crate::upgrade::handoff::{Carried, Chat, Checkpoint, Phase, Terminal};
    // What can stop a carry is checked now, while the upgrade can still be
    // refused: past this point a terminal that fails to carry is lost if a
    // restart cannot stand in for its running job.
    let holders = if require_continuity {
        let holders = holders.ok_or_else(|| {
            upgrade_error(
                "No terminal preservation directory is available; update was not committed",
            )
        })?;
        crate::secure_file::prepare_private_directory(holders).map_err(|error| {
            upgrade_error(format!(
                "Cannot prepare terminal preservation: {error}; update was not committed"
            ))
        })?;
        Some(holders)
    } else {
        holders.filter(|holders| {
            crate::secure_file::prepare_private_directory(holders)
                .inspect_err(|error| {
                    tracing::warn!(holders = %holders.display(), %error,
                    "cannot prepare the holders' directory; upgrade restarts every terminal")
                })
                .is_ok()
        })
    };
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
                "A native-to-terminal handoff is not durably acknowledged and cannot be checkpointed",
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
                "A native-to-terminal replacement is still pending and cannot be checkpointed",
            ));
        }
        // A durable native holder also keeps its paired terminal alive. Its
        // exact environment remains only in the two holders' private memory.
        let carries = |id: &TerminalId, record: &TerminalRecord| {
            (state
                .chats
                .get(id)
                .is_some_and(|chat| chat.handle.native_continuity().retains_provider())
                || (!state.chats.contains_key(id) && record.execution_environment.is_none()))
                && holders.is_some()
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
        for (&id, record) in state
            .chats
            .iter()
            .filter(|(_, record)| record.handle.owns_work())
        {
            // Nothing of an ended provider is left to checkpoint: its window
            // restarts the saved conversation, as after any daemon restart.
            if state.provider_ended(id, record) {
                continue;
            }
            if record.handle.is_completed() {
                return Err(upgrade_error(
                    "A native provider stopped but may still hold unconfirmed work and cannot be checkpointed",
                ));
            }
            let mut source = record.session.clone();
            source.native_continuity = record.handle.native_continuity();
            if source.native_continuity.retains_provider() {
                source.launch.session_id = record.handle.confirmed_session_id();
                // This is an issued descriptor, including after a provider
                // changed its thread. Only the separate effective launch below
                // may normalize resume argv for that confirmed identity.
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
            if launch.terminal_recipe().is_none()
                && !matches!(
                    source.native_continuity,
                    crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
                )
            {
                return Err(upgrade_error(format!(
                    "Native chat {} has no exact resumable conversation; wait for provider initialization or close it explicitly",
                    source.id
                )));
            }
            crate::upgrade::handoff::validate_chat_source(&source, &launch)
                .map_err(|reason| upgrade_error(format!(
                    "Native chat {} cannot migrate: {reason}. Its existing provider remains active.", source.id
                )))?;
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
                && state.terminals.get(&source.id).is_some_and(|terminal| {
                    !terminal.handle.is_completed() && !carries(&source.id, terminal)
                })
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
    // Exclusive control fences raw client input; it does not stop the PTY
    // parser generating reply frames from later child output. Stop each
    // carried reader losslessly before ACK, then retain the exact reversible
    // cut through commit and emergency preservation. Future output remains
    // unread in the original kernel PTY until its exact successor adopts it.
    let mut prepared_input = Vec::with_capacity(carried.len());
    for terminal in &carried {
        let handle = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.descriptor(0, terminal.source.id)?.session != terminal.source {
                return Err(upgrade_error(
                    "A carried terminal changed before its input could be verified",
                ));
            }
            state.terminals[&terminal.source.id].handle.clone()
        };
        prepared_input.push(PreparedTerminalInput::prepare(handle).map_err(upgrade_error)?);
    }
    let store = discover().map_err(upgrade_error)?;
    if let Some(previous) = store.load().map_err(upgrade_error)?
        && (previous.proof != proof || previous.phase != Phase::Prepared)
    {
        return Err(upgrade_error(format!(
            "An unresolved upgrade checkpoint exists at {}; it will not be overwritten",
            store.path.display()
        )));
    }
    store.preflight_recovered(&chats).map_err(upgrade_error)?;
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
        let recipe = if let Some((selected, native)) = paired_codex.get(&source.id) {
            // Only the still-confirmed attachment can supply execution for
            // the exact terminal descriptor already issued to its chat pane.
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
            source = super::chat::restored_terminal_source(
                chat.source.terminal.as_ref().expect("validated pair"),
                &source,
            )
            .map_err(upgrade_error)?;
            recipe
        } else {
            source.resume_hint = snapshot.hint.clone();
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
            recipe
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
    let mut checkpoint = Checkpoint::new(proof, saved, carried, chats);
    checkpoint.preserve_carried = require_continuity;
    let counts = (
        (checkpoint.terminals.len() + checkpoint.carried.len()) as u64,
        checkpoint.chats.len() as u64,
    );
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state
        .validate_upgrade_capability(&capability, Instant::now())
        .map_err(upgrade_error)?;
    let continuing = if require_continuity {
        checkpoint
            .carried
            .iter()
            .map(|terminal| {
                state.terminals.get(&terminal.source.id)
                .filter(|record| record.handle.holder_identity().is_some())
                .map(|record| record.handle.clone())
                .ok_or_else(|| upgrade_error(format!(
                    "Terminal {} has no registered persistent owner; update was not committed",
                    terminal.source.id
                )))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    // Failure leaves control enabled and drops reversible pidfd freeze guards.
    store.save(&checkpoint).map_err(upgrade_error)?;
    state.upgrade = Some(PreparedUpgrade {
        store,
        checkpoint,
        frozen,
        relaunch_agents,
        holders: holders.map(std::path::Path::to_owned),
        continuing,
        prepared_input,
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
    crate::terminal::holder::directory()
}

fn holder_socket(holders: &std::path::Path, terminal: TerminalId) -> std::path::PathBuf {
    crate::terminal::holder::socket(holders, terminal)
}

/// Provider close must succeed before Ready can authorize another writer.
pub(super) fn finish_upgrade(host: &SharedHost) -> Result<(), String> {
    let Some(PreparedUpgrade {
        store,
        mut checkpoint,
        mut frozen,
        relaunch_agents,
        holders,
        continuing,
        prepared_input,
    }) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade
        .take()
    else {
        return Ok(());
    };
    let require_continuity = checkpoint.preserve_carried;
    let result = (|| {
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
            .filter(|record| {
                record.handle.is_completed() && !restarting.contains(&record.session.id)
            })
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
            let durable = handle.native_continuity().retains_provider();
            if durable {
                handle.preserve_for_restart()?;
            } else {
                handle.close()?;
            }
            if handle.owns_work() {
                return Err(
                    "Native provider shutdown remains unconfirmed; checkpoint retained".into(),
                );
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
                require_continuity,
            )?;
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
    })();
    if result.is_err() {
        // The outer shutdown tail still calls shutdown() on every handle.
        // Transfer these exact registered owners before returning the failure,
        // including errors before carrying or after the final save. Prepared
        // checkpoint evidence remains on disk for explicit recovery.
        for handle in continuing {
            if let Err(error) = handle.preserve_for_restart() {
                tracing::error!(%error, "could not preserve a confirmed continuing terminal after failed update");
            }
        }
    }
    // Keep every successful cut alive through the complete error-preserve
    // loop above. A transferred worker only acknowledges settlement; dropping
    // this guard can never resurrect it or reopen its input.
    drop(prepared_input);
    result
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
    require_continuity: bool,
) -> Result<(), String> {
    if checkpoint.carried.is_empty() {
        return Ok(());
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
            if require_continuity {
                return Err(format!(
                    "Terminal {id} disappeared during confirmed update; checkpoint retained, no restart authorized"
                ));
            }
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
        if require_continuity {
            return Err(format!(
                "Could not carry terminal {id}: {error}. Its exact registered owner and prepared checkpoint were retained; no replacement was launched"
            ));
        }
        // Completion can race the carry, or VT serialization can fail while
        // its process still runs. A registered holder can keep either result
        // without converting ownership into permission to execute a recipe.
        if crate::daemon::holder_registry::recovery_candidate(
            &holder_socket(holders, id),
            crate::daemon::holder_registry::Kind::Terminal,
        )
        .ok()
        .flatten()
        .is_some_and(|record| {
            handle.holder_identity() == Some(&record) && record.holder.is_current()
        }) && handle.preserve_for_restart().is_ok()
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
    Ok(())
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

/// Runs while HostLease is held, before accepting clients on the public listener.
/// Returns the native conversations this restore left unattached: a retained
/// one's reattachment was just refused, so startup must not retry it at once,
/// and a relaunched one's failed restart was stopped for its pane to reopen.
pub(super) fn restore_upgrade(host: &SharedHost) -> Result<Vec<TerminalId>, String> {
    restore_upgrade_from(
        host,
        crate::upgrade::handoff::Store::discover()?,
        std::path::Path::new("/proc"),
        holder_directory().ok().as_deref(),
    )
}

/// Lifecycle fixtures inspect real kernel data for their owned process roster
/// and prove provider retirement without unrelated protected host processes.
#[cfg(test)]
pub(in crate::host) fn restore_upgrade_with_process_fixture(
    host: &SharedHost,
    processes: &std::path::Path,
) -> Result<Vec<TerminalId>, String> {
    restore_upgrade_from(
        host,
        crate::upgrade::handoff::Store::discover()?,
        processes,
        holder_directory().ok().as_deref(),
    )
}

/// An acknowledged incomplete update authorizes attaching its original holder
/// only. In particular, missing holders never fall back to a launch recipe.
pub(in crate::host) fn adopt_prepared_terminal(
    host: &SharedHost,
    terminal: &crate::upgrade::handoff::Carried,
) -> bool {
    adopt_carried_terminal(
        host,
        terminal,
        holder_directory().ok().as_deref(),
        std::path::Path::new("/proc"),
        false,
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

/// An agent row names the exact descriptor its provider was issued, and
/// dispatch refuses any other. A relaunched provider has a new descriptor under
/// the same ID, so move the agent, its console and its delegated authority to
/// it. Only the previous daemons' checkpointed sources are lineage. The agent
/// recorded one either as issued or after its provider confirmed the
/// conversation, and before any console was paired with it: a console is
/// recorded beside the agent's session, not in it.
///
/// A refusal leaves the agent on its previous descriptor, so its next prompt is
/// refused rather than delivered to an unproved process. It does not fail the
/// restore: the conversation itself was relaunched and confirmed, and windows
/// still resolve it through the upgrade aliases.
fn restore_relaunched_execution_authority(host: &SharedHost, id: TerminalId) {
    let (registry, successor, sources) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let (Some(registry), Some(record), Some(sources)) = (
            state.execution_registry.clone(),
            state.chats.get(&id),
            state.upgrade_chats.get(&id),
        ) else {
            return;
        };
        (registry, record.session.clone(), sources.clone())
    };
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    if let Err(reason) = adopt_relaunched_agent(&mut registry, &sources, successor) {
        tracing::warn!(session = %id, %reason,
            "relaunched conversation was restored, but its agent could not be moved to the new provider; the agent keeps its previous descriptor and refuses new prompts");
    }
}

fn adopt_relaunched_agent(
    registry: &mut super::model::Registry,
    sources: &[crate::upgrade::handoff::ChatSource],
    successor: crate::chat::Session,
) -> Result<bool, String> {
    use crate::agent_execution::HostedSession;
    let mut retained = Vec::new();
    for saved in sources {
        let mut confirmed = saved.source.clone();
        confirmed.launch.session_id = saved.launch.session_id.clone();
        for issued in [saved.source.clone(), confirmed] {
            if issued.terminal.is_some() {
                let mut unpaired = issued.clone();
                unpaired.terminal = None;
                retained.push(HostedSession::Chat(unpaired));
            }
            retained.push(HostedSession::Chat(issued));
        }
    }
    // This daemon issued the provider's capability as it started, before a
    // relaunched paired view replaced the descriptor's terminal.
    if let Some(HostedSession::Chat(started)) =
        registry.retained_agent_authority_source(successor.id)?
        && started.proof == successor.proof
        && (crate::chat::Session {
            terminal: successor.terminal.clone(),
            ..started.clone()
        }) == successor
    {
        retained.push(HostedSession::Chat(started));
    }
    registry.adopt_authenticated_agent_session(&retained, &HostedSession::Chat(successor))
}

pub(super) fn restore_orphaned_holders(host: &SharedHost) -> Result<(), String> {
    restore_orphaned_holders_preserving(host, &Default::default())
}

/// A settled recovery can pause a console's chat while keeping its terminal.
/// Those historical IDs remain eligible only for fresh holder authentication;
/// the ordinary orphan route below never falls back to executing a recipe.
pub(super) fn restore_orphaned_holders_preserving(
    host: &SharedHost,
    preserved: &std::collections::BTreeSet<TerminalId>,
) -> Result<(), String> {
    let directory = crate::terminal::holder::directory()?;
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let records = crate::daemon::holder_registry::recovery_candidates(
        &directory,
        &proof.boot_id,
        crate::daemon::holder_registry::Kind::Terminal,
    )?;
    let stopped = crate::upgrade::floor::load()?;
    for record in records {
        if stopped.blocks_terminal(record.terminal) && !preserved.contains(&record.terminal) {
            continue;
        }
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
        // What the adopted worker inherits, until it is recorded on it.
        let mut unrecorded = None;
        let restored = (|| {
            let (recovery, screen_lost) =
                crate::terminal::inspect_holder(&socket, &record, &proof)?;
            let mut session = recovery.source.clone();
            session.continuity = TerminalContinuity::RelaunchAvailable {
                recipe: session.launch.clone(),
                last_exit: None,
            };
            unrecorded = Some((recovery.clone(), screen_lost));
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
            let (recovery, screen_lost) = unrecorded.take().expect("inherited before starting");
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .record_adopted_original(
                    &recovery,
                    screen_lost,
                    recovery.source.resume_hint.as_deref(),
                )
                .map_err(|error| error.message)?;
            restore_terminal_execution_authority(
                host,
                &recovery.source,
                &recovery.previous_sources,
            )?;
            Ok::<_, String>(())
        })();
        if let Err(error) = restored {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            // A failed source publication returns before the inheritance is
            // recorded. Record it on the worker this attempt installed, live
            // or already completed, so it keeps the lineage and recovery facts
            // of an original whose later restoration failed. Recording it
            // also releases client 0.
            let inherited = match unrecorded.take() {
                Some((recovery, screen_lost)) if state.terminals.contains_key(&record.terminal) => {
                    Some(state.record_adopted_original(
                        &recovery,
                        screen_lost,
                        recovery.source.resume_hint.as_deref(),
                    ))
                }
                _ => None,
            };
            if state
                .terminals
                .get(&record.terminal)
                .is_some_and(|terminal| !terminal.handle.is_completed())
            {
                // Adoption may already have committed and activated the
                // reader before lineage/Registry restoration failed. Keep that
                // exact live owner and forbid automatic execution; this does
                // not claim its already-active prefix is still unparsed.
                let mut message = format!(
                    "Orphan terminal {} retained its original owner after restore failed: {error}",
                    record.terminal
                );
                // Restoration starts the view attached. With nothing left to
                // record, release client 0 directly. If the worker refuses the
                // visibility change, detach keeps client 0 attached.
                let released = inherited.unwrap_or_else(|| state.detach(0, record.terminal));
                if let Err(detach) = released {
                    tracing::warn!(terminal = %record.terminal, error = %detach.message, "failed orphan restore could not release its client 0 attachment");
                    message.push_str(&format!(
                        "; restoration could not release its attachment: {}",
                        detach.message
                    ));
                }
                state.retain_startup_recovery(message.clone());
                tracing::warn!(terminal = %record.terminal, %error, "orphan adoption retained original ownership; recovery-only startup required");
                return Err(message);
            }
            if let Some(Err(lineage)) = inherited {
                tracing::warn!(terminal = %record.terminal, error = %lineage.message,
                    "completed orphan could not record its lineage");
            }
            drop(state);
            tracing::warn!(terminal = %record.terminal, %error, "could not adopt orphaned terminal; preserved its holder");
        }
    }
    Ok(())
}

/// Sentinel restore failure for an operator-requested stop; see
/// [`restore_upgrade_from`].
const RESTORE_TERMINATED: &str = "daemon termination was requested during upgrade restore";

/// The one path that restores a checkpointed chat. Each chat takes exactly
/// one: a second attempt on a retained conversation finds the actor the first
/// attempt reattached and refuses it, failing the whole restore.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChatRestore {
    /// A managed Codex CLI, whose thread server must exist before its TUI.
    CodexListener,
    /// A provider that outlived the previous daemon. It is reattached, and a
    /// refusal is that conversation's problem alone; it is never relaunched.
    Retained,
    /// Relaunched from its checkpointed launch once carried terminals return.
    Relaunch,
}

impl ChatRestore {
    fn of(chat: &crate::upgrade::handoff::Chat) -> Self {
        if chat.source.native_continuity.retains_provider() {
            Self::Retained
        } else if super::chat::owns_codex_listener(&chat.source) {
            Self::CodexListener
        } else {
            Self::Relaunch
        }
    }
}

/// Restarts one conversation the previous daemon stopped for the update.
///
/// Its provider was stopped before the checkpoint became Ready, so a restart
/// that fails is that conversation's problem alone, as a refused reattachment
/// is for a retained one: what this restart started is positively stopped, the
/// conversation is recorded for its pane to reopen, and the rest of the update
/// is restored. Only a stop that cannot be proved, or a daemon asked to stop,
/// fails the whole restore, since then something of it may still run.
fn restart_or_stop(
    host: &SharedHost,
    chat: &crate::upgrade::handoff::Chat,
    unattached: &mut Vec<crate::upgrade::handoff::UnattachedChat>,
    queue_budget: &mut super::chat::UpgradeRestoreQueueBudget,
) -> Result<(), String> {
    // A debug fixture's stand-in for a restart that fails: nothing starts,
    // and the conversation is stopped and recorded as a failed one is.
    #[cfg(debug_assertions)]
    let injected = super::fixture_startup_recovery::stopped().map(str::to_owned);
    #[cfg(not(debug_assertions))]
    let injected: Option<String> = None;
    let restarted = match injected {
        Some(reason) => Err(reason),
        None => super::chat::restore_upgrade_with_queue_budget(host, chat, queue_budget),
    };
    let Err(reason) = restarted else {
        return Ok(());
    };
    if crate::daemon::termination_requested() {
        return Err(RESTORE_TERMINATED.to_owned());
    }
    super::chat::stop_failed_upgrade_restore(host, chat).map_err(|stop| {
        format!("{reason}; stopping that restart could not be confirmed: {stop}")
    })?;
    tracing::warn!(session = %chat.source.id, %reason,
        "a conversation the update stopped could not be restarted; its restart was stopped, nothing was resent, and the rest of the update was restored");
    unattached.push(crate::upgrade::handoff::UnattachedChat::stopped(
        chat.clone(),
        reason,
    ));
    Ok(())
}

/// Replays a checkpoint, abandoning the replay as soon as termination is
/// requested.
///
/// The accept loop is the only other place that observes
/// `daemon::termination_requested`, and it is not reached until this returns, so
/// without these checks the daemon kept launching checkpointed processes after
/// `cagents daemon stop-for-rebuild` had asked it to stop, and a
/// logout `SIGKILL` mid-restore left the checkpoint in `Phase::Restoring`, which
/// is never automatically replayed for native sessions. Aborting through
/// `close_failed_restore` positively closes whatever was already started and
/// leaves the checkpoint in a replayable `Ready` phase.
fn restore_upgrade_from(
    host: &SharedHost,
    store: crate::upgrade::handoff::Store,
    proc_root: &std::path::Path,
    holders: Option<&std::path::Path>,
) -> Result<Vec<TerminalId>, String> {
    use crate::upgrade::{
        ensure_no_surviving_process,
        handoff::{Phase, UnattachedChat},
    };
    let store = Arc::new(store);
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_evidence = None;
    forget_departed_unattached(&store);
    // Reattachment and close retire unattached evidence only once published
    // here. A chat reattached mid-restore is retired with the restore's own
    // record instead, so a failed restore never loses an earlier entry.
    let publish = || {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .upgrade_evidence = Some(store.clone());
    };
    let Some(mut checkpoint) = store.load()? else {
        publish();
        return Ok(Vec::new());
    };
    checkpoint.validate()?;
    if checkpoint
        .chats
        .iter()
        .any(|chat| ChatRestore::of(chat) != ChatRestore::Retained)
    {
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
        if ChatRestore::of(chat) == ChatRestore::Retained {
            continue;
        }
        let terminal = TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
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
        return Ok(Vec::new());
    }
    // Restoration records its progress on disk, but retained-owner admission
    // must still use the exact Ready payload accepted at this boundary.
    let ready_checkpoint = (checkpoint.phase == Phase::Ready).then(|| checkpoint.clone());
    checkpoint.phase = Phase::Restoring;
    store.save(&checkpoint)?;
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_startup_checkpoint = Some(checkpoint.clone());
    let mut adopted = Vec::new();
    let mut unattached = Vec::new();
    let mut reattached = Vec::new();
    let mut interrupted_setups = false;
    // Restarts stay serial: a Codex TUI needs its exact listener first, and a
    // failed restart must be positively stopped before restore proceeds. Share
    // lock queue time so one external lock holder cannot delay every restart
    // by another minute while this daemon cannot yet accept clients.
    let mut queue_budget = super::chat::UpgradeRestoreQueueBudget::default();
    let restored = (|| {
        // A managed CLI connects to its thread's server. Recreate that exact
        // actor first; starting the TUI first races a socket that does not exist.
        for chat in &checkpoint.chats {
            if ChatRestore::of(chat) == ChatRestore::CodexListener {
                restart_or_stop(host, chat, &mut unattached, &mut queue_budget)?;
            }
        }
        // Carried terminals are adopted before anything is relaunched: their
        // processes have been waiting unattended since the previous daemon left.
        for terminal in &checkpoint.carried {
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            let continued = adopt_carried_terminal(
                host,
                terminal,
                holders,
                proc_root,
                !checkpoint.preserve_carried,
            );
            let recovery = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .startup_recovery()
                .map(str::to_owned);
            if let Some(error) = recovery {
                return Err(error);
            }
            if continued {
                adopted.push(terminal.source.id);
            } else if crate::upgrade::is_worktree_setup_recipe(&terminal.source.launch) {
                interrupted_setups = true;
                setup_restore::retain_pane(host, &terminal.source, &terminal.source.launch);
            } else if checkpoint.preserve_carried {
                return Err(format!(
                    "Terminal {} could not be authenticated for continued ownership; no recipe restart was authorized. Its checkpoint was retained for explicit recovery",
                    terminal.source.id
                ));
            }
        }
        // Completed paired views are deliberately absent from the execution
        // checkpoint, but their authenticated holders retain the final screen.
        // Restore those receipts before requiring a native pair to match.
        if checkpoint.chats.iter().any(|chat| {
            chat.source.terminal.is_some() && chat.source.native_continuity.retains_provider()
        }) {
            restore_orphaned_holders(host)?;
        }
        // Each retained conversation has its own provider holder, which a
        // refused reattachment leaves untouched: nothing was started, relaunched
        // or resent for it. So one refusal, such as Codex no longer finding the
        // thread's rollout, is that conversation's problem to report, not a
        // reason to keep every other session and the daemon itself down.
        for chat in &checkpoint.chats {
            if ChatRestore::of(chat) != ChatRestore::Retained {
                continue;
            }
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            let restore = if let Some(ready) = &ready_checkpoint {
                super::chat::restore_ready_checkpoint(host, &chat.source, ready).map(|_| ())
            } else {
                super::chat::restore_upgrade(host, chat)
            };
            if let Err(reason) = restore {
                if crate::daemon::termination_requested() {
                    return Err(RESTORE_TERMINATED.to_owned());
                }
                tracing::warn!(session = %chat.source.id, %reason,
                    "retained native conversation could not be reattached after the upgrade; its holder and checkpoint evidence were kept and nothing was restarted or resent");
                host.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .native_adoption_failures
                    .remember_upgrade(chat, &reason);
                unattached.push(UnattachedChat::new(chat.clone(), reason));
            } else {
                reattached.push(chat.source.id);
            }
        }
        for terminal in &checkpoint.terminals {
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            if crate::upgrade::is_worktree_setup_recipe(&terminal.source.launch) {
                interrupted_setups = true;
                setup_restore::retain_pane(host, &terminal.source, &terminal.recipe);
                continue;
            }
            // An unattached conversation's paired view would reconnect to its
            // retained server as a new, unowned client. Keep it as evidence.
            if let Some(chat) = unattached
                .iter_mut()
                .find(|chat| chat.summary.id == terminal.source.id)
            {
                tracing::warn!(terminal = %terminal.source.id,
                    "not relaunching the paired view of an unattached native conversation; its checkpoint entry was kept as evidence");
                chat.terminal = Some(terminal.clone());
                continue;
            }
            let pair = checkpoint.chats.iter().find(|chat| {
                chat.launch.provider == crate::agent::Provider::Codex
                    && chat.source.id == terminal.source.id
                    && chat
                        .source
                        .terminal
                        .as_ref()
                        .is_some_and(|source| source.id == terminal.source.id)
            });
            let mut session = if let Some(chat) = pair {
                crate::upgrade::handoff::paired_terminal_source(chat, terminal)?
            } else {
                terminal.source.clone()
            };
            if pair.is_none() {
                session.launch = terminal.recipe.clone();
            }
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
            let descriptor = if pair.is_some() {
                start_terminal_view_owned(
                    host,
                    0,
                    session,
                    terminal.options.clone(),
                    None,
                    paired,
                    None,
                    None,
                    None,
                    Some(terminal.recipe.clone()),
                )
                .map(|(descriptor, _)| descriptor)
            } else {
                spawn_terminal_view(host, 0, session, terminal.options.clone(), None, paired)
            }
            .map_err(|error| error.message)?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if paired && let Some(chat) = state.chats.get_mut(&terminal.source.id) {
                chat.returning_terminal = false;
                chat.session.terminal = Some(Box::new(descriptor.session));
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
            if ChatRestore::of(chat) != ChatRestore::Relaunch {
                continue;
            }
            if crate::daemon::termination_requested() {
                return Err(RESTORE_TERMINATED.to_owned());
            }
            restart_or_stop(host, chat, &mut unattached, &mut queue_budget)?;
        }
        let recovered = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            checkpoint
                .chats
                .iter()
                .filter_map(|chat| {
                    if unattached
                        .iter()
                        .any(|entry| entry.summary.id == chat.source.id)
                    {
                        return None;
                    }
                    let record = state.chats.get(&chat.source.id)?;
                    let mut replacement = record.session.clone();
                    replacement.launch.session_id = record.handle.session_id();
                    replacement.native_continuity = record.handle.native_continuity();
                    Some(crate::upgrade::handoff::RecoveredChat {
                        evidence: chat.clone(),
                        replacement,
                        aliases: Vec::new(),
                    })
                })
                .collect::<Vec<_>>()
        };
        store.record_recovered(&recovered)?;
        // Until this is durable, the checkpoint is the only record of what
        // was left unattached, so it must be written before that is consumed.
        store.record_unattached(&unattached, &reattached)?;
        Ok::<_, String>(())
    })();
    if let Err(error) = restored {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .terminals
            .values()
            .any(|record| !record.handle.is_completed())
            || state.chats.values().any(|record| record.handle.owns_work())
        {
            let message = format!(
                "Upgrade restore retained original ownership: {error}; checkpoint retained at {}",
                store.path.display()
            );
            state.upgrade_evidence = Some(store.clone());
            state.retain_startup_recovery(message.clone());
            return Err(message);
        }
        drop(state);
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
            return Ok(Vec::new());
        }
        return Err(format!(
            "Upgrade restore failed: {error}; checkpoint retained at {}",
            store.path.display()
        ));
    }
    // Only now is every relaunched provider kept. Moving an agent earlier
    // would leave it on a descriptor that a failed restore then closes, which
    // a replay of this checkpoint could no longer recognize.
    for chat in &checkpoint.chats {
        if ChatRestore::of(chat) != ChatRestore::Retained {
            restore_relaunched_execution_authority(host, chat.source.id);
        }
    }
    let finalized = if interrupted_setups {
        store.archive_interrupted_setup().map(|evidence| {
            tracing::warn!(evidence = %evidence.display(),
                "Interrupted worktree setup requires explicit review; its checkpoint was preserved and other sessions were restored");
        })
    } else {
        store.remove()
    };
    if let Err(error) = finalized {
        let message = format!(
            "Upgrade restore finalization failed while retaining its original owners: {error}"
        );
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.upgrade_evidence = Some(store.clone());
        state.retain_startup_recovery(message.clone());
        return Err(message);
    }
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_startup_checkpoint
        .take();
    publish();
    if !unattached.is_empty() {
        tracing::warn!(
            conversations = unattached.len(),
            evidence = %store.unattached_path().display(),
            "the daemon upgrade finished, but native conversations it could not restore need attention"
        );
    }
    Ok(unattached.iter().map(|chat| chat.summary.id).collect())
}

/// Forgets unattached conversations whose native holder no longer exists,
/// as after a reboot or once the holder itself exited: nothing in cagents can
/// reattach or close them any more, so they would otherwise warn forever.
/// Their identity stays in the log for manual recovery from provider storage.
/// A conversation whose restart was stopped never has a holder, so its notice
/// lasts until the next daemon start; its pane can reopen it at any time.
fn forget_departed_unattached(store: &crate::upgrade::handoff::Store) {
    use crate::daemon::holder_registry::{self as registry, Kind};
    let conversations = match store.unattached() {
        Ok(conversations) if conversations.is_empty() => return,
        Ok(conversations) => conversations,
        Err(error) => {
            tracing::warn!(%error, "cannot read unattached-conversation evidence");
            return;
        }
    };
    let directory = match registry::directory(Kind::Native) {
        Ok(directory) => directory,
        Err(error) => {
            tracing::warn!(%error, "cannot inspect native holders for unattached conversations");
            return;
        }
    };
    let departed = conversations
        .iter()
        .filter(|conversation| {
            let gone = match registry::evidence(
                &registry::socket(&directory, conversation.id),
                Kind::Native,
            ) {
                Ok(None) => true,
                // A record outliving its holder process, as after that holder
                // was killed, leaves nothing to reattach or close either.
                Ok(Some(record)) => !record.holder.is_current(),
                Err(error) => {
                    tracing::warn!(session = %conversation.id, %error,
                        "kept unattached conversation whose holder evidence cannot be read");
                    false
                }
            };
            if gone {
                tracing::warn!(session = %conversation.id,
                    conversation = ?conversation.conversation,
                    working_directory = %conversation.working_directory.display(),
                    "the native holder of an unattached conversation is gone; reopen it from its provider's session storage if it is still wanted");
            }
            gone
        })
        .map(|conversation| conversation.id)
        .collect::<Vec<_>>();
    if let Err(error) = store.record_unattached(&[], &departed) {
        tracing::warn!(%error, "could not forget unattached conversations whose holders are gone");
    }
}

/// Explicit conversation closure ends the need to retain its issued pane aliases.
pub(super) fn retire_recovered_upgrade_chat(host: &SharedHost, id: TerminalId) {
    let store = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_evidence
        .clone();
    if let Some(store) = store
        && let Err(error) = store.retire_recovered(id)
    {
        tracing::warn!(session = %id, %error, "could not retire closed conversation recovery evidence");
    }
}

/// Retires the stopped-restart evidence of a conversation started again.
pub(super) fn retire_stopped_upgrade_chat(host: &SharedHost, id: TerminalId) {
    let Some(store) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_evidence
        .clone()
    else {
        return;
    };
    if let Err(error) = store.retire_stopped_unattached(id) {
        tracing::warn!(session = %id, %error,
            "could not retire the evidence of a reopened conversation");
    }
}

/// Why the last update's restore stopped the conversation `source` names.
pub(super) fn stopped_upgrade_reason(
    host: &SharedHost,
    source: &crate::chat::Session,
) -> Option<String> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_evidence
        .clone()?
        .stopped_reason(source)
}

/// Forgets that a retained native conversation was left unattached by an
/// upgrade, once it has been reattached or explicitly closed.
pub(super) fn retire_unattached_upgrade_chat(host: &SharedHost, id: TerminalId) {
    let Some(store) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .upgrade_evidence
        .clone()
    else {
        return;
    };
    if let Err(error) = store.retire_unattached(id) {
        tracing::warn!(session = %id, %error,
            "could not retire resolved unattached-conversation evidence");
    }
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
    allow_restart: bool,
) -> bool {
    let id = terminal.source.id;
    let mut error = "no directory for holders".to_owned();
    if let Some(holders) = holders {
        let socket = holder_socket(holders, id);
        for attempt in 1..=ADOPTION_ATTEMPTS {
            let previous = (|| {
                if let Some(record) = crate::daemon::holder_registry::recovery_candidate(
                    &socket,
                    crate::daemon::holder_registry::Kind::Terminal,
                )? {
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
                        execution_recipe: None,
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
            let hosted_before = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .contains_key(&id);
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
                    if let Err(error) = state.record_adopted_original(
                        &previous,
                        screen_lost,
                        terminal.source.resume_hint.as_deref(),
                    ) {
                        tracing::warn!(terminal = %id, error = %error.message,
                            "adopted a carried terminal but could not record its lineage");
                        state.retain_startup_recovery(format!("Terminal {id} adoption retained its original owner but lineage restoration failed: {}", error.message));
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
                        host.lock().unwrap_or_else(|p| p.into_inner()).retain_startup_recovery(format!("Terminal {id} adoption retained its original owner but execution authority restoration failed: {error}"));
                    }
                    return true;
                }
                Err(failure) => {
                    error = failure.message;
                    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                    if !hosted_before
                        && state
                            .terminals
                            .get(&id)
                            .is_some_and(|record| !record.handle.is_completed())
                    {
                        // This attempt installed a live worker before failing:
                        // the holder committed to it, or its commit is
                        // ambiguous. Either way, never adopt again or retry
                        // its source publication automatically. Keep this
                        // exact original for inspection and explicit Close,
                        // with everything a successful adoption records, and
                        // stop the restore without restoring its execution
                        // authority.
                        let mut message = format!(
                            "Terminal {id} adoption retained its original owner but its source publication failed: {error}"
                        );
                        if let Err(lineage) = state.record_adopted_original(
                            &previous,
                            screen_lost,
                            terminal.source.resume_hint.as_deref(),
                        ) {
                            message.push_str(&format!(
                                "; its lineage could not be recorded: {}",
                                lineage.message
                            ));
                        }
                        tracing::warn!(terminal = %id, %error,
                            "adopted a carried terminal whose source publication failed; recovery-only startup required");
                        state.retain_startup_recovery(message);
                        return true;
                    }
                    // A worker this attempt installed that already completed
                    // keeps its final screen; it still inherits the lineage.
                    if !hosted_before
                        && state.terminals.contains_key(&id)
                        && let Err(lineage) = state.record_adopted_original(
                            &previous,
                            screen_lost,
                            terminal.source.resume_hint.as_deref(),
                        )
                    {
                        tracing::warn!(terminal = %id, error = %lineage.message,
                            "completed carried terminal could not record its lineage");
                    }
                    if state.startup_recovery().is_some() {
                        return false; // never re-adopt an ACK-ambiguous original
                    }
                }
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
    if !allow_restart {
        tracing::warn!(terminal = %id, %error, "confirmed continuation requires its original holder; recipe restart is refused");
        return false;
    }
    if crate::upgrade::is_worktree_setup_recipe(&terminal.source.launch) {
        tracing::warn!(terminal = %id, %error,
            "worktree setup lost its authenticated holder; its command was not replayed");
        return false;
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
        if chat.native_continuity().retains_provider() {
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

fn pending_save_conflict() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::ViewConflict,
        super::model::Registry::VIEW_SOURCE_CONFLICT,
        true,
    )
}

fn read_pending_terminal(
    shared: &SharedHost,
    registry: &super::model::Registry,
    pane: crate::model::CardId,
    terminal_id: TerminalId,
) -> Result<super::workspace::PendingTerminal, ProtocolError> {
    super::workspace::pending_terminal(registry, pane, terminal_id).map_err(|error| {
        // An existing worker cannot borrow another pane's withdrawn intent.
        if error.code == ProtocolErrorCode::TerminalNotFound
            && shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .publication_pending
                .contains_key(&terminal_id)
        {
            pending_save_conflict()
        } else {
            error
        }
    })
}

/// Persist the exact resident publication intent; this never enters spawn
/// admission, even when retirement overtakes an earlier inventory read.
pub(super) fn retry_pending_terminal_save(
    shared: &SharedHost,
    pane: crate::model::CardId,
    terminal_id: TerminalId,
) -> Result<(), ProtocolError> {
    let writer = super::workspace::writer(shared)?;
    let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    let reservation = read_pending_terminal(shared, &registry, pane, terminal_id)?;
    let (handle, options) = {
        let state = shared.lock().unwrap_or_else(|p| p.into_inner());
        state.ensure_retention_available(&[terminal_id])?;
        let record = state.terminals.get(&terminal_id).ok_or_else(|| {
            ProtocolError::new(
                ProtocolErrorCode::TerminalNotFound,
                "This terminal is no longer hosted. Its pending save was kept.",
                false,
            )
        })?;
        match state.publication_pending.get(&terminal_id) {
            Some(pending)
                if pending.pane == Some(pane) && pending.handle.same_worker(&record.handle) => {}
            Some(_) => return Err(pending_save_conflict()),
            None if reservation.pane_is_pending() => return Err(pending_save_conflict()),
            None => return Ok(()),
        }
        (record.handle.clone(), record.options.clone())
    };
    // The publisher reacquires SQL before State and revalidates the original
    // pane under that transaction. A retry adds no subscriber or new worker.
    drop(registry);
    publish_installed_terminal(shared, None, terminal_id, &handle, &options).map(drop)
}

pub(super) fn spawn_pending_terminal(
    shared: &SharedHost,
    client_id: ClientId,
    pane: crate::model::CardId,
    terminal_id: TerminalId,
    options: TerminalSpawnOptions,
) -> Result<TerminalDescriptor, ProtocolError> {
    let session = {
        let writer = super::workspace::writer(shared)?;
        let registry = writer.lock().unwrap_or_else(|p| p.into_inner());
        read_pending_terminal(shared, &registry, pane, terminal_id)?.session
    };
    #[cfg(test)]
    super::workspace::pending_start_test_hook();
    start_terminal_view_owned(
        shared,
        client_id,
        session,
        options,
        None,
        false,
        None,
        None,
        Some(pane),
        None,
    )
    .map(|(descriptor, _)| descriptor)
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
    let execution_recipe = previous
        .as_ref()
        .and_then(|previous| previous.execution_recipe.clone());
    start_terminal_view_owned(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        paired,
        adoption,
        previous,
        None,
        execution_recipe,
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
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
    adoption: Option<std::path::PathBuf>,
    previous: Option<crate::terminal::holder::Recovery>,
    pending_pane: Option<crate::model::CardId>,
    execution_recipe: Option<crate::model::LaunchRecipe>,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    start_terminal_view_owned_reserved(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        paired,
        adoption,
        previous,
        pending_pane,
        execution_recipe,
        None,
    )
}

pub(super) fn spawn_codex_view_reserved(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    transition: &super::chat::TerminalViewReservation<'_>,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    start_terminal_view_owned_reserved(
        shared,
        client_id,
        session,
        options,
        execution_environment,
        true,
        None,
        None,
        None,
        None,
        Some(transition),
    )
}

/// Restart only a thin console whose previous exact holder has proved closed.
/// Keep the issued aliases and accepted launch in the new holder, so a crash
/// before native re-pairing or a window save remains recoverable.
pub(super) fn spawn_console_replacement(
    shared: &SharedHost,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    aliases: &[TerminalSession],
    requested: &crate::chat::Session,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    let (source, previous_sources) = aliases.split_last().ok_or_else(|| {
        ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "Console replacement has no authenticated previous view",
            false,
        )
    })?;
    let relaunches: Vec<_> = aliases
        .iter()
        .cloned()
        .map(|source| crate::terminal_ipc::UpgradeRelaunch {
            source,
            recipe: session.launch.clone(),
        })
        .collect();
    let previous = crate::terminal::holder::Recovery {
        source: source.clone(),
        previous_sources: previous_sources.to_vec(),
        relaunches: relaunches.clone(),
        options: options.clone(),
        execution_environment: None,
        execution_recipe: None,
    };
    let (_, handle) = start_terminal_view_owned_reserved(
        shared,
        0,
        session,
        options,
        None,
        true,
        None,
        Some(previous),
        None,
        None,
        Some(&super::chat::TerminalViewReservation::retained_console(
            requested,
        )),
    )?;
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    state.adopt_upgrade_lineage(source, previous_sources, &relaunches)?;
    Ok((state.descriptor(0, source.id)?, handle))
}

fn start_terminal_view_owned_reserved(
    shared: &SharedHost,
    client_id: ClientId,
    session: TerminalSession,
    options: TerminalSpawnOptions,
    mut execution_environment: Option<crate::terminal_ipc::ChatExecutionEnvironment>,
    paired: bool,
    adoption: Option<std::path::PathBuf>,
    previous: Option<crate::terminal::holder::Recovery>,
    pending_pane: Option<crate::model::CardId>,
    execution_recipe: Option<crate::model::LaunchRecipe>,
    transition: Option<&super::chat::TerminalViewReservation<'_>>,
) -> Result<(TerminalDescriptor, TerminalHandle), ProtocolError> {
    if shared
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .paused_update_terminals
        .contains(&session.id)
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            "This terminal is paused after an incomplete update; its original process could not be authenticated. Open a new terminal to continue without replaying its saved command.",
            false,
        ));
    }
    validate_launch(&session)?;
    validate_spawn_options(&options)?;
    // A native chat starts its terminal from the chat's own connection, which
    // reads only that chat's frames and ends its presentation on anything
    // else. The window attaches the terminal on a terminal connection once it
    // has the handoff, so that requester never subscribes to it here.
    let subscriber = execution_environment.is_none().then_some(client_id);
    let repair = {
        let state = shared.lock().unwrap_or_else(|p| p.into_inner());
        state
            .publication_pending
            .get(&session.id)
            .filter(|p| p.source == session && p.pane == pending_pane)
            .map(|p| p.handle.clone())
    };
    if let Some(handle) = repair {
        let descriptor =
            publish_installed_terminal(shared, subscriber, session.id, &handle, &options)?;
        return Ok((descriptor, handle));
    }
    let recovery = {
        let state = shared.lock().unwrap_or_else(|p| p.into_inner());
        // A terminal admitted as independent of recovery's originals starts
        // beside them, as a fresh conversation does.
        state
            .startup_recovery()
            .filter(|_| !state.recovery_independence.contains(session.id))
            .map(str::to_owned)
    };
    if let Some(message) = recovery {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            format!(
                "Startup recovery owns original terminals; no new starts are admitted: {message}"
            ),
            false,
        ));
    }

    if let Some(recipe) = &execution_recipe {
        if crate::upgrade::exact_conversation(recipe).is_none()
            || (adoption.is_some()
                && previous
                    .as_ref()
                    .and_then(|previous| previous.execution_recipe.as_ref())
                    != Some(recipe))
        {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Terminal restart execution does not match its authenticated source",
                false,
            ));
        }
        let mut effective = session.clone();
        effective.launch = recipe.clone();
        validate_launch(&effective)?;
    }
    if let Some(environment) = &mut execution_environment {
        execution_recipe
            .as_ref()
            .unwrap_or(&session.launch)
            .withhold_environment(&mut environment.0);
    }
    if adoption.is_some() {
        super::account_lifecycle::ensure_terminal_adoption(
            shared,
            execution_environment.as_ref().map(|value| &value.0),
            &crate::account::launch_credential_overrides(&session.launch),
        )?;
    }
    super::account_lifecycle::ensure_launch(
        shared,
        &session.launch.working_directory,
        execution_environment
            .as_ref()
            .map(|value| &value.0)
            .unwrap_or(&session.launch.environment),
        &crate::account::launch_credential_overrides(&session.launch),
        execution_environment.is_none(),
    )?;
    if let Some(recipe) = &execution_recipe {
        // The pane retains its issued recipe, while the validated restart can
        // use another credential directory. Admit the command actually passed
        // to Worker as well as the immutable source retained above.
        let removals = crate::account::launch_credential_overrides(recipe);
        if adoption.is_some() {
            super::account_lifecycle::ensure_terminal_adoption(
                shared,
                execution_environment.as_ref().map(|value| &value.0),
                &removals,
            )?;
        }
        super::account_lifecycle::ensure_launch(
            shared,
            &recipe.working_directory,
            execution_environment
                .as_ref()
                .map(|value| &value.0)
                .unwrap_or(&recipe.environment),
            &removals,
            execution_environment.is_none(),
        )?;
    }
    let terminal_id = session.id;
    let retained_environment = execution_environment.clone();
    // Adoption already inspected this holder's issued source. Keep that exact
    // receipt alias in the independently installed owner, including when the
    // startup commit fails; the relaunch projection has no ownership proof.
    let install_session = previous
        .as_ref()
        .map_or_else(|| session.clone(), |previous| previous.source.clone());
    // Pane reservations are read on the workspace writer, which is always
    // locked before HostState and never while holding it.
    let pending_writer = match super::workspace::writer(shared) {
        Ok(writer) => Some(writer),
        #[cfg(test)]
        Err(_) if pending_pane.is_none() => None,
        Err(error) => return Err(error),
    };
    let (holder_recovery, publication_source, pending_reservation) = {
        let registry = pending_writer
            .as_ref()
            .map(|writer| writer.lock().unwrap_or_else(|p| p.into_inner()));
        let reservation = registry
            .as_deref()
            .zip(pending_pane)
            .map(|(registry, pane)| read_pending_terminal(shared, registry, pane, terminal_id))
            .transpose()?;
        let publication_source = registry
            .as_deref()
            .map(|r| r.terminal_publication_source(terminal_id))
            .transpose()
            .map_err(model_unavailable)?
            .flatten();
        let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pending) = state.publication_pending.get(&terminal_id) {
            if pending.source != session || pending.pane != pending_pane {
                return Err(pending_save_conflict());
            }
            let handle = pending.handle.clone();
            drop(state);
            drop(registry);
            let descriptor =
                publish_installed_terminal(shared, subscriber, terminal_id, &handle, &options)?;
            return Ok((descriptor, handle));
        }
        if let Some(reservation) = &reservation {
            // Completion is idempotent across clients, but an actor alone is
            // insufficient: its authoritative pane must still name it.
            if state.terminals.contains_key(&terminal_id) {
                if reservation.pane_is_pending() {
                    return Err(ProtocolError::new(
                        ProtocolErrorCode::ViewConflict,
                        super::model::Registry::VIEW_SOURCE_CONFLICT,
                        true,
                    ));
                }
                let descriptor = state.attach(client_id, terminal_id)?;
                let handle = state.terminals[&terminal_id].handle.clone();
                return Ok((descriptor, handle));
            }
            if !reservation.accepts(&session) {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::ViewConflict,
                    super::model::Registry::VIEW_SOURCE_CONFLICT,
                    true,
                ));
            }
        }
        // A saved setup recipe is evidence, never automatic replay permission.
        // Existing actor attachment above and authenticated holder adoption
        // below keep their original ownership paths; neither spawns a recipe.
        if crate::upgrade::is_worktree_setup_recipe(&session.launch) && adoption.is_none() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "Worktree setup was interrupted; review and explicitly resume it instead of replaying its saved recipe",
                false,
            ));
        }
        state.reserve_spawn_for_view_reserved(
            terminal_id,
            paired.then_some(&session),
            transition,
        )?;
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
        let holder = state.recover_holders.then(|| {
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
                execution_recipe: execution_recipe.clone(),
            }
        });
        (holder, publication_source, reservation)
    };
    // Fresh holders receive this exact daemon-issued source. Retain it after
    // successful startup instead of the caller's proofless relaunch projection.
    // Adoption keeps its independently installed previous source above.
    let registered_session = holder_recovery
        .as_ref()
        .map_or_else(|| session.clone(), |recovery| recovery.source.clone());

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
                let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                // A relaunch or recovery names the session it replaces; it
                // inherits what that lineage owns once nothing else of it runs.
                // A session saved before predecessors were recorded names only
                // its root.
                let succeeded = session
                    .predecessor
                    .or(session.recovery_origin)
                    .filter(|predecessor| *predecessor != terminal_id)
                    .map_or(Ok(()), |predecessor| {
                        registry
                            .succeed(
                                terminal_id,
                                predecessor,
                                session.recovery_origin,
                                |members| {
                                    shared
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner())
                                        .any_live(members)
                                },
                            )
                            .map(drop)
                    });
                match succeeded.and_then(|()| {
                    registry.fresh_agent_authority(
                        &crate::agent_execution::HostedSession::Terminal(source),
                    )
                }) {
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
    let redraw = Arc::new(TerminalRedraw {
        host: Arc::downgrade(shared),
        terminal_id,
        worker: OnceLock::new(),
    });
    let config = TerminalConfig {
        agent_authority,
        console_startup_owner: (paired
            && crate::agent_execution::console_agent(&session.launch).is_some())
        .then(|| {
            shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone()
        }),
        columns: options.columns,
        rows: options.rows,
        cell_width_px: options.cell_width_px,
        cell_height_px: options.cell_height_px,
        max_scrollback_bytes: options.max_scrollback_bytes,
        initially_visible: options.initially_visible,
        theme: options.theme,
        execution_environment,
        adoption,
        holder_recovery,
        ..TerminalConfig::default()
    };
    let wake_redraw = Arc::clone(&redraw);
    let adopting = config.adoption.is_some();
    let install_options = options.clone();
    let install_environment = retained_environment.clone();
    let install_execution = execution_recipe.clone();
    let install_redraw = redraw.clone();
    let handle = match TerminalHandle::spawn_with_execution_installed(
        session.clone(),
        config,
        execution_recipe.clone(),
        move |wake| {
            wake_redraw.publish(wake.generation);
        },
        |handle| {
            if !adopting {
                return Ok(());
            }
            let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
            // This record, not the returned error, independently owns the
            // exact Handle before its thread may send CommitAdoption.
            state
                .install_startup_parent(
                    subscriber,
                    install_session,
                    handle.clone(),
                    false,
                    install_execution,
                    install_environment,
                )
                .map_err(|error| error.message)?;
            state
                .terminals
                .get_mut(&terminal_id)
                .expect("installed startup owner")
                .options = install_options;
            state.startup_custody.insert(terminal_id);
            let _ = install_redraw.worker.set(handle.downgrade());
            let event_host = Arc::downgrade(shared);
            handle.set_event_observer(move |source, event| {
                record_event_from(&event_host, terminal_id, event, Some(source));
            });
            #[cfg(test)]
            if state.test_fail_startup_commit == Some(terminal_id) {
                handle.fail_startup_commit_after_ack_for_test();
            }
            #[cfg(test)]
            if state
                .test_checkpoint_failure_at
                .as_ref()
                .is_some_and(|(id, _)| *id == terminal_id)
            {
                let (_, path) = state.test_checkpoint_failure_at.take().unwrap();
                std::fs::rename(&path, path.with_extension("before-failure.json")).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            Ok(())
        },
    ) {
        Ok(handle) => handle,
        Err(crate::terminal::TerminalStartError::CustodyRetained { handle, message }) => {
            let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                state
                    .terminals
                    .get(&terminal_id)
                    .is_some_and(|record| record.handle.same_worker(&handle)),
                "retained startup must already have its original parent owner"
            );
            state.retain_startup_recovery(format!(
                "Terminal {terminal_id} startup is unconfirmed: {message}"
            ));
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                format!(
                    "Terminal startup retained original ownership; inspect/List and explicitly Close its original actor: {message}"
                ),
                false,
            ));
        }
        Err(error) => {
            let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
            if adopting {
                state.startup_custody.remove(&terminal_id);
                state.terminals.remove(&terminal_id);
            }
            drop(state);
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
    // Bind before publishing the Host record. Pre-binding captures are covered
    // by the initial Spawn snapshot and acknowledge below, never by an ID lookup.
    let _ = redraw.worker.set(handle.downgrade());
    let output_host = Arc::downgrade(shared);
    handle
        .set_output_observer(move |sequence| broadcast_output(&output_host, terminal_id, sequence));

    let redraw_handle = handle.clone();
    {
        let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
        if !adopting
            && let Err(error) = state.install_startup_parent(
                subscriber,
                registered_session,
                handle.clone(),
                false,
                execution_recipe.clone(),
                retained_environment,
            )
        {
            drop(state);
            let _ = handle.shutdown();
            return Err(error);
        }
        let record = state
            .terminals
            .get_mut(&terminal_id)
            .expect("installed original parent");
        record.options = options.clone();
        if execution_recipe.is_some() {
            record.session.resume_hint = session.resume_hint.clone();
        }
        state.publication_pending.insert(
            terminal_id,
            PendingPublication {
                handle: handle.clone(),
                source: session.clone(),
                pane: pending_pane,
                original: publication_source,
                reservation: pending_reservation,
            },
        );
    }
    let activity_wake = Arc::clone(
        &shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_activity_wake,
    );
    redraw_handle.set_agent_process_observer(move || activity_wake.notify());
    let event_host = Arc::downgrade(shared);
    redraw_handle.set_event_observer(move |handle, event| {
        let completed = matches!(event, TerminalEvent::Completed { .. });
        record_event_from(&event_host, terminal_id, event, Some(handle));
        if completed {
            // Completed is emitted only after the final grid and parser
            // teardown. Notify visible subscribers from that exact worker.
            broadcast_redraw(&event_host, terminal_id, handle.generation(), handle);
        }
    });

    let descriptor = match publish_installed_terminal(
        shared,
        subscriber,
        terminal_id,
        &handle,
        &options,
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            // Starting has effects even when SQL refuses publication. Keep this
            // exact parent for inspect/explicit Close and an idempotent repair.
            tracing::warn!(%terminal_id, message=%error.message, "terminal source publication retained its original worker");
            return Err(error);
        }
    };
    Ok((descriptor, redraw_handle))
}

fn publish_installed_terminal(
    shared: &SharedHost,
    client: Option<ClientId>,
    id: TerminalId,
    expected: &TerminalHandle,
    options: &TerminalSpawnOptions,
) -> Result<TerminalDescriptor, ProtocolError> {
    let writer = super::workspace::writer(shared);
    #[cfg(test)]
    let writer = writer.ok();
    #[cfg(not(test))]
    let writer = Some(writer?);
    let mut registry = writer
        .as_ref()
        .map(|w| w.lock().unwrap_or_else(|p| p.into_inner()));
    // Writer admission precedes resident serialization, including repair.
    let sql = registry
        .as_deref_mut()
        .map(|r| r.source_admission())
        .transpose()
        .map_err(model_unavailable)?;
    let mut state = shared.lock().unwrap_or_else(|p| p.into_inner());
    let pending = state
        .publication_pending
        .get(&id)
        .filter(|p| p.handle.same_worker(expected))
        .ok_or_else(|| not_found(id))?;
    let original = pending.original;
    let pane = pending.pane;
    if let (Some(reservation), Some(sql)) = (&pending.reservation, &sql) {
        reservation.validate(sql)?;
    }
    if state.shutting_down
        || state.ordinary_exit_cleanup_pending()
        || !state
            .terminals
            .get(&id)
            .is_some_and(|r| r.handle.same_worker(expected))
    {
        return Err(not_found(id));
    }
    let canonical = state.descriptor_as(false, id)?.session;
    let basis = crate::workspace_model::views::TerminalRemovalBasis::Worker {
        owner: state.proof().clone(),
        instance: expected.execution_instance(),
    };
    let (permit, edges) = match sql {
        Some(sql) => sql
            .publish(&canonical, &basis, pane, original)
            .map_err(|error| {
                if error == super::model::Registry::VIEW_REMOVAL_CONFLICT
                    || error == super::model::Registry::VIEW_SOURCE_CONFLICT
                {
                    pending_save_conflict()
                } else {
                    model_unavailable(error)
                }
            })?,
        #[cfg(test)]
        None => (
            super::model::terminal_sources::PublishedTerminal::fixture(),
            Vec::new(),
        ),
        #[cfg(not(test))]
        None => return Err(missing_registry()),
    };
    let _permit = permit;
    state
        .published_terminal_workers
        .observe(id, expected.execution_instance());
    if let Some(client) = client {
        // Until now the spawning client was attached hidden, so a redraw or
        // another client may have hidden the worker meanwhile. Apply the
        // published visibility as `attach` does, or a hidden worker captures
        // nothing more until it exits.
        let old_visibility = state.subscriptions.visibility(id);
        state.subscriptions.attach(
            client,
            id,
            if options.initially_visible {
                TerminalVisibility::Visible
            } else {
                TerminalVisibility::Hidden
            },
        );
        let new_visibility = state.subscriptions.visibility(id);
        if old_visibility != new_visibility {
            expected.set_host_visibility(new_visibility);
        }
        state.rearm_output(client, id);
        state.publish_worker(client, id);
    }
    let descriptor = state.descriptor(client.unwrap_or(0), id)?;
    for (edit, receipt) in edges {
        state.announce_model_edit(0, receipt.sequence, &edit, &receipt.effects);
    }
    state.publication_pending.remove(&id);
    state.startup_custody.remove(&id);
    // Spawn/attachment responses carry the initial grid. A persistence-only
    // retry returns Accepted and grants no new snapshot acknowledgement.
    if client.is_some() {
        expected.acknowledge_redraw(expected.generation());
    }
    Ok(descriptor)
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
        || options.max_scrollback_bytes > TerminalConfig::MAX_SCROLLBACK_BYTES
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            "terminal dimensions or scrollback are outside supported bounds",
            false,
        ));
    }
    Ok(())
}

fn broadcast_output(host: &Weak<HostLock>, terminal_id: TerminalId, sequence: u64) {
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

/// Binds redraw wakes before Host publication without a strong callback cycle.
struct TerminalRedraw {
    host: Weak<HostLock>,
    terminal_id: TerminalId,
    worker: OnceLock<crate::terminal::WeakTerminalHandle>,
}

impl TerminalRedraw {
    fn publish(&self, generation: u64) {
        if let Some(worker) = self.worker.get().and_then(|worker| worker.upgrade()) {
            broadcast_redraw(&self.host, self.terminal_id, generation, &worker);
        }
    }
}

/// How long one redraw broadcast holds the host lock, for the opt-in
/// `cagents::terminal_pipeline` trace. Disabled, it reads no clock.
struct HostHold {
    enabled: bool,
    since: Option<Instant>,
    held: Duration,
}

impl HostHold {
    fn new() -> Self {
        Self {
            enabled: tracing::enabled!(target: "cagents::terminal_pipeline", tracing::Level::TRACE),
            since: None,
            held: Duration::ZERO,
        }
    }

    fn acquired(&mut self) {
        if self.enabled {
            self.since = Some(Instant::now());
        }
    }

    fn released(&mut self) {
        if let Some(since) = self.since.take() {
            self.held += since.elapsed();
        }
    }

    /// Records one sample, the total hold, per broadcast that queued at least
    /// one frame. Broadcasts that queued only notifications or a Hide are not
    /// sampled.
    fn report(&self, queued_frames: usize) {
        if self.enabled && queued_frames > 0 {
            tracing::trace!(target: "cagents::terminal_pipeline", stage = "redraw_host_lock",
                elapsed_ns = self.held.as_nanos() as u64, frames = queued_frames);
        }
    }
}

/// Publishes a repaint to every visible subscriber of this exact worker.
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
/// cannot run ahead of the UI.
///
/// Building wire rows copies every changed cell, and every cell of a full
/// repaint, once per subscriber. That work happens outside the host lock,
/// which every client request, chat actor and scheduler shares. The lock is
/// taken twice: [`plan_redraw`] validates the worker, reads visibility and
/// each subscriber's base, and [`commit_redraw`] queues each frame only if
/// nothing presented that subscriber in between. Recording a frame as sent
/// and queuing it happen under one hold, so the queue holds frames in the
/// order they were recorded and every delta is relative to the frame queued
/// before it.
fn broadcast_redraw(
    host: &Weak<HostLock>,
    terminal_id: TerminalId,
    generation: u64,
    worker: &TerminalHandle,
) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let mut hold = HostHold::new();
    let planned = {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        hold.acquired();
        let planned = plan_redraw(&mut state, terminal_id, generation, worker);
        hold.released();
        planned
    };
    if planned.is_empty() {
        return;
    }
    let encoded = planned
        .into_iter()
        .map(PlannedFrame::encode)
        .collect::<Vec<_>>();
    let committed = {
        let mut state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        hold.acquired();
        let committed = commit_redraw(&mut state, terminal_id, worker, encoded);
        hold.released();
        committed
    };
    hold.report(committed.queued);
    // Dropped here, outside the lock: the replaced bases free the rows only
    // they still referenced, and refused frames free their wire rows.
    drop(committed);
}

/// A repaint whose subscriber state was read under the host lock, to be
/// encoded outside it.
struct PlannedFrame {
    client_id: ClientId,
    delivery: Arc<DeliveryQueue>,
    /// The subscriber's presentation revision when planned. Every resize,
    /// scroll, snapshot reply and frame that presents the view advances it,
    /// so a commit that finds it changed knows the frame is stale.
    revision: u64,
    /// The worker generation `projected` was captured at.
    source: u64,
    /// The capture, numbered with the subscriber's presentation revision.
    projected: Arc<RenderSnapshot>,
    /// The frame this subscriber was last recorded as sent.
    recorded: Option<Arc<RenderSnapshot>>,
    /// Whether `recorded` is known to be queued ahead of this frame, and so
    /// can be its delta base.
    delta: bool,
}

struct EncodedFrame {
    planned: PlannedFrame,
    wire: TerminalSnapshot,
}

impl PlannedFrame {
    /// Builds the wire frame. Pure: both snapshots are immutable.
    fn encode(self) -> EncodedFrame {
        let wire = match &self.recorded {
            Some(base) if self.delta => TerminalSnapshot::diff(&self.projected, base),
            _ => TerminalSnapshot::full(&self.projected),
        };
        EncodedFrame {
            planned: self,
            wire,
        }
    }
}

/// The first hold of a redraw broadcast. Returns the frames to encode; every
/// other outcome is settled here.
fn plan_redraw(
    state: &mut HostState,
    terminal_id: TerminalId,
    generation: u64,
    worker: &TerminalHandle,
) -> Vec<PlannedFrame> {
    // Retirement does not drain queued callbacks. Validate the original
    // worker under the same lock as visibility and viewport work; an earlier
    // identity check could race a same-ID replacement.
    if !state
        .terminals
        .get(&terminal_id)
        .is_some_and(|record| record.handle.same_worker(worker))
    {
        return Vec::new();
    }
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
        // Requested under the lock, as attachment and reveal changes are, so
        // a newer request from either always replaces this one.
        if state.subscriptions.visibility(terminal_id) == TerminalVisibility::Hidden {
            worker.set_host_visibility(TerminalVisibility::Hidden);
        }
        return Vec::new();
    }
    let proof = state.proof_handle();
    // The identity check above makes this the record's own published capture.
    let capture = worker.snapshot();
    let mut planned = Vec::new();
    for client_id in visible {
        // A closed connection refuses every frame. Building one for it would
        // only free it under the commit's hold.
        let Some(delivery) = state
            .clients
            .get(&client_id)
            .filter(|delivery| !delivery.is_closed())
            .cloned()
        else {
            continue;
        };
        let key = (client_id, terminal_id);
        let mut revision = state.presentation_observed(client_id, terminal_id, generation);
        let Subscriptions {
            viewports, sent, ..
        } = &mut state.subscriptions;
        let Some(view) = viewports.get_mut(&key) else {
            continue;
        };
        let direct = !capture.detached
            && capture.scroll.offset == capture.scroll.max_offset()
            && view.offset.is_none()
            && view.size.is_none_or(|size| size.rows == capture.rows);
        if !direct {
            let _ = delivery.push(Outbound::broadcast(
                &proof,
                ServerMessage::Redraw {
                    terminal_id,
                    generation: revision,
                },
            ));
            continue;
        }
        let recorded = sent.get(&key);
        if recorded.is_some_and(|sent| sent.snapshot.generation >= revision) {
            // The subscriber was already sent a frame with this number; it
            // drops another. A stale wake carries nothing newer. A newer
            // capture under an old wake needs a number of its own, or it
            // would be recorded as sent while the subscriber discards it.
            if view.acknowledged_source == capture.generation {
                continue;
            }
            view.advance(capture.generation);
            revision = view.revision;
        }
        let mut projected = (*capture).clone();
        projected.generation = revision;
        planned.push(PlannedFrame {
            client_id,
            delivery,
            revision,
            source: capture.generation,
            projected: Arc::new(projected),
            recorded: recorded.map(|sent| Arc::clone(&sent.snapshot)),
            delta: recorded.is_some_and(|sent| sent.queued),
        });
    }
    planned
}

/// What a commit queued, and what it no longer needs. The caller drops it
/// after releasing the lock.
struct Committed {
    queued: usize,
    /// Each planned frame's recorded base, now replaced or abandoned.
    bases: Vec<Option<Arc<RenderSnapshot>>>,
    /// Frames refused because their subscriber changed or left.
    refused: Vec<EncodedFrame>,
}

/// The second hold of a redraw broadcast: queues each encoded frame whose
/// subscriber is still exactly as planned, and records it as sent.
///
/// A subscriber presented in between — a resize, scroll, snapshot reply,
/// reveal, or a concurrent broadcast's frame — gets a `Redraw` at its current
/// revision instead. The frame would be numbered or based on a presentation
/// the subscriber no longer has; the notification lets it fetch the current
/// one through the request path.
fn commit_redraw(
    state: &mut HostState,
    terminal_id: TerminalId,
    worker: &TerminalHandle,
    frames: Vec<EncodedFrame>,
) -> Committed {
    let mut committed = Committed {
        queued: 0,
        bases: Vec::with_capacity(frames.len()),
        refused: Vec::new(),
    };
    // A same-ID replacement may have been installed while encoding. Nothing
    // from the retired worker may reach its subscribers.
    if !state
        .terminals
        .get(&terminal_id)
        .is_some_and(|record| record.handle.same_worker(worker))
    {
        committed.refused = frames;
        return committed;
    }
    let proof = state.proof_handle();
    let HostState {
        subscriptions,
        clients,
        ..
    } = state;
    for frame in frames {
        let planned = &frame.planned;
        let key = (planned.client_id, terminal_id);
        let still_visible = subscriptions
            .by_terminal
            .get(&terminal_id)
            .and_then(|subscribers| subscribers.get(&planned.client_id))
            == Some(&TerminalVisibility::Visible);
        let same_connection = clients
            .get(&planned.client_id)
            .is_some_and(|current| Arc::ptr_eq(current, &planned.delivery))
            && !planned.delivery.is_closed();
        let unchanged_base = match (subscriptions.sent.get(&key), &planned.recorded) {
            (Some(sent), Some(recorded)) => Arc::ptr_eq(&sent.snapshot, recorded),
            (None, None) => true,
            _ => false,
        };
        let current_revision = subscriptions
            .viewports
            .get(&key)
            .filter(|_| still_visible && same_connection)
            .map(|view| view.revision);
        match current_revision {
            Some(revision) if revision == planned.revision && unchanged_base => {
                let EncodedFrame { planned, wire } = frame;
                if let Some(view) = subscriptions.viewports.get_mut(&key) {
                    view.acknowledged_source = planned.source;
                }
                subscriptions.record_sent(planned.client_id, terminal_id, planned.projected, true);
                let _ = planned.delivery.push(Outbound::broadcast(
                    &proof,
                    ServerMessage::Snapshot {
                        terminal_id,
                        snapshot: wire,
                    },
                ));
                committed.queued += 1;
                committed.bases.push(planned.recorded);
            }
            Some(revision) => {
                let _ = planned.delivery.push(Outbound::broadcast(
                    &proof,
                    ServerMessage::Redraw {
                        terminal_id,
                        generation: revision,
                    },
                ));
                committed.refused.push(frame);
            }
            None => committed.refused.push(frame),
        }
    }
    committed
}

#[cfg(test)]
pub(in crate::host) fn record_event(
    host: &Weak<HostLock>,
    terminal_id: TerminalId,
    event: TerminalEvent,
) {
    record_event_from(host, terminal_id, event, None);
}
fn record_event_from(
    host: &Weak<HostLock>,
    terminal_id: TerminalId,
    event: TerminalEvent,
    worker: Option<&TerminalHandle>,
) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let recipient = match &event {
        TerminalEvent::SearchResults { owner, .. } => *owner,
        _ => None,
    };
    let wire_event = WireTerminalEvent::from(event);
    let (proof, sequence, deliveries, wake_exit) = {
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
        if let WireTerminalEvent::TitleChanged { title } = &wire_event {
            record.display_title = Some(
                title
                    .chars()
                    .take(crate::terminal_ipc::MAX_TERMINAL_TITLE_CHARACTERS)
                    .collect(),
            );
        }
        if let WireTerminalEvent::ConversationTitleChanged { title } = &wire_event {
            record.conversation_title = Some((sequence, title.clone()));
        }
        if let WireTerminalEvent::WorkingDirectoryChanged { path } = &wire_event {
            record.working_directory = Some((sequence, path.clone()));
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
        if !matches!(
            wire_event,
            WireTerminalEvent::ClipboardText { .. } | WireTerminalEvent::SearchResults { .. }
        ) {
            record.events.push_back((sequence, wire_event.clone()));
        }
        while record.events.len() > EVENT_REPLAY_CAPACITY {
            record.events.pop_front();
        }
        let deliveries = client_ids
            .into_iter()
            .filter(|client_id| recipient.is_none_or(|owner| owner == *client_id))
            .filter_map(|client_id| state.clients.get(&client_id).cloned())
            .collect::<Vec<_>>();
        // This observer is already installed before adoption commit. It
        // covers natural completion even when later publication failed, and
        // a restored original has no local child-reaper reservation to wake.
        let wake_exit = state.ordinary_exit_cleanup_pending
            && worker.is_some()
            && matches!(wire_event, WireTerminalEvent::Completed { .. });
        (proof, sequence, deliveries, wake_exit)
    };
    if wake_exit {
        crate::daemon::wake();
    }
    let message = ServerMessage::Event {
        terminal_id,
        sequence,
        event: wire_event,
    };
    for delivery in deliveries {
        let _ = delivery.push(Outbound::broadcast(&proof, message.clone()));
    }
}

fn transcript_metadata(
    path: Option<&std::path::Path>,
    location: Option<crate::model::TranscriptLocationReceipt>,
) -> Option<TranscriptMetadata> {
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
        // Original writer identity evidence, not pathname availability. The
        // UI validates it against freshly opened descriptors when probing and
        // activating; listing terminals never mints a replacement receipt.
        location,
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
mod bounded_input_upgrade_tests;
#[cfg(test)]
mod native_upgrade_tests;
#[cfg(test)]
mod redraw_identity_tests;
#[cfg(test)]
mod redraw_ordering_tests;
#[cfg(test)]
mod relaunch_lineage_tests;
#[cfg(test)]
mod retained_child_shutdown_tests;
#[cfg(test)]
mod strict_upgrade_tests;

#[cfg(test)]
pub(super) mod tests;
