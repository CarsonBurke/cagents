use super::state::{ClientId, HostLock, HostState, SharedHost};
#[cfg(test)]
mod account_isolation_tests;
#[cfg(test)]
mod account_recovery_target_tests;
pub(super) mod capacity;
mod codex_owners;
mod console_relaunch;
pub(super) use console_relaunch::matched_cli;
#[cfg(test)]
mod console_relaunch_tests;
pub(super) mod floor_history;
#[cfg(test)]
mod lost_agent_close_tests;
#[cfg(test)]
mod lost_codex_owner_close_tests;
mod native_cleanup;
mod native_recovery;
pub(super) use native_recovery::restore_ready_checkpoint;
mod owned_listener;
pub(super) use owned_listener::{
    Roots as CodexListenerRoots, keep_legacy_listeners_fresh as keep_legacy_codex_listeners_fresh,
};
mod paired_close;
pub(in crate::host) mod pane_start;
pub(super) mod retained_actor;
#[cfg(test)]
mod retained_actor_tests;
#[cfg(test)]
mod retained_admission_tests;
#[cfg(test)]
mod retained_converted_cleanup_tests;
#[cfg(test)]
mod retained_execution_exit_tests;
#[cfg(test)]
mod retained_identity_tests;
#[cfg(test)]
mod retained_pending_cleanup_tests;
#[cfg(test)]
mod retained_pending_installation_tests;
#[cfg(test)]
mod retained_publication_tests;
#[cfg(test)]
mod retained_receipt_collection_tests;
#[cfg(test)]
mod retained_resolution_tests;
mod retained_resume;
#[cfg(test)]
mod start_retryability_tests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod unserved_resume_tests;
pub(super) mod upgrade_resolution;
use crate::{
    agent::Provider,
    chat::{
        self, Session,
        runtime::{Handle, Update},
    },
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
        ServerMessage, ServerResponse, validate_proof,
    },
};
pub(super) use native_cleanup::{NativeClose, close_retained as close_retained_native};
pub(super) use native_recovery::AdoptionFailures;
pub(super) use native_recovery::restore_orphaned_native;
pub(super) use native_recovery::restore_orphaned_native_preserving;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    thread,
    time::{Duration, Instant},
};

pub(super) struct Record {
    pub(super) execution_instance: uuid::Uuid,
    pub(super) console_recipe: Option<crate::model::LaunchRecipe>,
    pub(super) paired_terminal: Option<crate::terminal::TerminalHandle>,
    pub session: Session,
    pub handle: Handle,
    // Retain the exact CLI/account that authorized a shared Codex attachment.
    pub(super) source_process: Option<crate::model::agent_recovery::AgentProcess>,
    pub subscribers: BTreeMap<ClientId, bool>,
    pub(super) observations: BTreeMap<ClientId, Observation>,
    pub completion_observed: bool,
    // A failed terminal spawn must keep the closed actor's execution context
    // retryable and must not disappear through recovery or daemon upgrade.
    pub(super) returning_terminal: bool,
}

impl Record {
    /// The private recipe is installed only by the execution service after
    /// accepting this actor. A CLI basename or user-supplied argv is no proof.
    pub(super) fn owns_execution_console(&self) -> bool {
        self.session
            .terminal
            .as_ref()
            .is_some_and(|terminal| self.accepts_execution_console(terminal))
    }

    pub(super) fn accepts_execution_console(
        &self,
        terminal: &crate::model::TerminalSession,
    ) -> bool {
        terminal.id == self.session.id
            && terminal.owned_codex_listener.is_none()
            && self.console_recipe.as_ref() == Some(&terminal.launch)
    }
}

/// Terminal records keep their launch continuity; published descriptors project
/// Live/Detached for each subscriber. Compare the full remaining descriptor and
/// any published daemon proof. Resume hints are observed metadata that can be
/// enriched while the same worker runs; callers must match the private worker.
pub(super) fn matches_paired_terminal(
    actual: &crate::model::TerminalSession,
    source: &crate::model::TerminalSession,
    proof: &HostSessionProof,
) -> bool {
    if source
        .continuity
        .live_proof()
        .as_ref()
        .is_some_and(|source| source != proof)
    {
        return false;
    }
    let mut actual = actual.clone();
    actual.continuity = source.continuity.clone();
    actual.resume_hint = source.resume_hint.clone();
    actual == *source
}

// Transition evidence is not an actor: no transcript, socket, store or execution
// environment survives a successful replacement. Keep it until the GUI confirms
// durable ownership of the terminal, never merely until a response or attach.
pub(super) struct TerminalHandoff {
    source: Session,
    terminal: crate::model::TerminalSession,
    generation: u64,
    subscribers: BTreeMap<ClientId, bool>,
    /// After this moment no window can still be waiting to save this
    /// replacement, counted from the conversion or from the most recent replay
    /// of its response, whichever happened last.
    acknowledgement_deadline: Instant,
}

/// A window acknowledges a receipt one round trip after its own durable save,
/// and it retries on every refresh and every start, so a receipt still
/// outstanding long afterwards belongs to a window that will never save it: it
/// crashed, it discarded the card, or its own store is permanently unwritable.
/// Without this bound the daemon could never checkpoint again, which leaves
/// terminating every hosted session as the only way to install a new build.
pub(super) const ACKNOWLEDGEMENT_GRACE: Duration = Duration::from_secs(300);

impl TerminalHandoff {
    /// Where both sides of the handoff run.
    pub(super) fn working_directories(&self) -> [&std::path::Path; 2] {
        [
            &self.source.launch.recipe.working_directory,
            &self.terminal.launch.working_directory,
        ]
    }

    pub(super) fn new(
        source: Session,
        terminal: crate::model::TerminalSession,
        generation: u64,
        subscribers: BTreeMap<ClientId, bool>,
        acknowledgement_deadline: Instant,
    ) -> Self {
        Self {
            source,
            terminal,
            generation,
            subscribers,
            acknowledgement_deadline,
        }
    }

    /// Whether no window can still be waiting to save this replacement.
    ///
    /// Attachment is deliberately not the question: a window drops its chat
    /// connection as part of the conversion and acknowledges over a fresh one,
    /// so every receipt is unattached from the moment it exists.
    pub(super) fn unacknowledgeable(&self, now: Instant) -> bool {
        now >= self.acknowledgement_deadline
    }

    /// A window that just received the replacement's identity again owes its
    /// durable save from that moment, not from the original conversion.
    pub(super) fn reissue(&mut self, now: Instant) {
        self.acknowledgement_deadline = now + ACKNOWLEDGEMENT_GRACE;
    }

    pub(super) fn replacement(&self) -> &crate::model::TerminalSession {
        &self.terminal
    }

    fn snapshot(&self) -> chat::Snapshot {
        chat::Snapshot {
            generation: self.generation,
            session_id: self.source.launch.session_id.clone(),
            exited: true,
            detail: "Returned to terminal; complete the retained terminal handoff".into(),
            ..Default::default()
        }
    }

    fn validate_source(&self, source: &Session) -> Result<(), ProtocolError> {
        if self.source.launch.provider != source.launch.provider
            || self.source.launch.recipe != source.launch.recipe
            || (source.launch.session_id.is_some()
                && source.launch.session_id != self.source.launch.session_id)
        {
            return Err(error(
                "Native chat recovery identity does not match the terminal handoff",
            ));
        }
        Ok(())
    }

    fn require_attached(&self, client: ClientId) -> Result<(), ProtocolError> {
        if !self.subscribers.contains_key(&client) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::PermissionDenied,
                "Client is not attached to this chat",
                false,
            ));
        }
        Ok(())
    }

    pub(super) fn detach(&mut self, client: ClientId) {
        self.subscribers.remove(&client);
    }
}

pub(super) struct PendingBinding {
    preview: chat::BindingPreview,
    process: crate::model::agent_recovery::AgentProcess,
    expires: Instant,
}

impl PendingBinding {
    pub(super) fn working_directory(&self) -> &std::path::Path {
        &self.process.launch.working_directory
    }

    fn authorizes(
        &self,
        process: &crate::model::agent_recovery::AgentProcess,
        id: &str,
        now: Instant,
    ) -> bool {
        self.expires > now
            && self.process == *process
            && process.provider == Provider::Codex
            && self.preview.session_id == id
    }
}

fn preview(
    host: &SharedHost,
    session: Session,
    source: HostSessionProof,
) -> Result<ServerResponse, ProtocolError> {
    let id = session
        .launch
        .session_id
        .as_deref()
        .filter(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
        .ok_or_else(|| error("Enter the complete conversation UUID, not a name or prefix"))?;
    if session.launch.provider != Provider::Codex {
        return Err(error(
            "Explicit shared-server selection is only supported for Codex",
        ));
    }
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(Some(&source), state.proof())?;
        state
            .terminals
            .get(&session.id)
            .filter(|record| !record.handle.is_completed())
            .ok_or_else(|| unavailable("The source terminal is no longer running"))?
            .handle
            .clone()
    };
    let process = handle
        .observe_agent_process()
        .filter(|process| process.provider == Provider::Codex)
        .ok_or_else(|| unavailable("The source is no longer a foreground Codex CLI"))?;
    let (title, working_directory) =
        crate::agent::observe::codex_preview(&process.recovery, id).map_err(unavailable)?;
    // Cwd is displayed to the user, not used to infer a conversation binding.
    if handle.observe_agent_process().as_ref() != Some(&process) {
        return Err(error(
            "The foreground process changed during preview; retry",
        ));
    }
    let preview = chat::BindingPreview {
        token: uuid::Uuid::new_v4().to_string(),
        session_id: id.to_owned(),
        title,
        working_directory,
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    validate_proof(Some(&source), state.proof())?;
    state
        .chat_bindings
        .retain(|_, binding| binding.expires > Instant::now());
    if state.chat_bindings.len() >= 128 && !state.chat_bindings.contains_key(&session.id) {
        return Err(unavailable(
            "Too many pending conversation selections; retry after they expire",
        ));
    }
    state.chat_bindings.insert(
        session.id,
        PendingBinding {
            preview: preview.clone(),
            process,
            expires: Instant::now() + Duration::from_secs(120),
        },
    );
    Ok(ServerResponse::ChatPreviewed { preview })
}

/// Enumerates the source account's open conversations without issuing a token
/// or recording a pending binding, because listing must grant no attach right.
fn candidates(
    host: &SharedHost,
    session: Session,
    source: HostSessionProof,
) -> Result<ServerResponse, ProtocolError> {
    if session.launch.provider != Provider::Codex {
        return Err(error(
            "Explicit shared-server selection is only supported for Codex",
        ));
    }
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(Some(&source), state.proof())?;
        state
            .terminals
            .get(&session.id)
            .filter(|record| !record.handle.is_completed())
            .ok_or_else(|| unavailable("The source terminal is no longer running"))?
            .handle
            .clone()
    };
    let process = handle
        .observe_agent_process()
        .filter(|process| process.provider == Provider::Codex)
        .ok_or_else(|| unavailable("The source is no longer a foreground Codex CLI"))?;
    let candidates =
        crate::agent::observe::codex_candidates(&process.recovery).map_err(unavailable)?;
    // The listing describes the account observed above, so a replaced foreground
    // process means it describes some other CLI and must not be presented.
    if handle.observe_agent_process().as_ref() != Some(&process) {
        return Err(error(
            "The foreground process changed while the conversations were being listed; retry",
        ));
    }
    Ok(ServerResponse::ChatCandidates { candidates })
}

#[derive(Default)]
pub(super) struct Observation {
    epoch: u64,
    after_generation: u64,
    notified: bool,
}

impl Observation {
    fn acknowledge(&mut self, epoch: u64, generation: u64) {
        if epoch > self.epoch {
            self.epoch = epoch;
            self.after_generation = generation;
            self.notified = false;
        }
    }

    fn accepts(&self, generation: u64) -> bool {
        !self.notified && generation > self.after_generation
    }
}
fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, false)
}
fn setup_error(failure: chat::protocol::SetupError) -> ProtocolError {
    match failure {
        chat::protocol::SetupError::Invalid(message) => error(message),
        chat::protocol::SetupError::Unavailable(message) => retryable_unavailable(message),
    }
}
fn retryable_unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
}

fn launch_account_error(failure: crate::account::registry::LaunchAccountError) -> ProtocolError {
    let retryable = matches!(&failure,
        crate::account::registry::LaunchAccountError::Unreadable(error)
            if !matches!(error.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::Unsupported)
    );
    ProtocolError::new(
        if matches!(
            failure,
            crate::account::registry::LaunchAccountError::Removed { .. }
        ) {
            ProtocolErrorCode::AccountRemoved
        } else {
            ProtocolErrorCode::TerminalUnavailable
        },
        failure.to_string(),
        retryable,
    )
}
fn check_launch_account(launch: &chat::Launch) -> Result<(), ProtocolError> {
    crate::account::registry::authorize_spawn(&launch.recipe.environment, Some(launch.provider))
        .map(drop)
        .map_err(launch_account_error)
}

fn attached(state: &HostState, client: ClientId, id: TerminalId) -> Result<&Record, ProtocolError> {
    let record = state.chats.get(&id).ok_or_else(|| {
        unavailable("Exact native chat is not hosted; explicit recovery is required")
    })?;
    if record.handle.is_close_only() {
        return Err(unavailable(
            "This exact retained actor is reserved for cleanup",
        ));
    }
    if !record.subscribers.contains_key(&client) {
        return Err(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "Client is not attached to this chat",
            false,
        ));
    }
    Ok(record)
}
fn attached_mut(
    state: &mut HostState,
    client: ClientId,
    id: TerminalId,
) -> Result<&mut Record, ProtocolError> {
    let record = state.chats.get_mut(&id).ok_or_else(|| {
        unavailable("Exact native chat is not hosted; explicit recovery is required")
    })?;
    if record.handle.is_close_only() {
        return Err(unavailable(
            "This exact retained actor is reserved for cleanup",
        ));
    }
    if !record.subscribers.contains_key(&client) {
        return Err(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "Client is not attached to this chat",
            false,
        ));
    }
    Ok(record)
}
/// Helper callbacks enqueue durable work; they never refresh or close their
/// own proxy while its ordered publication fence is held.
fn changed_owned(host: &Weak<HostLock>, id: TerminalId, instance: uuid::Uuid, update: Update) {
    let Some(owner) = host.upgrade() else {
        return;
    };
    match update {
        Update::RetainedOutcomes => retained_actor::wake(&owner, id),
        Update::Execution { run, update } => {
            super::agent_execution::observed(&owner, id, instance, run, update)
        }
        update => changed_for_instance(host, id, Some(instance), update),
    }
}

fn changed(host: &Weak<HostLock>, id: TerminalId, update: Update) {
    changed_for_instance(host, id, None, update);
}

#[cfg(test)]
thread_local! {
    // A one-shot witness before acquiring the attention ownership registry.
    static ATTENTION_REGISTRY_REACHED: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

fn changed_for_instance(
    host: &Weak<HostLock>,
    id: TerminalId,
    instance: Option<uuid::Uuid>,
    update: Update,
) {
    let Some(host) = host.upgrade() else {
        return;
    };
    if matches!(
        &update,
        Update::Attention(_) | Update::AttentionContext { .. }
    ) {
        // Scheduled outcomes belong to the accepted run, not its temporary
        // provider process. The scheduler publishes committed run evidence.
        // Read outside HostState; a failed ownership read cannot safely turn
        // that evidence into a second, session-scoped notification.
        if let Ok(registry) = super::agent_execution::registry(&host) {
            #[cfg(test)]
            ATTENTION_REGISTRY_REACHED.with(|signal| {
                if let Some(signal) = signal.borrow_mut().take() {
                    let _ = signal.send(());
                }
            });
            match registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .agent_for_session(id)
            {
                Ok(Some(agent))
                    if matches!(
                        agent.origin,
                        crate::agent_execution::Origin::Automation { .. }
                    ) =>
                {
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot resolve native attention ownership");
                    return;
                }
                _ => {}
            }
        }
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    // Validate the producer under the same lock that publishes its update.
    // A retired callback must never target a replacement sharing this pane ID.
    if instance.is_some_and(|instance| {
        state
            .chats
            .get(&id)
            .is_none_or(|record| record.execution_instance != instance)
    }) {
        return;
    }
    let proof = state.proof().clone();
    // A conversation that starts or settles may take the messages left for
    // it, and may have hit a usage limit or left one behind.
    if matches!(&update, Update::PromptAvailable | Update::Activity { .. })
        && let Some(courier) = &state.mailbox
    {
        courier.wake();
    }
    if let Update::Attention(evidence) | Update::AttentionContext { evidence, .. } = &update {
        if let Some(record) = state.chats.get(&id)
            && let Some(notifications) = &state.notifications
        {
            let mut event = crate::notifications::Event::session(
                id,
                record.session.launch.provider,
                record.session.launch.recipe.working_directory.clone(),
                evidence.clone(),
            );
            if let Update::AttentionContext { content, .. } = &update {
                event.content = content.clone();
            }
            notifications.record(event);
        }
        return;
    }
    let HostState { chats, clients, .. } = &mut *state;
    let Some(record) = chats.get_mut(&id) else {
        return;
    };
    // This is a request to reevaluate lifecycle state, not a cleanup receipt.
    // Check the exact current producer under HostState; a failed/retained owner
    // must not masquerade as released work. Remote proxies publish Resident
    // metadata before this callback, so this reads no helper or transcript.
    let wake_completed = instance.is_some()
        && matches!(update, Update::Observed { .. })
        && record.handle.is_completed()
        && !record.handle.owns_work();
    for (&client, &visible) in &record.subscribers {
        let message = match &update {
            &Update::Activity { state, generation } => ServerMessage::ChatActivity {
                chat_id: id,
                state,
                generation,
            },
            Update::Presentation if visible => ServerMessage::ChatChanged { chat_id: id },
            &Update::Observed { generation } if !visible => {
                let observation = record.observations.entry(client).or_default();
                if !observation.accepts(generation) {
                    continue;
                }
                ServerMessage::ChatObserved {
                    chat_id: id,
                    epoch: observation.epoch,
                }
            }
            // Every surface keeps the account's figures, whether or not it
            // is showing this conversation.
            Update::AccountUsage {
                report,
                observed_at,
            } => ServerMessage::ChatAccountUsage {
                chat_id: id,
                report: report.clone(),
                observed_at: *observed_at,
            },
            // Every surface keeps them, whether or not it is showing this
            // conversation, since it learns of them only as they change.
            Update::AgentAddresses(addresses) => ServerMessage::ChatAgentAddresses {
                chat_id: id,
                addresses: addresses.clone(),
            },
            Update::ActiveModel { revision, model } => ServerMessage::ChatActiveModel {
                chat_id: id,
                revision: *revision,
                model: model.clone(),
            },
            _ => continue,
        };
        if let Some(delivery) = clients.get(&client) {
            let delivered = delivery.push(ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof.clone(),
                message,
            });
            if matches!(update, Update::Observed { .. }) && delivered.is_ok() {
                record.observations.entry(client).or_default().notified = true;
            }
        }
    }
    // An old producer callback can consume the shared rearm after an ACK but
    // arrive here with a pre-ACK generation. Keep waiting subscribers armed;
    // only successful delivery in their own epoch coalesces future output.
    if matches!(update, Update::Observed { .. })
        && record.subscribers.iter().any(|(client, visible)| {
            !visible
                && record
                    .observations
                    .get(client)
                    .is_none_or(|observation| !observation.notified)
        })
    {
        record.handle.observe_next_update();
    }
    drop(state);
    if wake_completed {
        crate::daemon::wake();
    }
}
/// Hydration may contact a retained helper and must run outside HostState.
fn session_snapshot(session: &Session, handle: &Handle) -> Result<ServerResponse, String> {
    if handle.is_close_only() {
        return Err(
            "The retained owner is reserved for exact cleanup; ordinary attachment is refused"
                .into(),
        );
    }
    let snapshot = handle.snapshot(None).ok_or(
        "The exact provider could not publish its attachment snapshot; ownership was preserved",
    )?;
    let mut session = session.clone();
    session.launch.session_id = snapshot.session_id.clone();
    session.native_continuity = snapshot.native_continuity.clone();
    Ok(ServerResponse::ChatAttached { session, snapshot })
}

/// What a new subscriber is told of how the provider addresses a chat's
/// subagents, when it knows any.
pub(super) fn agent_addresses(host: &SharedHost, chat_id: TerminalId) -> Option<ServerMessage> {
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&chat_id)?
        .handle
        .clone();
    // Read outside HostState: a retained actor answers from its last
    // published metadata, a local one from its own lock.
    let addresses = handle.agent_addresses();
    (!addresses.is_empty()).then_some(ServerMessage::ChatAgentAddresses { chat_id, addresses })
}

pub(super) fn active_model(host: &SharedHost, chat_id: TerminalId) -> Option<ServerMessage> {
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&chat_id)?
        .handle
        .clone();
    let (revision, model) = handle.active_model();
    Some(ServerMessage::ChatActiveModel {
        chat_id,
        revision,
        model,
    })
}

fn current_session_snapshot(
    host: &SharedHost,
    session: Session,
    handle: Handle,
    execution_instance: uuid::Uuid,
) -> Result<ServerResponse, ProtocolError> {
    let response = session_snapshot(&session, &handle).map_err(retryable_unavailable)?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down
        || state.chats.get(&session.id).is_none_or(|current| {
            current.session != session
                || current.execution_instance != execution_instance
                || !current.handle.same_attachment(&handle)
        })
    {
        let changed_identity = state.chats.get(&session.id).is_some_and(|current| {
            current.session.launch.provider != session.launch.provider
                || current.session.launch.recipe != session.launch.recipe
                || current.session.launch.session_id != session.launch.session_id
        });
        return Err(if changed_identity {
            unavailable("Native attachment changed while its exact snapshot was loading")
        } else {
            retryable_unavailable("Native attachment changed while its exact snapshot was loading")
        });
    }
    Ok(response)
}

fn same_conversation(a: &chat::Launch, b: &chat::Launch) -> bool {
    a.provider == b.provider && a.session_id.is_some() && a.session_id == b.session_id
}

fn resolve_source(
    handle: &crate::terminal::TerminalHandle,
    provider: Provider,
    selected: Option<&str>,
    selection_authorized: bool,
    explicit_environment: &BTreeMap<String, String>,
    transient_environment: Option<&BTreeMap<String, String>>,
) -> Result<
    (
        crate::model::agent_recovery::AgentProcess,
        crate::model::LaunchRecipe,
        String,
        Option<String>,
        Option<std::path::PathBuf>,
        Option<BTreeMap<String, String>>,
    ),
    ProtocolError,
> {
    use crate::model::agent_recovery::{bind_session, recipe_from_hint};
    let process = handle.observe_agent_process().ok_or_else(|| {
        retryable_unavailable(
            "No supported foreground agent is running in this terminal. Focus its CLI and retry.",
        )
    })?;
    if process.provider != provider {
        return Err(error(
            "The foreground provider changed; retry from the current terminal",
        ));
    }
    let hint = handle.observe_agent().map_err(retryable_unavailable)?;
    let fresh = crate::model::agent_recovery::fresh_omp_session(&process);
    if fresh.is_some()
        && !handle.agent_activity().is_some_and(|activity| {
            activity.provider == provider && activity.state == crate::agent::ActivityState::Idle
        })
    {
        return Err(retryable_unavailable(
            "Wait for the fresh agent to be idle before switching; an unsaved in-flight exchange will not be discarded.",
        ));
    }
    let public_id =
        crate::agent::observe::exact_session(&process).map_err(retryable_unavailable)?;
    let observed_id = public_id.or_else(|| {
        // A shared-server TUI can switch threads without changing argv or PID.
        // Even an original `resume ID` is only a preview suggestion, not current
        // authority. Codex activation requires confirmed shared-thread selection.
        if provider == Provider::Codex {
            return None;
        }
        let recipe = hint.as_deref().and_then(recipe_from_hint)?;
        provider.resume_id(&recipe.arguments).map(str::to_owned)
    });
    if observed_id
        .as_deref()
        .zip(selected)
        .is_some_and(|(actual, selected)| actual != selected)
    {
        return Err(error(
            "The active conversation changed before handoff; select the current conversation again",
        ));
    }
    // Codex's public shared server confirms the selected thread on attachment.
    // This is explicit selection, not a claim that cwd binds this TUI to a thread;
    // the source TUI stays alive and no second provider writer is spawned.
    let id = observed_id.or_else(|| {
        fresh.as_deref().and_then(|path| provider.session_file_id(path))
    }).or_else(|| {
        (provider == Provider::Codex && selection_authorized).then(|| selected.map(str::to_owned)).flatten()
    }).ok_or_else(|| {
        let message = match provider {
        Provider::Codex => "This terminal is not associated with a managed thread. Open New thread to use native and CLI views of one conversation.",
        Provider::Claude => "Claude has not published an exact live conversation yet. Retry once its live session is available; the terminal has not been stopped.",
        Provider::Omp => "OMP has not published an exact live conversation or fresh-session boundary for this terminal. The terminal has not been stopped; no recent history will be selected.",
        };
        if provider == Provider::Codex { unavailable(message) } else { retryable_unavailable(message) }
    })?;
    let mut recipe = bind_session(&process.recovery, &id).ok_or_else(|| error(
        "The active launch contains options that cannot be safely preserved. Resume the exact conversation with supported account and permission options, then retry.",
    ))?;
    if let Some(path) = &fresh {
        // bind_session validates and preserves account/runtime/permission options.
        // A provider-marked empty boundary has no resumable UUID yet. Explicit
        // session-dir makes OMP create a new session instead of auto-resuming one.
        recipe.arguments.truncate(recipe.arguments.len() - 2);
        recipe.arguments.extend([
            "--session-dir".into(),
            path.parent()
                .and_then(|parent| parent.to_str())
                .ok_or_else(|| unavailable("The fresh session directory cannot be preserved"))?
                .to_owned(),
        ]);
    }
    let execution = if provider != Provider::Codex {
        let (durable, execution) = crate::model::agent_recovery::native_environment(
            &process,
            explicit_environment,
            transient_environment,
        )
        .map_err(retryable_unavailable)?;
        recipe.environment = durable;
        Some(execution)
    } else {
        None
    };
    // The hint and conversion agree on durable configuration; proven transient
    // credentials travel separately to the replacement ProcessSpec.
    let hint = if fresh.is_some() {
        crate::model::agent_recovery::observed_hint_from_launch(&recipe)
    } else {
        crate::model::agent_recovery::hint_from_launch(&recipe)
    }.ok_or_else(|| unavailable("The native launch cannot be saved as a bounded recovery hint; the source has not been stopped"))?;
    if handle.observe_agent_process().as_ref() != Some(&process) {
        return Err(error(
            "The foreground process changed while resolving its conversation; retry",
        ));
    }
    Ok((process, recipe, id, Some(hint), fresh, execution))
}
fn stop_source(
    handle: &crate::terminal::TerminalHandle,
    frozen: &mut crate::terminal::UpgradeSnapshot,
) -> Result<(), ProtocolError> {
    frozen.stop().map_err(retryable_unavailable)?;
    if !handle.is_completed()
        && let Err(failure) = handle.shutdown()
        && !handle.is_completed()
    {
        return Err(super::state::send_error(failure));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        if Instant::now() >= deadline {
            return Err(retryable_unavailable(
                "CLI shutdown did not complete in ten seconds; source evidence is retained",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn confirm_fresh_session(
    handle: &Handle,
    launch: &mut chat::Launch,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<(), ProtocolError> {
    loop {
        if cancelled() {
            return Err(retryable_unavailable(
                "Fresh chat handoff was cancelled; the source terminal is retained",
            ));
        }
        if handle.is_completed() {
            return Err(retryable_unavailable(
                "The fresh native provider exited before its identity was confirmed; the source terminal is retained",
            ));
        }
        if let Some(id) = handle.confirmed_session_id() {
            launch.session_id = Some(id);
            return launch.terminal_recipe().map(|_| ()).ok_or_else(|| unavailable(
                "The fresh provider reported an unresumable identity; the source terminal is retained",
            ));
        }
        if Instant::now() >= deadline {
            return Err(retryable_unavailable(
                "The fresh provider did not confirm its conversation identity; the source terminal is retained",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn start(
    host: &SharedHost,
    client: ClientId,
    session: Session,
    source: Option<HostSessionProof>,
    terminate_source: bool,
    selection: Option<PendingBinding>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<ServerResponse, ProtocolError> {
    start_with_execution(
        host,
        client,
        session,
        source,
        terminate_source,
        selection,
        execution_environment,
        None,
        None,
        None,
    )
}

pub(super) fn start_execution(
    host: &SharedHost,
    client: ClientId,
    session: Session,
    context: Arc<chat::protocol::ExecutionContext>,
) -> Result<ServerResponse, ProtocolError> {
    start_with_execution(
        host,
        client,
        session,
        None,
        false,
        None,
        Default::default(),
        Some(context),
        None,
        None,
    )
}

/// Discovery can route an old presentation descriptor to recovery, but grants
/// no launch authority. Recovery authenticates the complete issued descriptor
/// with the holder before attaching, and never falls back to recipe execution.
pub(super) fn has_native_owner(session: &Session) -> Result<bool, ProtocolError> {
    if !matches!(session.launch.provider, Provider::Codex | Provider::Claude) {
        return Ok(false);
    }
    use crate::daemon::holder_registry::{self as registry, Kind};
    let directory = registry::directory(Kind::Native).map_err(retryable_unavailable)?;
    let socket = registry::socket(&directory, session.id);
    if session.launch.provider == Provider::Codex {
        let record = registry::execution_record(&socket).map_err(|failure| {
            retryable_unavailable(format!("Cannot read holder record: {failure}"))
        })?;
        if let Some(record) = record {
            let boot = crate::daemon::read_boot_id().map_err(|error| {
                retryable_unavailable(format!("Cannot read kernel boot: {error}"))
            })?;
            if !record.boot.as_str().is_empty() && record.boot != boot
                || chat::runtime::codex_execution_abandoned(&record.holder, &record.root)
                    .map_err(retryable_unavailable)?
            {
                return Ok(false);
            }
        }
    }
    registry::recovery_candidate(&socket, Kind::Native)
        // Unsupported retained protocols still own this allocation. Discovery
        // fences launch; only authenticated recovery may interpret its version.
        .map(|record| record.is_some())
        .map_err(retryable_unavailable)
}

/// Whether `session`'s complete proof names a kernel boot other than this one,
/// which proves every process it named is gone.
pub(super) fn from_earlier_boot(host: &SharedHost, session: &Session) -> bool {
    let boot = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .boot_id
        .clone();
    session
        .proof
        .as_ref()
        .is_some_and(|saved| saved.is_complete() && saved.boot_id != boot)
}

/// Holds a lost agent's allocation while its close commits, once nothing could
/// be running under it: no owner, no start, handoff or close in flight, no
/// live paired terminal or console, and no capability issued in this boot.
/// Start registers its capability before adoption rebinds the agent, so the
/// agent's old proof alone is not enough. A Start admitted while this is held
/// is refused as already starting or closing.
pub(super) struct LostHolderFence(#[allow(dead_code)] capacity::Reservation);

pub(super) fn fence_lost_holder(
    host: &SharedHost,
    session: &Session,
    console: Option<&crate::model::TerminalSession>,
) -> Result<LostHolderFence, String> {
    const REFUSED: &str =
        "A provider may be running for this agent since the computer restarted; it was not closed";
    let fence =
        reserve_quiet_allocation(host, session, console).map_err(|()| REFUSED.to_owned())?;
    let boot = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .boot_id
        .clone();
    let authority = super::agent_execution::registry(host)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_authority_source(session.id)?;
    let issued_now = authority.is_some_and(|stored| match stored {
        crate::agent_execution::HostedSession::Chat(stored) => stored
            .proof
            .as_ref()
            .is_none_or(|proof| proof.boot_id == boot),
        crate::agent_execution::HostedSession::Terminal(_) => true,
    });
    if issued_now {
        return Err(REFUSED.into());
    }
    Ok(LostHolderFence(fence))
}

/// Reserve `session`'s allocation for a close that ends nothing itself, once
/// nothing in this daemon holds it or the terminals that belong with it (its
/// paired CLI view and an agent's console): no resident or live terminal, no
/// start, handoff or close in flight, and no discovery record of a holder.
/// The reservation comes first, so a Start cannot begin between these checks
/// and the close; discovery is read only after it is held.
fn reserve_quiet_allocation(
    host: &SharedHost,
    session: &Session,
    console: Option<&crate::model::TerminalSession>,
) -> Result<capacity::Reservation, ()> {
    let fence = reserve_unhosted_allocation(host, session, console)?;
    let terminals = session
        .terminal
        .iter()
        .chain(console)
        .map(|terminal| terminal.id)
        .collect::<Vec<_>>();
    use crate::daemon::holder_registry::{self as registry, Kind};
    let terminal_owner = |id: &TerminalId| {
        registry::directory(Kind::Terminal)
            .and_then(|directory| {
                registry::recovery_candidate(&registry::socket(&directory, *id), Kind::Terminal)
            })
            // An unreadable record still claims the terminal.
            .map_or(true, |record| record.is_some())
    };
    if has_native_owner(session).unwrap_or(true) || terminals.iter().any(terminal_owner) {
        return Err(());
    }
    Ok(fence)
}

/// Reserve before execution inspection. In-memory owners, pending transitions
/// and terminals still fence the allocation; the caller must classify kernel
/// and registry evidence before treating this reservation as an ended pane.
fn reserve_unhosted_allocation(
    host: &SharedHost,
    session: &Session,
    console: Option<&crate::model::TerminalSession>,
) -> Result<capacity::Reservation, ()> {
    let mut terminals: Vec<TerminalId> = session
        .terminal
        .iter()
        .chain(console)
        .map(|terminal| terminal.id)
        .collect();
    terminals.sort();
    terminals.dedup();
    let fence = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        // A terminal counts from its spawn reservation until its teardown
        // completes, and a chat while any record of it remains.
        let held = |id: &TerminalId| {
            state.chats.contains_key(id) || terminal_claim_pending(&state, *id) || state.is_live(id)
        };
        if state.shutting_down || held(&session.id) || terminals.iter().any(held) {
            return Err(());
        }
        capacity::Reservation::closing(host, &mut state, session).map_err(|_| ())?
    };
    Ok(fence)
}

/// What a pane's Close asks of a conversation with no resident actor. An agent
/// lost with an earlier boot ends as an agent. A plain retained Claude
/// conversation an earlier boot left, with no owner, agent or launch, has
/// nothing to close. Everything else needs its exact close receipt.
pub(super) fn close_retained_from_pane(host: &SharedHost, session: &Session) -> Result<(), String> {
    if super::agent_execution::close_lost_agent_from_pane(host, session)? {
        return Ok(());
    }
    let plain = matches!(
        session.native_continuity,
        chat::NativeContinuity::RetainedClaudeActor { .. }
    ) && session.launch.provider == Provider::Claude
        && from_earlier_boot(host, session)
        // An owner or resident has something to close, through its receipt.
        && !has_native_owner(session).map_err(|error| error.message)?
        && !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&session.id)
        && super::agent_execution::registry(host).is_ok_and(|registry| {
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            registry
                .agent_for_session(session.id)
                .is_ok_and(|agent| agent.is_none())
                && registry
                    .has_agent_launch_for_session(session.id)
                    .is_ok_and(|pending| !pending)
        });
    if plain {
        const CHANGED: &str = "The conversation is starting or changing; it was not closed";
        // A Start can own this allocation before publishing a resident or a
        // helper. Hold the same reservation through the final no-op decision;
        // observing an empty resident map alone does not settle that Start.
        let _fence =
            reserve_quiet_allocation(host, session, None).map_err(|()| CHANGED.to_owned())?;
        let registry = super::agent_execution::registry(host)?;
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if registry.agent_for_session(session.id)?.is_some()
            || registry.has_agent_launch_for_session(session.id)?
        {
            return Err(CHANGED.into());
        }
        return Ok(());
    }
    close_retained_native(host, session)
}

/// Settles the Close of a conversation whose native owner ended without a
/// close receipt, once nothing of it can still run. Exact cleanup comes here
/// when it has nothing left to act on: a Codex conversation with no pending
/// close, receipt or live holder record, or whose pending close settled
/// without a receipt, and a retained Claude actor that exited without its
/// verified retirement. The caller holds the conversation's close claim, and
/// removes a finished resident once this succeeds.
///
/// An owner that dies - killed, crashed, out of memory, or taken down with
/// its provider - writes no receipt, and discovery deletes its record once it
/// has gone. Exact cleanup then has nothing to adopt, retire or confirm, and
/// refused every Close of the pane again, so the pane could never be closed.
///
/// An open agent that still runs as this conversation ends with it, as it
/// would have with a receipt: a person closing the pane closes the agent.
fn settle_lost_execution(
    host: &SharedHost,
    session: &Session,
    proof: &HostSessionProof,
) -> Result<(), String> {
    refuse_scheduled_agent(host, session)?;
    if let Some(holds) = what_holds(host, session, proof)? {
        return Err(lost_execution_refusal(session, holds));
    }
    let _ = return_listener_allocation(session);
    let Ok(registry) = super::agent_execution::registry(host) else {
        return Ok(());
    };
    let sources = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_sources(session.id)?;
    for source in sources {
        let crate::agent_execution::HostedSession::Chat(source) = source else {
            return Err("Retained execution ownership changed to another surface".into());
        };
        // Every descriptor of this allocation, under older conversation
        // metadata too: the evidence covers the allocation, not one descriptor.
        super::agent_execution::retired(host, &source)?;
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retire_agent_process_authority(
                &crate::agent_execution::HostedSession::Chat(source),
                true,
            )?;
    }
    Ok(())
}

/// A scheduled agent is its automation's to end, as when a changed boot
/// leaves one behind, so a lost conversation it runs as is not settled here.
fn refuse_scheduled_agent(host: &SharedHost, session: &Session) -> Result<(), String> {
    if let Ok(registry) = super::agent_execution::registry(host)
        && registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .agent_for_session(session.id)?
            .is_some_and(|agent| {
                matches!(
                    agent.origin,
                    crate::agent_execution::Origin::Automation { .. }
                )
            })
    {
        return Err(SCHEDULED_AGENT.into());
    }
    Ok(())
}

/// Gives back the listener allocation of a Codex conversation whose provider
/// has ended, as its holder does when it releases. A holder that died never
/// will, and its directory would outlive the conversation. An endpoint that
/// something still serves keeps it.
fn return_listener_allocation(session: &Session) -> Result<(), String> {
    let Some(listener) = session
        .owned_codex_listener
        .as_ref()
        .filter(|_| session.owns_codex_listener())
    else {
        return Ok(());
    };
    if owned_listener::served(&listener.socket).map_err(|error| error.to_string())? {
        return Ok(());
    }
    if let Some(parent) = listener.socket.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// What still holds a conversation whose native owner ended without its
/// close receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holds {
    /// A start, handoff or exact close is under way for it.
    InProgress,
    /// Its agent's launch, or that launch's teardown, never confirmed how it
    /// ended.
    LaunchUnconfirmed,
    /// A process of its provider survives the helper that controlled it.
    /// [`lost_processes`] says which, and can end them.
    Running,
    /// Nothing proves whether something of it still runs.
    Unproved,
}

/// Why a Close of a conversation whose owner is gone was refused, and what
/// the person can do about it. Both a pane's Close and `cagents agent close`
/// show it.
fn lost_execution_refusal(session: &Session, holds: Holds) -> String {
    let provider = session.launch.provider.label();
    match holds {
        Holds::InProgress => "This conversation is still starting, moving or closing elsewhere in Cagents. Close it again once that finishes.".into(),
        Holds::LaunchUnconfirmed => format!(
            "{LOST_EXECUTION_UNPROVED}. Its agent's launch never confirmed whether {provider} started or stopped, so it and its agent were kept rather than leave {provider} running unseen."
        ),
        Holds::Running => format!(
            "{provider} is still running for this conversation, but the Cagents helper that controlled it is gone, so the conversation was kept rather than leave {provider} running unseen."
        ),
        Holds::Unproved => format!(
            "{LOST_EXECUTION_UNPROVED}. Its Cagents helper is gone, and Cagents can't tell whether its {provider} process stopped, so it was kept rather than leave {provider} running unseen. After the computer restarts nothing of it can run, so closing it will work."
        ),
    }
}

/// The refusal for a conversation whose native owner's helper has died while
/// its provider runs on, if that is what happened to `owner`.
///
/// Nothing can ask that provider to stop any more: Release reaches only a
/// closed channel, and settling would leave it running unseen. Close says so,
/// rather than report the dead channel's I/O error, and [`lost_processes`]
/// can end what is left.
fn holder_lost_refusal(
    source: &Session,
    owner: &crate::daemon::holder_registry::Record,
) -> Result<Option<String>, String> {
    if owner.holder.is_current()
        || crate::chat::runtime::retained_actor_abandoned(&owner.holder, &owner.root)?
    {
        return Ok(None);
    }
    Ok(Some(lost_execution_refusal(source, Holds::Running)))
}

/// [`holder_lost_refusal`] for any owner of this boot registered for
/// `source`: its Codex holder, or a retained Claude helper.
///
/// With no live owner registered, the provider may still run after its
/// helper's record went: the evidence [`what_holds`] reads, such as a Codex
/// process still naming its listener, says so in the same words.
fn registered_holder_lost_refusal(
    host: &SharedHost,
    source: &Session,
) -> Result<Option<String>, String> {
    let owners = lost_owners(host, source)?;
    for owner in &owners {
        if let Some(refusal) = holder_lost_refusal(source, owner)? {
            return Ok(Some(refusal));
        }
    }
    if owners.iter().any(|owner| owner.holder.is_current()) {
        return Ok(None);
    }
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    Ok((what_holds(host, source, &proof)? == Some(Holds::Running))
        .then(|| lost_execution_refusal(source, Holds::Running)))
}

/// Every owner of this boot registered for `session`: its native Codex
/// holder's discovery record and its retained Claude helpers.
fn lost_owners(
    host: &SharedHost,
    session: &Session,
) -> Result<Vec<crate::daemon::holder_registry::Record>, String> {
    let boot = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .boot_id
        .clone();
    let mut owners: Vec<crate::daemon::holder_registry::Record> =
        registered_native_owner(session)?.into_iter().collect();
    if let Ok(registry) = super::agent_execution::registry(host) {
        owners.extend(
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .native_actor_owners_of(session.id)?
                .into_iter()
                .map(|(owner, _)| owner),
        );
    }
    owners.retain(|owner| owner.boot == boot);
    owners.dedup();
    Ok(owners)
}

fn registered_native_owner(
    source: &Session,
) -> Result<Option<crate::daemon::holder_registry::Record>, String> {
    use crate::daemon::holder_registry::{self as registry, Kind};
    let socket = registry::socket(&registry::directory(Kind::Native)?, source.id);
    registry::recovery_candidate(&socket, Kind::Native)
}

/// The processes still running for a conversation whose Cagents helper has
/// died, which is why its Close was refused, and with `end`, ends those
/// proved to be the conversation's first (see
/// [`crate::daemon::lost_processes`]). The person closes it again afterwards.
///
/// A conversation's processes are those in a dead helper's session or its
/// provider's process group, for every owner of this boot registered for it,
/// and for Codex the provider named by its listener directory with its group:
/// exactly what its Close counts. Only those of `end` are ended, the ones the
/// person was shown, so a process found later is shown before it is ended. A
/// conversation whose helper still runs has nothing here to end; its own
/// Close stops it.
pub(super) fn lost_processes(
    host: &SharedHost,
    session: &Session,
    end: &[crate::terminal::ipc::ProcessStart],
) -> Result<Vec<crate::terminal::ipc::LostProcess>, String> {
    // Ending holds the conversation as its Close does, so no start or handoff
    // can put a new provider where the lost one ran meanwhile.
    let _claim = if !end.is_empty() {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chat_handoffs.contains_key(&session.id)
            || state.native_actor_startups.values().any(|startup| {
                startup.source.id == session.id || startup.requested.id == session.id
            })
        {
            return Err(lost_execution_refusal(session, Holds::InProgress));
        }
        Some(
            capacity::Reservation::closing(host, &mut state, session)
                .map_err(|_| lost_execution_refusal(session, Holds::InProgress))?,
        )
    } else {
        None
    };
    let owners = lost_owners(host, session)?;
    if owners.iter().any(|owner| owner.holder.is_current()) {
        return Err(HELPER_STILL_RUNS.into());
    }
    let listener = session
        .owned_codex_listener
        .as_ref()
        .filter(|_| session.owns_codex_listener());
    let census = || -> Result<Vec<crate::daemon::lost_processes::Survivor>, String> {
        let mut found = Vec::new();
        for owner in &owners {
            found.extend(crate::daemon::lost_processes::census(
                &owner.holder,
                &owner.root,
            )?);
        }
        if let Some(listener) = &listener {
            let providers =
                super::upgrade_inspect::codex_listener_providers(session.id, &listener.socket)?;
            if !providers.is_empty() {
                found.extend(crate::daemon::lost_processes::listener_census(&providers)?);
            }
        }
        // One process can be in an owner's session and name the listener;
        // it is proved if either proves it.
        found.sort_by_key(|survivor| (survivor.pid, !survivor.proved));
        found.dedup_by_key(|survivor| survivor.pid);
        Ok(found)
    };
    let found = if end.is_empty() {
        census()?
    } else {
        let chosen = end
            .iter()
            .map(|process| (process.pid, process.start))
            .collect::<Vec<_>>();
        crate::daemon::lost_processes::end(census, &chosen)?
    };
    Ok(found
        .into_iter()
        // One that has exited is collected in a moment and needs nothing.
        .filter(|survivor| !survivor.zombie)
        .map(|survivor| crate::terminal::ipc::LostProcess {
            process: crate::terminal::ipc::ProcessStart {
                pid: survivor.pid,
                start: survivor.start,
            },
            name: survivor.name,
            command: survivor.command,
            proved: survivor.proved,
        })
        .collect())
}

/// The processes a person ending what `found` lists would choose: every one
/// proved to be the conversation's, as the pane's action does.
#[cfg(test)]
fn shown(found: &[crate::terminal::ipc::LostProcess]) -> Vec<crate::terminal::ipc::ProcessStart> {
    found
        .iter()
        .filter(|process| process.proved)
        .map(|process| process.process)
        .collect()
}

/// Why [`lost_processes`] ends nothing for a conversation whose helper runs.
const HELPER_STILL_RUNS: &str =
    "This conversation's Cagents helper is still running, so closing it stops it as usual.";

/// The start of what a Close says when nothing proves a conversation whose
/// owner is gone stopped.
pub(super) const LOST_EXECUTION_UNPROVED: &str =
    "Cagents couldn't confirm this conversation stopped";

/// What a Close says of a scheduled agent's conversation whose owner is gone.
pub(super) const SCHEDULED_AGENT: &str =
    "A scheduled automation's agent runs this conversation, and only its automation can end it";

/// What still holds a conversation whose native owner ended without its
/// close receipt, if anything. Its Close settles without one only when
/// nothing does.
///
/// The owner is lost only when no start or handoff claims the conversation,
/// no unconfirmed agent launch or startup cleanup is pending for it, and
/// nothing of it can still run by the evidence an upgrade requires before
/// replacing its service: no live
/// holder record, and for Codex no process naming the conversation's
/// listener and nothing bound or served where it listens, or for a retained
/// Claude actor every registered owner gone with its process group and
/// session. A changed kernel boot proves the same. Anything else, including
/// an unavailable agent registry, is not proof of the loss.
fn what_holds(
    host: &SharedHost,
    session: &Session,
    proof: &HostSessionProof,
) -> Result<Option<Holds>, String> {
    // A receipt exact cleanup did not consume names another descriptor of
    // the terminal. It confirms that one's close, and says nothing either way
    // about this one, so it neither holds nor settles this Close: the evidence
    // below does.
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if codex_terminal_owner(&state, session).is_some()
            || state.chat_handoffs.contains_key(&session.id)
            || state.native_actor_startups.values().any(|startup| {
                startup.source.id == session.id || startup.requested.id == session.id
            })
        {
            return Ok(Some(Holds::InProgress));
        }
    }
    if has_native_owner(session).map_err(|error| error.message)? {
        return Ok(Some(Holds::InProgress));
    }
    let (agent, owners) = {
        let registry = super::agent_execution::registry(host)?;
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        if registry.has_agent_launch_for_session(session.id)? {
            return Ok(Some(Holds::LaunchUnconfirmed));
        }
        (
            registry.agent_for_session(session.id)?,
            registry.native_actor_owners_of(session.id),
        )
    };
    // A startup cleanup owner retained for the agent is a launch of this boot
    // whose teardown is unconfirmed.
    if agent.is_some_and(|agent| {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .execution_startups
            .contains_key(&agent.id)
    }) {
        return Ok(Some(Holds::LaunchUnconfirmed));
    }
    use super::upgrade_inspect::ExecutionEnd;
    match super::upgrade_inspect::execution_end(session, &proof.boot_id, owners.clone()) {
        ExecutionEnd::Ended => {}
        ExecutionEnd::Running => return Ok(Some(Holds::Running)),
        ExecutionEnd::Unproved => return Ok(Some(Holds::Unproved)),
    }
    match session
        .owned_codex_listener
        .as_ref()
        .filter(|_| session.owns_codex_listener())
    {
        Some(listener)
            if owned_listener::served(&listener.socket).map_err(|error| error.to_string())? =>
        {
            Ok(Some(Holds::Running))
        }
        _ => Ok(None),
    }
}

/// A recorded terminal conversation is writer evidence even without a pane
/// marker (for example, a CLI resumed against the shared Codex daemon).
fn codex_terminal_owner(state: &HostState, session: &Session) -> Option<TerminalId> {
    let id = session.launch.session_id.as_ref()?;
    state.terminals.iter().find_map(|(terminal_id, record)| {
        if record.handle.is_completed() {
            return None;
        }
        let observed = record
            .session
            .resume_hint
            .as_deref()
            .and_then(crate::model::agent_recovery::observed_launch_from_hint);
        let recipe = observed.as_ref().unwrap_or(&record.session.launch);
        ((Provider::from_program(&recipe.program) == Some(Provider::Codex)
            || Provider::recognize_launch(recipe) == Some(Provider::Codex))
            && recipe
                .arguments
                .iter()
                .any(|arg| arg == id || arg.split_once('=').is_some_and(|(_, value)| value == id)))
        .then_some(*terminal_id)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedCodexService {
    Missing,
    Stopped,
    Listening,
    Unknown,
}

fn shared_codex_failure(failure: chat::protocol::SetupError) -> ProtocolError {
    match failure {
        chat::protocol::SetupError::Invalid(message) => error(message),
        chat::protocol::SetupError::Unavailable(message) => {
            ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, true)
        }
    }
}

/// Probe the exact shared transport both preparation and fallback would use.
/// The adapter owns any verified short alias until the probe has completed.
/// Only an absent entry or ConnectionRefused proves this endpoint unserved;
/// unreadable/replaced evidence and every other connection failure stay unknown.
fn shared_codex_service(
    launch: &chat::Launch,
    before_probe: impl FnOnce() -> Result<(), ProtocolError>,
) -> Result<SharedCodexService, ProtocolError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Some(shared) = chat::protocol::codex::shared_daemon_socket(launch) else {
        return Ok(SharedCodexService::Unknown);
    };
    let endpoint = match std::fs::metadata(&shared) {
        Ok(endpoint) if endpoint.file_type().is_socket() => endpoint,
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SharedCodexService::Missing);
        }
        _ => return Ok(SharedCodexService::Unknown),
    };
    before_probe()?;
    let (spec, _adapter) = match chat::protocol::codex::create_for_start(launch) {
        Ok(created) => created,
        Err(failure) => {
            if !std::fs::metadata(&shared).is_ok_and(|now| {
                now.file_type().is_socket()
                    && now.dev() == endpoint.dev()
                    && now.ino() == endpoint.ino()
            }) {
                return Ok(SharedCodexService::Unknown);
            }
            return Err(shared_codex_failure(failure));
        }
    };
    let service = match &spec.transport {
        chat::protocol::TransportSpec::UnixWebSocket {
            socket,
            device,
            inode,
        } if *device == endpoint.dev() && *inode == endpoint.ino() => {
            match crate::agent::observe::socket::connect(socket, *device, *inode) {
                Ok(_) => SharedCodexService::Listening,
                Err(failure) if failure.not_listening => SharedCodexService::Stopped,
                Err(_) => SharedCodexService::Unknown,
            }
        }
        _ => SharedCodexService::Unknown,
    };
    // Refusal is not a post-connect identity proof. In particular, replacing
    // the published endpoint before or during the probe must not turn another
    // socket's ConnectionRefused into authority to start a private server.
    Ok(
        if std::fs::metadata(&shared).is_ok_and(|now| {
            now.file_type().is_socket()
                && now.dev() == endpoint.dev()
                && now.ino() == endpoint.ino()
        }) {
            service
        } else {
            SharedCodexService::Unknown
        },
    )
}

/// A proofless history selection can join the account's live shared daemon
/// without retiring an old pane. This grants no replacement-writer authority:
/// Start receives the unchanged bare descriptor and rechecks the endpoint.
fn prepare_shared_codex_join(host: &SharedHost, source: &Session) -> Result<bool, ProtocolError> {
    if source.launch.provider != Provider::Codex
        || source.proof.is_some()
        || source.terminal.is_some()
        || source.owned_codex_listener.is_some()
        || source.native_continuity != chat::NativeContinuity::ReconnectOnly
        || source.launch.session_id.is_none()
    {
        return Ok(false);
    }
    let process_launch = prepare_native_process_launch(
        &source.launch,
        Some(crate::model::agent_recovery::fresh_native_environment(
            Provider::Codex,
            false,
        )),
        &BTreeMap::new(),
        None,
    )?;
    Ok(shared_codex_service(&process_launch, || {
        // Preserve Start's recorded-selector, effective account and directory
        // checks before constructing/probing a potential shared transport.
        super::account_lifecycle::ensure_launch(
            host,
            &source.launch.recipe.working_directory,
            &source.launch.recipe.environment,
            &Default::default(),
            true,
        )?;
        super::account_lifecycle::ensure_launch_at_directory(
            host,
            &process_launch.recipe.working_directory,
            &process_launch.recipe.environment,
            &process_launch.recipe.unset_environment,
            false,
            None,
        )?;
        super::account_lifecycle::ensure_launch(
            host,
            &process_launch.recipe.working_directory,
            &process_launch.recipe.environment,
            &process_launch.recipe.unset_environment,
            false,
        )
    })? == SharedCodexService::Listening)
}

/// Read the same execution evidence for restart and picker preparation, so
/// unreadable evidence retains its concrete diagnostic in both paths.
fn require_ended_codex_execution(
    host: &SharedHost,
    session: &Session,
    boot: &crate::model::BootId,
) -> std::io::Result<()> {
    let launch = prepare_native_process_launch(
        &session.launch,
        Some(crate::model::agent_recovery::fresh_native_environment(
            Provider::Codex,
            false,
        )),
        &BTreeMap::new(),
        None,
    )
    .map_err(|failure| std::io::Error::other(failure.message))?;
    require_ended_codex_execution_for_launch(host, session, boot, &launch)
}

/// The process launch may add the effective account home without changing the
/// saved descriptor whose old execution must be proved ended.
fn require_ended_codex_execution_for_launch(
    host: &SharedHost,
    session: &Session,
    boot: &crate::model::BootId,
    launch: &chat::Launch,
) -> std::io::Result<()> {
    use super::upgrade_inspect::{CodexExecution, codex_execution};
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(id) = codex_terminal_owner(&state, session) {
            return Err(std::io::Error::other(format!(
                "Codex is still running this conversation in terminal {id}. Close that terminal, then try again."
            )));
        }
        // Holders only publish records when recovery was enabled. Older and
        // in-process consoles can lack them, so a live paired console remains
        // unproved even if its CLI has not recorded a conversation ID yet.
        // Check current state rather than the saved descriptor: once its
        // terminal is gone, a clean kernel/holder scan can prove it ended.
        if session.terminal.as_ref().is_some_and(|terminal| {
            state
                .terminals
                .get(&terminal.id)
                .is_some_and(|record| !record.handle.is_completed())
        }) {
            return Err(std::io::Error::other(
                "The paired Codex console is still live; its conversation could not be checked. Close that terminal, then try again.",
            ));
        }
    }
    let execution = match codex_execution(session, boot) {
        CodexExecution::Ended => match launch.session_id.as_deref() {
            Some(thread) => match codex_owners::writer(
                chat::protocol::codex::thread_writer_lock(launch, thread).as_deref(),
                thread,
            ) {
                Ok(Some(writer)) => {
                    return Err(std::io::Error::other(running_codex_process(writer.pid)));
                }
                Ok(None) => CodexExecution::Ended,
                Err(reason) => {
                    return Err(std::io::Error::new(
                        reason.kind(),
                        unproved_codex_execution(&reason.to_string()),
                    ));
                }
            },
            None => CodexExecution::Ended,
        },
        execution => execution,
    };
    let message = match execution {
        CodexExecution::Ended => return Ok(()),
        CodexExecution::Running { pid: Some(pid), .. } => running_codex_process(pid),
        CodexExecution::Running { pid: None, socket } => format!(
            "Codex is still serving this conversation at {}. Stop that server, then try again.",
            socket.display()
        ),
        CodexExecution::RetainedProcesses =>
            "The previous Codex execution still has live processes. Wait for them to finish, then try again.".into(),
        CodexExecution::Unproved { reason }
            if reason == super::upgrade_inspect::CODEX_PROCESS_STARTING =>
        {
            "A process was still starting; try again in a moment.".into()
        }
        CodexExecution::Unproved { reason } => unproved_codex_execution(&reason),
    };
    Err(std::io::Error::other(message))
}

fn unproved_codex_execution(reason: &str) -> String {
    format!(
        "The old Codex execution could not be checked: {reason}. This conversation was not restarted. Restore access to this evidence and try again."
    )
}

fn running_codex_process(pid: impl std::fmt::Display) -> String {
    format!(
        "Codex is still running this conversation (process {pid}). Stop process {pid}, then try again."
    )
}

/// Admit a replacement writer using the original descriptor, then retire only
/// ended discovery under the caller's allocation reservation.
fn prepare_codex_restart(
    host: &SharedHost,
    session: &Session,
    boot: &crate::model::BootId,
    launch: &chat::Launch,
) -> std::io::Result<()> {
    use crate::daemon::holder_registry::{self as registry, Kind};
    use std::os::unix::fs::MetadataExt;
    if session.launch.session_id.is_none() {
        return Ok(());
    }
    require_ended_codex_execution_for_launch(host, session, boot, launch)?;
    let directory = registry::directory(Kind::Native).map_err(std::io::Error::other)?;
    let control = registry::socket(&directory, session.id);
    let record = registry::execution_record(&control)?;
    if let Some(record) = &record
        && record.boot == *boot
        && !chat::runtime::codex_execution_abandoned(&record.holder, &record.root)
            .map_err(std::io::Error::other)?
    {
        return Err(std::io::Error::other(
            "A Codex holder appeared during restart; try again",
        ));
    }
    // A crashed holder may leave its control socket with or without a
    // record. Retire the stopped inode before its successor binds it.
    let entry = match std::fs::symlink_metadata(&control) {
        Ok(entry) => Some(entry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(entry) = entry {
        // Holder paths can exceed sun_path; use the same pinned directory
        // address as their bind, with a nonblocking, inode-checked probe.
        let stopped = registry::with_socket_address(&control, |address| {
            match crate::agent::observe::socket::connect(address, entry.dev(), entry.ino()) {
                Ok(_) => Ok(false),
                Err(error) if error.not_listening => Ok(true),
                Err(error) => Err(std::io::Error::other(format!(
                    "Cannot inspect holder control socket: {}",
                    String::from(error)
                ))),
            }
        })?;
        if !stopped {
            return Err(std::io::Error::other(
                "A Codex holder is still serving this pane; try again",
            ));
        }
        if std::fs::symlink_metadata(&control)
            .is_ok_and(|current| current.dev() == entry.dev() && current.ino() == entry.ino())
        {
            std::fs::remove_file(&control)?;
        }
    }
    if let Some(record) = record {
        record.remove_record(&directory)?;
    }
    Ok(())
}

pub(super) fn require_unreserved_terminals(
    state: &HostState,
    session: &Session,
) -> Result<(), ProtocolError> {
    state.ensure_retention_available(&[session.id])?;
    if let Some(terminal) = &session.terminal {
        state.ensure_retention_available(&[terminal.id])?;
    }
    // Failed retirement remains readable/retryable, but it is not permission
    // to install a new native writer over unconfirmed original cleanup.
    for id in std::iter::once(session.id).chain(session.terminal.as_ref().map(|source| source.id)) {
        if state.terminal_retirements.get(&id).is_some_and(|entry| {
            state
                .terminals
                .get(&id)
                .is_some_and(|record| record.handle.same_worker(&entry.handle))
        }) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "Terminal retirement remains unconfirmed; explicitly retry its close before starting a native writer",
                true,
            ));
        }
    }
    Ok(())
}

/// Starts admitted before explicit close reserved its probe may still own an
/// unpublished actor. Keep the original terminal until that claim settles.
pub(super) fn terminal_claim_pending(state: &HostState, terminal: TerminalId) -> bool {
    let claims = |session: &Session| {
        session.id == terminal
            || session
                .terminal
                .as_ref()
                .is_some_and(|source| source.id == terminal)
    };
    state.chat_spawning.values().any(claims)
        || state
            .native_actor_startups
            .values()
            .any(|startup| claims(&startup.source) || claims(&startup.requested))
        || state
            .native_closing
            .values()
            .any(|closing| claims(closing.source()))
        || state.chat_handoffs.contains_key(&terminal)
}

pub(super) fn shared_removal_in_transition(
    state: &HostState,
    before: &crate::workspace_model::views::Pane,
) -> bool {
    let crate::model::CardKind::Chat(source) = &before.kind else {
        unreachable!("chat removal source")
    };
    let claims = |session: &crate::chat::Session| {
        session.id == source.id
            || session
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.id == source.id)
    };
    let same = |session: &crate::chat::Session| {
        crate::workspace_model::views::same_removal_source(
            before,
            &crate::workspace_model::views::Pane {
                kind: crate::model::CardKind::Chat(session.clone()),
                ..before.clone()
            },
        )
    };
    state.chat_bindings.contains_key(&source.id)
        || state.chat_handoffs.contains_key(&source.id)
        || state
            .chat_spawning
            .values()
            .filter(|session| claims(session))
            .any(|session| {
                state.chat_close_reservations.get(&session.id) != Some(session) || !same(session)
            })
        || state
            .native_actor_startups
            .values()
            .any(|startup| claims(&startup.source) || claims(&startup.requested))
        || state
            .native_closing
            .values()
            .any(|closing| claims(closing.source()) && !same(closing.source()))
}

/// Why a native start that built its provider may no longer publish it.
///
/// Shutdown, a reserved terminal and a new daemon proof are transient, and a
/// chat that gained a live owner can be attached instead, as at entry: all are
/// retryable. Only a lost reservation means another action took this start
/// over, which repeating it cannot undo.
fn publication_refusal(
    state: &HostState,
    session: &Session,
    proof: &HostSessionProof,
    reservation_id: TerminalId,
    reservation: &Session,
) -> Option<ProtocolError> {
    const MESSAGE: &str = "Native startup ownership changed before attachment publication";
    let transient = state.shutting_down
        || require_unreserved_terminals(state, session).is_err()
        || state.proof() != proof
        || state
            .chats
            .get(&session.id)
            .is_some_and(|current| current.handle.owns_work());
    if transient {
        Some(retryable_unavailable(MESSAGE))
    } else if state.chat_spawning.get(&reservation_id) != Some(reservation) {
        Some(unavailable(MESSAGE))
    } else {
        None
    }
}

fn start_with_execution(
    host: &SharedHost,
    client: ClientId,
    mut session: Session,
    source: Option<HostSessionProof>,
    terminate_source: bool,
    selection: Option<PendingBinding>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
    execution: Option<Arc<chat::protocol::ExecutionContext>>,
    restart_launch: Option<&chat::Launch>,
    pane: Option<crate::terminal_ipc::PaneChatStart>,
) -> Result<ServerResponse, ProtocolError> {
    // Reading the floor can fail transiently. The client routes an ordinary
    // start of a fenced conversation through its update resume, so a start
    // that still reaches the fence cannot pass it by repeating itself. Name no
    // action here: the pane shows the recovery it actually offers.
    if crate::upgrade::floor::load()
        .map_err(retryable_unavailable)?
        .blocks_id(session.id)
    {
        return Err(unavailable(
            "This conversation was stopped for the update, and this start cannot continue it. Its saved files are kept; nothing was restarted.",
        ));
    }
    // A helper may have committed installation before host publication failed.
    // Recover its durable outbox through authenticated adoption before asking
    // an unexposed startup guard to retire it without a semantic consumer.
    let pending = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        require_unreserved_terminals(&state, &session)?;
        state
            .native_actor_startups
            .get(&session.id)
            .filter(|pending| {
                (pending.source == session || pending.requested == session)
                    && !state.chats.contains_key(&session.id)
                    && !state.chat_spawning.contains_key(&session.id)
            })
            .cloned()
    };
    if let Some(pending) = pending
        && has_native_owner(&pending.source)?
    {
        let response = native_recovery::restore_for_start(host, client, &pending.source)?;
        pending.context.installed();
        retained_actor::forget_startup(host, &pending.source, &pending.context);
        return Ok(response);
    }
    // A failed pre-discovery helper still owns its exact cleanup context.
    retained_actor::reconcile_startup_for_start(host, &session).map_err(setup_error)?;
    // A named Claude Start in place of an owner an earlier boot left runs under
    // the account home that owner used.
    if session.launch.provider == Provider::Claude
        && session.launch.session_id.is_some()
        && !session.native_continuity.retains_provider()
        && super::agent_execution::registry(host).is_ok()
    {
        retained_resume::ensure_prior_boot_account_for_start(host, &session)
            .map_err(setup_error)?;
    }
    // Reserve before provider/account initialization. Pending owners recovered above
    // use their own exact reservation through restore.
    let mut admission = capacity::Reservation::new_with_source(host, &session, source.as_ref())?;
    if pane.is_none() {
        pane_start::validate_known_allocation(host, &session)?;
    }
    let _pane_start = pane
        .as_ref()
        .map(|pane| pane_start::Reservation::new(host, pane, &session))
        .transpose()?;
    let reservation_id = session.id;
    super::account_lifecycle::ensure_launch(
        host,
        &session.launch.recipe.working_directory,
        &session.launch.recipe.environment,
        &crate::account::launch_credential_overrides(&session.launch.recipe),
        true,
    )?;
    // Main has no immutable captured-home authority for legacy/native
    // reattachment. Refuse that path during mutation rather than infer it.
    let already_hosted = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .contains_key(&session.id);
    if already_hosted
        || source.is_some()
        || session.terminal.is_some()
        || !matches!(
            session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        )
    {
        super::account_lifecycle::ensure_idle(host)?;
    }
    // A managed CLI is a second view of the thread that this daemon owns.
    // Reattach that proven pair before trying to infer anything from argv or
    // the account-wide list of conversations.
    if source.is_some() || session.owns_codex_listener() {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(proof) = source.as_ref() {
            validate_proof(Some(proof), state.proof())?;
        }
        let paired = state.chats.get(&session.id).is_some_and(|record| {
            record.handle.owns_work()
                && session.launch.provider == Provider::Codex
                && record.session.launch.provider == Provider::Codex
                && record.handle.session_id() == session.launch.session_id
                && record.session.terminal.as_ref().is_some_and(|terminal| {
                    session.terminal.as_ref().is_some_and(|source| {
                        terminal.id == source.id && terminal.launch == source.launch
                    }) && state
                        .terminals
                        .get(&terminal.id)
                        .is_some_and(|live| live.session.launch == terminal.launch)
                })
        });
        if paired {
            if source.is_none() {
                validate_proof(session.proof.as_ref(), state.proof())?;
            }
            let record = state
                .chats
                .get_mut(&session.id)
                .expect("checked managed pair");
            record.subscribers.entry(client).or_insert(false);
            record
                .observations
                .entry(client)
                .or_insert_with(|| Observation {
                    after_generation: record.handle.observe_next_update(),
                    ..Observation::default()
                });
            let attachment = (
                record.session.clone(),
                record.handle.clone(),
                record.execution_instance,
            );
            drop(state);
            return current_session_snapshot(host, attachment.0, attachment.1, attachment.2);
        }
    }
    if !execution_environment.0.is_empty() && (source.is_some() || session.terminal.is_some()) {
        return Err(error(
            "A terminal handoff must use the proven source environment, not a client override",
        ));
    }
    if session.native_continuity.retains_provider() || has_native_owner(&session)? {
        return native_recovery::restore_with_admission_for_start(
            host, client, &session, admission,
        );
    }
    let mut source_process = None;
    let mut resolved_restart_recipe = None;
    let mut source_fresh = None;
    let mut source_environment = BTreeMap::new();
    let mut source_transient = None;
    let requested_source = session.clone();
    let mut source_execution = None;
    let mut selection_authorized = selection.is_some();
    if let Some(proof) = source.as_ref() {
        let handle = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            validate_proof(Some(proof), state.proof())?;
            if let Some(record) = state.chats.get(&session.id) {
                selection_authorized |= record.handle.owns_work()
                    && record.session.launch == session.launch
                    && record
                        .session
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| terminal.id == session.id);
            }
            let record = state
                .terminals
                .get(&session.id)
                .ok_or_else(|| error("The proven source terminal no longer exists"))?;
            if record.handle.is_completed() {
                return Err(retryable_unavailable(
                    "The source agent has exited; resume its exact conversation in the terminal first",
                ));
            }
            source_environment = record.session.launch.environment.clone();
            source_transient = record.execution_environment.clone();
            record.handle.clone()
        };
        let (process, recipe, id, hint, fresh, execution) = resolve_source(
            &handle,
            session.launch.provider,
            if session.launch.provider == Provider::Codex {
                session.launch.session_id.as_deref()
            } else {
                None
            },
            selection_authorized,
            &source_environment,
            source_transient.as_ref().map(|environment| &environment.0),
        )?;
        source_execution = execution;
        if let Some(binding) = &selection {
            if !binding.authorizes(&process, &id, Instant::now()) {
                return Err(error(
                    "Conversation preview expired or its source changed; preview the intended conversation again",
                ));
            }
            let (title, cwd) = crate::agent::observe::codex_preview(&process.recovery, &id)
                .map_err(retryable_unavailable)?;
            if cwd != binding.preview.working_directory || title != binding.preview.title {
                return Err(error(
                    "Conversation metadata changed; review a fresh preview before attaching",
                ));
            }
        }
        source_process = Some(process);
        if let Some(restart) = restart_launch {
            if fresh.is_some()
                || restart.provider != session.launch.provider
                || restart.session_id.as_deref() != Some(id.as_str())
                || restart.terminal_recipe().as_ref() != Some(&recipe)
            {
                return Err(error(
                    "Restored source differs from its exact confirmed restart recipe",
                ));
            }
            resolved_restart_recipe = Some(recipe);
        } else {
            session.launch.recipe = recipe;
            session.launch.session_id = fresh.is_none().then_some(id);
        }
        source_fresh = fresh;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(Some(proof), state.proof())?;
        let record = state
            .terminals
            .get_mut(&session.id)
            .ok_or_else(|| error("The proven source terminal no longer exists"))?;
        record.session.resume_hint = hint;
    }
    // A Codex TUI remains live while its native view is detached. Switching
    // that proven source back is an attachment, never a second daemon writer.
    if source.is_some() && !terminate_source && session.launch.provider == Provider::Codex {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err(retryable_unavailable("Daemon is shutting down"));
        }
        validate_proof(source.as_ref(), state.proof())?;
        if let Some(record) = state.chats.get_mut(&session.id)
            && record.handle.owns_work()
        {
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || record.handle.session_id() != session.launch.session_id
                || record.session.terminal.as_ref().map(|terminal| terminal.id) != Some(session.id)
            {
                return Err(error(
                    "Retained chat does not match this exact Codex source",
                ));
            }
            if let std::collections::btree_map::Entry::Vacant(entry) =
                record.subscribers.entry(client)
            {
                entry.insert(false);
                record.observations.insert(
                    client,
                    Observation {
                        after_generation: record.handle.observe_next_update(),
                        ..Observation::default()
                    },
                );
            }
            let attachment = (
                record.session.clone(),
                record.handle.clone(),
                record.execution_instance,
            );
            drop(state);
            return current_session_snapshot(host, attachment.0, attachment.1, attachment.2);
        }
    }
    // All adapter validation happens before a source terminal can be changed.
    // Claude reads the selected home's transcript while building its adapter.
    // Classify a removed account before that read can mask it as missing
    // history. Codex also supports a live socket attachment, whose account is
    // deliberately not reauthorized; its transport is checked below instead.
    let saved_history_only = session.launch.needs_read_only_history();
    if session.launch.provider != Provider::Codex && !saved_history_only {
        check_launch_account(&session.launch)?;
    }
    let (allocator, listeners) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (state.proof().clone(), state.codex_listeners.clone())
    };
    let owned = source.is_none() && listeners.claim(&mut session, &allocator);
    let owned_socket = owned.then(|| {
        session
            .owned_codex_listener
            .as_ref()
            .unwrap()
            .socket
            .clone()
    });
    // An authenticated checkpoint may need different resume argv from the
    // immutable recipe issued to windows. Keep that execution detail out of
    // the descriptor, authority registry and runtime's installed identity.
    if let Some(restart) = restart_launch
        && (session.launch.provider != restart.provider
            || session.launch.session_id != restart.session_id
            || session
                .launch
                .terminal_recipe()
                .is_none_or(|recipe| Some(recipe) != restart.terminal_recipe()))
    {
        return Err(error(
            "Native restart launch differs from its issued conversation",
        ));
    }
    let recorded_process_launch = restart_launch.unwrap_or(&session.launch);
    // Retain the original recorded-selector admission independently of the
    // private environment used by constructors and the eventual process.
    super::account_lifecycle::ensure_launch(
        host,
        &recorded_process_launch.recipe.working_directory,
        &recorded_process_launch.recipe.environment,
        &Default::default(),
        true,
    )?;
    // History/title/account-root construction must see the same effective
    // environment as the process. Keep this launch private: neither transient
    // values nor normalized restart arguments rewrite the issued descriptor.
    // Saved read-only history receives recorded removals, never fresh defaults
    // or transient execution credentials.
    let empty_execution_environment = BTreeMap::new();
    let mut process_launch = prepare_native_process_launch(
        recorded_process_launch,
        (!saved_history_only && source.is_none()).then(|| {
            crate::model::agent_recovery::fresh_native_environment(session.launch.provider, false)
        }),
        if saved_history_only {
            &empty_execution_environment
        } else {
            &execution_environment.0
        },
        if saved_history_only {
            None
        } else {
            source_execution.as_ref()
        },
    )?;
    // A begun thread's bare remote joins the account's shared daemon, which
    // owns the thread while it runs. With nothing serving that daemon, the
    // thread is reopened from its history on a listener of its own instead of
    // failing to join a daemon that is absent.
    let owned_socket = match owned_socket {
        None if !owned
            && source.is_none()
            && restart_launch.is_none()
            && !saved_history_only
            && process_launch.provider == Provider::Codex
            && matches!(
                shared_codex_service(&process_launch, || Ok(()))?,
                SharedCodexService::Missing | SharedCodexService::Stopped
            )
            && listeners.claim_for_unserved_thread(&mut session, &allocator) =>
        {
            process_launch.recipe.arguments = session.launch.recipe.arguments.clone();
            let socket = session
                .owned_codex_listener
                .as_ref()
                .unwrap()
                .socket
                .clone();
            Some(socket)
        }
        owned_socket => owned_socket,
    };
    // Join a served listener. Before replacing a saved conversation's server,
    // prove its old execution ended and no outside process writes the thread.
    // Inspect the original descriptor before retiring any endpoint: assigning
    // a listener above grants no evidence about the execution that served it.
    let owned_socket = match owned_socket {
        Some(socket)
            if !owned_listener::served_before_retire(&socket, || {
                prepare_codex_restart(host, &requested_source, &allocator.boot_id, &process_launch)
            })
            .map_err(|failure| {
                ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    format!("Cannot restart Codex: {failure}"),
                    true,
                )
            })? =>
        {
            Some(socket)
        }
        _ => None,
    };
    if process_launch.provider != Provider::Codex && !saved_history_only {
        check_launch_account(&process_launch)?;
    }
    // This map is fully materialized. Match env_clear: absent selectors stay
    // absent, and constructor admission uses the same captured removals.
    super::account_lifecycle::ensure_launch_at_directory(
        host,
        &process_launch.recipe.working_directory,
        &process_launch.recipe.environment,
        &process_launch.recipe.unset_environment,
        false,
        execution
            .as_ref()
            .and_then(|context| context.directory.as_deref()),
    )?;
    super::account_lifecycle::ensure_launch(
        host,
        &process_launch.recipe.working_directory,
        &process_launch.recipe.environment,
        &process_launch.recipe.unset_environment,
        false,
    )?;
    let (mut spec, mut adapter) = if let Some(owned_socket) = owned_socket {
        chat::protocol::codex::create_with_listener_for_start(&process_launch, owned_socket)
    } else {
        chat::protocol::create_for_start(&process_launch)
    }
    .map_err(|message| {
        // Removal may have won after the early check while the adapter
        // was reading history. Do not hold the lifecycle lease across
        // those reads, but preserve the actionable account classification.
        check_launch_account(&session.launch)
            .err()
            .unwrap_or_else(|| setup_error(message))
    })?;
    let remote = retained_actor::enabled(session.launch.provider, &spec.transport);
    let execution = if remote && execution.is_none() {
        Some(Arc::new(
            chat::protocol::ExecutionContext::capture_for_start(&spec.working_directory)
                .map_err(setup_error)?,
        ))
    } else {
        execution
    };
    if matches!(spec.transport, chat::protocol::TransportSpec::History)
        && (source.is_some()
            || terminate_source
            || execution.is_some()
            || selection.is_some()
            || restart_launch.is_some())
    {
        return Err(error(
            "Read-only history cannot take over a provider or execute an agent run",
        ));
    }
    if !matches!(
        &spec.transport,
        chat::protocol::TransportSpec::History
            | chat::protocol::TransportSpec::UnixWebSocket { .. }
    ) {
        check_launch_account(&session.launch)?;
    }
    // Provider-specific process fields derive from the prepared launch.
    // No environment overlay may follow constructor history/title reads.
    spec.withhold_environment();
    spec.messages = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox_seal
        .as_ref()
        .map(|seal| Arc::new(seal.recognizer()));
    super::account_lifecycle::ensure_launch(
        host,
        &spec.working_directory,
        &spec.environment,
        &spec.unset_environment,
        false,
    )?;
    let mut initial = adapter.initialize_for_start().map_err(setup_error)?;
    let store = chat::runtime::preflight_for_start(&spec, &mut initial).map_err(setup_error)?;
    if terminate_source && source.is_none() {
        return Err(error("Source termination requires an exact host proof"));
    }
    let source_handle = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        require_unreserved_terminals(&state, &session)?;
        if state.shutting_down {
            return Err(retryable_unavailable("Daemon is shutting down"));
        }
        if let Some(receipt) = state.chat_handoffs.get(&session.id) {
            receipt.validate_source(&session)?;
            return Err(retryable_unavailable(
                "This chat already returned to terminal; reattach and complete that handoff instead of starting another writer",
            ));
        }
        let recovering = if let Some(record) = state.chats.get(&session.id) {
            if record.returning_terminal {
                if record.session.launch.provider != session.launch.provider
                    || record.session.launch.recipe != session.launch.recipe
                    || record.handle.session_id() != session.launch.session_id
                {
                    return Err(error(
                        "Explicit recovery must retain the exact closed chat identity",
                    ));
                }
                return Err(retryable_unavailable(
                    "Retry the pending terminal handoff or explicitly close it before starting another native writer",
                ));
            }
            if record.handle.owns_work() {
                if record.session.launch.provider != session.launch.provider
                    || record.session.launch.recipe != session.launch.recipe
                    || record.handle.session_id() != session.launch.session_id
                {
                    return Err(error(
                        "Explicit recovery must retain the exact closed chat identity",
                    ));
                }
                return Err(retryable_unavailable(
                    "Chat already owns work; attach instead of starting another writer",
                ));
            }
            validate_proof(session.proof.as_ref(), state.proof())?;
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || record.handle.session_id() != session.launch.session_id
            {
                return Err(error(
                    "Explicit recovery must retain the exact closed chat identity",
                ));
            }
            true
        } else {
            false
        };
        if !recovering && admission.is_none() {
            capacity::require_available(&state)?;
        }
        if (state.chat_spawning.contains_key(&session.id) && admission.is_none())
            || state.native_closing.contains_key(&session.id)
        {
            return Err(retryable_unavailable("Chat is already starting"));
        }
        for record in state.chats.values() {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.session_id();
            if same_conversation(&launch, &session.launch) && record.handle.owns_work() {
                return Err(retryable_unavailable(
                    "This exact conversation already has a daemon writer or unconfirmed backend work; attach to its existing chat",
                ));
            }
        }
        if state.chat_spawning.iter().any(|(id, pending)| {
            *id != session.id && same_conversation(&pending.launch, &session.launch)
        }) {
            return Err(retryable_unavailable(
                "This exact conversation is already starting",
            ));
        }
        if let Some(id) = &session.launch.session_id {
            for (&terminal_id, record) in &state.terminals {
                if record.handle.is_completed() || (source.is_some() && terminal_id == session.id) {
                    continue;
                }
                let hint = record
                    .session
                    .resume_hint
                    .as_deref()
                    .and_then(crate::model::agent_recovery::observed_launch_from_hint);
                let recipe = hint.as_ref().unwrap_or(&record.session.launch);
                if recipe.arguments.iter().any(|arg| {
                    arg == id || arg.split_once('=').is_some_and(|(_, value)| value == id)
                }) {
                    return Err(retryable_unavailable(format!(
                        "The exact conversation is already owned by CLI terminal {terminal_id}; convert that proven source rather than launching another writer"
                    )));
                }
            }
        }
        let source_handle = if let Some(source) = source.as_ref() {
            validate_proof(Some(source), state.proof())?;
            let record = state.terminals.get(&session.id).ok_or_else(|| {
                error("Exact source terminal does not exist on the proven daemon")
            })?;
            if session.launch.provider != Provider::Codex
                && !terminate_source
                && !record.handle.is_completed()
            {
                return Err(error(
                    "This provider requires stopping the CLI before exact native resume",
                ));
            }
            let handle = record.handle.clone();
            // Capture this worker's current daemon proof, not the raw launch
            // continuity retained before it was first attached.
            let current = state.descriptor(client, session.id)?.session;
            session.terminal = Some(if restart_launch.is_some() {
                restored_terminal_source(
                    session
                        .terminal
                        .as_ref()
                        .ok_or_else(|| error("Upgrade pair source is missing"))?,
                    &current,
                )
                .map_err(error)?
            } else {
                current
            });
            Some(handle)
        } else {
            if state.terminals.contains_key(&session.id) && !recovering {
                return Err(error(
                    "Existing source terminal requires exact host proof; it will not be replaced implicitly",
                ));
            }
            None
        };
        session.proof = Some(state.proof().clone());
        capacity::Reservation::install(&mut admission, host, &mut state, &session)?;
        state.inventory_generation += 1;
        source_handle
    };
    let result = (|| {
        let mut pending_fresh = None;
        if let Some(handle) = source_handle {
            let (current_process, current_recipe, _, _, current_fresh, current_execution) =
                resolve_source(
                    &handle,
                    session.launch.provider,
                    session.launch.session_id.as_deref(),
                    selection_authorized,
                    &source_environment,
                    source_transient.as_ref().map(|environment| &environment.0),
                )?;
            if source_process.as_ref() != Some(&current_process)
                || &current_recipe
                    != resolved_restart_recipe
                        .as_ref()
                        .unwrap_or(&session.launch.recipe)
                || current_fresh != source_fresh
                || current_execution != source_execution
            {
                return Err(error(
                    "The foreground conversation changed during handoff; no native writer was started",
                ));
            }
            if session.launch.provider != Provider::Codex && !handle.is_completed() {
                // Freeze the exact source family before the final identity check.
                // Dropping the guard on any error resumes it without losing work.
                let mut frozen = handle.prepare_upgrade().map_err(retryable_unavailable)?;
                if !frozen.only_agent_tree(current_process.pid) {
                    return Err(retryable_unavailable(
                        "This terminal also owns unrelated jobs. Finish or move those jobs before converting the agent.",
                    ));
                }
                let (frozen_process, frozen_recipe, _, _, frozen_fresh, frozen_execution) =
                    resolve_source(
                        &handle,
                        session.launch.provider,
                        session.launch.session_id.as_deref(),
                        false,
                        &source_environment,
                        source_transient.as_ref().map(|environment| &environment.0),
                    )?;
                if frozen_process != current_process
                    || frozen_recipe != current_recipe
                    || frozen_fresh != current_fresh
                    || frozen_execution != current_execution
                {
                    return Err(error(
                        "The source changed before it was frozen; no native writer was started",
                    ));
                }
                if source_fresh.is_some() {
                    // A fresh boundary has no existing conversation to resume.
                    // Keep its exact source frozen but reversible until the new
                    // actor proves the identity needed by the durable checkpoint.
                    pending_fresh = Some((handle.clone(), frozen));
                } else {
                    stop_source(&handle, &mut frozen)?;
                }
            }
        }
        if source_fresh.is_some() && pending_fresh.is_none() {
            return Err(unavailable(
                "The fresh source exited before it could be frozen; no replacement was started",
            ));
        }
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.shutting_down {
                return Err(retryable_unavailable(
                    "Daemon shutdown cancelled chat start; exact source/session evidence remains recoverable",
                ));
            }
        }
        spec.execution = execution.clone();
        if !matches!(spec.transport, chat::protocol::TransportSpec::History)
            && let Some(context) = &spec.execution
        {
            context
                .verify_directory_for_start(&spec.working_directory)
                .map_err(setup_error)?;
        }
        spec.withhold_environment();
        let recovery_environment = spec.environment.clone();
        let delegated_source = session.clone();
        // Capability is transient process context, never a public launch
        // recipe. A fresh provider may delegate only as its real session.
        if !matches!(
            spec.transport,
            chat::protocol::TransportSpec::History
                | chat::protocol::TransportSpec::UnixWebSocket { .. }
        ) && let Ok(registry) = super::agent_execution::registry(host)
        {
            let authority = registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(
                    session.clone(),
                ))
                .map_err(retryable_unavailable)?;
            if let Some(token) = authority.token() {
                spec.environment
                    .insert(crate::agent_execution::AUTHORITY_ENV.into(), token.into());
            }
        }
        // Equally transient: the provider's own `cagents` must reach this
        // development instance's daemon.
        if let Some(instance) = crate::instance::development_instance() {
            spec.environment.insert(
                crate::instance::DEVELOPMENT_INSTANCE_ENV.into(),
                instance.into(),
            );
        }
        // Like the capability, transient: a cagents process the provider
        // starts must not take this conversation's account for the system
        // account.
        let home_of = |name: &str| spec.environment.get(name).cloned();
        let selection = crate::account::registry::selection_marker(home_of);
        // It must keep the Claude home the reader started cagents with,
        // though, rather than take it for the default earlier cagents
        // exported to every launch.
        let system = crate::account::system_home_marker(home_of, selection.as_deref(), |name| {
            std::env::var(name).ok()
        });
        for (marker, value) in [
            (crate::account::SELECTION_MARKER, selection),
            (crate::account::SYSTEM_HOME_MARKER, system),
        ] {
            match value {
                Some(value) => {
                    spec.environment.insert(marker.into(), value);
                }
                None => {
                    spec.environment.remove(marker);
                }
            }
        }
        let execution_instance = uuid::Uuid::new_v4();
        if !matches!(
            spec.transport,
            chat::protocol::TransportSpec::History
                | chat::protocol::TransportSpec::UnixWebSocket { .. }
        ) {
            let _account = crate::account::registry::authorize_spawn(&spec.environment, None)
                .map_err(launch_account_error)?;
            crate::agent_execution::bundled::install(
                Some(session.launch.provider),
                &spec.environment,
                &spec.working_directory,
            );
            if let Some(path) = crate::agent_execution::bundled::matched_path(
                spec.environment.get("PATH").map(String::as_str),
            ) {
                spec.environment.insert("PATH".into(), path);
            }
        }
        // Installation and process creation each validate the selected account
        // under their own lease. Release the installation lease before startup:
        // the native holder synchronously acquires the same non-reentrant lock.
        // If removal wins between them, startup's validation refuses the launch.
        let weak = Arc::downgrade(host);
        let id = session.id;
        let owner_instance = Arc::new(std::sync::OnceLock::<uuid::Uuid>::new());
        let notified_instance = owner_instance.clone();
        let notify = move |update| {
            if let Some(instance) = notified_instance.get() {
                changed_owned(&weak, id, *instance, update);
            }
        };
        let startup_source = session.clone();
        // Validate typed bootstrap admission before reserving a retained startup.
        let prepared_identity = if remote {
            Some(adapter.prepared_launch_identity().ok_or_else(|| {
                unavailable("Retained Claude adapter lacks its prepared launch identity")
            })?)
        } else {
            None
        };
        if remote {
            let context = execution
                .clone()
                .ok_or_else(|| unavailable("Retained startup lost its pinned execution context"))?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            require_unreserved_terminals(&state, &session)?;
            if state.native_actor_startups.contains_key(&id) {
                return Err(retryable_unavailable(
                    "An exact retained startup still owns cleanup; no second helper was started",
                ));
            }
            state.native_actor_startups.insert(
                id,
                retained_actor::Startup {
                    source: startup_source.clone(),
                    requested: requested_source.clone(),
                    context,
                    failed_close: None,
                },
            );
        }
        let handle = if let Some(prepared_identity) = prepared_identity {
            // The helper creates and owns the sole live reducer and Store.
            drop((adapter, initial, store));
            Handle::start_remote_owned(
                spec,
                session.clone(),
                prepared_identity,
                recovery_environment,
                notify,
            )
        } else {
            Handle::start_owned(
                spec,
                adapter,
                initial,
                store,
                session.clone(),
                recovery_environment,
                notify,
            )
        }
        .map_err(|message| {
            check_launch_account(&session.launch)
                .err()
                .unwrap_or_else(|| retryable_unavailable(message))
        })?;
        if let Some((source, mut frozen)) = pending_fresh {
            let committed = confirm_fresh_session(
                &handle,
                &mut session.launch,
                Instant::now() + Duration::from_secs(10),
                || {
                    crate::daemon::termination_requested()
                        || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down
                },
            )
            .and_then(|()| {
                if host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down {
                    return Err(retryable_unavailable(
                        "Daemon shutdown cancelled fresh chat handoff",
                    ));
                }
                stop_source(&source, &mut frozen)
            });
            if let Err(failure) = committed {
                let _ = handle.close();
                // The still-uncommitted pidfd guard resumes the source on drop.
                return Err(failure);
            }
        }
        if !remote {
            handle
                .commit_native_installation()
                .map_err(retryable_unavailable)?;
        }
        if remote {
            // A converted Claude source has been positively stopped above; it
            // remains a historical alias rather than a live helper-owned view.
            if source.is_some() {
                session.terminal = None;
                handle
                    .update_native_source(session.clone(), None, None)
                    .map_err(retryable_unavailable)?;
            }
            let mut aliases = vec![
                crate::agent_execution::HostedSession::Chat(requested_source.clone()),
                crate::agent_execution::HostedSession::Chat(delegated_source.clone()),
                crate::agent_execution::HostedSession::Chat(session.clone()),
            ];
            if source.is_some()
                && let Some(terminal) = &delegated_source.terminal
            {
                aliases.push(crate::agent_execution::HostedSession::Terminal(
                    terminal.clone(),
                ));
            }
            session = retained_actor::publish_authenticated_source_for_start(
                host,
                &handle,
                session,
                &aliases,
                None,
                None,
                retained_actor::Publication::Install,
            )
            .map_err(setup_error)?;
        }
        let execution_instance = handle
            .actor_identity()
            .map_or(execution_instance, |identity| identity.instance);
        let _ = owner_instance.set(execution_instance);
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || require_unreserved_terminals(&state, &session).is_err() {
            drop(state);
            let _ = handle.close();
            return Err(retryable_unavailable(
                "Daemon shutdown cancelled chat start",
            ));
        }
        let observation = Observation {
            after_generation: handle.observe_next_update(),
            ..Observation::default()
        };
        let paired_terminal = session
            .terminal
            .as_ref()
            .and_then(|source| state.terminals.get(&source.id))
            .map(|record| record.handle.clone());
        let record = Record {
            execution_instance,
            console_recipe: None,
            paired_terminal,
            session,
            handle,
            source_process,
            subscribers: BTreeMap::from([(client, false)]),
            observations: BTreeMap::from([(client, observation)]),
            completion_observed: false,
            returning_terminal: false,
        };
        let reservation = state
            .chat_spawning
            .get(&reservation_id)
            .cloned()
            .ok_or_else(|| unavailable("Native startup lost its exact reservation"))?;
        let proof = state.proof().clone();
        drop(state);
        let expected_saved = if remote
            && source.is_none()
            && requested_source.terminal.is_none()
            && !requested_source.native_continuity.retains_provider()
        {
            requested_source.launch.session_id.as_deref()
        } else {
            None
        };
        // A fast named SDK failure may already prevent a snapshot. Install
        // its exact cleanup owner before observing confirmation or that error.
        let mut response = if expected_saved.is_some() {
            None
        } else {
            Some(session_snapshot(&record.session, &record.handle).map_err(retryable_unavailable)?)
        };

        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(refusal) = publication_refusal(
            &state,
            &record.session,
            &proof,
            reservation_id,
            &reservation,
        ) {
            drop(state);
            let _ = record.handle.close();
            return Err(refusal);
        }
        let installed_handle = record.handle.clone();
        let retired = state.chats.insert(id, record);
        if let Some(context) = &execution {
            context.installed();
        }
        state.inventory_generation += 1;
        drop(state);
        drop(retired);
        if remote {
            installed_handle.wake_retained_outcomes();
            retained_actor::wake(host, id);
            if let Some(expected) = expected_saved {
                let confirmation = confirm_saved_conversation(
                    &installed_handle,
                    expected,
                    true,
                    Instant::now() + Duration::from_secs(15),
                    || {
                        crate::daemon::termination_requested()
                            || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down
                    },
                )
                .and_then(|()| {
                    let session = {
                        let state = host.lock().unwrap_or_else(|p| p.into_inner());
                        let record = state
                            .chats
                            .get(&id)
                            .ok_or("The resumed conversation disappeared")?;
                        if !record.handle.same_attachment(&installed_handle) {
                            return Err("The resumed conversation changed its owner".into());
                        }
                        record.session.clone()
                    };
                    let current = current_session_snapshot(
                        host,
                        session,
                        installed_handle.clone(),
                        execution_instance,
                    )
                    .map_err(|error| error.message)?;
                    match &current {
                        ServerResponse::ChatAttached { snapshot, .. }
                            if snapshot.ready
                                && !snapshot.exited
                                && snapshot.session_id.as_deref() == Some(expected) =>
                        {
                            Ok(current)
                        }
                        _ => Err(
                            "The provider stopped before its saved conversation was ready".into(),
                        ),
                    }
                });
                match confirmation {
                    Ok(confirmed) => response = Some(confirmed),
                    Err(message) => {
                        #[cfg(test)]
                        retained_admission_tests::pause_failed_confirmation(id);
                        // A named resume confirms its exact conversation in the
                        // initialize reply, without a prompt. Keep the ordinary HOST
                        // record and semantic consumer until normal Close commits all
                        // attention/loss evidence, retires the helper and revokes its
                        // authority. A successful attachment must not hide SDK exit.
                        let target = {
                            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                            let record = state.chats.get(&id).ok_or_else(|| {
                                unavailable("The unconfirmed conversation lost its cleanup owner")
                            })?;
                            if !record.handle.same_attachment(&installed_handle) {
                                // A new authenticated proxy for this very same
                                // installed actor owns cleanup now. Release
                                // only this obsolete startup reservation; a
                                // different actor keeps its original evidence.
                                let same_actor = record.execution_instance == execution_instance
                                    && record.handle.same_actor(&installed_handle);
                                drop(state);
                                if same_actor && let Some(context) = &execution {
                                    retained_actor::forget_startup(host, &startup_source, context);
                                }
                                return Err(unavailable(
                                    "The unconfirmed conversation changed its owner",
                                ));
                            }
                            let target = CloseTarget::from_record(record);
                            let pending =
                                state.native_actor_startups.get_mut(&id).ok_or_else(|| {
                                    unavailable(
                                        "The unconfirmed conversation lost its startup reservation",
                                    )
                                })?;
                            pending.failed_close = Some(target.clone());
                            target
                        };
                        // Publication now owns capacity. Do not hold the startup
                        // reservation while normal Close takes its own exact fence.
                        drop(admission.take());
                        if let Err(cleanup) = close_captured(host, target) {
                            return Err(retryable_unavailable(format!(
                                "{message}. The exact startup cleanup owner is retained: {}. Retry finishes its normal close before another launch.",
                                cleanup.message
                            )));
                        }
                        if let Some(context) = &execution {
                            retained_actor::forget_startup(host, &startup_source, context);
                        }
                        return Err(retryable_unavailable(message));
                    }
                }
            }
            if let Some(context) = &execution {
                retained_actor::forget_startup(host, &startup_source, context);
            }
        }
        response.ok_or_else(|| {
            unavailable("The saved conversation did not publish its confirmed attachment")
        })
    })();
    drop(admission);
    result
}

/// Attach a CLI view without stopping the server-owned thread or its native
/// connection. Both descriptors retain the same daemon-owned session ID.
fn codex_terminal_view(
    host: &SharedHost,
    client: ClientId,
    chat_id: TerminalId,
    mut owners: paired_close::Owners,
    closing: &capacity::Reservation,
    mut session: Session,
    recipe: crate::model::LaunchRecipe,
) -> Result<ServerResponse, ProtocolError> {
    let handle = owners.target.handle.clone();
    let completed = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        require_unreserved_terminals(&state, &session)?;
        owners.require_locked(&state, closing)?;
        let record = attached(&state, client, chat_id)?;
        if !owners.target.matches(record) {
            return Err(unavailable(
                "The thread changed while opening its CLI view.",
            ));
        }
        if let Some(terminal) = &record.session.terminal
            && let Some(live) = state.terminals.get(&terminal.id)
            && !live.handle.is_completed()
        {
            if !record
                .paired_terminal
                .as_ref()
                .is_some_and(|paired| paired.same_worker(&live.handle))
            {
                return Err(unavailable(
                    "The CLI view was replaced; it is not owned by this thread.",
                ));
            }
            return Ok(ServerResponse::ChatTerminal {
                chat_id,
                session: state.descriptor(client, terminal.id)?.session,
                snapshot: handle.snapshot(None).ok_or_else(|| {
                    unavailable("Exact native snapshot is unavailable; no replacement was started")
                })?,
            });
        }
        if record.returning_terminal {
            return Err(unavailable("The CLI view is already opening."));
        }
        let completed = if let Some(terminal) = state.terminals.get(&chat_id) {
            if !terminal.handle.is_completed()
                || !record
                    .paired_terminal
                    .as_ref()
                    .is_some_and(|paired| paired.same_worker(&terminal.handle))
            {
                return Err(unavailable(
                    "This thread has a different terminal owner; it cannot be replaced.",
                ));
            }
            Some((
                state.descriptor(client, chat_id)?.session,
                terminal.handle.clone(),
                state.proof().clone(),
            ))
        } else if record.session.terminal.is_some() {
            return Err(unavailable(
                "The prior CLI owner is unavailable; explicitly close or recover it before opening another view.",
            ));
        } else {
            None
        };
        state
            .chats
            .get_mut(&chat_id)
            .expect("attached actor")
            .returning_terminal = true;
        completed
    };
    if let Some((source, terminal, proof)) = completed {
        let release = (|| {
            owners.require_current(host, closing)?;
            if terminal.holder_identity().is_some() {
                terminal
                    .close_with_receipt(&source, &proof)
                    .map_err(unavailable)?;
            } else {
                terminal
                    .shutdown()
                    .map_err(|error| unavailable(error.to_string()))?;
            }
            let mut unpaired = session.clone();
            unpaired.launch.session_id = handle
                .confirmed_session_id()
                .or_else(|| session.launch.session_id.clone());
            chat::protocol::align_resume_identity(&mut unpaired.launch).map_err(unavailable)?;
            unpaired.native_continuity = handle.native_continuity();
            unpaired.terminal = None;
            handle
                .update_native_source(unpaired.clone(), None, None)
                .map_err(unavailable)?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            owners.require_locked(&state, closing)?;
            if !state
                .chats
                .get(&chat_id)
                .is_some_and(|record| owners.target.matches(record))
                || !state
                    .terminals
                    .get(&chat_id)
                    .is_some_and(|record| record.handle.same_worker(&terminal))
            {
                return Err(unavailable(
                    "The CLI owner changed while releasing its completed view.",
                ));
            }
            state.retire_terminal(chat_id);
            let record = state.chats.get_mut(&chat_id).expect("checked actor");
            record.session.terminal = None;
            record.paired_terminal = None;
            owners = paired_close::Owners::capture(&state, &state.chats[&chat_id])?;
            session = unpaired;
            Ok(())
        })();
        if let Err(error) = release {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            owners.require_locked(&state, closing)?;
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| owners.target.matches(record))
            {
                record.returning_terminal = false;
            }
            return Err(error);
        }
    }
    let terminal = crate::model::TerminalSession {
        owned_codex_listener: session.owned_codex_listener.clone(),
        recovery_origin: None,
        predecessor: None,
        id: chat_id,
        launch: recipe.clone(),
        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    };
    owners.require_current(host, closing)?;
    let spawned = super::state::spawn_codex_view_reserved(
        host,
        client,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions {
            initially_visible: false,
            ..Default::default()
        },
        Some(handle.execution_environment()),
        &TerminalViewReservation {
            kind: TerminalViewKind::Codex {
                owners: &owners,
                closing,
            },
        },
    );
    let (descriptor, paired_terminal) = match spawned {
        Ok(pair) => pair,
        Err(error) => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            owners.require_locked(&state, closing)?;
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| owners.target.matches(record))
            {
                record.returning_terminal = false;
            }
            return Err(error);
        }
    };
    let mut before = session.clone();
    before.launch.session_id = handle
        .confirmed_session_id()
        .or_else(|| session.launch.session_id.clone());
    before.native_continuity = handle.native_continuity();
    let result = (|| {
        chat::protocol::align_resume_identity(&mut before.launch).map_err(unavailable)?;
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            owners.require_locked(&state, closing)?;
            require_unreserved_terminals(&state, &session)?;
            state.ensure_retention_available(&[descriptor.session.id])?;
            if !state
                .terminals
                .get(&chat_id)
                .is_some_and(|record| record.handle.same_worker(&paired_terminal))
            {
                return Err(unavailable("The exact CLI view changed before pairing."));
            }
            let record = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| owners.target.matches(record))
                .ok_or_else(|| unavailable("The thread closed while opening its CLI view."))?;
            // Retain local cleanup ownership even if the holder acknowledgment
            // is lost. Neither owner is rediscovered through a reusable ID.
            record.session.terminal = Some(descriptor.session.clone());
            record.paired_terminal = Some(paired_terminal.clone());
            owners = paired_close::Owners::capture(&state, &state.chats[&chat_id])?;
        }
        let mut updated = before.clone();
        updated.terminal = Some(descriptor.session.clone());
        handle
            .update_native_source(updated, None, paired_terminal.holder_identity().cloned())
            .map_err(unavailable)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        owners.require_locked(&state, closing)?;
        require_unreserved_terminals(&state, &session)?;
        state.ensure_retention_available(&[descriptor.session.id])?;
        if !state
            .terminals
            .get(&chat_id)
            .is_some_and(|record| record.handle.same_worker(&paired_terminal))
        {
            return Err(unavailable("The exact CLI view changed during pairing."));
        }
        let record = state
            .chats
            .get_mut(&chat_id)
            .filter(|record| owners.target.matches(record))
            .ok_or_else(|| unavailable("The thread closed while publishing its CLI view."))?;
        record.session.launch.session_id = before.launch.session_id.clone();
        record.source_process = paired_terminal.observe_agent_process();
        record.returning_terminal = false;
        owners = paired_close::Owners::capture(&state, &state.chats[&chat_id])?;
        state.inventory_generation += 1;
        Ok(ServerResponse::ChatTerminal {
            chat_id,
            session: descriptor.session.clone(),
            snapshot: handle.snapshot(None).ok_or_else(|| {
                unavailable("Exact native snapshot is unavailable; no replacement was started")
            })?,
        })
    })();
    if let Err(error) = result {
        let proof = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(record) = state
                .chats
                .get_mut(&chat_id)
                .filter(|record| owners.target.matches(record))
            {
                record.returning_terminal = false;
            }
            if !state
                .terminals
                .get(&chat_id)
                .is_some_and(|record| record.handle.same_worker(&paired_terminal))
            {
                return Err(unavailable(format!(
                    "{} Exact CLI ownership changed; its replacement was retained.",
                    error.message
                )));
            }
            state.proof().clone()
        };
        let cleanup = if paired_terminal.holder_identity().is_some() {
            paired_terminal.close_with_receipt(&descriptor.session, &proof)
        } else {
            paired_terminal
                .shutdown()
                .map_err(|error| error.to_string())
        };
        cleanup.map_err(|cleanup| {
            unavailable(format!(
                "{} CLI cleanup is unconfirmed: {cleanup}",
                error.message
            ))
        })?;
        // Do not forget the durable pair before the exact terminal has its
        // own teardown receipt, including if the daemon stops at this boundary.
        owners.require_current(host, closing)?;
        handle
            .update_native_source(before, None, None)
            .map_err(|rollback| {
                unavailable(format!(
                    "{} Pair rollback is unconfirmed: {rollback}",
                    error.message
                ))
            })?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        owners.require_locked(&state, closing)?;
        if state
            .terminals
            .get(&chat_id)
            .is_some_and(|record| record.handle.same_worker(&paired_terminal))
        {
            state.retire_terminal(chat_id);
        }
        if let Some(record) = state
            .chats
            .get_mut(&chat_id)
            .filter(|record| owners.target.matches(record))
        {
            record.session.terminal = None;
            record.paired_terminal = None;
        }
        return Err(error);
    }
    result
}

/// Client descriptors cannot expand allocation authority beyond this daemon's
/// exact namespace or retained conversation. Checkpoint restore invokes `start`
/// directly with the daemon-owned durable source instead.
fn client_session(host: &SharedHost, mut session: Session) -> Session {
    if session.native_continuity.retains_provider() {
        // This descriptor can only enter authenticated holder adoption, which
        // checks its full retained value. The caller's TMPDIR grants no authority.
        return session;
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let retained = state
        .chats
        .get(&session.id)
        .filter(|record| {
            record.session.launch.provider == session.launch.provider
                && record.handle.session_id() == session.launch.session_id
                && record
                    .session
                    .owned_codex_listener
                    .as_ref()
                    .is_some_and(|owned| owned.matches(session.id, &session.launch.recipe))
        })
        .and_then(|record| record.session.owned_codex_listener.clone());
    // The exact allocation namespace already authorizes this daemon to reopen
    // this endpoint, including legacy recipes. Retain its original allocator
    // across an unclean restart instead of replacing provenance with today's
    // host proof. A caller cannot extend that authority to another root.
    session.owned_codex_listener = retained.or_else(|| {
        session.owned_codex_listener.take().filter(|owned| {
            owned.matches(session.id, &session.launch.recipe)
                && state.codex_listeners.allocates(session.id, &owned.socket)
        })
    });
    if let Some(terminal) = &mut session.terminal {
        terminal.owned_codex_listener = state
            .terminals
            .get(&terminal.id)
            .filter(|record| record.session.launch == terminal.launch)
            .and_then(|record| record.session.owned_codex_listener.clone())
            .or_else(|| {
                session
                    .owned_codex_listener
                    .clone()
                    .filter(|owned| owned.matches(terminal.id, &terminal.launch))
            });
    }
    session
}

struct AccountSwitchReservation {
    host: SharedHost,
    source: Session,
}

impl Drop for AccountSwitchReservation {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chat_spawning.get(&self.source.id) == Some(&self.source) {
            state.chat_spawning.remove(&self.source.id);
        }
    }
}

/// The original conversation an automatic account switch may close, held
/// against every other start of it until its successor starts.
struct AdmittedSwitch {
    handle: Handle,
    reservation: AccountSwitchReservation,
}

/// Everything [`recover_account`] checks before it changes anything. An
/// automatic switch is given its original's actor and a reservation of it; an
/// explicit recovery, whose original is no longer hosted, is given nothing.
///
/// Every refusal here leaves the original exactly as it was: the only thing
/// held is the reservation, which drops with the refusal.
fn admit_account_recovery(
    host: &SharedHost,
    source: &Session,
    replacement: &Session,
    automatic: bool,
) -> Result<Option<AdmittedSwitch>, ProtocolError> {
    if !chat::is_account_recovery(source, replacement)
        || replacement.proof.is_some()
        || replacement.owned_codex_listener.is_some()
    {
        return Err(error(
            "Account recovery must retain the exact provider, conversation and launch options",
        ));
    }
    match check_launch_account(&source.launch) {
        Err(failure) if failure.code == ProtocolErrorCode::AccountRemoved => {}
        Err(failure) => return Err(failure),
        Ok(()) if automatic => {}
        Ok(()) => {
            return Err(error(
                "The original account is available; retry the saved conversation normally",
            ));
        }
    }
    if !automatic {
        return Ok(None);
    }
    if source
        .launch
        .recipe
        .environment
        .get(crate::account::routing::AUTOMATIC_MARKER)
        .map(String::as_str)
        != Some("1")
    {
        return Err(error("This conversation has an explicitly pinned account"));
    }
    let admitted = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(source.proof.as_ref(), state.proof())?;
        let record = state.chats.get(&source.id).ok_or_else(|| {
            error("The original conversation is no longer hosted; reconnect before switching accounts")
        })?;
        if record.session.launch.provider != source.launch.provider
            || record.session.launch.recipe != source.launch.recipe
            || record.handle.confirmed_session_id() != source.launch.session_id
            || record.returning_terminal
            || record.paired_terminal.is_some()
            || state.chat_handoffs.contains_key(&source.id)
            || state.chat_spawning.contains_key(&source.id)
        {
            return Err(error(
                "The original conversation changed or has another live owner; account switching was paused",
            ));
        }
        let handle = record.handle.clone();
        state.chat_spawning.insert(source.id, source.clone());
        AdmittedSwitch {
            handle,
            reservation: AccountSwitchReservation {
                host: host.clone(),
                source: source.clone(),
            },
        }
    };
    // A refused replacement must leave the original actor and history
    // intact. The temporary source reservation drops on this error;
    // actual spawn still reauthorizes the account at its own boundary.
    // An account the registry no longer lists is the replacement's, not the
    // original's, so it is refused as any other replacement would be.
    check_launch_account(&replacement.launch)?;
    crate::account::handoff::preflight(source, replacement).map_err(unavailable)?;
    Ok(Some(admitted))
}

fn recover_account(
    host: &SharedHost,
    client: ClientId,
    source: Session,
    replacement: Session,
    idle_generation: Option<u64>,
    execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
) -> Result<ServerResponse, ProtocolError> {
    let automatic = idle_generation.is_some();
    // An automatic switch refused here changed nothing, so its client keeps
    // the conversation on its account. Nothing after the close below is
    // reported this way, not even the close's own refusal: once the actor has
    // taken it, the original may already be partly closed.
    let admitted =
        admit_account_recovery(host, &source, &replacement, automatic).map_err(|failure| {
            if automatic {
                failure.not_admitted()
            } else {
                failure
            }
        })?;
    let mut reservation = None;
    if let (
        Some(generation),
        Some(AdmittedSwitch {
            handle,
            reservation: held,
        }),
    ) = (idle_generation, admitted)
    {
        reservation = Some(held);
        // The actor checks this generation after every previously admitted
        // command. A UI snapshot alone cannot prove a timed-out turn is idle.
        handle
            .close_if_idle_for_resume(generation)
            .map_err(unavailable)?;
        crate::account::handoff::prepare(&source, &replacement).map_err(unavailable)?;
    }
    {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.chats.get(&source.id).is_some_and(|record| {
            record.session.launch.provider != source.launch.provider
                || record.session.launch.recipe != source.launch.recipe
                || record.handle.session_id() != source.launch.session_id
        }) {
            return Err(error(
                "The original hosted conversation changed before account recovery",
            ));
        }
        if (!automatic && state.chat_spawning.contains_key(&source.id))
            || state
                .chats
                .get(&source.id)
                .is_some_and(|record| record.handle.owns_work())
            || state
                .terminals
                .get(&source.id)
                .is_some_and(|record| !record.handle.is_completed())
            || state.chat_handoffs.contains_key(&source.id)
        {
            return Err(error(
                "The original conversation still owns work; reconnect or close it before changing accounts",
            ));
        }
    }
    if !automatic {
        check_launch_account(&replacement.launch)?;
    }
    let id = replacement.id;
    // The checks above prove the candidate replaces the source, so it joins the
    // source's lineage, unless another of its sessions runs, before its
    // provider can delegate as anything else.
    if let Ok(registry) = super::agent_execution::registry(host) {
        registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .succeed(
                id,
                source.id,
                source
                    .terminal
                    .as_ref()
                    .and_then(|terminal| terminal.recovery_origin),
                |members| {
                    host.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .any_other_live(members, source.id)
                },
            )
            .map_err(unavailable)?;
    }
    // Automatic requests hold the exclusive daemon control gate. Consume
    // the source reservation before starting its successor, whose exact
    // conversation ID intentionally matches it.
    drop(reservation.take());
    start(
        host,
        client,
        replacement,
        None,
        false,
        None,
        execution_environment,
    )?;
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
        .ok_or_else(|| unavailable("The account recovery candidate disappeared"))?
        .handle
        .clone();
    let confirmation = confirm_saved_conversation(
        &handle,
        source.launch.session_id.as_deref().unwrap(),
        false,
        Instant::now() + Duration::from_secs(15),
        || crate::daemon::termination_requested(),
    );
    let response = confirmation.map_err(unavailable).and_then(|()| {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = attached(&state, client, id)?;
        if !record.handle.same_actor(&handle)
            || !chat::is_account_recovery(&source, &record.session)
        {
            return Err(error(
                "The account recovery candidate changed before confirmation",
            ));
        }
        let attachment = (
            record.session.clone(),
            record.handle.clone(),
            record.execution_instance,
        );
        drop(state);
        current_session_snapshot(host, attachment.0, attachment.1, attachment.2)
    });
    if let Err(failure) = response {
        if let Err(close) = process(host, client, ClientRequest::ChatClose { chat_id: id }) {
            return Err(unavailable(format!(
                "{}. The candidate could not be confirmed stopped: {}",
                failure.message, close.message
            )));
        }
        return Err(failure);
    }
    response
}

/// With `require_ready`, confirmation also waits for the provider to answer
/// initialize, for a caller that goes on to require a ready provider.
fn confirm_saved_conversation(
    handle: &Handle,
    expected: &str,
    require_ready: bool,
    deadline: Instant,
    cancelled: impl Fn() -> bool,
) -> Result<(), String> {
    loop {
        if cancelled() {
            return Err(
                "Opening the saved conversation was cancelled; it remains unchanged".into(),
            );
        }
        if let Some(failure) = handle.failure() {
            return Err(failure);
        }
        if handle.is_completed() {
            return Err("The provider exited before confirming the saved conversation; it remains unchanged".into());
        }
        if let Some(actual) = handle.confirmed_session_id() {
            if actual != expected {
                return Err("The provider opened a different conversation; the saved conversation is unchanged".into());
            }
            if !require_ready || handle.is_ready() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("The provider did not confirm the exact saved conversation in time; it remains unchanged".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn process(
    host: &SharedHost,
    client: ClientId,
    request: ClientRequest,
) -> Result<ServerResponse, ProtocolError> {
    super::agent_execution::interactive(host, client, request, |request| {
        process_inner(host, client, request)
    })
}

pub(super) fn read_presentation(
    host: &SharedHost,
    client: ClientId,
    chat_id: crate::model::TerminalId,
    after_generation: Option<u64>,
) -> Result<crate::terminal_ipc::PreparedResponse, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(receipt) = state.chat_handoffs.get(&chat_id) {
        receipt.require_attached(client)?;
        if !receipt.subscribers.get(&client).copied().unwrap_or(false) {
            return Err(error(
                "Hidden chat has no presentation stream; make it visible before reading",
            ));
        }
        return Ok(ServerResponse::ChatSnapshot {
            chat_id,
            snapshot: (!after_generation
                .is_some_and(|generation| generation >= receipt.generation))
            .then(|| receipt.snapshot()),
        }
        .into());
    }
    let record = attached(&state, client, chat_id)?;
    if !record.subscribers.get(&client).copied().unwrap_or(false) {
        return Err(error(
            "Hidden chat has no presentation stream; make it visible before reading",
        ));
    }
    let handle = record.handle.clone();
    drop(state);
    let response = handle
        .presentation_wire(chat_id, after_generation)
        .map_err(unavailable)?;
    // A close, detach, replacement actor, or visibility change during
    // hydration must not publish data under the old attachment.
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let current = attached(&state, client, chat_id)?;
    if !current.handle.same_attachment(&handle)
        || !current.subscribers.get(&client).copied().unwrap_or(false)
    {
        return Err(error(
            "Chat attachment changed while preparing its transcript",
        ));
    }
    Ok(response)
}

fn process_inner(
    host: &SharedHost,
    client: ClientId,
    request: ClientRequest,
) -> Result<ServerResponse, ProtocolError> {
    match request {
        ClientRequest::ChatCapacityInspect => Ok(ServerResponse::ChatCapacity {
            capacity: capacity::status(host),
            persistence_warning: None,
        }),
        ClientRequest::ChatCapacitySet { limit } => {
            crate::native_chat_capacity::validate(limit).map_err(error)?;
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let saved = crate::native_chat_capacity::save(limit).map_err(|failure| {
                unavailable(format!("Cannot persist native chat capacity: {failure}"))
            })?;
            state.native_chat_capacity = limit as usize;
            Ok(ServerResponse::ChatCapacity {
                capacity: capacity::status_from_state(&state),
                persistence_warning: saved.durability_warning,
            })
        }
        ClientRequest::ChatReleaseInspect { chat_id } => Ok(ServerResponse::ChatReleaseInspected {
            inspection: capacity::inspect(host, chat_id)?,
        }),
        ClientRequest::ChatRelease { proof, dry_run } => Ok(ServerResponse::ChatReleased {
            inspection: capacity::release(host, &proof, dry_run)?,
            dry_run,
        }),
        ClientRequest::ChatPrepareResume { source } => {
            if let Some(session) =
                floor_history::prepare_resume(host, &source).map_err(unavailable)?
            {
                return Ok(ServerResponse::ChatResumePrepared { session });
            }
            let proof = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .proof()
                .clone();
            let unnamed = source.has_unnamed_retained_claude_actor();
            if !source.has_valid_native_continuity()
                || (source.launch.session_id.is_none() && !unnamed)
            {
                return Err(error("Saved conversation has invalid continuity evidence"));
            }
            if let Some(session) = retained_actor::prepare_failed_start_for_start(host, &source)
                .map_err(setup_error)?
            {
                return Ok(ServerResponse::ChatResumePrepared { session });
            }
            if let Some(session) = capacity::prepare_saved_history(host, &source)? {
                return Ok(ServerResponse::ChatResumePrepared { session });
            }
            // A card still attached to an actor whose provider exited under
            // this daemon resumes the same way as one a daemon stop ended:
            // the exited actor is retired as a stop first, so the checks
            // below find its verified retirement rather than a present owner.
            retained_actor::finish_exited_attachment_for_start(host, &source)
                .map_err(setup_error)?;
            // An unnamed pane resumes the conversation its ended actors
            // confirmed, through the same checks as a named one; when they
            // confirmed none, every actor's end is already proved and the
            // pane starts a conversation in place.
            let ended = source.clone();
            let mut source = source;
            let mut unconfirmed = false;
            if unnamed {
                match retained_resume::unnamed_conversation_for_start(host, &source)
                    .map_err(setup_error)?
                {
                    Some(conversation) => source.launch.session_id = Some(conversation),
                    None => unconfirmed = true,
                }
            }
            // Classify retained Claude execution before generic discovery:
            // an exited helper may still await its original parent's wait.
            // Errors/unproved exit never fall back to treating it as live.
            let retirement = if unconfirmed {
                retained_resume::ResumeRetirement::Abandoned
            } else if source.launch.provider == Provider::Claude {
                retained_resume::inspect_resume_retirement_for_start(host, &source)
                    .map_err(setup_error)?
            } else {
                retained_resume::ResumeRetirement::Unclassified
            };
            // A helper whose daemon exited between closing its provider and
            // retiring it finishes that exit here, then classifies as retired.
            let retirement = if !retirement.is_classified()
                && retained_actor::finish_interrupted_exit_of_for_start(host, &ended)
                    .map_err(setup_error)?
            {
                retained_resume::inspect_resume_retirement_for_start(host, &source)
                    .map_err(setup_error)?
            } else {
                retirement
            };
            // A present owner must be restored, not replaced. Missing evidence
            // is never proof of a completed close; a runtime record left by a
            // proved retired or abandoned actor is not a present owner.
            if !retirement.is_classified() && has_native_owner(&ended)? {
                return Ok(ServerResponse::ChatResumePrepared { session: ended });
            }
            if prepare_shared_codex_join(host, &source)? {
                return Ok(ServerResponse::ChatResumePrepared { session: source });
            }
            // A kernel boot change proves every process from the saved boot
            // is gone. Unlike missing runtime files within the same boot, it
            // authorizes an exact disk resume without its ephemeral receipt.
            let previous_boot = source.launch.provider != Provider::Codex
                && source.proof.as_ref().is_some_and(|previous| {
                    previous.is_complete() && previous.boot_id != proof.boot_id
                });
            let unstarted = if previous_boot || unconfirmed {
                None
            } else {
                retained_resume::unstarted_for_start(host, &source).map_err(setup_error)?
            };
            // A successor no lineage records here resumes on its own, as any
            // conversation reopened without an owner to take over.
            if unstarted == Some(retained_resume::Unstarted::Unrecorded) {
                let mut session = source;
                session.id = TerminalId::new();
                return Ok(ServerResponse::ChatResumePrepared { session });
            }
            let closed = if previous_boot
                || unconfirmed
                || unstarted == Some(retained_resume::Unstarted::InLineage)
            {
                true
            } else if source.launch.provider == Provider::Claude {
                retained_resume::prepare_resume_verified_for_start(
                    host,
                    &source,
                    retirement.verified(),
                )
                .map_err(setup_error)?
            } else {
                // An owner lost without its receipt was closed all the same,
                // proved while no start or close can take its allocation.
                chat::native_holder::closed_source(&source, &proof)
                    .map_err(retryable_unavailable)?
                    .is_some_and(|(_, paired_closed)| paired_closed)
                    || match reserve_unhosted_allocation(host, &source, None) {
                        Ok(_fence) => {
                            require_ended_codex_execution(host, &source, &proof.boot_id).map_err(
                                |failure| {
                                    ProtocolError::new(
                                        ProtocolErrorCode::TerminalUnavailable,
                                        failure.to_string(),
                                        source.launch.provider == Provider::Codex,
                                    )
                                },
                            )?;
                            true
                        }
                        Err(()) => false,
                    }
            };
            if !closed {
                return Err(retryable_unavailable(
                    "The old provider owner has no confirmed close receipt. Restore its original pane and close it before reopening; no replacement was started.",
                ));
            }
            // A changed boot proves the old owner is gone, not that the
            // recorded launch still selects the account home it ran under.
            if previous_boot && source.launch.provider == Provider::Claude {
                retained_resume::ensure_prior_boot_account_for_start(host, &source)
                    .map_err(setup_error)?;
            }
            // The old process is proved ended, as after a daemon stop: its
            // authority and unfinished runs end and its agent stays open, so
            // the successor's start can take the agent over.
            super::agent_execution::stopped(host, &ended).map_err(retryable_unavailable)?;
            let predecessor = source.id;
            let root = source
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.recovery_origin);
            let mut session = source;
            session.reset_owned_codex_listener();
            session.id = TerminalId::new();
            session.proof = None;
            session.terminal = None;
            // The old owner's close is proved, so the reopened conversation
            // takes over its lineage, unless another of its sessions runs.
            if let Ok(registry) = super::agent_execution::registry(host) {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .succeed(session.id, predecessor, root, |members| {
                        host.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .any_other_live(members, predecessor)
                    })
                    .map_err(retryable_unavailable)?;
            }
            Ok(ServerResponse::ChatResumePrepared { session })
        }
        ClientRequest::ChatRecoverAccount {
            source,
            replacement,
            idle_generation,
            execution_environment,
        } => recover_account(
            host,
            client,
            source,
            replacement,
            idle_generation,
            execution_environment,
        ),
        ClientRequest::ChatPreview { session, source } => preview(host, session, source),
        ClientRequest::ChatCandidates { session, source } => candidates(host, session, source),
        ClientRequest::ChatStartSelected {
            session,
            source,
            token,
        } => {
            let selection = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(Some(&source), state.proof())?;
                let matched = state
                    .chat_bindings
                    .get(&session.id)
                    .is_some_and(|binding| binding.preview.token == token);
                // Consume the preview only for its own token: a mismatched
                // request must leave the pending selection intact for the client
                // that owns it.
                matched
                    .then(|| state.chat_bindings.remove(&session.id))
                    .flatten()
                    .ok_or_else(|| {
                        error(
                            "This selection has no matching preview; preview the conversation again",
                        )
                    })?
            };
            start(
                host,
                client,
                client_session(host, session),
                Some(source),
                false,
                Some(selection),
                Default::default(),
            )
        }
        ClientRequest::ChatList => {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let mut sessions = state
                .chats
                .values()
                .filter(|record| {
                    record.handle.owns_work()
                        || record.returning_terminal
                        || state
                            .native_actor_startups
                            .get(&record.session.id)
                            .is_some_and(|pending| {
                                pending
                                    .failed_close
                                    .as_ref()
                                    .is_some_and(|target| target.matches(record))
                            })
                })
                .map(|record| {
                    let mut session = record.session.clone();
                    session.launch.session_id = record.handle.session_id();
                    session.native_continuity = record.handle.native_continuity();
                    session
                })
                .collect::<Vec<_>>();
            sessions.extend(
                state
                    .chat_spawning
                    .values()
                    .filter(|pending| {
                        state.historical_chat_retirements.get(&pending.id) != Some(*pending)
                    })
                    .map(|pending| {
                        let mut session = pending.clone();
                        // Inventory describes this daemon's pending attachment. Keep
                        // the reservation's exact caller token unchanged for its CAS.
                        session.proof = Some(state.proof().clone());
                        session
                    }),
            );
            Ok(ServerResponse::ChatListed { sessions })
        }
        ClientRequest::ChatStartInPane {
            pane,
            session,
            execution_environment,
        } => start_with_execution(
            host,
            client,
            client_session(host, session),
            None,
            false,
            None,
            execution_environment,
            None,
            None,
            Some(pane),
        ),
        ClientRequest::ChatStart {
            session,
            source,
            terminate_source,
            execution_environment,
        } => {
            // Keep the exact old alias, including its allocation provenance.
            // A client can mount before the provider confirms durable support;
            // normalizing that saved ReconnectOnly descriptor would destroy the
            // holder's immutable evidence before recovery can authenticate it.
            let session = if has_native_owner(&session)? {
                session
            } else {
                client_session(host, session)
            };
            let id = session.id;
            let response = start(
                host,
                client,
                session,
                source,
                terminate_source,
                None,
                execution_environment,
            )?;
            // A resumed conversation keeps the agent its stopped predecessor
            // bound: its row, pane and channel move to the running successor.
            // The provider already runs, so a refusal leaves the agent with
            // the ended process (still closable) rather than failing the start.
            let started = host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .chats
                .get(&id)
                .map(|record| record.session.clone());
            if let Some(started) = started {
                match super::agent_execution::adopt_resumed(host, &started) {
                    Ok(true) => {
                        host.lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .idle_adoptions
                            .insert(id);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(chat=%id, %error, "The resumed conversation did not take over its agent");
                    }
                }
            }
            Ok(response)
        }
        ClientRequest::ChatAttach { session } => {
            if let Some(response) = upgrade_resolution::attach(host, client, &session)? {
                return Ok(response);
            }
            let cleanup = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(session.proof.as_ref(), state.proof())?;
                state
                    .chats
                    .get(&session.id)
                    .is_some_and(|record| record.handle.is_close_only())
            };
            if cleanup {
                retained_actor::recover_cleanup_attachment_for_start(host, &session)
                    .map_err(setup_error)?;
            }
            let missing = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(session.proof.as_ref(), state.proof())?;
                !state.chats.contains_key(&session.id)
                    && !state.chat_handoffs.contains_key(&session.id)
            };
            if missing {
                return capacity::restore_saved_history(host, client, session);
            }
            // A live actor remains attachable after account removal. A stopped
            // actor cannot resume with that home, so expose the same recovery
            // choice without creating a replacement process.
            let stopped_launch = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                validate_proof(session.proof.as_ref(), state.proof())?;
                if state.chat_spawning.contains_key(&session.id) {
                    return Err(retryable_unavailable(
                        "Native chat ownership is changing; retry its normal attach after the transition finishes",
                    ));
                }
                state
                    .chats
                    .get(&session.id)
                    .filter(|record| record.handle.is_completed())
                    .map(|record| record.session.launch.clone())
            };
            if let Some(launch) = stopped_launch {
                check_launch_account(&launch)?;
            }
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            validate_proof(session.proof.as_ref(), state.proof())?;
            if state.chat_spawning.contains_key(&session.id) {
                return Err(retryable_unavailable(
                    "Native chat ownership is changing; retry its normal attach after the transition finishes",
                ));
            }
            if let Some(receipt) = state.chat_handoffs.get_mut(&session.id) {
                receipt.validate_source(&session)?;
                receipt.subscribers.entry(client).or_insert(false);
                return Ok(ServerResponse::ChatAttached {
                    session: receipt.source.clone(),
                    snapshot: receipt.snapshot(),
                });
            }
            let record = state.chats.get_mut(&session.id).ok_or_else(|| {
                unavailable("Exact native chat not found; no replacement process was spawned")
            })?;
            let actual_id = record.handle.session_id();
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || (session.launch.session_id.is_some() && session.launch.session_id != actual_id)
            {
                return Err(error(
                    "Native chat recovery identity does not match the hosted conversation",
                ));
            }
            if let std::collections::btree_map::Entry::Vacant(entry) =
                record.subscribers.entry(client)
            {
                entry.insert(false);
                record.observations.insert(
                    client,
                    Observation {
                        after_generation: record.handle.observe_next_update(),
                        ..Observation::default()
                    },
                );
            }
            let attachment = (
                record.session.clone(),
                record.handle.clone(),
                record.execution_instance,
            );
            drop(state);
            current_session_snapshot(host, attachment.0, attachment.1, attachment.2)
        }
        ClientRequest::ChatDetach { chat_id } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(receipt) = state.chat_handoffs.get_mut(&chat_id) {
                receipt.require_attached(client)?;
                receipt.detach(client);
                return Ok(ServerResponse::Detached);
            }
            let record = attached_mut(&mut state, client, chat_id)?;
            record.subscribers.remove(&client);
            record.observations.remove(&client);
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
            Ok(ServerResponse::Detached)
        }
        ClientRequest::ChatCommand { chat_id, command } => {
            let handle = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.shutting_down {
                    return Err(unavailable("Daemon is shutting down"));
                }
                attached(&state, client, chat_id)?.handle.clone()
            };
            if let crate::chat::Command::ReadTextCopy { source, offset } = &command {
                let page = handle.text_copy(source, *offset).map_err(unavailable)?;
                return Ok(ServerResponse::ChatTextCopy { chat_id, page });
            }
            if let Some(reply) = handle.file_query(&command).map_err(unavailable)? {
                return Ok(match reply {
                    crate::chat::FileChangeReply::Files(summary) => {
                        ServerResponse::ChatFileChanges { chat_id, summary }
                    }
                    crate::chat::FileChangeReply::Patches(page) => {
                        ServerResponse::ChatFileChangePatches { chat_id, page }
                    }
                });
            }
            if command.gives_work() {
                super::agent_execution::given_work(host, chat_id);
            }
            handle.command(command).map_err(unavailable)?;
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatAcknowledge { chat_id, epoch } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(receipt) = state.chat_handoffs.get(&chat_id) {
                receipt.require_attached(client)?;
                return Ok(ServerResponse::Accepted);
            }
            let record = attached_mut(&mut state, client, chat_id)?;
            let observation = record.observations.entry(client).or_default();
            if epoch > observation.epoch {
                observation.acknowledge(epoch, record.handle.observe_next_update());
            }
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatVisibility { chat_id, visible } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(receipt) = state.chat_handoffs.get_mut(&chat_id) {
                receipt.require_attached(client)?;
                receipt.subscribers.insert(client, visible);
                return Ok(ServerResponse::Accepted);
            }
            let record = attached_mut(&mut state, client, chat_id)?;
            let previous = record.subscribers.insert(client, visible);
            if previous != Some(visible) && !visible {
                record.handle.observe_next_update();
            }
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
            drop(state);
            if visible {
                changed(&Arc::downgrade(host), chat_id, Update::Presentation);
            }
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatRead {
            chat_id,
            after_generation,
        } => read_presentation(host, client, chat_id, after_generation)?
            .decode()
            .map_err(unavailable),
        ClientRequest::ChatReturnTerminal { chat_id } => {
            super::account_lifecycle::ensure_idle(host)?;
            let (owners, closing) = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.shutting_down {
                    return Err(unavailable("Daemon is shutting down"));
                }
                if let Some(receipt) = state.chat_handoffs.get_mut(&chat_id) {
                    receipt.require_attached(client)?;
                    receipt.reissue(Instant::now());
                    return Ok(ServerResponse::ChatTerminal {
                        chat_id,
                        session: receipt.terminal.clone(),
                        snapshot: receipt.snapshot(),
                    });
                }
                if state.chat_handoffs.len() >= 1024 {
                    return Err(unavailable(
                        "Too many unacknowledged terminal handoffs; save existing replacements before converting another chat",
                    ));
                }
                let owners =
                    paired_close::Owners::capture(&state, attached(&state, client, chat_id)?)?;
                let closing =
                    capacity::Reservation::closing(host, &mut state, &owners.target.source)?;
                (owners, closing)
            };
            let handle = owners.target.handle.clone();
            let mut session = owners.target.source.clone();
            owners.require_current(host, &closing)?;
            session.launch.session_id = handle.confirmed_session_id();
            let snapshot = handle.snapshot(None).ok_or_else(|| {
                unavailable("Exact native snapshot is unavailable; no replacement was started")
            })?;
            let recipe = session.launch.terminal_recipe_with_permission_mode(snapshot.permission_mode.as_deref()).ok_or_else(|| unavailable(
                "Wait for the provider's confirmed exact conversation before returning to terminal",
            ))?;
            if session.launch.provider == Provider::Codex
                && (session.terminal.is_some()
                    || session
                        .launch
                        .recipe
                        .arguments
                        .iter()
                        .any(|arg| arg == "--remote" || arg.starts_with("--remote=")))
            {
                return codex_terminal_view(
                    host, client, chat_id, owners, &closing, session, recipe,
                );
            }
            if session.launch.provider == Provider::Codex && session.terminal.is_some() {
                return Err(error(
                    "A shared Codex source must be reattached, not replaced",
                ));
            }
            let terminal_id = crate::model::TerminalId::new();
            let mut terminal = crate::model::TerminalSession {
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                id: terminal_id,
                launch: recipe.clone(),
                continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
                resume_hint: None,
                transcript_path: session
                    .terminal
                    .as_ref()
                    .and_then(|source| source.transcript_path.clone()),
            };
            // Retain the actor and its execution-only environment until spawn
            // succeeds, so a failed replacement can be retried without falling
            // back to the daemon's credentials or losing the exact identity.
            {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                owners.require_locked(&state, &closing)?;
                let record = state
                    .chats
                    .get_mut(&chat_id)
                    .expect("validated exact attachment");
                record.returning_terminal = true;
                record.completion_observed = false;
                state.inventory_generation += 1;
            }
            // Closing interrupts active work and waits for confirmed teardown.
            // Never start a second writer before teardown has been proved.
            owners.close(host, &closing)?;
            let snapshot = handle.snapshot(None).ok_or_else(|| {
                unavailable(
                    "Exact native final snapshot is unavailable; retirement remains unconfirmed",
                )
            })?;
            if snapshot.session_id != session.launch.session_id {
                return Err(unavailable(
                    "Provider identity changed during shutdown; retry the exact terminal handoff",
                ));
            }
            // Autonomous policy changes can arrive while the native writer
            // shuts down. The final confirmed snapshot wins over launch flags.
            let recipe = session
                .launch
                .terminal_recipe_with_permission_mode(snapshot.permission_mode.as_deref())
                .ok_or_else(|| unavailable("Cannot form the confirmed terminal resume command"))?;
            terminal.launch = recipe.clone();
            terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            };
            // Teardown is proved, so the terminal takes over the chat's lineage
            // and whatever agents it started, unless another of its sessions runs.
            owners.require_current(host, &closing)?;
            if let Ok(registry) = super::agent_execution::registry(host) {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .succeed(
                        terminal_id,
                        chat_id,
                        session
                            .terminal
                            .as_ref()
                            .and_then(|terminal| terminal.recovery_origin),
                        |members| {
                            host.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .any_other_live(members, chat_id)
                        },
                    )
                    .map_err(unavailable)?;
            }
            owners.require_current(host, &closing)?;
            let (descriptor, replacement_worker) = super::state::spawn_terminal_view_owned(
                host,
                client,
                terminal,
                crate::terminal_ipc::TerminalSpawnOptions {
                    initially_visible: false,
                    ..Default::default()
                },
                Some(handle.execution_environment()),
                false,
            )?;
            let publication = (|| {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                owners.require_locked(&state, &closing)?;
                if !state.terminals.get(&terminal_id).is_some_and(|current| {
                    current.handle.same_worker(&replacement_worker)
                        && matches_paired_terminal(
                            &current.session,
                            &descriptor.session,
                            state.proof(),
                        )
                }) {
                    return Err(unavailable(
                        "The replacement terminal changed before handoff publication",
                    ));
                }
                let old_pair = owners.remove_pair(&mut state, &closing)?;
                let mut record = state
                    .chats
                    .remove(&chat_id)
                    .expect("validated exact attachment");
                state.chat_handoffs.insert(
                    chat_id,
                    TerminalHandoff::new(
                        session,
                        descriptor.session.clone(),
                        snapshot.generation,
                        std::mem::take(&mut record.subscribers),
                        Instant::now() + ACKNOWLEDGEMENT_GRACE,
                    ),
                );
                state.inventory_generation += 1;
                Ok((record, old_pair))
            })();
            match publication {
                Ok(retired) => drop(retired),
                Err(error) => {
                    // Retire only the worker returned by this spawn. A reused
                    // terminal ID cannot authorize removing its replacement.
                    let proof = host
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .proof()
                        .clone();
                    if replacement_worker.holder_identity().is_some() {
                        replacement_worker
                            .close_with_receipt(&descriptor.session, &proof)
                            .map_err(|cleanup| {
                                unavailable(format!(
                                    "{} Replacement cleanup is unconfirmed: {cleanup}",
                                    error.message
                                ))
                            })?;
                    } else {
                        replacement_worker.shutdown().map_err(|cleanup| {
                            unavailable(format!(
                                "{} Replacement cleanup is unconfirmed: {cleanup}",
                                error.message
                            ))
                        })?;
                    }
                    let orphan = {
                        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                        if state.terminals.get(&terminal_id).is_some_and(|current| {
                            current.handle.same_worker(&replacement_worker)
                                && matches_paired_terminal(
                                    &current.session,
                                    &descriptor.session,
                                    state.proof(),
                                )
                        }) {
                            state.retire_terminal(terminal_id)
                        } else {
                            None
                        }
                    };
                    drop(orphan);
                    return Err(error);
                }
            }
            Ok(ServerResponse::ChatTerminal {
                chat_id,
                session: descriptor.session,
                snapshot,
            })
        }
        ClientRequest::ChatCommitTerminal { terminal_id } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            // The authenticated envelope pins this receipt to the saved host.
            // Terminal retirement cannot invalidate a durable commit or its retry.
            // The GUI must flush replacement of all of its source references first.
            let source_id = state
                .chat_handoffs
                .iter()
                .find_map(|(&id, receipt)| (receipt.terminal.id == terminal_id).then_some(id));
            if let Some(source_id) = source_id {
                state.chat_handoffs.remove(&source_id);
                state.inventory_generation += 1;
            }
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatClose { chat_id } => {
            let target = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                CloseTarget::capture(&state, attached(&state, client, chat_id)?)?
            };
            close_captured(host, target)
        }
        _ => Err(error("Not a native chat request")),
    }
}

/// Borrow the complete native transition only while its closing reservation is
/// alive. Ordinary terminal spawns cannot bypass native admission with an ID.
pub(super) struct TerminalViewReservation<'a> {
    kind: TerminalViewKind<'a>,
}

enum TerminalViewKind<'a> {
    Codex {
        owners: &'a paired_close::Owners,
        closing: &'a capacity::Reservation,
    },
    /// Only retained-actor recovery constructs this after authenticating the
    /// helper and committing its exact console replacement intent.
    RetainedConsole { requested: &'a Session },
}

impl TerminalViewReservation<'_> {
    pub(super) fn retained_console(requested: &Session) -> TerminalViewReservation<'_> {
        TerminalViewReservation {
            kind: TerminalViewKind::RetainedConsole { requested },
        }
    }

    pub(super) fn authorizes(&self, state: &HostState, terminal_id: TerminalId) -> bool {
        match &self.kind {
            TerminalViewKind::Codex { owners, closing } => {
                owners.target.source.id == terminal_id
                    && owners.require_locked(state, closing).is_ok()
                    && state.chats.get(&terminal_id).is_some_and(|record| {
                        record.returning_terminal
                            && record.session.launch.provider == Provider::Codex
                            && record.session.terminal.is_none()
                            && record.paired_terminal.is_none()
                    })
            }
            TerminalViewKind::RetainedConsole { requested } => {
                requested.id == terminal_id
                    && state.chat_spawning.get(&terminal_id) == Some(*requested)
                    && !state.chats.contains_key(&terminal_id)
                    && !state.native_closing.contains_key(&terminal_id)
                    && !state.chat_handoffs.contains_key(&terminal_id)
            }
        }
    }
}

/// A close capability captures one hosted attachment, not just its reusable ID.
/// Execution callers authenticate their model origin before obtaining it; UI
/// callers must pass the ordinary subscriber admission above.
#[derive(Clone)]
pub(super) struct CloseTarget {
    source: Session,
    handle: Handle,
    instance: uuid::Uuid,
}

impl CloseTarget {
    fn capture(state: &HostState, record: &Record) -> Result<Self, ProtocolError> {
        if state.shutting_down || state.chat_handoffs.contains_key(&record.session.id) {
            return Err(unavailable(
                "Native ownership is shutting down or returning to terminal",
            ));
        }
        if record.handle.is_close_only() {
            return Err(unavailable(
                "This exact retained actor is reserved for cleanup",
            ));
        }
        Ok(Self::from_record(record))
    }

    fn from_record(record: &Record) -> Self {
        Self {
            source: record.session.clone(),
            handle: record.handle.clone(),
            instance: record.execution_instance,
        }
    }

    pub(super) fn for_execution(
        state: &HostState,
        record: &Record,
        expected: &Session,
    ) -> Result<Self, ProtocolError> {
        if !crate::agent_execution::same_chat_process(&record.session, expected) {
            return Err(unavailable(
                "The agent ownership changed before close; its successor was not stopped",
            ));
        }
        Self::capture(state, record)
    }

    fn matches(&self, record: &Record) -> bool {
        record.session == self.source
            && record.execution_instance == self.instance
            && record.handle.same_attachment(&self.handle)
    }

    fn require_current(&self, host: &SharedHost) -> Result<(), ProtocolError> {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || state.chat_handoffs.contains_key(&self.source.id)
            || !state
                .chats
                .get(&self.source.id)
                .is_some_and(|record| self.matches(record))
        {
            return Err(unavailable(
                "The thread changed while its previous actor was closing.",
            ));
        }
        Ok(())
    }
}

pub(super) fn close_captured(
    host: &SharedHost,
    target: CloseTarget,
) -> Result<ServerResponse, ProtocolError> {
    let chat_id = target.source.id;
    let handle = target.handle.clone();
    let source = target.source.clone();
    let closing = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !state
            .chats
            .get(&chat_id)
            .is_some_and(|record| target.matches(record))
        {
            return Err(unavailable(
                "The exact native attachment changed before close",
            ));
        }
        capacity::Reservation::removing(host, &mut state, &source)?
    };
    super::agent_execution::preflight_retirement(host, &source).map_err(unavailable)?;
    target.require_current(host)?;
    handle.decide_detached_work(super::agent_execution::detached_work_on_close(
        host, source.id,
    ));
    if let Err(error) = handle.close_for_resume() {
        // Its Release failed. When that is because the helper died while the
        // provider runs on, say what holds the conversation; the pane offers
        // to end it.
        let refusal = registered_holder_lost_refusal(host, &source).ok().flatten();
        return Err(unavailable(refusal.unwrap_or(error)));
    }
    let snapshot = handle.snapshot(None).ok_or_else(|| {
        unavailable("Exact native final snapshot is unavailable; retirement remains unconfirmed")
    })?;
    target.require_current(host)?;
    let (retained, pending_cleanup) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let retained = state
            .chats
            .get(&chat_id)
            .filter(|record| target.matches(record))
            .map(|record| record.session.clone())
            .ok_or_else(|| {
                unavailable("The thread changed while its previous actor was closing.")
            })?;
        (retained, state.native_closing.contains_key(&chat_id))
    };
    if handle.actor_identity().is_some() {
        // The actor's final transcript is available, but its paired console
        // still needs an authenticated terminal cleanup receipt. Keep helper
        // discovery and exact close authority until both owners are proved
        // closed and their semantic retirement has committed.
        native_cleanup::close_captured(host, &retained, &target, &closing).map_err(unavailable)?;
        return Ok(ServerResponse::ChatClosed { chat_id, snapshot });
    }
    let closed_receipt = if matches!(
        retained.native_continuity,
        chat::NativeContinuity::Durable { .. }
    ) {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        chat::native_holder::closed_source(&retained, state.proof())
            .map_err(unavailable)?
            .is_some()
    } else {
        false
    };
    if pending_cleanup || closed_receipt || has_native_owner(&retained)? {
        // Closing an exited attachment proves only that local actor is
        // gone. A server preserved by an earlier failed adoption must
        // also acknowledge exact cleanup before this close can succeed.
        target.require_current(host)?;
        native_cleanup::close_captured(host, &retained, &target, &closing).map_err(unavailable)?;
        return Ok(ServerResponse::ChatClosed { chat_id, snapshot });
    }
    let terminal = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state
            .chats
            .get(&chat_id)
            .filter(|record| target.matches(record));
        let paired = record
            .and_then(|record| record.session.terminal.as_ref())
            .filter(|terminal| terminal.id == chat_id);
        match paired.and_then(|source| {
            state
                .terminals
                .get(&source.id)
                .map(|record| (source, record))
        }) {
            Some((source, record))
                if matches_paired_terminal(&record.session, source, state.proof())
                    && state.chats[&chat_id]
                        .paired_terminal
                        .as_ref()
                        .is_some_and(|expected| record.handle.same_worker(expected)) =>
            {
                Some((record.session.clone(), record.handle.clone()))
            }
            Some(_) => {
                return Err(unavailable(
                    "The paired terminal descriptor changed; its replacement was not stopped.",
                ));
            }
            None => None,
        }
    };
    // A console is part of the owned execution. Retain both records
    // until its shutdown is proved, so a failed close remains retryable.
    target.require_current(host)?;
    if let Some((_, terminal)) = &terminal {
        terminal
            .shutdown()
            .map_err(|error| unavailable(error.to_string()))?;
    }
    let record = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !closing.holds(&state, chat_id)
            || !state
                .chats
                .get(&chat_id)
                .is_some_and(|record| target.matches(record))
        {
            return Err(unavailable(
                "The thread changed while its previous actor was closing.",
            ));
        }
        if let Some((source, handle)) = &terminal
            && state.terminals.get(&source.id).is_none_or(|record| {
                !matches_paired_terminal(&record.session, source, state.proof())
                    || !record.handle.same_worker(handle)
            })
        {
            return Err(unavailable(
                "The paired terminal changed while its previous worker was closing.",
            ));
        }
        state.inventory_generation += 1;
        let record = state.chats.remove(&chat_id);
        if let Some(notifications) = &state.notifications {
            notifications.retire(chat_id);
        }
        if let Some((source, _)) = terminal {
            state.retire_terminal(source.id);
        }
        record
    };
    if let Some(record) = &record {
        // A holder that died before its Release left the allocation behind.
        let _ = return_listener_allocation(&record.session);
        super::agent_execution::retired(host, &record.session).map_err(unavailable)?;
    }
    drop(record);
    super::state::retire_recovered_upgrade_chat(host, chat_id);
    Ok(ServerResponse::ChatClosed { chat_id, snapshot })
}

pub(super) fn resolve_upgrade(
    host: &SharedHost,
    source: &Session,
) -> Result<Session, ProtocolError> {
    let (sources, evidence, session) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let record = state.chats.get(&source.id).ok_or_else(|| unavailable(
            state.native_adoption_failures.get(source).unwrap_or(
                "The exact upgraded provider is unavailable. Its recovery evidence was retained; reopen this conversation from the provider's saved sessions."
            )))?;
        if !record.handle.owns_work()
            && !((source.is_saved_claude_history()
                || crate::upgrade::floor::load().is_ok_and(|ledger| ledger.blocks_chat(source)))
                && record.handle.is_read_only_history())
        {
            return Err(unavailable(
                "Restored native provider has closed; no writer was restarted",
            ));
        }
        let mut session = record.session.clone();
        session.launch.session_id = record.handle.session_id();
        session.native_continuity = record.handle.native_continuity();
        (
            state.upgrade_chats.get(&source.id).cloned(),
            state.upgrade_evidence.clone(),
            session,
        )
    };
    if sources.as_ref().is_some_and(|sources| {
        sources
            .iter()
            .any(|previous| crate::upgrade::handoff::issued_chat_source_matches(previous, source))
    }) {
        return Ok(session);
    }
    // An unvisited pane can predate the current in-memory checkpoint's aliases.
    // Completed durable evidence remains authoritative only for this exact live
    // replacement, even when the RAM map already contains newer source proofs.
    if let Some(evidence) = evidence
        && evidence
            .recovered_sources(source, &session)
            .map_err(unavailable)?
            .is_some()
    {
        return Ok(session);
    }
    Err(error(
        "Native upgrade source does not match the exact former daemon conversation",
    ))
}

pub(super) fn paired_upgrade_recipe(
    selected: &crate::model::agent_recovery::AgentProcess,
    current: &crate::model::agent_recovery::AgentProcess,
    launch: &chat::Launch,
    confirmed_id: Option<&str>,
) -> Result<crate::model::LaunchRecipe, String> {
    if selected != current
        || current.provider != Provider::Codex
        || launch.provider != Provider::Codex
        || confirmed_id.is_none()
        || confirmed_id != launch.session_id.as_deref()
    {
        return Err("The confirmed Codex source or conversation changed before upgrade".into());
    }
    let recipe = crate::model::agent_recovery::bind_session(
        &current.recovery,
        confirmed_id.expect("checked identity"),
    )
    .ok_or("The confirmed Codex source no longer has a safe exact resume recipe")?;
    if launch.terminal_recipe().as_ref() != Some(&recipe) {
        return Err("The confirmed Codex source account or launch changed before upgrade".into());
    }
    Ok(recipe)
}

fn restore_selection(
    handle: &crate::terminal::TerminalHandle,
    launch: &chat::Launch,
    deadline: Instant,
) -> Result<PendingBinding, String> {
    let expected = launch
        .terminal_recipe()
        .ok_or("Restored Codex conversation has no exact recipe")?;
    let id = launch
        .session_id
        .as_deref()
        .ok_or("Missing restored Codex thread")?;
    loop {
        // Restore runs before the listener is bound, so a termination request is
        // observable only through this atomic; a rebuild stop must not have to
        // wait out the readiness deadline.
        if crate::daemon::termination_requested() {
            return Err("daemon termination was requested during upgrade restore".into());
        }
        if handle.is_completed() {
            return Err("Restored Codex CLI exited before loading its exact thread".into());
        }
        if Instant::now() >= deadline {
            return Err(
                "Restored Codex CLI did not load its exact thread within 30 seconds".into(),
            );
        }
        // Launchers can briefly expose no native child, and a running child
        // need not have loaded its shared thread yet. Neither is readiness.
        if let Some(process) = handle.observe_agent_process()
            && process.provider == Provider::Codex
            && crate::model::agent_recovery::resume_recipe(&process.recovery).as_ref()
                == Some(&expected)
            && let Ok((title, working_directory)) =
                crate::agent::observe::codex_preview(&process.recovery, id)
            && handle.observe_agent_process().as_ref() == Some(&process)
            && Instant::now() < deadline
        {
            return Ok(PendingBinding {
                preview: chat::BindingPreview {
                    token: uuid::Uuid::new_v4().to_string(),
                    session_id: id.to_owned(),
                    title,
                    working_directory,
                },
                process,
                expires: deadline,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// The environment a conversation started without a proven source runs with:
/// the daemon's native configuration, then configuration the user selected,
/// then the recipe's own environment, each taking precedence over the last.
fn sourceless_environment(
    daemon: BTreeMap<String, String>,
    selected: BTreeMap<String, String>,
    recipe: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = daemon;
    environment.extend(selected);
    environment.extend(recipe);
    environment
}

/// Materialize the exact constructor/process environment once, off HostLock.
/// Source execution replaces the recorded map; sourceless precedence remains
/// daemon, selected configuration, then recipe. Explicit and legacy account
/// removals are applied before any adapter/history/title constructor runs.
fn prepare_native_process_launch(
    launch: &chat::Launch,
    daemon: Option<BTreeMap<String, String>>,
    selected: &BTreeMap<String, String>,
    source: Option<&BTreeMap<String, String>>,
) -> Result<chat::Launch, ProtocolError> {
    let mut spec = chat::protocol::ProcessSpec::from(&launch.recipe);
    if let Some(daemon) = daemon {
        // A client older than this daemon, started inside another agent's
        // session, still sends that session along. Withhold it rather than
        // refuse the conversation.
        let selected = selected
            .iter()
            .filter(|(name, _)| !crate::agent::is_session_variable(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>();
        if selected.iter().any(|(name, value)| {
            !crate::model::agent_recovery::fresh_environment_key(
                launch.provider,
                name,
                value,
                false,
            )
        }) {
            return Err(error(
                "Native execution environment contains an unrelated or private variable",
            ));
        }
        spec.environment = sourceless_environment(daemon, selected, spec.environment);
    }
    if let Some(source) = source {
        spec.environment = source.clone();
    }
    let mut process_launch = launch.clone();
    process_launch.recipe.environment = spec.environment;
    process_launch.recipe.unset_environment = spec.unset_environment;
    // The effective home itself can select legacy managed-home removals.
    // Resolve those now too, so ProcessSpec::from cannot later scrub a
    // credential which a constructor already observed in the private launch.
    process_launch.recipe.unset_environment =
        crate::account::launch_credential_overrides(&process_launch.recipe);
    let removed = &process_launch.recipe.unset_environment;
    process_launch
        .recipe
        .environment
        .retain(|name, _| !removed.contains(name));
    Ok(process_launch)
}

/// The environment `restore_upgrade` will start `launch` with in the next
/// daemon. A conversation whose terminal the successor restores before it
/// restarts from that terminal; any other, including one that merely names a
/// stopped terminal, starts as `start` starts every conversation without a
/// source. Selections the UI made for it are not saved, so they are not
/// restored.
fn upgrade_environment(
    launch: &chat::Launch,
    hosted_source: bool,
    daemon: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let process_launch = prepare_native_process_launch(
        launch,
        (!hosted_source && !launch.needs_read_only_history()).then_some(daemon),
        &BTreeMap::new(),
        None,
    )
    .map_err(|error| error.message)?;
    let (mut spec, _) = chat::protocol::create(&process_launch)?;
    spec.withhold_environment();
    Ok(spec.environment)
}

pub(super) fn validate_upgrade_environment(
    launch: &chat::Launch,
    hosted_source: bool,
    handle: &Handle,
) -> Result<(), String> {
    if handle.native_continuity().retains_provider() {
        // The authenticated holder retains the original execution environment;
        // a successor reconnects and never recreates it from daemon defaults.
        return Ok(());
    }
    let restored = upgrade_environment(
        launch,
        hosted_source,
        crate::model::agent_recovery::fresh_native_environment(launch.provider, false),
    )?;
    if !handle.recovery_environment_matches(&restored) {
        return Err(
            "Native execution environment cannot survive daemon upgrade without losing selected credentials or account settings. The updater can restore this conversation stopped after confirmation; no source was stopped."
                .into(),
        );
    }
    Ok(())
}

/// Preserve the exact paired descriptor already issued to the saved pane.
/// Resume hints are live observations; they do not replace that descriptor's
/// historical metadata. The exact process/thread is checked independently.
pub(super) fn restored_terminal_source(
    issued: &crate::model::TerminalSession,
    current: &crate::model::TerminalSession,
) -> Result<crate::model::TerminalSession, String> {
    let mut owner = current.clone();
    owner.continuity = issued.continuity.clone();
    owner.resume_hint = issued.resume_hint.clone();
    if owner != *issued
        || current
            .continuity
            .live_proof()
            .is_none_or(|proof| !proof.is_complete())
    {
        return Err("The exact paired terminal owner changed during upgrade".into());
    }
    let mut replacement = issued.clone();
    replacement.continuity = current.continuity.clone();
    Ok(replacement)
}

pub(super) fn restore_upgrade(
    host: &SharedHost,
    saved: &crate::upgrade::handoff::Chat,
) -> Result<(), String> {
    if saved.source.native_continuity.retains_provider() {
        native_recovery::restore(host, 0, &saved.source)?;
        return Ok(());
    }
    if saved.launch.needs_read_only_history() {
        return Err("The saved Claude working directory is unavailable. No provider was restarted; open the saved pane to read its conversation history.".into());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut session = crate::upgrade::handoff::restored_chat(&saved.source, &saved.launch)?;
    // Every provider keeps the descriptor already issued to windows. The
    // checkpoint's normalized resume arguments are execution context only.
    let restart_launch = Some(&saved.launch);
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        session.proof = Some(state.proof().clone());
        if let Some(terminal) = session.terminal.as_mut() {
            if state.terminals.contains_key(&terminal.id) {
                let current = state
                    .descriptor(0, terminal.id)
                    .map_err(|error| error.message)?
                    .session;
                *terminal = restored_terminal_source(terminal, &current)?;
            } else {
                terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
                    recipe: session
                        .launch
                        .terminal_recipe()
                        .ok_or("Native exact recipe unavailable")?,
                    last_exit: None,
                };
            }
        }
        state
            .terminals
            .contains_key(&session.id)
            .then(|| state.proof().clone())
    };
    // A durable native-chat checkpoint already records the user's selection.
    // Re-verify its loaded thread and new source process rather than requiring
    // another UI confirmation or treating its cwd as identity.
    let selection = if session.launch.provider == Provider::Codex && source.is_some() {
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
            .get(&session.id)
            .ok_or("Restored Codex source disappeared")?
            .handle
            .clone();
        Some(restore_selection(&handle, &session.launch, deadline)?)
    } else {
        None
    };
    let response = start_with_execution(
        host,
        0,
        session,
        source,
        false,
        selection,
        Default::default(),
        None,
        restart_launch,
        None,
    )
    .map_err(|error| error.message)?;
    let id = match response {
        ServerResponse::ChatAttached { session, .. } => session.id,
        _ => return Err("Native restore did not attach its exact session".into()),
    };
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state.chats.get(&id) else {
            return Err("Restored native chat disappeared before its confirmation".into());
        };
        record.handle.clone()
    };
    // Startup, loaded-thread verification, and native confirmation share one deadline.
    loop {
        // This polls at 50 Hz: only accessors that neither rebuild the transcript
        // window nor clone a snapshot belong here, because the actor needs the
        // very store and state locks to persist what this loop is waiting for.
        if crate::daemon::termination_requested() {
            return Err("daemon termination was requested during upgrade restore".into());
        }
        if let Some(failure) = handle.failure() {
            return Err(failure);
        }
        if handle.is_completed() {
            return Err(
                "Restored native provider exited before confirming its exact conversation".into(),
            );
        }
        if let Some(actual) = handle.confirmed_session_id() {
            if Some(&actual) != saved.launch.session_id.as_ref() {
                return Err(
                    "Restored provider confirmed a different conversation; checkpoint retained"
                        .into(),
                );
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(
                "Native provider did not confirm its exact conversation within 30 seconds".into(),
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(record) = state.chats.get_mut(&saved.source.id) else {
        return Err("Restored native chat disappeared before its confirmation".into());
    };
    record.subscribers.remove(&0);
    record.observations.remove(&0);
    let mut sources = saved.previous_sources.clone();
    sources.push(crate::upgrade::handoff::ChatSource {
        source: saved.source.clone(),
        launch: saved.launch.clone(),
    });
    state.upgrade_chats.insert(saved.source.id, sources);
    Ok(())
}

pub(super) fn owns_codex_listener(session: &Session) -> bool {
    session.owns_codex_listener()
}

/// Check a paired managed presentation before the terminal worker itself can
/// stop. This is read-only; the actor path repeats its checks after teardown.
pub(super) fn preflight_for_terminal_close(
    host: &SharedHost,
    terminal_id: TerminalId,
    expected: &crate::terminal::TerminalHandle,
) -> Result<(), ProtocolError> {
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .chats
            .get(&terminal_id)
            .filter(|record| {
                (record.session.launch.provider == Provider::Codex
                    || record.owns_execution_console())
                    && record
                        .session
                        .terminal
                        .as_ref()
                        .is_some_and(|source| source.id == terminal_id)
            })
            .map(|record| {
                if !record
                    .paired_terminal
                    .as_ref()
                    .is_some_and(|handle| handle.same_worker(expected))
                    || !state
                        .terminals
                        .get(&terminal_id)
                        .is_some_and(|current| current.handle.same_worker(expected))
                {
                    return Err(unavailable(
                        "The terminal worker no longer belongs to this provider pairing",
                    ));
                }
                Ok(record.session.clone())
            })
            .transpose()?
    };
    if let Some(source) = source {
        super::agent_execution::preflight_retirement(host, &source).map_err(unavailable)?;
    }
    Ok(())
}

/// A terminal card restored after a GUI restart has no parked UI runtime, but
/// the daemon still owns its exact native actor. Explicit card close retires both.
#[cfg(test)]
pub(super) fn close_for_terminal(
    host: &SharedHost,
    terminal_id: TerminalId,
) -> Result<(), ProtocolError> {
    let handle = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state
            .chats
            .get(&terminal_id)
            .filter(|record| {
                (record.session.launch.provider == Provider::Codex
                    || record.owns_execution_console())
                    && record
                        .session
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| terminal.id == terminal_id)
            })
            .map(|record| {
                let owned = record.paired_terminal.as_ref().is_some_and(|expected| {
                    state
                        .terminals
                        .get(&terminal_id)
                        .is_some_and(|actual| actual.handle.same_worker(expected))
                });
                if !owned {
                    return Err(unavailable(
                        "The terminal worker no longer belongs to this provider pairing",
                    ));
                }
                Ok(record.handle.clone())
            })
            .transpose()?
    };
    if let Some(handle) = handle {
        close_terminal_actor(host, terminal_id, &handle)?;
    }
    Ok(())
}

/// Close only the actor inspected before leaving the host lock. A replacement
/// with the same durable ID must not be stopped or retired by an old receipt.
pub(super) fn close_terminal_actor(
    host: &SharedHost,
    terminal_id: TerminalId,
    handle: &Handle,
) -> Result<(), ProtocolError> {
    let (owners, closing) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = state
            .chats
            .get(&terminal_id)
            .filter(|record| record.handle.same_attachment(handle))
        else {
            // An old close receipt has no authority over a new attachment.
            return Ok(());
        };
        let owners = paired_close::Owners::capture(&state, record)?;
        let closing = capacity::Reservation::closing(host, &mut state, &owners.target.source)?;
        (owners, closing)
    };
    super::agent_execution::preflight_retirement(host, &owners.target.source)
        .map_err(unavailable)?;
    owners.close(host, &closing)?;
    owners.require_current(host, &closing)?;
    // A holder that died before its Release left the allocation behind.
    let _ = return_listener_allocation(&owners.target.source);
    // Preserve the exact closed owners until durable execution retirement succeeds.
    super::agent_execution::retired(host, &owners.target.source).map_err(unavailable)?;
    let removed = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        owners.require_locked(&state, &closing)?;
        // Explicit terminal close retains its completed descriptor for history
        // and the caller's exact-worker attention fence. A successful native
        // handoff removes the old pair when it publishes its replacement.
        let record = state
            .chats
            .remove(&terminal_id)
            .expect("validated exact attachment");
        state.inventory_generation += 1;
        record
    };
    drop(removed);
    super::state::retire_recovered_upgrade_chat(host, terminal_id);
    Ok(())
}

#[cfg(test)]
pub(in crate::host) mod tests;
